//! The attempt a play's screen prepares, as a skin takes it (design.md
//! "Play Lifecycle"): the game's [`Attempt`] translated into owned skin
//! data, sent to the render thread in its message, from which a borrowed
//! [`PlayContext`] is made only for the call that prepares the screen,
//! with the chart's labels as [`ChartText`], and the attempt's tempo
//! provider ([`AttemptTempo`]) beside it, and the play's best and target
//! with the chart's 1× profile and MAIN tempo ([`skin_model::PlayScore`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use bms_model::{ChartProfile, Tempo, TimeUs};
use game::{Attempt, AttemptKind, AttemptTempo, ChartLabels};
use skin_beatoraja::JudgeClass;
use skin_model::{BpmRange, ChartMetadata, ChartText, GaugeDesc, PlayContext, PlayScore, Side};

use crate::imports::facts::{difficulty_of, level_of};

/// The scratch side the simple skin draws, its author's layout: it has no
/// imported side and no player layout.
pub const SIMPLE_SCRATCH: Side = Side::Left;

/// One attempt's skin data, owned, made afresh for every play.
#[derive(Clone, Debug)]
pub struct AttemptContext {
    /// The play preset's gauges, in its order.
    gauges: Vec<GaugeDesc>,
    /// The whole chart or a section.
    kind: AttemptKind,
    empty_poor_breaks_combo: bool,
    empty_poor_counted: bool,
    scratch: Side,
    ln_default: rules::LnMode,
    last_note_end: TimeUs,
    /// The chart's labels, which its prepared text shows.
    labels: ChartLabels,
    /// The tempo provider and references of the chart as loaded at the
    /// attempt's speed, over its section.
    tempo: AttemptTempo,
    /// The chart's play level, parsed from its label as the attempt's
    /// import facts are ([`level_of`]).
    play_level: i32,
    /// The chart's metadata the `chart.*` fields read: its difficulty and
    /// whether it has a BGA from its labels, its song total from the game,
    /// its judgment class from its own rank as the reference computes it,
    /// as the import facts give it ([`JudgeClass::of_headers`]).
    meta: ChartMetadata,
    /// The presentation epoch the attempt was attached as: a snapshot of
    /// another epoch gives its frames no gain.
    epoch: u64,
    /// Whether the attempt is autoplay.
    autoplay: bool,
    /// The play's best and target, read when it began.
    score: skin_model::ScoreBasis,
    /// The chart's 1× profile, shared with the loaded chart.
    profile: Option<Arc<ChartProfile>>,
    /// The whole chart's MAIN tempo.
    main: Option<Tempo>,
    /// The preset's key judge areas for the chart's rank.
    areas: [[i16; 2]; 5],
    /// The speed the chart is loaded at.
    speed: bms_model::Speed,
    /// The scratch lane's notes autoplay's turntable presses, built before
    /// READY; none for a play by hand.
    scratch_notes: Vec<game::ScratchNote>,
}

impl AttemptContext {
    /// `attempt` as a skin takes it, with the lanes laid out for `scratch`,
    /// attached as presentation epoch `epoch`, autoplay when `autoplay`.
    pub fn new(attempt: Attempt, scratch: Side, epoch: u64, autoplay: bool) -> Self {
        Self {
            gauges: attempt.gauges.iter().map(GaugeDesc::from_rule).collect(),
            kind: attempt.kind,
            empty_poor_breaks_combo: attempt.empty_poor_breaks_combo,
            empty_poor_counted: attempt.empty_poor_counted,
            scratch,
            ln_default: attempt.ln_default,
            last_note_end: attempt.last_note_end,
            play_level: level_of(&attempt.labels.playlevel),
            meta: ChartMetadata {
                difficulty: Some(difficulty_of(&attempt.labels.difficulty)),
                judge_class: JudgeClass::of_headers(&attempt.judgment_headers)
                    .map(JudgeClass::index),
                total: Some(attempt.song_total),
                bga: Some(attempt.labels.bga),
            },
            labels: attempt.labels,
            tempo: attempt.tempo,
            epoch,
            autoplay,
            score: skin_model::ScoreBasis {
                best: attempt.score.best,
                best_known: attempt.score.best_known,
                target: attempt.score.target,
            },
            profile: attempt.profile,
            main: attempt.main,
            areas: attempt.areas,
            speed: attempt.speed,
            scratch_notes: Vec::new(),
        }
    }

    /// The attempt's tempo provider and references, which every play frame
    /// reads at its display head.
    pub const fn tempo(&self) -> &AttemptTempo {
        &self.tempo
    }

    /// The attempt's READY on the chart's axis: where its recording opens,
    /// as the session's `opens_at` says, and its end, where play starts
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub const fn ready(&self) -> (TimeUs, TimeUs) {
        match &self.kind {
            AttemptKind::Full { lead_in } => (TimeUs(-lead_in.0), TimeUs(0)),
            AttemptKind::Section(s) => (
                TimeUs(s.start.0 - s.lead_in.0 - game::RESTORE_AHEAD.0),
                s.start,
            ),
        }
    }

    /// The attempt's gauges' looks, in the preset's order (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn gauge_looks(&self) -> Vec<skin_model::GaugeLook> {
        self.gauges.iter().map(|g| g.look).collect()
    }

    /// The presentation epoch the attempt was attached as.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The attempt with `notes` as autoplay's scratch notes.
    #[must_use]
    pub fn with_scratch_notes(mut self, notes: Vec<game::ScratchNote>) -> Self {
        self.scratch_notes = notes;
        self
    }

    /// Autoplay's scratch notes, taken out for the render thread's
    /// turntable.
    pub fn take_scratch_notes(&mut self) -> Vec<game::ScratchNote> {
        std::mem::take(&mut self.scratch_notes)
    }

    /// The chart's five labels, borrowed for one preparation.
    pub fn text(&self) -> ChartText<'_> {
        let l = &self.labels;
        ChartText {
            title: &l.title,
            subtitle: &l.subtitle,
            artist: &l.artist,
            subartist: &l.subartist,
            genre: &l.genre,
        }
    }

    /// The borrowed context a skin prepares with, for one call.
    pub fn context(&self) -> PlayContext<'_> {
        PlayContext {
            meta: self.meta,
            autoplay: self.autoplay,
            gauges: &self.gauges,
            attempt: match &self.kind {
                AttemptKind::Full { lead_in } => skin_model::Attempt::Full { lead_in: *lead_in },
                AttemptKind::Section(section) => skin_model::Attempt::Section(section),
            },
            empty_poor_breaks_combo: self.empty_poor_breaks_combo,
            empty_poor_counted: self.empty_poor_counted,
            scratch: self.scratch,
            ln_default: self.ln_default,
            last_note_end: self.last_note_end,
            bpm_range: bpm_range(self.tempo.meta()),
            bpm_whole_change: Some(bpm_whole_change(self.tempo.meta())),
            play_level: Some(self.play_level),
            score: PlayScore {
                basis: self.score,
                profile: self.profile.as_deref(),
                main: self.main,
                areas: Some(self.areas),
                speed: self.speed,
            },
        }
    }
}

/// The chart's original lowest and highest tempos in floored hundredths of
/// a BPM, not scaled by the practice speed (design.md "Play readouts");
/// `None` past [`skin_model::READOUT_MAX`].
pub(crate) fn bpm_range(meta: &bms_model::TempoMeta) -> Option<BpmRange> {
    let x100 = |t: bms_model::Tempo| {
        let v = t.units() / (bms_model::Tempo::UNITS_PER_BPM / 100);
        u32::try_from(v)
            .ok()
            .filter(|&v| i64::from(v) <= i64::from(skin_model::READOUT_MAX))
    };
    Some(BpmRange {
        min_x100: x100(meta.min)?,
        max_x100: x100(meta.max)?,
    })
}

/// Whether the chart's original lowest and highest tempos differ in whole
/// BPMs, each floored, uncapped by the readouts' maximum (design.md "Play
/// readouts").
pub(crate) fn bpm_whole_change(meta: &bms_model::TempoMeta) -> bool {
    let whole = |t: bms_model::Tempo| t.units() / bms_model::Tempo::UNITS_PER_BPM;
    whole(meta.min) < whole(meta.max)
}

#[cfg(test)]
pub(crate) mod meta_tests;

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use game::{Attempt, ChartLabels};
    use skin_model::{GaugeDesc, Side};

    use super::AttemptContext;

    /// The context carries the preset's gauges in its order, its flags and
    /// LN default, the section and the note end as the game made them,
    /// the scratch side given, and the chart's original BPM range; the
    /// chart's five labels; and the attempt's tempo provider, sharing the
    /// chart's timeline, over the same section.
    #[test]
    fn the_context_is_the_attempts_own_data() {
        let mut preset = rules::tapestry();
        preset.gauges.reverse();
        preset.empty_poor.counted = !preset.empty_poor.counted;
        let section = eval::Section {
            start: TimeUs(0),
            end: TimeUs(2_000_000),
            lead_in: TimeUs(1_500_000),
            initial_gauges: Vec::new(),
        };
        let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
        let attempt = Attempt::new(
            2,
            &preset,
            TimeUs(2_000_000),
            Some(&section),
            TimeUs(1_000_000),
            &ChartLabels {
                title: "t".to_owned(),
                subtitle: "s".to_owned(),
                artist: "a".to_owned(),
                subartist: "sa".to_owned(),
                genre: "g".to_owned(),
                playlevel: "12".to_owned(),
                difficulty: "4".to_owned(),
                bga: true,
                stagefile: "s.png".to_owned(),
                backbmp: "b.bmp".to_owned(),
            },
            &chart,
        );
        let tempo = attempt.tempo.clone();
        let owned = AttemptContext::new(attempt, Side::Right, 7, true);
        assert_eq!(owned.epoch(), 7);
        assert_eq!(owned.tempo(), &tempo);
        assert_eq!(
            owned.tempo().timeline().points().as_ptr(),
            chart.timeline.points().as_ptr()
        );
        assert_eq!(
            owned.tempo().section(),
            Some((TimeUs(0), TimeUs(2_000_000)))
        );
        let ctx = owned.context();
        let expected: Vec<GaugeDesc> = preset.gauges.iter().map(GaugeDesc::from_rule).collect();
        assert_eq!(ctx.gauges, expected.as_slice());
        assert!(matches!(ctx.attempt, skin_model::Attempt::Section(s) if *s == section));
        assert_eq!(ctx.empty_poor_counted, preset.empty_poor.counted);
        assert_eq!(ctx.empty_poor_breaks_combo, preset.empty_poor.breaks_combo);
        assert_eq!(ctx.ln_default, preset.ln_mode.default);
        assert_eq!(ctx.scratch, Side::Right);
        assert_eq!(ctx.last_note_end, TimeUs(1_000_000));
        assert_eq!(ctx.bpm_whole_change, Some(false));
        assert!(ctx.autoplay);
        assert_eq!(ctx.play_level, Some(12));
        // An empty chart: the default total for no notes, and no rank, so
        // the class of `#RANK 2`.
        assert_eq!(
            ctx.meta,
            skin_model::ChartMetadata {
                difficulty: Some(4),
                judge_class: Some(2),
                total: Some(260),
                bga: Some(true),
            }
        );
        assert_eq!(
            ctx.bpm_range,
            Some(skin_model::BpmRange {
                min_x100: 15_000,
                max_x100: 15_000
            })
        );
        let text = owned.text();
        assert_eq!(
            [
                text.title,
                text.subtitle,
                text.artist,
                text.subartist,
                text.genre
            ],
            ["t", "s", "a", "sa", "g"]
        );
    }

    /// The BPM-change fact compares the chart's floored extremes, uncapped,
    /// apart from the capped BPM range.
    #[test]
    fn the_bpm_change_fact_compares_uncapped_whole_extremes() {
        let preset = rules::tapestry();
        let context = |initial: i64, change: i64| {
            let chart = bms_model::ChartBuilder::new(initial)
                .bpm(TimeUs(1_000_000), change)
                .build()
                .unwrap();
            let attempt = Attempt::new(
                1,
                &preset,
                TimeUs(0),
                None,
                TimeUs(2_000_000),
                &ChartLabels::default(),
                &chart,
            );
            let owned = AttemptContext::new(attempt, Side::Left, 0, false);
            let ctx = owned.context();
            (ctx.bpm_whole_change, ctx.bpm_range)
        };
        let (change, _) = context(150_000, 150_500);
        assert_eq!(change, Some(false), "150 -> 150.5");
        let (change, _) = context(150_000, 151_000);
        assert_eq!(change, Some(true), "150 -> 151");
        let (change, range) = context(150_000, 30_000_000_000);
        assert_eq!(change, Some(true), "150 -> 30 000 000");
        assert_eq!(range, None, "past the readouts' maximum");
        let (change, _) = context(3_000_000_000_000, 4_000_000_000_000);
        assert_eq!(change, Some(true), "3e9 -> 4e9, both past `i32`");
        let (change, _) = context(3_000_000_000_000, 3_000_000_000_500);
        assert_eq!(change, Some(false), "3e9 -> 3e9 + 0.5");
        let (change, _) = context(2_147_483_646_000, 2_147_483_647_000);
        assert_eq!(change, Some(true), "2 147 483 646 -> 2 147 483 647");
    }

    /// The context hands the skin the attempt's best and target, the
    /// load's shared profile itself (not a copy) and the whole chart's
    /// MAIN tempo; an attempt made without them gives none.
    #[test]
    fn the_context_carries_the_score_basis_and_profile() {
        let preset = rules::tapestry();
        let chart = bms_model::ChartBuilder::new(150_000)
            .tap(bms_model::Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let bare = Attempt::new(
            1,
            &preset,
            TimeUs(0),
            None,
            TimeUs(1_000_000),
            &ChartLabels::default(),
            &chart,
        );
        let none = AttemptContext::new(bare.clone(), Side::Left, 0, false);
        let ctx = none.context();
        assert_eq!(ctx.score.basis, skin_model::ScoreBasis::default());
        assert!(ctx.score.profile.is_none() && ctx.score.main.is_none());
        let profile = std::sync::Arc::new(bms_model::ChartProfile::of(&chart, &[]));
        let main = chart.timeline.meta().initial;
        let attempt = Attempt {
            score: game::ScoreBasis {
                best: 5,
                best_known: true,
                target: 8,
            },
            profile: Some(std::sync::Arc::clone(&profile)),
            main: Some(main),
            areas: [[-1, 2], [-3, 4], [-5, 6], [-7, 8], [-9, 10]],
            speed: bms_model::Speed::from_twentieths(30).unwrap(),
            ..bare
        };
        let owned = AttemptContext::new(attempt, Side::Left, 0, false);
        let ctx = owned.context();
        assert_eq!(
            ctx.score.basis,
            skin_model::ScoreBasis {
                best: 5,
                best_known: true,
                target: 8
            }
        );
        assert!(std::ptr::eq(ctx.score.profile.unwrap(), &*profile));
        assert_eq!(ctx.score.main, Some(main));
        // The judge areas and the speed the visualizers and the notes
        // graph's cursors read.
        assert_eq!(
            ctx.score.areas,
            Some([[-1, 2], [-3, 4], [-5, 6], [-7, 8], [-9, 10]])
        );
        assert_eq!(ctx.score.speed.twentieths(), 30);
        // A bare attempt has the preset's areas for the chart, at 1×.
        assert_eq!(
            none.context().score.areas,
            Some(game::judge_areas(&chart, &preset))
        );
        assert_eq!(none.context().score.speed, bms_model::Speed::NORMAL);
    }
}
