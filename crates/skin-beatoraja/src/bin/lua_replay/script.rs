//! The frames a replay plays: a self-made chart, stepped by the evaluator
//! with a scripted input log, each frame built as the player's view gives
//! it (`game::render`'s fields: tally, gauges, note facts, input,
//! completion).
//!
//! A script is a play state: its phases (loading, the practice page and
//! rounds) share one chart and one state clock (s7-06e-2-design.md §3).

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, Evaluator, InputKind, InputStart, JudgeEvent,
    PhysicalInputEvent, Section, SourceId, SourceProfile, Tally,
};

/// One script: the chart's note count, the state clock at its first
/// phase, and its phases.
#[derive(Clone, Debug)]
pub struct Script {
    pub name: &'static str,
    pub notes: usize,
    /// The state clock (µs) when the first phase begins.
    pub origin: i64,
    pub phases: Vec<Phase>,
    /// The speed policy's hi-speed fix every frame gives (image index 55,
    /// `settings.fix_hispeed`): 0 Classic, Floating 1 to 4.
    pub fix_hispeed: u8,
}

/// One phase of a play state.
#[derive(Clone, Debug)]
pub enum Phase {
    /// The play screen loading: production builds loading frames only.
    Loading {
        len_us: i64,
    },
    /// The practice page: production builds no frame of the play skin.
    Page {
        len_us: i64,
    },
    Round(Round),
}

/// How a round's frames end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Through the last note and the audio tail.
    Tail,
    /// The last frame at this chart time.
    Quit(i64),
    /// Closing from this chart time, then the end.
    QuitClosing(i64),
    /// The shown gauge's failure starts Closing.
    Fail,
    /// No frame from this chart time on; the evaluator runs to its end.
    Unbuilt(i64),
}

/// One round: an attempt of the whole chart or of a section, with its
/// inputs and how it ends.
#[derive(Clone, Debug)]
pub struct Round {
    /// A section attempt (a practice round), else the whole chart.
    pub section: bool,
    /// Per note, the offset of its press from the note (µs), `None` for
    /// no press.
    pub presses: Vec<Option<i64>>,
    /// Presses with no note near (empty POORs).
    pub ghosts: Vec<i64>,
    /// Further frame times (chart µs).
    pub extra: Vec<i64>,
    /// The start gate's rearm at this READY time (chart µs).
    pub rearm_at: Option<i64>,
    pub end: End,
    /// The frames show the survival gauge, started at 1 %.
    pub survival: bool,
}

/// What one frame of a round gives.
pub struct Step<'e, 'a> {
    /// The frame's chart time.
    pub now: TimeUs,
    /// The state clock (µs) since the round began.
    pub rel_us: i64,
    pub effects: &'e [JudgeEvent],
    pub ev: &'e Evaluator<'a>,
    /// After the start gate's rearm.
    pub rearmed: bool,
    /// Where Closing began, on the play clock.
    pub closing: Option<TimeUs>,
}

/// The first note's time and the gap between notes.
const FIRST: i64 = 1_000_000;
const GAP: i64 = 400_000;
/// Frames start this far before chart time 0 (READY) and run this long
/// past the last note (the audio tail).
pub const LEAD: i64 = 1_000_000;
const TAIL: i64 = 2_000_000;
/// Closing's length on the play clock.
const CLOSING: i64 = 500_000;
/// The state clock at chart time 0 in a whole-chart script: the play
/// state began this long before (its loading).
pub const STATE_AT_ZERO: i64 = 1_500_000;
/// The frame step: 240 Hz.
pub const STEP: i64 = 4_167;
/// An outside phase's length.
const OUTSIDE: i64 = 500_000;

impl Round {
    /// Every note pressed on time, through the tail.
    pub fn on_time(notes: usize, section: bool) -> Self {
        Self {
            section,
            presses: vec![Some(0); notes],
            ghosts: Vec::new(),
            extra: Vec::new(),
            rearm_at: None,
            end: End::Tail,
            survival: false,
        }
    }
}

impl Script {
    fn whole(name: &'static str, round: Round) -> Self {
        Self {
            name,
            notes: round.presses.len(),
            origin: STATE_AT_ZERO - LEAD,
            phases: vec![Phase::Round(round)],
            fix_hispeed: 0,
        }
    }

    /// `full`: 7 notes, every one pressed on time.
    pub fn full() -> Self {
        Self::whole("full", Round::on_time(7, false))
    }

    /// `mixed`: 13 notes, every judgment, misses and empty POORs.
    pub fn mixed() -> Self {
        let mut r = Round::on_time(13, false);
        r.presses = vec![
            Some(0),
            Some(30_000),
            Some(-60_000),
            Some(0),
            None,
            Some(110_000),
            Some(0),
            Some(-25_000),
            None,
            Some(0),
            Some(70_000),
            Some(0),
            Some(-140_000),
        ];
        r.ghosts = vec![FIRST + GAP * 4 + GAP / 2, FIRST + GAP * 9 + GAP / 2];
        Self::whole("mixed", r)
    }

    /// A practice play state of 7 notes: loading, the page, then `rounds`
    /// each followed by the page. The last page is compared like every
    /// outside frame, but no later play frame observes a write made there
    /// (s7-06e-2-design.md §6.3).
    pub fn practice(name: &'static str, rounds: Vec<Round>) -> Self {
        let mut phases = vec![
            Phase::Loading { len_us: OUTSIDE },
            Phase::Page { len_us: OUTSIDE },
        ];
        for r in rounds {
            phases.push(Phase::Round(r));
            phases.push(Phase::Page { len_us: OUTSIDE });
        }
        Self {
            name,
            notes: 7,
            origin: 0,
            phases,
            fix_hispeed: 0,
        }
    }

    /// `rounds`: R1 every note on time and witnessed; R2 rearmed in its
    /// READY and quit after note 3; R3 every note on time.
    pub fn rounds() -> Self {
        let r1 = Round::on_time(7, true);
        let mut r2 = Round::on_time(7, true);
        r2.rearm_at = Some(-LEAD / 2);
        r2.end = End::Quit(Self::note_time(2) + 100_000);
        let r3 = Round::on_time(7, true);
        Self::practice("rounds", vec![r1, r2, r3])
    }

    /// `rounds-unwitnessed`: as `rounds`, but R1's frames stop just before
    /// note 7's press, and R2 runs through its tail.
    pub fn rounds_unwitnessed() -> Self {
        let mut s = Self::rounds();
        s.name = "rounds-unwitnessed";
        if let Some(r) = s.round_mut(0) {
            r.end = End::Unbuilt(Self::note_time(6) - 1);
        }
        if let Some(r) = s.round_mut(1) {
            r.end = End::Tail;
        }
        s
    }

    /// `closing`: R1 shows the survival gauge at 1 % and misses note 1,
    /// failing into Closing; R2 quits into Closing after note 2.
    pub fn closing() -> Self {
        let mut r1 = Round::on_time(7, true);
        r1.survival = true;
        r1.presses[0] = None;
        r1.end = End::Fail;
        let mut r2 = Round::on_time(7, true);
        r2.end = End::QuitClosing(Self::note_time(1) + 100_000);
        Self::practice("closing", vec![r1, r2])
    }

    /// `zero-score`: R1 misses note 1 and quits at its miss deadline, so
    /// its only score update has EX 0; R2 every note on time.
    pub fn zero_score() -> Self {
        let mut r1 = Round::on_time(7, true);
        r1.presses[0] = None;
        r1.end = End::Quit(Self::note_time(0) + 180_000);
        Self::practice("zero-score", vec![r1, Round::on_time(7, true)])
    }

    /// The script named `name`.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "full" => Some(Self::full()),
            "mixed" => Some(Self::mixed()),
            "rounds" => Some(Self::rounds()),
            "rounds-unwitnessed" => Some(Self::rounds_unwitnessed()),
            "closing" => Some(Self::closing()),
            "zero-score" => Some(Self::zero_score()),
            _ => None,
        }
    }

    /// Round `i` (0-based), to change.
    pub fn round_mut(&mut self, i: usize) -> Option<&mut Round> {
        self.phases
            .iter_mut()
            .filter_map(|p| match p {
                Phase::Round(r) => Some(r),
                _ => None,
            })
            .nth(i)
    }

    pub fn note_time(i: usize) -> i64 {
        FIRST + GAP * i64::try_from(i).unwrap_or(i64::MAX / GAP)
    }

    /// The chart: `notes` notes on key 1.
    pub fn chart(&self) -> Chart {
        (0..self.notes)
            .fold(ChartBuilder::new(150_000), |b, i| {
                b.tap(lane(), TimeUs(Self::note_time(i)))
            })
            .build()
            .expect("a self-made chart builds")
    }

    /// The last note's time.
    pub fn last_note(&self) -> TimeUs {
        TimeUs(Self::note_time(self.notes.saturating_sub(1)))
    }

    /// A section over the whole chart, its lead-in READY's, each gauge
    /// starting at `starts`.
    pub fn section(&self, starts: Vec<rules::GaugeValue>) -> Section {
        Section {
            start: TimeUs(0),
            end: TimeUs(self.last_note().0 + GAP),
            lead_in: TimeUs(LEAD),
            initial_gauges: starts,
        }
    }
}

impl Round {
    /// The input log, in time order: a press and its release 50 ms later.
    fn inputs(&self) -> Vec<PhysicalInputEvent> {
        let mut at: Vec<i64> = self
            .presses
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.map(|d| Script::note_time(i) + d))
            .chain(self.ghosts.iter().copied())
            .collect();
        at.sort_unstable();
        let mut out = Vec::new();
        for t in at {
            out.push(set(t, 1));
            out.push(set(t + 50_000, 0));
        }
        out.sort_by_key(|e| e.received.0);
        out
    }

    /// The chart time the round's frames run to.
    fn last_time(&self, last_note: i64) -> i64 {
        last_note + TAIL
    }

    /// The frame times of one segment: every step from READY through
    /// `until`, each input's time and the extra times.
    fn times(&self, until: i64) -> Vec<i64> {
        let mut t: Vec<i64> = (0..)
            .map(|k: i64| -LEAD + k * STEP)
            .take_while(|&t| t <= until)
            .collect();
        t.extend(self.inputs().iter().map(|e| e.received.0));
        t.extend(self.extra.iter().copied());
        t.retain(|&x| x <= until);
        t.sort_unstable();
        t.dedup();
        t
    }

    /// Plays the round over `chart`, calling `frame` once per built frame;
    /// returns the tally the round settles with and the state time it
    /// took.
    ///
    /// # Errors
    ///
    /// The evaluator refusing, or `frame`'s error.
    pub fn play(
        &self,
        chart: &Chart,
        preset: &rules::RulePreset,
        section: Option<&Section>,
        (last, shown): (i64, usize),
        judged: &dyn Fn(&Tally),
        mut frame: impl FnMut(&Step<'_, '_>) -> Result<(), String>,
    ) -> Result<(Tally, i64), String> {
        let device = device();
        let start = InputStart {
            opened_at: TimeUs(-LEAD),
            held: Vec::new(),
        };
        let inputs = self.inputs();
        let end = self.last_time(last);
        let new_ev = || {
            Evaluator::new(chart, preset, &device, &start, section, inputs.len() + 1)
                .map_err(|e| format!("evaluator: {e:?}"))
        };
        let mut out = Vec::new();
        let mut base = 0;
        let mut rearmed = false;
        if let Some(r) = self.rearm_at {
            let ev = new_ev()?;
            for now in self.times(r) {
                frame(&Step {
                    now: TimeUs(now),
                    rel_us: now + LEAD,
                    effects: &[],
                    ev: &ev,
                    rearmed: false,
                    closing: None,
                })?;
            }
            base = r + LEAD + STEP;
            rearmed = true;
        }
        let mut ev = new_ev()?;
        let mut next = 0;
        let cut = match self.end {
            End::Quit(t) | End::QuitClosing(t) => t,
            End::Unbuilt(t) => t - 1,
            End::Tail | End::Fail => end,
        };
        let mut failed: Option<TimeUs> = None;
        // A quit is an instant of its own: its frame is built and the
        // round settles with the tally there, a judgment due at it included.
        let mut times = self.times(cut);
        if matches!(self.end, End::Quit(_) | End::QuitClosing(_)) && times.last() != Some(&cut) {
            times.push(cut);
        }
        for now in times {
            while let Some(i) = inputs.get(next).filter(|i| i.received.0 <= now) {
                ev.push_input(*i).map_err(|e| format!("input: {e:?}"))?;
                next += 1;
            }
            out.clear();
            ev.advance_to(TimeUs(now), &mut out)
                .map_err(|e| format!("advance: {e:?}"))?;
            judged(ev.tally());
            frame(&Step {
                now: TimeUs(now),
                rel_us: base + now + LEAD,
                effects: &out,
                ev: &ev,
                rearmed,
                closing: None,
            })?;
            if self.end == End::Fail
                && let Some(f) = ev.gauges().get(shown).and_then(|g| g.failed_at)
            {
                failed = Some(f.max(TimeUs(now)));
                break;
            }
        }
        let closing = match self.end {
            End::QuitClosing(t) => Some(TimeUs(t)),
            End::Fail => failed,
            _ => None,
        };
        let mut last_rel = base + cut + LEAD;
        if let Some(c) = closing {
            let mut now = c.0;
            while now + STEP <= c.0 + CLOSING {
                now += STEP;
                last_rel = base + now + LEAD;
                frame(&Step {
                    now: TimeUs(now),
                    rel_us: last_rel,
                    effects: &[],
                    ev: &ev,
                    rearmed,
                    closing,
                })?;
            }
        }
        if let End::Unbuilt(_) = self.end {
            while let Some(i) = inputs.get(next) {
                ev.push_input(*i).map_err(|e| format!("input: {e:?}"))?;
                next += 1;
            }
            out.clear();
            ev.advance_to(TimeUs(end), &mut out)
                .map_err(|e| format!("advance: {e:?}"))?;
            // The source updates its score data at every judgment, built
            // frame or not (s7-06e-3-design.md §2a).
            judged(ev.tally());
            last_rel = base + end + LEAD;
        }
        Ok((*ev.tally(), last_rel + STEP))
    }
}

/// The length of an outside phase.
impl Phase {
    pub fn outside_len(&self) -> Option<i64> {
        match self {
            Self::Loading { len_us } | Self::Page { len_us } => Some(*len_us),
            Self::Round(_) => None,
        }
    }
}

fn lane() -> Lane {
    Lane::new(1).expect("key 1")
}

fn set(at: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(1),
            value,
        },
    }
}

/// One source binding control `n` to lane `n`, with no offset.
fn device() -> DeviceProfile {
    DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: ControlId(u32::try_from(l.index()).unwrap_or(u32::MAX)),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}
