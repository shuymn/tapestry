//! The production run: the production import's skin built frame by frame
//! with `Skin::build`, the program's trace recorded (design.md "beatoraja
//! Lua Skins", the reference executor; s7-06e-design.md §3.1, §3.5).
//!
//! [`ProductionRun`] owns the skin, the play and the trace; nothing
//! outside this module and its child [`compare`] can read them. The
//! reference gets only the frames' inputs ([`FrameInputs`]).

pub mod compare;
pub mod continuation;

use std::collections::BTreeMap;

use bms_model::TimeUs;
use skin_beatoraja::lua::replay::Recorded;
use skin_model::present::trace::{self, Event};
use skin_model::{
    AtlasLimits, Attempt, ChartMetadata, CoverSettings, FrameReadouts, Gain, GaugeDesc, LaneView,
    Layout, LoadState, LoadingField, LoadingFrame, PlayContext, QuadBatch, Side, Skin, SkinFrame,
    Variant, field, loading_field,
};

use self::continuation::{Carried, Model};
use crate::inputs::FrameInputs;
use crate::script::{End, Phase, STEP, Script};

#[cfg(test)]
thread_local! {
    /// A test's offset of the score input's EX score: a wrong model of
    /// the source's score data, which the record loop must refuse.
    pub static SCORE_SKEW: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Whether a test drops the carried score data from each round's
    /// offer: a wrong model of the player's carry, which the record loop
    /// must refuse at a carried round's first frame.
    pub static DROP_SCORE_CARRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The round's offer: the model's, its score data dropped by a test.
fn offer(model: &Model) -> skin_model::Continuation<'_> {
    let offer = model.offer();
    #[cfg(test)]
    if DROP_SCORE_CARRY.with(std::cell::Cell::get)
        && let skin_model::Continuation::Carried { bank, .. } = offer
    {
        return skin_model::Continuation::Carried { score: None, bank };
    }
    offer
}

#[cfg(test)]
thread_local! {
    /// A test's best and target for the play: the score figures read
    /// non-zero values, where a run has neither.
    pub static SCORE_BASIS: std::cell::Cell<Option<(u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// The play's best and target: a test's ([`SCORE_BASIS`]), else none.
fn play_score() -> skin_model::PlayScore<'static> {
    #[cfg(test)]
    if let Some((best, target)) = SCORE_BASIS.with(std::cell::Cell::get) {
        return skin_model::PlayScore {
            basis: skin_model::ScoreBasis {
                best,
                best_known: true,
                target,
            },
            ..skin_model::PlayScore::NONE
        };
    }
    skin_model::PlayScore::NONE
}

/// The score input's EX-score offset: a test's, else 0.
fn skew() -> i32 {
    #[cfg(test)]
    {
        SCORE_SKEW.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        0
    }
}

/// Where a play frame stands in its round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Ready,
    RearmedReady,
    Play,
    Tail,
    Failed,
    Closing,
}

/// What a frame is: a play frame of a round, or a frame production
/// builds no play frame for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::production) enum FrameKind {
    Play {
        round: usize,
        stage: Stage,
        carried: Carried,
    },
    Loading,
    Page,
}

impl FrameKind {
    /// The label a report counts the frame under.
    pub(in crate::production) fn label(self) -> &'static str {
        match self {
            Self::Play { stage, .. } => match stage {
                Stage::Ready => "ready",
                Stage::RearmedReady => "rearmed ready",
                Stage::Play => "play",
                Stage::Tail => "tail",
                Stage::Failed => "failed",
                Stage::Closing => "closing",
            },
            Self::Loading => "loading",
            Self::Page => "page",
        }
    }
}

/// One frame's trace. Its fields, and [`ProductionRun`]'s, are
/// `pub(in crate::production)`: readable here and in [`compare`] only, as
/// `cargo xtask check-arch`'s privacy harness checks.
pub(in crate::production) struct FrameTrace {
    pub(in crate::production) events: Vec<Event>,
    pub(in crate::production) now: TimeUs,
    pub(in crate::production) state_us: i64,
    /// The program's output cells after the build: callback `k`'s value
    /// at `2k`, whether it gave one at `2k + 1`, as expressions read them.
    pub(in crate::production) outs: Vec<i32>,
    pub(in crate::production) kind: FrameKind,
    /// The play schema's fields and the timers' ms the frame's expressions
    /// read (empty off play).
    pub(in crate::production) fields: Vec<i32>,
    pub(in crate::production) timers: Vec<i32>,
}

/// One round as production played it: what it took over and the witness
/// its `Play` showed at its end.
#[derive(Clone, Copy, Debug)]
pub(in crate::production) struct RoundTrace {
    pub(in crate::production) carried: Carried,
    pub(in crate::production) slots_run: bool,
    pub(in crate::production) terminal: bool,
}

/// What production did, owned and unreadable outside this module.
pub struct ProductionRun {
    pub(in crate::production) recorded: Recorded,
    pub(in crate::production) frames: Vec<FrameTrace>,
    /// Each timer callback's timer slot.
    pub(in crate::production) timer_slots: BTreeMap<u16, usize>,
    /// The program's callbacks.
    pub(in crate::production) callbacks: usize,
    /// The program's structure ([`skin_model::Skin::program_structure`]).
    pub(in crate::production) structure: String,
    /// Per callback, whether it loads or stores a slot.
    pub(in crate::production) slotted: Vec<bool>,
    pub(in crate::production) carry_safe: bool,
    pub(in crate::production) rounds: Vec<RoundTrace>,
}

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 8_192,
    max_layers: 16,
    reserved_layers: 1,
};

/// The readouts of a frame: every cover on at 0, the script's hi-speed
/// fix, nothing else known.
fn readouts(fix_hispeed: u8) -> FrameReadouts {
    FrameReadouts {
        fix_hispeed,
        covers: CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
        ..FrameReadouts::UNAVAILABLE
    }
}

/// The fields a loading frame's host answers (s7-06e-2-design.md §5):
/// those loading gives with no gauge descriptor and only the chosen
/// chart's play level (when known), and the point, 0 there as the
/// source's fresh score data holds it.
fn loading_allowed(level: bool) -> Vec<bool> {
    (0..field::COUNT)
        .map(|slot| {
            let slot = u16::try_from(slot).unwrap_or(u16::MAX);
            slot == field::POINT
                || match loading_field(slot) {
                    Some(LoadingField::Live | LoadingField::Setting | LoadingField::Before) => true,
                    Some(LoadingField::Chosen) => {
                        level && matches!(slot, field::CHART_LEVEL | field::CHART_LEVEL_VALID)
                    }
                    _ => false,
                }
        })
        .collect()
}

/// The fields a page frame's host answers: the point only.
fn page_allowed() -> Vec<bool> {
    (0..field::COUNT)
        .map(|slot| u16::try_from(slot).is_ok_and(|s| s == field::POINT))
        .collect()
}

/// Plays `script` over `skin`, as production imported it, recording
/// each frame: the run and the frames' inputs. Each round is prepared
/// with what the harness's model of the carry offers
/// (s7-06e-2-design.md §4).
///
/// # Errors
///
/// The atlas, a preparation, a frame's build or the evaluator failing.
#[expect(clippy::too_many_lines, reason = "one loop over the phases")]
pub fn record(
    skin: &Skin,
    recorded: Recorded,
    script: &Script,
    play_level: Option<i32>,
) -> Result<(ProductionRun, Vec<FrameInputs>), String> {
    let chart = script.chart();
    let preset = rules::tapestry();
    let gauges: Vec<GaugeDesc> = preset.gauges.iter().map(GaugeDesc::from_rule).collect();
    let survival = preset
        .gauges
        .iter()
        .position(|g| g.id == "survival")
        .unwrap_or(0);
    let uvs = skin
        .atlas(Variant::Lo, LIMITS, &|| false)
        .map_err(|e| format!("atlas: {e:?}"))?
        .into_parts()
        .1;
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let view = LaneView {
        gain: Gain::new(8, 3),
        ..LaneView::NONE
    };
    let end = script.last_note();
    let carry_safe = skin.program_carry_safe();
    let mut model = Model::default();
    let mut frames = Vec::new();
    let mut inputs = Vec::new();
    let mut rounds = Vec::new();
    let mut clock = script.origin;
    let mut pages = 0;
    let mut slots = 0;
    let score: std::cell::Cell<Option<[i32; 3]>> = std::cell::Cell::new(None);
    trace::arm();
    let mut played = || -> Result<(), String> {
        for phase in &script.phases {
            match phase {
                Phase::Loading { len_us } => {
                    let mut loading = skin.prepare_loading(Side::Left);
                    let allowed = loading_allowed(play_level.is_some());
                    let since = TimeUs(clock);
                    let mut t = 0;
                    while t < *len_us {
                        let state_us = clock + t;
                        let frame = LoadingFrame {
                            app: skin_model::AppReadouts::LAUNCH,
                            now: TimeUs(state_us),
                            load: LoadState {
                                since,
                                percent: i32::try_from(t * 100 / len_us).unwrap_or(100),
                            },
                            target: [1_920, 1_080],
                            layout: &Layout::AUTHOR,
                            lanes: view,
                            covers: readouts(script.fix_hispeed).covers,
                            display_offset: TimeUs(0),
                            fix_hispeed: script.fix_hispeed,
                            gauge: None,
                            play_level,
                            difficulty: None,
                            bga: None,
                            length: None,
                            autoplay: false,
                            turntable: 0,
                        };
                        batch.clear();
                        skin.build_loading(&mut loading, &frame, &uvs, &mut batch)
                            .map_err(|e| format!("loading build at {state_us}: {e:?}"))?;
                        let st = loading.state();
                        let mut fields =
                            st.fields.get(..field::COUNT).unwrap_or(st.fields).to_vec();
                        if let Some(p) = fields.get_mut(usize::from(field::POINT)) {
                            *p = 0;
                        }
                        inputs.push(FrameInputs::outside(
                            &fields,
                            allowed.clone(),
                            None,
                            state_us,
                        ));
                        frames.push(FrameTrace {
                            events: trace::take(),
                            now: TimeUs(state_us),
                            state_us,
                            outs: Vec::new(),
                            kind: FrameKind::Loading,
                            fields: Vec::new(),
                            timers: Vec::new(),
                        });
                        t += STEP;
                    }
                    clock += len_us;
                }
                Phase::Page { len_us } => {
                    let mut fields = vec![0; field::COUNT];
                    if let Some(p) = fields.get_mut(usize::from(field::POINT)) {
                        *p = model.score().map_or(0, |s| s.point);
                    }
                    let mut t = 0;
                    while t < *len_us {
                        let state_us = clock + t;
                        inputs.push(FrameInputs::outside(
                            &fields,
                            page_allowed(),
                            Some(pages),
                            state_us,
                        ));
                        frames.push(FrameTrace {
                            events: trace::take(),
                            now: TimeUs(state_us),
                            state_us,
                            outs: Vec::new(),
                            kind: FrameKind::Page,
                            fields: Vec::new(),
                            timers: Vec::new(),
                        });
                        t += STEP;
                    }
                    pages += 1;
                    clock += len_us;
                }
                Phase::Round(r) => {
                    let mut starts: Vec<rules::GaugeValue> =
                        gauges.iter().map(|g| g.start).collect();
                    if r.survival
                        && let Some(s) = starts.get_mut(survival)
                    {
                        *s = rules::GaugeValue::centi_percent(100);
                    }
                    let section = script.section(starts);
                    let shown = if r.survival { survival } else { 0 };
                    let ctx = PlayContext {
                        meta: ChartMetadata::UNKNOWN,
                        autoplay: false,
                        gauges: &gauges,
                        attempt: if r.section {
                            Attempt::Section(&section)
                        } else {
                            Attempt::Full { lead_in: TimeUs(0) }
                        },
                        empty_poor_breaks_combo: false,
                        empty_poor_counted: false,
                        scratch: Side::Left,
                        ln_default: rules::LnMode::Ln,
                        last_note_end: end,
                        bpm_range: None,
                        bpm_whole_change: None,
                        play_level,
                        score: play_score(),
                    };
                    let carried = model.carried(carry_safe);
                    let mut play = skin
                        .prepare_carried(&chart, &ctx, offer(&model))
                        .map_err(|e| format!("prepare: {e:?}"))?;
                    let round = rounds.len();
                    let mut rearmed = false;
                    let base = clock;
                    let targets = skin_model::attempt_targets(&chart, &ctx.attempt);
                    let fresh = skin_model::score_fields(
                        &eval::Tally::default(),
                        targets,
                        ctx.empty_poor_counted,
                    );
                    let judged = |t: &eval::Tally| {
                        if *t != eval::Tally::default() {
                            score.set(
                                skin_model::score_fields(t, targets, ctx.empty_poor_counted)
                                    .map(|[e, c, w]| [e.saturating_add(skew()), c, w]),
                            );
                        }
                    };
                    let (tally, took) = r.play(
                        &chart,
                        &preset,
                        r.section.then_some(&section),
                        (end.0, shown),
                        &judged,
                        |st| {
                            if st.rearmed && !rearmed {
                                play.rearm();
                                rearmed = true;
                            }
                            let now = st.now;
                            let state_us = base + st.rel_us;
                            let ready = now.0 < 0;
                            let playing = if r.section {
                                !ready && st.closing.is_none() && now <= end
                            } else {
                                now <= end
                            };
                            let frame = SkinFrame {
                                app: skin_model::AppReadouts::LAUNCH,
                                state_us,
                                now,
                                display_offset: TimeUs(0),
                                tally: st.ev.tally(),
                                gauges: st.ev.gauges(),
                                gauge: shown,
                                effects: st.effects,
                                notes: st.ev.notes(),
                                offsets: skin_model::Offsets::NONE,
                                input: st.ev.input(),
                                complete: st.ev.is_complete(),
                                target: [1_920, 1_080],
                                layout: &Layout::AUTHOR,
                                turntable: 0,
                                lanes: view,
                                playing,
                                closing: st.closing,
                                readouts: readouts(script.fix_hispeed),
                            };
                            batch.clear();
                            skin.build(&mut play, &frame, &uvs, &mut batch)
                                .map_err(|e| format!("build at {}: {e:?}", now.0))?;
                            let s = play.state();
                            let fields = s.fields.get(..field::COUNT).unwrap_or(s.fields);
                            let outs = s.fields.get(field::COUNT..).unwrap_or_default().to_vec();
                            // The score input is the source's score data:
                            // on every play frame production's score fields
                            // must be it, the carried data before the
                            // round's first judgment included, and before
                            // the play state's first judgment the round's
                            // own for no judgment (s7-06s-design.md §2.5);
                            // else the harness's model or production's carry
                            // is wrong.
                            let want = score.get().or(fresh);
                            let own = [
                                field::EX_SCORE,
                                field::RATE_CURRENT_BP,
                                field::RATE_WHOLE_BP,
                            ]
                            .map(|f| fields.get(usize::from(f)).copied().unwrap_or(0));
                            if want != Some(own) {
                                return Err(format!(
                                    "score input {:?} differs from the frame's {own:?} at {}",
                                    score.get(),
                                    now.0
                                ));
                            }
                            inputs.push(
                                FrameInputs::new(fields, s.timers, state_us)
                                    .with_score(score.get()),
                            );
                            let stage = match (st.closing, ready, st.rearmed) {
                                (Some(_), _, _) if r.end == End::Fail => Stage::Failed,
                                (Some(_), _, _) => Stage::Closing,
                                (None, true, false) => Stage::Ready,
                                (None, true, true) => Stage::RearmedReady,
                                (None, false, _) if now > end => Stage::Tail,
                                (None, false, _) => Stage::Play,
                            };
                            frames.push(FrameTrace {
                                events: trace::take(),
                                now,
                                state_us,
                                outs,
                                kind: FrameKind::Play {
                                    round,
                                    stage,
                                    carried,
                                },
                                fields: fields.to_vec(),
                                timers: s.timers.to_vec(),
                            });
                            Ok(())
                        },
                    )?;
                    let carry = play.carry();
                    rounds.push(RoundTrace {
                        carried,
                        slots_run: carry.slots_run,
                        terminal: carry.terminal,
                    });
                    model.settle(&tally, &carry, ctx.empty_poor_counted);
                    slots = play.state().timers.len();
                    clock = base + took;
                }
            }
        }
        Ok(())
    };
    let played = played();
    trace::disarm();
    played?;
    let mut timers: Vec<u16> = last_records(&recorded)
        .values()
        .filter(|o| o.field == "timer")
        .filter_map(|o| o.program)
        .collect();
    timers.sort_unstable();
    timers.dedup();
    let base = slots.saturating_sub(timers.len());
    let timer_slots = timers
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, base + i))
        .collect();
    Ok((
        ProductionRun {
            recorded,
            frames,
            timer_slots,
            callbacks: skin.program_callbacks(),
            structure: skin.program_structure(),
            slotted: skin.program_slotted(),
            carry_safe,
            rounds,
        },
        inputs,
    ))
}

/// An occurrence's identity: its destination, consumer, `op` index and
/// node (s7-06e-1-r1-tables.md, A).
type Identity = (Option<usize>, &'static str, u32, u32);

/// Each occurrence's last record: a later record supersedes an earlier
/// one of the same occurrence only.
fn last_records(r: &Recorded) -> BTreeMap<Identity, &skin_beatoraja::lua::replay::Occurrence> {
    let mut out = BTreeMap::new();
    for o in &r.occurrences {
        out.insert((o.destination, o.field, o.index, o.node), o);
    }
    out
}

impl ProductionRun {
    /// Whether the reference import is production's: the same
    /// occurrence records and the same program structure.
    pub fn same_import(&self, other: &Recorded, structure: &str) -> bool {
        self.recorded.occurrences == other.occurrences && self.structure == structure
    }
}
