//! The submitting thread's work in a submission, done less: two switches, both
//! off by default, and the counters that say where the thread's steps go.
//!
//! One thread prepares every submission (`queue::behind`), so what it does
//! between a list closing and the lists going to the card is the frame. Run 9
//! (foxhound-20261126-232504, ray tracing) split its 55-80 ms a frame like
//! this: asking the write watch 16 (`watch.rs`, not here), the lists' steps
//! 19-20 of which refreshes 10-11 in 38-40 flushes of 7,400 spans (6,300 of them
//! known to copy nothing), copies in the card's memory 5-6, planning steps for
//! the card 4-5, and the rest waiting for flights. This file is for the part
//! of the lists' steps that `host.rs` does itself.
//!
//! - [`spread`] (`VICTORY_REFRESH_PROBE_US`): a small flush is tried here first.
//! - [`Declared`] (`VICTORY_DECLARE_ACROSS_LISTS=1`): a range shaders write is
//!   declared once to a copy per submission, not once per list.
//! - [`ours`] and [`declared`] only count, and only while a switch of this file
//!   is on ([`counting`], [`declare_across`]): with all unset the steps are not
//!   timed and nothing is counted.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long a small flush of refreshes is worked on this thread alone before
/// the rest goes to the kept threads (`VICTORY_REFRESH_PROBE_US=<microseconds>`,
/// 1 to 5000; unset, 0 or anything else: every flush of four or more goes to the
/// threads at once, as before).
///
/// Handing work to the kept threads costs the submitting thread a wake for each
/// (a condition variable and the lock behind it) and a wait at the end until
/// the last of them is done, whether or not they had anything to do. A frame
/// has 38-40 flushes (run 9), most of them a few dozen spans after the ones
/// known to copy nothing are left out, so a flush takes 0.27 ms of the thread
/// on average. Earlier notes found the refreshes' rate the same (2-3 GB/s over
/// all threads) with and without ray tracing, so the time per flush is not
/// memory speed; how much of it is the wake and the wait is a guess until a
/// run with the switch shows the line's "done on this thread alone". With the
/// switch a flush goes through its refreshes one after the other here until the
/// probe has run out and only then hands the rest over, so a flush that is
/// small is done without any wake and a flush that is not loses at most the
/// probe and the one refresh it was in.
///
/// Where the bytes are does not change: title memory is read into the copy by
/// the same refresh, once; only which thread calls it does. Flushes with
/// textures due (long, hashed whole) and the flush of the heap's thousands of
/// copies go to the threads at once, as before.
fn probe() -> Option<Duration> {
    static PROBE: OnceLock<Option<Duration>> = OnceLock::new();
    *PROBE.get_or_init(|| probe_of(std::env::var("VICTORY_REFRESH_PROBE_US").ok().as_deref()))
}

/// [`probe`] for a setting.
fn probe_of(setting: Option<&str>) -> Option<Duration> {
    let us: u64 = setting?.trim().parse().ok()?;
    // A probe past a few milliseconds is a flush done here whatever it is.
    (us > 0).then(|| Duration::from_micros(us.min(5_000)))
}

/// Since the last frame-rate line: flushes that were done on this thread
/// alone, those that went on to the threads after the probe, the nanoseconds
/// spent here before either, and the refreshes done here.
static PROBED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Bytes of title memory a due texture may read and still count as light for
/// [`textures_small`].
const SMALL_TEXTURE: usize = 256 << 10;

/// Whether a flush with only light textures due still counts as small for
/// [`spread`] (`VICTORY_REFRESH_PROBE_TEXTURES=1`; off by default: a flush with
/// any texture due goes to the threads at once, as before). Only meaningful
/// with [`probe`] set, so it is off without it.
///
/// 30 of the 36.5 flushes a frame in run12-live went straight to the kept
/// threads, most because a texture was due or the flush was over 256 spans. A
/// wake and a barrier for work of a few dozen microseconds is what the probe
/// was written to avoid, and a texture that reads under 256 KiB, or was read
/// ahead already, costs about as little as a few copies. The probe still bounds
/// the time on this thread (its microseconds, plus the one refresh in progress,
/// which for these textures is small). A texture refresh that blocks on another
/// thread's hold of the same texture can run past the probe, since the probe is
/// only looked at between refreshes; the line's "waiting for a texture another
/// thread is refreshing" says whether that happens.
fn probe_textures() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| probe().is_some() && textures_of(std::env::var("VICTORY_REFRESH_PROBE_TEXTURES").ok().as_deref()))
}

/// [`probe_textures`] for a setting: only "1".
fn textures_of(setting: Option<&str>) -> bool {
    setting == Some("1")
}

/// Since the last frame-rate line: flushes with textures due that were taken
/// as small only because the textures were light.
static TEXTURES_PROBED: AtomicU64 = AtomicU64::new(0);

/// Whether every texture in `due` is light enough ([`SMALL_TEXTURE`]) for the
/// flush to be tried on this thread first. False with the switch off, so the
/// caller's own test stands as it was. Counts a yes for a non-empty `due` when
/// with the `spans` the flush has two or more pieces: with fewer it never went
/// to the threads, so the textures did not change where it ran.
pub(super) fn textures_small(due: &[std::sync::Arc<crate::texture::Staging>], spans: usize) -> bool {
    if !probe_textures() || !due.iter().all(|t| t.cheap(SMALL_TEXTURE)) {
        return false;
    }
    if !due.is_empty() && spans + due.len() >= 2 {
        TEXTURES_PROBED.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Since the last frame-rate line (`VICTORY_REFRESH_SPLIT=1`): flushes with
/// nothing left to do after the checks, flushes handed straight to the threads
/// (one or more to do, or not small), the nanoseconds in those calls, and the
/// refreshes in them.
static SPLIT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// What [`spread_with`] did.
#[derive(Debug, PartialEq, Eq)]
struct Spread {
    /// How many of the `n` were done on this thread before the rest left it.
    here: usize,
    /// Whether the rest went to the kept threads.
    went: bool,
}

/// Calls `work` with every number below `n`, on this thread while the clock
/// says less than `probe` has passed since the first, and with what is left on
/// the kept threads and this one (`crate::shadow::wide_each`).
///
/// With no `probe`, or `small` false, or fewer than two to do: all of it goes
/// to `wide_each` as it did.
fn spread_with(n: usize, small: bool, probe: Option<Duration>, work: impl Fn(usize) + Send + Sync + 'static) -> Spread {
    let Some(probe) = probe.filter(|_| small && n >= 2) else {
        crate::shadow::wide_each(n, work);
        return Spread { here: 0, went: true };
    };
    let started = std::time::Instant::now();
    let here = serial_first(n, probe, || started.elapsed(), &work);
    if here >= n {
        return Spread { here, went: false };
    }
    // What the thread did not get to: numbers from `here` on.
    crate::shadow::wide_each(n - here, move |i| work(here + i));
    Spread { here, went: true }
}

/// Calls `work` with 0, 1, ... while `elapsed` says the probe has not run out;
/// at least once. How many it called, `n` if it called all.
fn serial_first(n: usize, probe: Duration, mut elapsed: impl FnMut() -> Duration, work: &impl Fn(usize)) -> usize {
    let mut i = 0;
    while i < n {
        work(i);
        i += 1;
        if i < n && elapsed() >= probe {
            break;
        }
    }
    i
}

/// [`spread_with`] under [`probe`], counted. `small`: the caller's flush is one
/// the probe is for (few spans, and no textures due, or with
/// `VICTORY_REFRESH_PROBE_TEXTURES=1` only light ones: [`textures_small`]).
pub(super) fn spread(n: usize, small: bool, work: impl Fn(usize) + Send + Sync + 'static) {
    let Some(probe) = probe().filter(|_| small && n >= 2) else {
        // (with the split switch off no clock is read and no counter touched.)
        if !crate::shadow::refresh_split() {
            crate::shadow::wide_each(n, work);
            return;
        }
        if n == 0 {
            SPLIT[0].fetch_add(1, Ordering::Relaxed);
            crate::shadow::wide_each(n, work);
            return;
        }
        let started = std::time::Instant::now();
        crate::shadow::wide_each(n, work);
        SPLIT[1].fetch_add(1, Ordering::Relaxed);
        SPLIT[2].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLIT[3].fetch_add(n as u64, Ordering::Relaxed);
        return;
    };
    let started = std::time::Instant::now();
    let done = spread_with(n, small, Some(probe), work);
    // (The time is the whole call: the part on this thread is what the probe
    // bounds, and when the rest went out the call is longer. The line says so.)
    let k = usize::from(done.went);
    PROBED[k].fetch_add(1, Ordering::Relaxed);
    PROBED[2].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    PROBED[3].fetch_add(done.here as u64, Ordering::Relaxed);
}

/// Whether a range shaders write is declared to a host copy once for a whole
/// submission (`VICTORY_DECLARE_ACROSS_LISTS=1`; off by default: once for each
/// list, as before).
///
/// A list declares a range once until it next writes title memory
/// (`HostList::defer_gpu_write`), but a submission has ten to sixteen lists and
/// each declares the ranges it reaches again: the world's lists name the
/// views of the title's one big heap dozens of times over, and
/// [`super::declared_writes`] already counts each once for the copy-back. At
/// the submission each repeat is a call of `Shadow::writing`, which takes the
/// copy's lock, looks through the ranges already declared (a list, one by one)
/// and, with the relay, takes a second lock to count the range for this
/// submission; for a range declared already it then changes nothing. The
/// repeat is left out here, behind the copy's pending refresh, which is still
/// done where it was (`Batch::flush_one`): a submission's later refreshes of a
/// declared range are made around it either way.
///
/// What ends the counting is what ends it for a list: a step that writes
/// title memory ([`super::Batch::title_written`]). Not measured in a run: the
/// line says how many of the declarations were repeats.
pub(super) fn declare_across() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DECLARE_ACROSS_LISTS").is_some_and(|v| v == "1"))
}

/// The ranges (copy by address, address, bytes) declared to host copies since a
/// submission last wrote title memory.
#[derive(Default)]
pub(super) struct Declared(std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<super::AddressHash>>);

impl Declared {
    /// Notes the range; whether it is the first time since the last [`Declared::clear`].
    pub(super) fn first(&mut self, copy: usize, address: usize, len: usize) -> bool {
        self.0.insert((copy, address, len))
    }

    /// Title memory was written: what was declared before has to be again.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}

/// Since the last frame-rate line: ranges shaders write, as the submissions'
/// lists declared them, and those of them that were repeats left out.
static DECLARED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Counts a declaration (`repeat`: left out). Called only with [`declare_across`]
/// on, so with the switch unset a declaration costs no counting at all.
pub(super) fn declared(repeat: bool) {
    DECLARED[0].fetch_add(1, Ordering::Relaxed);
    if repeat {
        DECLARED[1].fetch_add(1, Ordering::Relaxed);
    }
}

/// Since the last frame-rate line: steps of ours the lists run on this thread
/// (`Step::Run` and `Step::Host`: what the memory's landing, the heaps' records
/// and the like are), and their nanoseconds.
static OURS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether the steps of ours are timed: only while one of this file's switches is
/// on, so that with all of them unset the submitting thread reads no clock and
/// writes no counter for them. One load of a cell read once.
pub(super) fn counting() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| declare_across() || probe().is_some())
}

/// Counts a step of ours that took `took` (the caller timed it under [`counting`]).
pub(super) fn ours(took: Duration) {
    OURS[0].fetch_add(1, Ordering::Relaxed);
    OURS[1].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
}

/// The counters, per frame, for the frame-rate line (called from
/// `submit_split_line`): what the lists' steps that are not refreshes, copies
/// or fills come to, and what the two switches did.
pub(super) fn line(frames: f64) {
    let frames = frames.max(1.0);
    let o: [f64; 2] = std::array::from_fn(|i| OURS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let d: [f64; 2] = std::array::from_fn(|i| DECLARED[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let p: [f64; 4] = std::array::from_fn(|i| PROBED[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let sp: [f64; 4] = std::array::from_fn(|i| SPLIT[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let textures = TEXTURES_PROBED.swap(0, Ordering::Relaxed) as f64 / frames;
    if o[0] + d[0] + p[0] + p[1] + sp[0] + sp[1] <= 0.0 {
        return;
    }
    let across = if declare_across() {
        format!("{:.0} of them repeats within a submission left out (VICTORY_DECLARE_ACROSS_LISTS=1)", d[1])
    } else {
        "VICTORY_DECLARE_ACROSS_LISTS is off".to_string()
    };
    let mut probed = match probe() {
        Some(probe) => format!(
            "{:.1} small flushes done on this thread alone and {:.1} that went on to the threads, {:.2} ms in all these calls, \
             {:.0} refreshes made here (VICTORY_REFRESH_PROBE_US={})",
            p[0],
            p[1],
            p[2] / 1e6,
            p[3],
            probe.as_micros()
        ),
        None => "VICTORY_REFRESH_PROBE_US is off".to_string(),
    };
    if probe_textures() {
        probed.push_str(&format!(
            ", {textures:.1} flushes taken as small only because their due textures were light (VICTORY_REFRESH_PROBE_TEXTURES=1)"
        ));
    }
    tracing::info!(
        "the lists' other steps, per frame, the submitting thread: {:.1} steps of ours in {:.1} ms; {:.0} ranges shaders write \
         declared to host copies, {across}; {probed}",
        o[0],
        o[1] / 1e6,
        d[0]
    );
    if crate::shadow::refresh_split() {
        tracing::info!(
            "refresh flushes, per frame (VICTORY_REFRESH_SPLIT=1): {:.1} had nothing left after the checks, {:.1} went straight to the threads ({:.2} ms in the calls, {:.0} refreshes among them)",
            sp[0],
            sp[1],
            sp[2] / 1e6,
            sp[3]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// A table of how many times each number was called.
    fn tally(n: usize) -> Arc<Vec<AtomicUsize>> {
        Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect())
    }

    #[test]
    fn the_texture_switch_is_one_and_nothing_else() {
        assert!(textures_of(Some("1")));
        assert!(!textures_of(None));
        assert!(!textures_of(Some("")));
        assert!(!textures_of(Some("0")));
        assert!(!textures_of(Some("true")));
    }

    #[test]
    fn the_probe_is_a_number_of_microseconds_or_nothing() {
        assert_eq!(probe_of(None), None);
        assert_eq!(probe_of(Some("")), None);
        assert_eq!(probe_of(Some("0")), None);
        assert_eq!(probe_of(Some("fast")), None);
        assert_eq!(probe_of(Some("-5")), None);
        assert_eq!(probe_of(Some(" 150 ")), Some(Duration::from_micros(150)));
        // A probe is bounded.
        assert_eq!(probe_of(Some("90000000")), Some(Duration::from_micros(5_000)));
    }

    #[test]
    fn the_thread_works_until_the_probe_has_run_out_and_at_least_once() {
        let called = tally(10);
        let work = {
            let called = called.clone();
            move |i: usize| {
                called[i].fetch_add(1, Ordering::Relaxed);
            }
        };
        // A clock that has run out at once: the first is still done.
        let here = serial_first(10, Duration::from_micros(1), || Duration::from_secs(1), &work);
        assert_eq!(here, 1);
        // A clock that never does.
        let here = serial_first(10, Duration::from_secs(1), || Duration::ZERO, &work);
        assert_eq!(here, 10);
        // One that runs out after three looks.
        let mut looks = 0;
        let here = serial_first(
            10,
            Duration::from_micros(30),
            || {
                looks += 1;
                Duration::from_micros(10 * looks)
            },
            &work,
        );
        assert_eq!(here, 3);
        assert_eq!(called[0].load(Ordering::Relaxed), 3);
        assert_eq!(called[3].load(Ordering::Relaxed), 1);
        assert_eq!(called[9].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn every_number_is_called_once_whoever_does_it() {
        for (n, small, probe) in [
            (0, true, Some(Duration::from_secs(1))),
            (1, true, Some(Duration::from_secs(1))),
            (2, true, None),
            (37, false, Some(Duration::from_secs(1))),
            // All here: the probe is long.
            (37, true, Some(Duration::from_secs(60))),
            // Next to none here: the probe has run out after the first.
            (37, true, Some(Duration::from_nanos(1))),
            (500, true, Some(Duration::from_nanos(1))),
        ] {
            let called = tally(n);
            let work = {
                let called = called.clone();
                move |i: usize| {
                    called[i].fetch_add(1, Ordering::Relaxed);
                }
            };
            let done = spread_with(n, small, probe, work);
            assert!(called.iter().all(|c| c.load(Ordering::Relaxed) == 1), "n {n}, small {small}, probe {probe:?}");
            // Where it went.
            match (n, small, probe) {
                (37, true, Some(p)) if p >= Duration::from_secs(60) => assert_eq!(done, Spread { here: 37, went: false }),
                (37 | 500, true, Some(_)) => assert!(done.went && done.here >= 1),
                _ => assert!(done.went && done.here == 0),
            }
        }
    }

    #[test]
    fn a_range_is_declared_again_after_title_memory_is_written() {
        let mut seen = Declared::default();
        assert!(seen.first(0x1000, 0, 64));
        assert!(!seen.first(0x1000, 0, 64));
        // Another copy, another place, another length: not the same range.
        assert!(seen.first(0x2000, 0, 64));
        assert!(seen.first(0x1000, 64, 64));
        assert!(seen.first(0x1000, 0, 128));
        seen.clear();
        assert!(seen.first(0x1000, 0, 64));
    }
}
