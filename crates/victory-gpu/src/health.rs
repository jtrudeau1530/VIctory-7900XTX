//! Frame health (`VICTORY_FRAME_HEALTH=1` or `=<path.csv>`, off by
//! default, read once at the start): one CSV row per accepted present with
//! pipeline misses and texture counts, and a line on stderr every five
//! seconds. With it off nothing is counted; with it on the hot
//! paths add one relaxed atomic add per draw or dispatch not run, and each
//! present one video-memory query and one buffered row.
//!
//! Columns ([`HEADER`]): the frame number, its ms since the previous
//! present; draws and compute dispatches run, and those not run because
//! their host pipeline was still being made (`compiling`) or will never be
//! (`refused`: a shader that does not translate, a pipeline the driver
//! refused, a skipped shader, no pipeline set); dispatches left out on
//! purpose because they only serve rays (the ray-tracing
//! structure builder off); textures picked by index that were not on the
//! card at the last pass (`textures_empty_or_unready`: written by the title
//! and due, plus never uploaded for want of their memory), and of the due
//! ones those held back because the card had no room
//! (`textures_waiting_room`, 0 when the pass had room); the card's memory
//! in use and the system's budget for it, MB. Indirect executions dropped
//! for another reason than their pipeline are not counted here.

use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(crate) const HEADER: &str = "frame,ms,draws_ok,draws_dropped_compiling,draws_dropped_refused,compute_ok,\
compute_dropped_compiling,compute_dropped_refused,rt_skipped,textures_empty_or_unready,textures_waiting_room,vram_mb,vram_budget_mb";

/// What a draw or dispatch not run was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Missed {
    DrawCompiling,
    DrawRefused,
    ComputeCompiling,
    ComputeRefused,
    /// Left out on purpose: it only serves rays.
    Rays,
}

static DROPS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Where rows go: the file named by the switch, else
/// `victory-frame-health.csv` in the working folder.
fn path_of(value: &str) -> Option<std::path::PathBuf> {
    match value.trim() {
        "" | "0" => None,
        "1" => Some("victory-frame-health.csv".into()),
        path => Some(path.into()),
    }
}

/// Whether the switch is on (one load once it has been read).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_FRAME_HEALTH").ok().as_deref().and_then(path_of).is_some())
}

/// Counts one draw or dispatch not run (nothing with the switch off).
#[inline]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn note(what: Missed) {
    if on() {
        DROPS[what as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// One frame's row.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Row {
    pub(crate) frame: u64,
    pub(crate) ms: f64,
    pub(crate) draws_ok: u64,
    pub(crate) draws_compiling: u64,
    pub(crate) draws_refused: u64,
    pub(crate) compute_ok: u64,
    pub(crate) compute_compiling: u64,
    pub(crate) compute_refused: u64,
    pub(crate) rt_skipped: u64,
    pub(crate) textures_unready: u64,
    pub(crate) textures_room: u64,
    pub(crate) vram_mb: u64,
    pub(crate) budget_mb: u64,
}

impl Row {
    pub(crate) fn csv(&self) -> String {
        format!(
            "{},{:.2},{},{},{},{},{},{},{},{},{},{},{}",
            self.frame,
            self.ms,
            self.draws_ok,
            self.draws_compiling,
            self.draws_refused,
            self.compute_ok,
            self.compute_compiling,
            self.compute_refused,
            self.rt_skipped,
            self.textures_unready,
            self.textures_room,
            self.vram_mb,
            self.budget_mb
        )
    }

    /// Draws and dispatches that missed their host pipeline.
    fn missing(&self) -> u64 {
        self.draws_compiling + self.draws_refused + self.compute_compiling + self.compute_refused
    }
}

/// The five-second summary.
#[derive(Default)]
pub(crate) struct Window {
    frames: u64,
    ms: f64,
    worst_ms: f64,
    incomplete: u64,
    sum: Row,
    unready_max: u64,
    room_max: u64,
    vram_max: u64,
    budget: u64,
}

impl Window {
    pub(crate) fn add(&mut self, r: &Row) {
        self.frames += 1;
        self.ms += r.ms;
        self.worst_ms = self.worst_ms.max(r.ms);
        self.incomplete += u64::from(r.missing() > 0);
        let s = &mut self.sum;
        s.draws_ok += r.draws_ok;
        s.draws_compiling += r.draws_compiling;
        s.draws_refused += r.draws_refused;
        s.compute_ok += r.compute_ok;
        s.compute_compiling += r.compute_compiling;
        s.compute_refused += r.compute_refused;
        s.rt_skipped += r.rt_skipped;
        self.unready_max = self.unready_max.max(r.textures_unready);
        self.room_max = self.room_max.max(r.textures_room);
        self.vram_max = self.vram_max.max(r.vram_mb);
        self.budget = r.budget_mb;
    }

    /// The line, `None` before any frame.
    pub(crate) fn line(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let n = self.frames as f64;
        let per = |v: u64| v as f64 / n;
        let s = &self.sum;
        let verdict = if self.incomplete == 0 { "COMPLETE" } else { "INCOMPLETE" };
        Some(format!(
            "frame health: {verdict} {} frames, {:.1} FPS (worst {:.0} ms); {} frames missing draws or dispatches; \
             per frame: draws {:.0} ok, {:.1} compiling, {:.1} refused; compute {:.0} ok, {:.1} compiling, {:.1} refused; \
             rays left out {:.0}; textures not ready max {}, waiting for room max {}; VRAM max {} of {} MB",
            self.frames,
            n * 1000.0 / self.ms.max(1e-3),
            self.worst_ms,
            self.incomplete,
            per(s.draws_ok),
            per(s.draws_compiling),
            per(s.draws_refused),
            per(s.compute_ok),
            per(s.compute_compiling),
            per(s.compute_refused),
            per(s.rt_skipped),
            self.unready_max,
            self.room_max,
            self.vram_max,
            self.budget,
        ))
    }
}

/// Counters read at the previous present.
#[cfg_attr(not(windows), allow(dead_code))]
struct Sink {
    out: Option<std::io::BufWriter<std::fs::File>>,
    last: Instant,
    drawn: u64,
    dispatched: u64,
    full: u64,
    window: Window,
    since: Instant,
}

#[cfg_attr(not(windows), allow(dead_code))]
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// A frame was presented: its row from what was counted since the last
/// one. `drawn`/`dispatched` are the running totals of run draws and
/// dispatches, `full` the texture passes that found no room so far,
/// `queue` the texture queue census (`crate::draw::HEAP_TEXTURES_QUEUE`),
/// `memory` the card's (use, budget) in bytes.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn presented(frame: u64, drawn: u64, dispatched: u64, full: u64, queue: [u64; 5], memory: Option<(u64, u64)>) {
    if !on() {
        return;
    }
    let now = Instant::now();
    let mut sink = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let s = sink.get_or_insert_with(|| {
        let path = std::env::var("VICTORY_FRAME_HEALTH").ok().as_deref().and_then(path_of);
        let out = path.as_ref().and_then(|p| match std::fs::File::create(p) {
            Ok(f) => {
                let mut w = std::io::BufWriter::new(f);
                let _ = writeln!(w, "{HEADER}");
                tracing::info!(path = %p.display(), "frame health rows (VICTORY_FRAME_HEALTH)");
                Some(w)
            }
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "frame health: cannot write the rows, summary only");
                None
            }
        });
        Sink { out, last: now, drawn, dispatched, full, window: Window::default(), since: now }
    });
    let take = |d: Missed| DROPS[d as usize].swap(0, Ordering::Relaxed);
    let no_room = full > s.full;
    let row = Row {
        frame,
        ms: now.duration_since(s.last).as_secs_f64() * 1000.0,
        draws_ok: drawn.saturating_sub(s.drawn),
        draws_compiling: take(Missed::DrawCompiling),
        draws_refused: take(Missed::DrawRefused),
        compute_ok: dispatched.saturating_sub(s.dispatched),
        compute_compiling: take(Missed::ComputeCompiling),
        compute_refused: take(Missed::ComputeRefused),
        rt_skipped: take(Missed::Rays),
        textures_unready: queue[1] + queue[2],
        textures_room: if no_room { queue[1] } else { 0 },
        vram_mb: memory.map_or(0, |m| m.0 >> 20),
        budget_mb: memory.map_or(0, |m| m.1 >> 20),
    };
    (s.last, s.drawn, s.dispatched, s.full) = (now, drawn, dispatched, full);
    if let Some(out) = &mut s.out {
        let _ = writeln!(out, "{}", row.csv());
    }
    s.window.add(&row);
    if now.duration_since(s.since) >= Duration::from_secs(5) {
        if let Some(line) = s.window.line() {
            eprintln!("{line}");
        }
        if let Some(out) = &mut s.out {
            let _ = out.flush();
        }
        (s.window, s.since) = (Window::default(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_the_header_and_the_summary_flags_missing_work() {
        assert_eq!(path_of("0"), None);
        assert_eq!(path_of(""), None);
        assert_eq!(path_of("1"), Some("victory-frame-health.csv".into()));
        assert_eq!(path_of(" C:\\h.csv "), Some("C:\\h.csv".into()));
        let mut row = Row { frame: 7, ms: 98.456, draws_ok: 3000, compute_ok: 1200, vram_mb: 13800, budget_mb: 14862, ..Default::default() };
        assert_eq!(row.csv(), "7,98.46,3000,0,0,1200,0,0,0,0,0,13800,14862");
        assert_eq!(row.csv().split(',').count(), HEADER.split(',').count());
        let mut w = Window::default();
        assert_eq!(w.line(), None);
        w.add(&row);
        assert!(w.line().unwrap().starts_with("frame health: COMPLETE 1 frames"));
        row.rt_skipped = 40;
        w.add(&row);
        assert!(w.line().unwrap().contains(" 0 frames missing"));
        row.draws_compiling = 2;
        w.add(&row);
        let line = w.line().unwrap();
        assert!(line.starts_with("frame health: INCOMPLETE 3 frames"), "{line}");
        assert!(line.contains(" 1 frames missing") && line.contains("VRAM max 13800 of 14862 MB"), "{line}");
    }
}
