//! `VirtualQuery`, by call site. Under Wine every question takes the
//! process's one address-space lock (`virtual_mutex`) that every change of
//! protection, write-watch asking, mapping and guard fault takes too: half
//! a millisecond a question while the title streams (speed-wine-20261125).
//!
//! The diagnostics build counts each site's queries and their time for the
//! frame-rate log.
//!
//! With `VICTORY_QUERY_CACHE=1`, [`coarse`] answers from what an earlier
//! question found, per 64 KiB block: the state, kind and allocation of
//! committed memory (never the protection, which the title may change
//! without telling). Only memory the runtime gave the title is kept
//! (`XMemVirtualAlloc`, physical-page views: [`came`]), and it is dropped
//! when the runtime is about to take it back or unmap it ([`gone`]). What
//! the title commits or frees without the runtime (its own `VirtualAlloc`)
//! is never kept, as committed memory turns into something else only by a
//! free, a decommit or an unmapping, which the runtime is told of first.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery};

/// Where a question comes from.
#[derive(Clone, Copy)]
pub(crate) enum Site {
    IsView,
    UncommittedEnd,
    ReadableView,
    AllocationEnd,
    Writable,
    PrivateWhole,
    InplacePart,
    CommittedByQuery,
    MappedBytes,
    ReadSparse,
    Other,
}

#[cfg(feature = "diagnostics")]
const SITES: usize = 11;
#[cfg(feature = "diagnostics")]
const NAMES: [&str; SITES] = [
    "is_view",
    "uncommitted_end",
    "readable_view",
    "allocation_end",
    "writable",
    "private_whole",
    "inplace_part",
    "committed_by_query",
    "mapped_bytes",
    "read_sparse",
    "other",
];

/// Since the last line, per site: questions put to Windows, their
/// nanoseconds, questions answered from the cache.
#[cfg(feature = "diagnostics")]
static CALLS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
#[cfg(feature = "diagnostics")]
static NS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
#[cfg(feature = "diagnostics")]
static HITS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// `VirtualQuery(at)`, counted for `site`.
pub(crate) fn query(site: Site, at: usize, info: &mut MEMORY_BASIC_INFORMATION) -> usize {
    #[cfg(not(feature = "diagnostics"))]
    let _ = site;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    #[cfg(feature = "diagnostics")]
    {
        CALLS[site as usize].fetch_add(1, Ordering::Relaxed);
        NS[site as usize].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    n
}

/// What [`coarse`] answers: of the pages from `at`'s on, `[base, end)`
/// (`end` may lie short of the region's real end), their state, kind and
/// allocation base. No protection.
pub(crate) struct Coarse {
    pub base: usize,
    pub end: usize,
    pub alloc: usize,
    pub state: u32,
    pub kind: u32,
}

impl Coarse {
    fn of(info: &MEMORY_BASIC_INFORMATION) -> Coarse {
        Coarse {
            base: info.BaseAddress as usize,
            end: (info.BaseAddress as usize).saturating_add(info.RegionSize),
            alloc: info.AllocationBase as usize,
            state: info.State,
            kind: info.Type,
        }
    }
}

fn cache_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUERY_CACHE").is_some_and(|v| v == "1"))
}

const BLOCK: usize = 64 << 10;
const KERNEL_PAGE: usize = 4096;
/// Blocks a hit's answer reaches over at most (callers ask on from its end).
const CHAIN: usize = 256;
/// Blocks one question fills in at most.
const FILL: usize = 4096;

/// A block, uniform from `from` (a page of it) to its end.
#[derive(Clone, Copy, PartialEq)]
struct Entry {
    from: usize,
    alloc: usize,
    state: u32,
    kind: u32,
}

impl Entry {
    fn same(&self, other: &Entry) -> bool {
        self.alloc == other.alloc && self.state == other.state && self.kind == other.kind
    }
}

struct Cache {
    blocks: HashMap<usize, Entry>,
    /// The runtime's memory of the title's, joined: start -> end.
    title: BTreeMap<usize, usize>,
}

impl Cache {
    /// Whether all of `[at, end)` is the title's.
    fn titles(&self, at: usize, end: usize) -> bool {
        self.title.range(..=at).next_back().is_some_and(|(_, &e)| e >= end)
    }

    fn drop_blocks(&mut self, at: usize, end: usize) {
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        if last - first > self.blocks.len() {
            self.blocks.retain(|b, _| *b < first || *b >= last);
        } else {
            for b in first..last {
                self.blocks.remove(&b);
            }
        }
    }

    /// `[at, end)` is the title's.
    fn entitle(&mut self, mut at: usize, mut end: usize) {
        // Joined with what it touches.
        let hit: Vec<(usize, usize)> =
            self.title.range(..=end).filter(|&(_, &e)| e >= at).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in hit {
            self.title.remove(&s);
            at = at.min(s);
            end = end.max(e);
        }
        self.title.insert(at, end);
    }

    /// `[at, end)` is the title's no more.
    fn untitle(&mut self, at: usize, end: usize) {
        let hit: Vec<(usize, usize)> =
            self.title.range(..end).filter(|&(_, &e)| e > at).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in hit {
            self.title.remove(&s);
            if s < at {
                self.title.insert(s, at);
            }
            if e > end {
                self.title.insert(end, e);
            }
        }
    }
}

static CACHE: std::sync::LazyLock<Mutex<Cache>> =
    std::sync::LazyLock::new(|| Mutex::new(Cache { blocks: HashMap::new(), title: BTreeMap::new() }));

/// Bumped by every change: a question put before one is not kept.
static EPOCH: AtomicU64 = AtomicU64::new(0);

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// `VirtualQuery(at)` for a caller that needs no protection; `None` if
/// Windows does not answer.
pub(crate) fn coarse(site: Site, at: usize) -> Option<Coarse> {
    if !cache_on() {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        return (query(site, at, &mut info) != 0).then(|| Coarse::of(&info));
    }
    let page = at & !(KERNEL_PAGE - 1);
    {
        let c = cache();
        let b = at / BLOCK;
        if let Some(e) = c.blocks.get(&b)
            && page >= e.from
        {
            let mut k = b + 1;
            while k < b + CHAIN && c.blocks.get(&k).is_some_and(|n| n.from == k * BLOCK && n.same(e)) {
                k += 1;
            }
            #[cfg(feature = "diagnostics")]
            HITS[site as usize].fetch_add(1, Ordering::Relaxed);
            return Some(Coarse { base: page, end: k * BLOCK, alloc: e.alloc, state: e.state, kind: e.kind });
        }
    }
    let epoch = EPOCH.load(Ordering::SeqCst);
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    if query(site, at, &mut info) == 0 {
        return None;
    }
    let answer = Coarse::of(&info);
    if answer.state == MEM_COMMIT && matches!(answer.kind, MEM_MAPPED | MEM_PRIVATE) && answer.base == page {
        let mut c = cache();
        // Nothing changed since the question, and the runtime's memory.
        if EPOCH.load(Ordering::SeqCst) == epoch {
            let first = page / BLOCK;
            let last = (answer.end / BLOCK).min(first + FILL);
            for b in first..last {
                let from = (b * BLOCK).max(page);
                if !c.titles(from, (b + 1) * BLOCK) {
                    break;
                }
                let e = Entry { from, alloc: answer.alloc, state: answer.state, kind: answer.kind };
                match c.blocks.get_mut(&b) {
                    Some(had) if had.same(&e) => had.from = had.from.min(from),
                    Some(had) => *had = e,
                    None => {
                        c.blocks.insert(b, e);
                    }
                }
            }
        }
    }
    Some(answer)
}

/// The runtime gave the title `[at, at + len)` (allocated, committed, or
/// mapped a view there): done, before it is told.
pub fn came(at: usize, len: usize) {
    if !cache_on() || len == 0 {
        return;
    }
    let mut c = cache();
    EPOCH.fetch_add(1, Ordering::SeqCst);
    let end = at.saturating_add(len);
    c.drop_blocks(at, end);
    c.entitle(at, end);
}

/// `[at, at + len)` is about to be freed, decommitted or unmapped (with no
/// length: the whole allocation at `at`).
pub fn gone(at: usize, len: usize) {
    if !cache_on() {
        return;
    }
    let end = if len != 0 {
        at.saturating_add(len)
    } else {
        // The whole allocation, still there: asked, region by region.
        let mut end = at.saturating_add(KERNEL_PAGE);
        let mut next = at;
        loop {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            if query(Site::Other, next, &mut info) == 0 || info.AllocationBase as usize != at {
                break;
            }
            let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
            if reach <= next {
                break;
            }
            (end, next) = (end.max(reach), reach);
        }
        end
    };
    let mut c = cache();
    EPOCH.fetch_add(1, Ordering::SeqCst);
    if len == 0 {
        c.blocks.retain(|_, e| e.alloc != at);
    }
    c.drop_blocks(at, end);
    c.untitle(at, end);
}

/// The frame-rate line's: per site, questions, ms and cache answers per
/// frame.
#[cfg(feature = "diagnostics")]
pub(crate) fn line(frames: f64) {
    let mut parts = Vec::new();
    let (mut calls, mut ns) = (0u64, 0u64);
    for i in 0..SITES {
        let n = CALLS[i].swap(0, Ordering::Relaxed);
        let t = NS[i].swap(0, Ordering::Relaxed);
        let h = HITS[i].swap(0, Ordering::Relaxed);
        calls += n;
        ns += t;
        if n != 0 || h != 0 {
            parts.push(format!("{} {:.0} ({:.2} ms, {:.0} cached)", NAMES[i], n as f64 / frames, t as f64 / 1e6 / frames, h as f64 / frames));
        }
    }
    tracing::info!(
        "VirtualQuery per frame: {:.0} questions, {:.2} ms (VICTORY_QUERY_CACHE is {}{}): {}",
        calls as f64 / frames,
        ns as f64 / 1e6 / frames,
        if cache_on() { "on" } else { "off" },
        if cache_on() { format!(", {} blocks kept", cache().blocks.len()) } else { String::new() },
        parts.join(", ")
    );
}
