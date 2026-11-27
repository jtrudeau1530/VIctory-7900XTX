//! Prepare a bounded supply of exact-size host allocations for title buffers.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::{HOST, Host, Spare};

const ALIGN: u64 = 64 << 10;
const MAX_BUFFER: u64 = 4 << 20;
const MAX_SPARE: u64 = 64 << 20;
const MAX_PENDING: usize = 128;

type Mapped = (ID3D12Resource, usize);
static SPARE: Mutex<Spare<Mapped>> = Mutex::new(Spare::new());
static SEND: OnceLock<Option<SyncSender<u64>>> = OnceLock::new();
static QUEUED: AtomicU64 = AtomicU64::new(0);
static PREPARED: AtomicU64 = AtomicU64::new(0);
static USED: AtomicU64 = AtomicU64::new(0);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let enabled = std::env::var_os("VICTORY_BUFFER_PREFETCH").is_some_and(|value| value == "1");
        tracing::info!(
            "title buffer prefetch (VICTORY_BUFFER_PREFETCH={})",
            u8::from(enabled)
        );
        enabled
    })
}

/// Queue a size from a title buffer declaration. A request need not survive
/// the buffer's release: its allocation can serve another buffer of that
/// size. The worker never sees the resource pointer or reads title memory.
pub(crate) fn request(len: u64) {
    if !(1..=MAX_BUFFER).contains(&len) || !enabled() {
        return;
    }
    let Some(host) = HOST.get().and_then(Option::as_ref) else {
        return;
    };
    let sender = SEND.get_or_init(|| {
        let (sender, receiver) = sync_channel(MAX_PENDING);
        match std::thread::Builder::new()
            .name("victory-buffer-ahead".into())
            .spawn(move || prepare(host, receiver))
        {
            Ok(_) => Some(sender),
            Err(error) => {
                tracing::warn!(%error, "could not start title buffer prefetch");
                None
            }
        }
    });
    if sender
        .as_ref()
        .is_some_and(|sender| sender.try_send(len.next_multiple_of(ALIGN)).is_ok())
    {
        QUEUED.fetch_add(1, Relaxed);
    }
}

fn prepare(host: &'static Host, receiver: Receiver<u64>) {
    crate::queue::no_drain_here();
    for size in receiver {
        // This is the sole producer, so no other thread can increase the
        // spare byte count while this allocation is being made.
        if SPARE.lock().unwrap_or_else(|e| e.into_inner()).bytes + size > MAX_SPARE {
            continue;
        }
        let Some((buffer, data)) = host.shared_buffer(size) else {
            continue;
        };
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        if spare
            .give((buffer, data as usize), size, MAX_SPARE)
            .is_none()
        {
            PREPARED.fetch_add(1, Relaxed);
        }
    }
}

/// Consume a fresh allocation for the ordinary shared-buffer fallback.
pub(super) fn take(size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if size > MAX_BUFFER || !enabled() {
        return None;
    }
    let (buffer, data) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)?;
    USED.fetch_add(1, Relaxed);
    Some((buffer, data as *mut u8))
}

pub(super) fn report() {
    let queued = QUEUED.load(Relaxed);
    if queued == 0 {
        return;
    }
    let prepared = PREPARED.load(Relaxed);
    let used = USED.load(Relaxed);
    let spare_mib = SPARE.lock().unwrap_or_else(|e| e.into_inner()).bytes >> 20;
    tracing::info!(
        "title buffer prefetch since launch: {queued} queued, {prepared} made, {used} used, {spare_mib} MiB spare"
    );
}

#[cfg(test)]
mod tests {
    use super::{PREPARED, Relaxed, USED, enabled, request, take};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "requires D3D12 hardware and VICTORY_BUFFER_PREFETCH=1 in an isolated process"]
    fn takes_a_prepared_buffer_only_at_its_exact_size() {
        assert!(enabled());
        let host = super::super::host().expect("D3D12 host");
        request(65_537);
        let deadline = Instant::now() + Duration::from_secs(5);
        while PREPARED.load(Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(PREPARED.load(Relaxed), 1);
        assert!(take(1 << 16).is_none());
        let (_buffer, data) = host
            .title_buffer(2 << 16)
            .expect("prepared host allocation");
        assert_eq!(USED.load(Relaxed), 1);
        assert!(take(2 << 16).is_none());
        // SAFETY: the mapping is live and two 64 KiB pages long.
        unsafe {
            std::ptr::write_volatile(data, 0x7a);
            std::ptr::write_volatile(data.add((2 << 16) - 1), 0x42);
            assert_eq!(std::ptr::read_volatile(data), 0x7a);
            assert_eq!(std::ptr::read_volatile(data.add((2 << 16) - 1)), 0x42);
        }
        eprintln!("title buffer consumed one exact-size prefetched mapping");
    }
}
