//! Reflex by itself (`VICTORY_REFLEX`), the part that needs no library: the
//! switch, which latency markers a frame has sent and which it owes at each
//! point of its life, and what the reports of a Reflex state average to.
//!
//! A frame here is one the title began with a wait for the frame origin
//! (`crate::frame`). It gets an index of its own there, and the Reflex sleep
//! and every marker of the frame are given a frame token made from that
//! index, on whichever thread they happen: the title's (the sleep, the
//! simulation, the input reading, the first submission) or the presenting
//! one (the end of the submissions, the present). [`Frames`] is what the
//! threads share: who holds it decides and sends, so a frame's markers leave
//! in order and once each.

use super::abi;

/// `VICTORY_REFLEX`: Reflex by itself, with or without frame generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reflex {
    /// `on` or `1`: low latency.
    On,
    /// `boost`: low latency with boost.
    Boost,
}

impl Reflex {
    /// The switch's value: unset, empty, `0` and `off` are no Reflex by
    /// itself (everything as without the switch); any other word is an
    /// error, as an unknown DLSS mode is.
    pub fn from_switch(value: Option<&str>) -> Result<Option<Self>, String> {
        let Some(value) = value else { return Ok(None) };
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" => None,
            "1" | "on" => Some(Reflex::On),
            "boost" => Some(Reflex::Boost),
            _ => return Err(format!("VICTORY_REFLEX={value}: not one of on, 1, boost, off, 0")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Reflex::On => "on",
            Reflex::Boost => "boost",
        }
    }

    /// The `sl::ReflexMode` of the switch.
    pub fn sl_mode(self) -> u32 {
        match self {
            Reflex::On => abi::REFLEX_LOW_LATENCY,
            Reflex::Boost => abi::REFLEX_LOW_LATENCY_WITH_BOOST,
        }
    }
}

/// The points of a frame's life that have a marker, in the order a frame
/// passes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The Reflex sleep at the frame's begin returned.
    SimulationStart = 0,
    /// The title took a pad or keyboard reading.
    InputSample = 1,
    /// The title's first submission of command lists: its simulation is over
    /// and its submissions begin.
    SimulationEnd = 2,
    SubmitStart = 3,
    /// The present begins on the host.
    SubmitEnd = 4,
    PresentStart = 5,
    PresentEnd = 6,
}

/// The `sl::PCLMarker` of each stage.
const ORDER: [u32; 7] = [
    abi::SIMULATION_START,
    abi::CONTROLLER_INPUT_SAMPLE,
    abi::SIMULATION_END,
    abi::RENDER_SUBMIT_START,
    abi::RENDER_SUBMIT_END,
    abi::PRESENT_START,
    abi::PRESENT_END,
];

const fn bit(stage: Stage) -> u8 {
    1 << stage as u8
}

/// Markers to send now, as bits by stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Markers(u8);

impl Markers {
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Their `sl::PCLMarker` values, in the order they are to be sent.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        ORDER.iter().enumerate().filter(move |(stage, _)| self.0 & (1 << stage) != 0).map(|(_, &marker)| marker)
    }
}

/// One frame that is not over yet.
struct Frame {
    /// The index its frame tokens are made from.
    index: u32,
    /// The token the title's origin wait gave it (0: the frame was made at a
    /// present, the title began none for it).
    origin: u64,
    /// The number of the title's present that shows it (0: none yet).
    present: u64,
    /// The stages whose markers were sent.
    sent: u8,
    /// A reading was taken after its simulation's end, with no input-sample
    /// marker sent before.
    late: bool,
}

impl Frame {
    fn has(&self, stage: Stage) -> bool {
        self.sent & bit(stage) != 0
    }

    /// The frame's simulation began (its sleep is over): from here on the
    /// title's readings, submissions and presents may be this frame's.
    fn started(&self) -> bool {
        self.has(Stage::SimulationStart)
    }

    /// The markers of every stage up to `stage` that were not sent, now
    /// counted as sent. The input sample is never owed: it is sent when the
    /// title reads, or not at all. What was sent is always a run from the
    /// first stage on, so nothing ever leaves out of order.
    fn reach(&mut self, stage: Stage) -> Markers {
        let up_to = (bit(stage) << 1).wrapping_sub(1);
        let owed = up_to & !bit(Stage::InputSample) & !self.sent;
        self.sent |= owed;
        Markers(owed)
    }
}

/// What happened since the counts were last taken (the ten-second line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Frames the title began.
    pub begun: u32,
    /// Frames that got an input-sample marker.
    pub sampled: u32,
    /// Frames whose first reading came after their simulation's end (no
    /// input-sample marker).
    pub late: u32,
    /// Frames presented without a submission of the title's before.
    pub unsubmitted: u32,
    /// Presents the title began no frame for.
    pub unbegun: u32,
    /// Presents that found their frame by the title's token, and by order.
    pub by_token: u32,
    pub by_order: u32,
    /// Frames whose simulation was ended by a submission that named them
    /// with the title's token, and by one that did not (no token, or one no
    /// frame here has): the newest running frame was taken then, which is
    /// the wrong one if the title still submits a frame after it began the
    /// next.
    pub submitted_by_token: u32,
    pub submitted_by_order: u32,
    /// Frames begun and never presented.
    pub never_presented: u32,
}

/// The frames that are not over, oldest first, and the last index given.
pub(crate) struct Frames {
    last: u32,
    live: Vec<Frame>,
    counts: Counts,
}

/// Frames kept at most (Streamline expects no more than three in flight; the
/// rest is room for frames the title begins and never presents).
const KEPT: usize = 8;

impl Frames {
    pub const fn new() -> Self {
        Frames {
            last: 0,
            live: Vec::new(),
            counts: Counts {
                begun: 0,
                sampled: 0,
                late: 0,
                unsubmitted: 0,
                unbegun: 0,
                by_token: 0,
                by_order: 0,
                submitted_by_token: 0,
                submitted_by_order: 0,
                never_presented: 0,
            },
        }
    }

    /// A new frame with the next index (indexes count from 1 and wrap).
    fn push(&mut self, origin: u64, present: u64) -> u32 {
        // The oldest goes if the title begins frames it never presents.
        if self.live.len() >= KEPT {
            self.live.remove(0);
            self.counts.never_presented += 1;
        }
        self.last = self.last.wrapping_add(1);
        self.live.push(Frame { index: self.last, origin, present, sent: 0, late: false });
        self.last
    }

    /// The title begins a frame (its origin wait made token `origin`): the
    /// frame's index, for its sleep. Nothing is the frame's until
    /// [`Frames::reach`] is told its simulation started.
    pub fn begin(&mut self, origin: u64) -> u32 {
        self.counts.begun += 1;
        self.push(origin, 0)
    }

    /// The markers frame `index` owes up to `stage`, now sent; none if the
    /// frame is over.
    pub fn reach(&mut self, index: u32, stage: Stage) -> Markers {
        self.live.iter_mut().find(|f| f.index == index).map_or(Markers(0), |f| f.reach(stage))
    }

    /// The title takes a pad or keyboard reading: the input-sample marker of
    /// the newest frame whose simulation runs, if the frame has none yet.
    /// A reading after the frame's first submission is not marked (the
    /// marker would follow the simulation's end).
    pub fn input(&mut self) -> Option<(u32, Markers)> {
        let frame = self.live.iter_mut().rev().find(|f| f.started())?;
        // One input sample a frame.
        if frame.has(Stage::InputSample) {
            return None;
        }
        if frame.has(Stage::SimulationEnd) {
            // Counted once a frame: its first reading came too late.
            if !frame.late {
                frame.late = true;
                self.counts.late += 1;
            }
            return None;
        }
        frame.sent |= bit(Stage::InputSample);
        self.counts.sampled += 1;
        Some((frame.index, Markers(bit(Stage::InputSample))))
    }

    /// The title submits command lists to a queue that draws, tagged with
    /// frame token `origin` (0: not tagged): at the frame's first submission
    /// its simulation ends and its submissions begin. The frame is the one
    /// of that token, else the newest whose simulation runs and whose
    /// present the title has not called. Which of the two ended a frame's
    /// simulation is counted, for the line.
    pub fn submit(&mut self, origin: u64) -> Option<(u32, Markers)> {
        let tagged = (origin != 0).then(|| self.live.iter().position(|f| f.origin == origin)).flatten();
        let at = match tagged {
            Some(at) => at,
            None => self.live.iter().rposition(|f| f.started() && f.present == 0)?,
        };
        let frame = &mut self.live[at];
        // (A frame still in its sleep has no simulation to end.)
        if !frame.started() {
            return None;
        }
        let owed = frame.reach(Stage::SubmitStart);
        // Later submissions of the frame owe nothing and count nothing.
        if owed.is_empty() {
            return None;
        }
        // Counted where it has an effect: this submission is the one that
        // ends the frame's simulation.
        match tagged {
            Some(_) => self.counts.submitted_by_token += 1,
            None => self.counts.submitted_by_order += 1,
        }
        Some((frame.index, owed))
    }

    /// The title calls its present number `present` (counted from 1) with
    /// frame token `origin` in its first plane (0: none): the present is
    /// that frame's, else the newest's whose simulation runs and that has no
    /// present yet. Frames begun before it that have none are over: the
    /// title never presented them.
    pub fn title_presents(&mut self, present: u64, origin: u64) {
        let tagged = (origin != 0).then(|| self.live.iter().position(|f| f.origin == origin)).flatten();
        let at = match tagged {
            Some(at) if self.live[at].started() && self.live[at].present == 0 => {
                self.counts.by_token += 1;
                at
            }
            // The token's frame has its present: this one gets a frame of
            // its own when it begins on the host.
            Some(_) => return,
            None => {
                let Some(at) = self.live.iter().rposition(|f| f.started() && f.present == 0) else { return };
                self.counts.by_order += 1;
                at
            }
        };
        self.live[at].present = present;
        let before = self.live.len();
        let mut place = 0;
        self.live.retain(|f| {
            let keep = place >= at || f.present != 0;
            place += 1;
            keep
        });
        self.counts.never_presented += (before - self.live.len()) as u32;
    }

    /// Present number `present` begins on the host: its frame's index. A
    /// present the title began no frame for (none of the title's, or two of
    /// one frame) gets a frame made here, which owes every marker.
    pub fn presenting(&mut self, present: u64) -> u32 {
        if present != 0
            && let Some(frame) = self.live.iter().find(|f| f.present == present)
        {
            if !frame.has(Stage::SubmitStart) {
                self.counts.unsubmitted += 1;
            }
            return frame.index;
        }
        self.counts.unbegun += 1;
        self.push(0, present)
    }

    /// The present of frame `index` returned: its last marker, and the frame
    /// is over.
    pub fn presented(&mut self, index: u32) -> Markers {
        let Some(at) = self.live.iter().position(|f| f.index == index) else { return Markers(0) };
        let owed = self.live[at].reach(Stage::PresentEnd);
        self.live.remove(at);
        owed
    }

    /// The counts since they were last taken.
    pub fn take_counts(&mut self) -> Counts {
        std::mem::take(&mut self.counts)
    }
}

/// A Reflex sleep longer than this, in microseconds, is said in the log,
/// once. The sleep is there to delay a frame's begin by part of a frame; a
/// quarter of a second is several whole frames at any rate the title runs
/// at.
pub(crate) const SLEEP_LONG_US: u64 = 250_000;
/// After a Reflex sleep longer than this, in microseconds, no further one is
/// asked for. The header gives the sleep no upper bound, it runs on the
/// title's thread inside its wait for the frame origin, and a frame that
/// stands for 30 seconds ends the run: two seconds is far outside anything
/// the sleep is for and well inside what the title forgives once.
pub(crate) const SLEEP_TOO_LONG_US: u64 = 2_000_000;

/// What the length of one Reflex sleep asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slept {
    /// As a sleep is meant to be.
    Fine,
    /// Longer than [`SLEEP_LONG_US`]: worth a warning.
    Long,
    /// Longer than [`SLEEP_TOO_LONG_US`]: the last one asked for.
    TooLong,
}

/// What a Reflex sleep that took `micros` microseconds asks for.
pub(crate) fn slept(micros: u64) -> Slept {
    if micros > SLEEP_TOO_LONG_US {
        Slept::TooLong
    } else if micros > SLEEP_LONG_US {
        Slept::Long
    } else {
        Slept::Fine
    }
}

/// What the frame reports of a Reflex state average to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Latency {
    /// The reports that hold a frame.
    pub frames: u32,
    /// From the simulation's start to the end of the GPU's rendering (the
    /// frame's whole way from game to render), in the unit of the report's
    /// time stamps.
    pub game_to_render: u64,
    /// The GPU's frame time and the time it rendered, in microseconds.
    pub gpu_frame_us: u64,
    pub gpu_active_render_us: u64,
}

/// The averages over the reports that hold a frame (an index, and a
/// rendering that ended after its simulation started); `None` if none does.
/// The header has no field for the total: it is the difference of the two
/// time stamps at its ends.
pub(crate) fn latency(reports: &[abi::ReflexReport]) -> Option<Latency> {
    let mut frames = 0u64;
    let mut sums = [0u64; 3];
    for report in reports {
        if report.frame_id == 0 || report.sim_start_time == 0 || report.gpu_render_end_time <= report.sim_start_time {
            continue;
        }
        frames += 1;
        sums[0] += report.gpu_render_end_time - report.sim_start_time;
        sums[1] += u64::from(report.gpu_frame_time_us);
        sums[2] += u64::from(report.gpu_active_render_time_us);
    }
    (frames > 0).then(|| Latency {
        frames: frames as u32,
        game_to_render: sums[0] / frames,
        gpu_frame_us: sums[1] / frames,
        gpu_active_render_us: sums[2] / frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(markers: Markers) -> Vec<u32> {
        markers.iter().collect()
    }

    #[test]
    fn the_switch_reads_on_boost_and_off() {
        assert_eq!(Reflex::from_switch(None), Ok(None));
        assert_eq!(Reflex::from_switch(Some("")), Ok(None));
        assert_eq!(Reflex::from_switch(Some("0")), Ok(None));
        assert_eq!(Reflex::from_switch(Some(" Off ")), Ok(None));
        assert_eq!(Reflex::from_switch(Some("1")), Ok(Some(Reflex::On)));
        assert_eq!(Reflex::from_switch(Some(" ON ")), Ok(Some(Reflex::On)));
        assert_eq!(Reflex::from_switch(Some("boost")), Ok(Some(Reflex::Boost)));
        assert!(Reflex::from_switch(Some("fast")).is_err_and(|why| why.contains("VICTORY_REFLEX=fast")));
        // The modes of `sl_reflex.h`: low latency 1, with boost 2.
        assert_eq!(Reflex::On.sl_mode(), 1);
        assert_eq!(Reflex::Boost.sl_mode(), 2);
        assert_eq!((Reflex::On.name(), Reflex::Boost.name()), ("on", "boost"));
    }

    #[test]
    fn a_frame_sends_each_marker_once_and_in_order() {
        let mut frames = Frames::new();
        let frame = frames.begin(41);
        assert_eq!(frame, 1);
        // In its sleep nothing is the frame's yet.
        assert_eq!(frames.input(), None);
        assert_eq!(frames.submit(0), None);
        assert_eq!(sent(frames.reach(frame, Stage::SimulationStart)), [abi::SIMULATION_START]);
        assert!(frames.reach(frame, Stage::SimulationStart).is_empty());
        // The first reading of the frame is its input sample.
        assert_eq!(frames.input().map(|(i, m)| (i, sent(m))), Some((frame, vec![abi::CONTROLLER_INPUT_SAMPLE])));
        assert_eq!(frames.input(), None);
        // The first submission ends the simulation and begins the submissions.
        assert_eq!(
            frames.submit(0).map(|(i, m)| (i, sent(m))),
            Some((frame, vec![abi::SIMULATION_END, abi::RENDER_SUBMIT_START]))
        );
        assert_eq!(frames.submit(0), None);
        assert_eq!(frames.submit(41), None);
        // The present: the submissions end where it begins.
        frames.title_presents(7, 41);
        assert_eq!(frames.presenting(7), frame);
        assert_eq!(sent(frames.reach(frame, Stage::SubmitEnd)), [abi::RENDER_SUBMIT_END]);
        assert_eq!(sent(frames.reach(frame, Stage::PresentStart)), [abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        // The frame is over.
        assert!(frames.presented(frame).is_empty());
        assert!(frames.reach(frame, Stage::PresentEnd).is_empty());
        let counts = frames.take_counts();
        assert_eq!((counts.begun, counts.sampled, counts.late, counts.by_token), (1, 1, 0, 1));
        assert_eq!((counts.unsubmitted, counts.unbegun, counts.by_order, counts.never_presented), (0, 0, 0, 0));
        // One submission ended the simulation, an untagged one; those after
        // it are not counted.
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (0, 1));
        assert_eq!(frames.take_counts(), Counts::default());
    }

    #[test]
    fn a_frame_that_never_submits_is_completed_at_its_present() {
        let mut frames = Frames::new();
        let frame = frames.begin(1);
        frames.reach(frame, Stage::SimulationStart);
        // (No token in the present's plane: the frame is found by order.)
        frames.title_presents(1, 0);
        assert_eq!(frames.presenting(1), frame);
        assert_eq!(
            sent(frames.reach(frame, Stage::SubmitEnd)),
            [abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END]
        );
        assert_eq!(sent(frames.reach(frame, Stage::PresentStart)), [abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        let counts = frames.take_counts();
        assert_eq!((counts.unsubmitted, counts.by_order, counts.by_token), (1, 1, 0));
    }

    #[test]
    fn a_present_with_no_frame_begun_gets_one_of_its_own() {
        let mut frames = Frames::new();
        // (As the check's presents are: no origin wait, no title present.)
        let frame = frames.presenting(5);
        assert_eq!(
            sent(frames.reach(frame, Stage::PresentStart)),
            [abi::SIMULATION_START, abi::SIMULATION_END, abi::RENDER_SUBMIT_START, abi::RENDER_SUBMIT_END, abi::PRESENT_START]
        );
        assert_eq!(sent(frames.presented(frame)), [abi::PRESENT_END]);
        // The next one has another index.
        assert_eq!(frames.presenting(6), frame + 1);
        // A second present of a frame's token does not take the frame again.
        let begun = frames.begin(9);
        frames.reach(begun, Stage::SimulationStart);
        frames.title_presents(7, 9);
        frames.title_presents(8, 9);
        assert_eq!(frames.presenting(7), begun);
        assert_ne!(frames.presenting(8), begun);
        assert_eq!(frames.take_counts().unbegun, 3);
    }

    #[test]
    fn readings_submissions_and_presents_find_their_frame() {
        let mut frames = Frames::new();
        // The title is two frames deep: 10 is being submitted while 11 is
        // simulated, and 12 is in its sleep.
        let [a, b] = [10, 11].map(|origin| {
            let frame = frames.begin(origin);
            frames.reach(frame, Stage::SimulationStart);
            frame
        });
        let c = frames.begin(12);
        // A reading is the newest running simulation's.
        assert_eq!(frames.input().map(|(i, _)| i), Some(b));
        // A tagged submission is its token's frame's, an untagged one the
        // newest running frame's; neither is the sleeping frame's.
        assert_eq!(frames.submit(10).map(|(i, _)| i), Some(a));
        assert_eq!(frames.submit(0).map(|(i, _)| i), Some(b));
        assert_eq!(frames.submit(12), None);
        // The presents arrive on the host after the title called both.
        frames.title_presents(1, 10);
        frames.title_presents(2, 11);
        assert_eq!((frames.presenting(1), frames.presenting(2)), (a, b));
        assert_eq!(sent(frames.reach(a, Stage::PresentStart)), [abi::RENDER_SUBMIT_END, abi::PRESENT_START]);
        assert_eq!(sent(frames.presented(a)), [abi::PRESENT_END]);
        // The third frame's simulation starts only now.
        assert_eq!(sent(frames.reach(c, Stage::SimulationStart)), [abi::SIMULATION_START]);
        assert_eq!(frames.submit(0).map(|(i, _)| i), Some(c));
        let counts = frames.take_counts();
        assert_eq!(counts.by_token, 2);
        // The first frame's simulation was ended by its token, the other
        // two's by order.
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (1, 2));
    }

    #[test]
    fn a_reading_after_the_simulation_end_is_not_marked() {
        let mut frames = Frames::new();
        let frame = frames.begin(1);
        frames.reach(frame, Stage::SimulationStart);
        assert!(frames.submit(1).is_some());
        assert_eq!(frames.input(), None);
        assert_eq!(frames.input(), None);
        let counts = frames.take_counts();
        assert_eq!((counts.sampled, counts.late), (0, 1));
    }

    #[test]
    fn a_submission_with_a_token_no_frame_has_is_taken_by_order() {
        let mut frames = Frames::new();
        let frame = frames.begin(5);
        frames.reach(frame, Stage::SimulationStart);
        // (A token of a frame that is over, or one the title makes itself.)
        assert_eq!(frames.submit(99).map(|(i, _)| i), Some(frame));
        let counts = frames.take_counts();
        assert_eq!((counts.submitted_by_token, counts.submitted_by_order), (0, 1));
        // With no frame running there is none to take, and none is counted.
        frames.title_presents(1, 5);
        assert_eq!(frames.submit(0), None);
        assert_eq!(frames.take_counts().submitted_by_order, 0);
    }

    #[test]
    fn a_sleep_is_fine_long_or_the_last() {
        assert_eq!(slept(0), Slept::Fine);
        assert_eq!(slept(16_000), Slept::Fine);
        assert_eq!(slept(SLEEP_LONG_US), Slept::Fine);
        assert_eq!(slept(SLEEP_LONG_US + 1), Slept::Long);
        assert_eq!(slept(SLEEP_TOO_LONG_US), Slept::Long);
        assert_eq!(slept(SLEEP_TOO_LONG_US + 1), Slept::TooLong);
        assert_eq!(slept(u64::MAX), Slept::TooLong);
        // The second bound is the larger, and far below the 30 seconds a
        // standing frame is given.
        const { assert!(SLEEP_LONG_US < SLEEP_TOO_LONG_US && SLEEP_TOO_LONG_US < 30_000_000) };
    }

    #[test]
    fn frames_never_presented_are_let_go() {
        let mut frames = Frames::new();
        let begun: Vec<u32> = (1..=3)
            .map(|origin| {
                let frame = frames.begin(origin);
                frames.reach(frame, Stage::SimulationStart);
                frame
            })
            .collect();
        // The title presents the third: the two before it are over.
        frames.title_presents(1, 3);
        assert_eq!(frames.take_counts().never_presented, 2);
        assert!(frames.reach(begun[0], Stage::PresentEnd).is_empty());
        assert_eq!(frames.presenting(1), begun[2]);
        // And however many frames begin, few are kept.
        for origin in 4..40 {
            frames.begin(origin);
        }
        assert_eq!(frames.live.len(), KEPT);
        assert_eq!(frames.take_counts().never_presented, 36 + 1 - KEPT as u32);
    }

    #[test]
    fn the_reports_average_over_their_frames() {
        let mut reports = [abi::ReflexReport::empty(); 4];
        assert_eq!(latency(&reports), None);
        for (frame, total, gpu) in [(0usize, 30_000u64, 9_000u32), (1, 50_000, 11_000)] {
            reports[frame].frame_id = frame as u64 + 1;
            reports[frame].sim_start_time = 1_000_000;
            reports[frame].gpu_render_end_time = 1_000_000 + total;
            reports[frame].gpu_frame_time_us = gpu;
            reports[frame].gpu_active_render_time_us = gpu / 2;
        }
        // A report whose rendering has no end yet does not count.
        reports[2].frame_id = 3;
        reports[2].sim_start_time = 2_000_000;
        assert_eq!(
            latency(&reports),
            Some(Latency { frames: 2, game_to_render: 40_000, gpu_frame_us: 10_000, gpu_active_render_us: 5_000 })
        );
    }
}
