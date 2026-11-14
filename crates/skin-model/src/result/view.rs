//! The result screen's snapshot (docs/stage3.md, the presentation
//! snapshot's Result row): one view's result, its gauge's history, its
//! timing statistics, deviations and judge areas, its timelines, its best
//! before this play, and the chart's facts. The player
//! fills it from the result's projections; whatever draws the screen reads
//! it, with the chart's strings in [`ResultLabels`].
//!
//! Each value is shown, hidden or unavailable. A view that is not
//! [`ResultStatus::Ready`] has no result, timing or best value; a best that
//! is [`BestView::Unavailable`] has no best value; a [`ResultNumber::Hidden`]
//! value is known and draws nothing.

use bms_model::TimeUs;
use eval::Tally;
use rules::GaugeValue;

use crate::{GaugeLook, LampKey};

/// The columns a [`GaugeHistory`] holds over the play's time.
pub const GRAPH_COLUMNS: usize = 288;
/// The slots of a view's deviations: −150 ms to +150 ms.
pub const DEVIATION_SLOTS: usize = 301;

/// The most bins a [`Timelines`] holds.
pub const MAX_TIMELINE_BINS: usize = 512;
/// The channels of a timeline bin: PERFECT; GREAT, GOOD, BAD and POOR
/// early; then the same four late.
pub const TIMELINE_CHANNELS: usize = 9;

/// What one result view shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultView {
    pub status: ResultStatus,
    /// The chart's facts, under every status.
    pub chart: ResultChart,
    /// The view's result; read only while `Ready`.
    pub score: ResultScore,
    /// The best under the view's preset before this play; read only while
    /// `Ready`.
    pub best: BestView,
    /// The timing statistics; `None` without a sample. Read only while
    /// `Ready`.
    pub timing: Option<TimingStats>,
    /// The judgments over chart time; `None` without them. Read only while
    /// `Ready`.
    pub timelines: Option<Timelines>,
    /// The emphasized gauge's history; `None` without an emphasized gauge.
    /// Read only while `Ready`.
    pub gauge: Option<Box<GaugeHistory>>,
    /// The timing statistics' samples by ms, early positive: slot `i`
    /// holds those whose ms value, truncated toward 0, is `i − 150`. Read
    /// only while `Ready`.
    pub deviations: Option<Box<[u32; DEVIATION_SLOTS]>>,
    /// The key lanes' judge areas in ms, early positive: PERFECT, GREAT,
    /// GOOD and BAD as `[−late, early]`, the empty POOR range as
    /// `[−behind, ahead]`. Read only while `Ready`.
    pub timing_areas: Option<[[i16; 2]; 5]>,
}

/// One column of a gauge's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeColumn {
    /// The lowest value in the column, the one carried into it included.
    pub low: GaugeValue,
    /// The highest.
    pub high: GaugeValue,
    /// The value at the column's end.
    pub last: GaugeValue,
}

/// A view's emphasized gauge over the play's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeHistory {
    /// The history, from the play's start to its as-played final deadline.
    pub columns: [GaugeColumn; GRAPH_COLUMNS],
    /// The gauge's largest value.
    pub max: GaugeValue,
    /// Its border: the clear threshold, or the value a survival gauge
    /// fails below ([`GaugeDesc::from_rule`](crate::GaugeDesc::from_rule)).
    pub border: GaugeValue,
    pub look: GaugeLook,
    /// The columns the evaluation covers; none from here on is drawn.
    pub reached: u16,
    /// The column a what-if's recorded input ends in.
    pub no_input_from: Option<u16>,
}

/// Whether a view has a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultStatus {
    Ready,
    Unavailable(Unavailable),
}

/// Why a view has no result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The play recorded nothing to evaluate.
    NothingRecorded,
    /// The view's preset or chart cannot be loaded.
    CannotLoad,
    /// Its evaluation would pass the memory budget.
    OverBudget,
    /// Its evaluation failed.
    Failed,
    /// The play was cancelled before its playback started.
    CancelledBeforePlayback,
}

/// The chart's facts a result skin shows, each `None` while unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultChart {
    /// Its `#PLAYLEVEL` as a number.
    pub level: Option<i32>,
    /// Its gauge total, whole.
    pub total: Option<u32>,
    pub bpm: Option<ResultBpm>,
    /// Its `#DIFFICULTY` when it is 1 to 5.
    pub difficulty: Option<u8>,
    /// Its length at 1×: its last note's end or BGM event, whichever is
    /// later, the source's `SongData.getLastTime()`
    /// (`IntegerPropertyFactory.java:473-478`), whatever section or speed
    /// is played.
    pub length: Option<TimeUs>,
    /// The judged notes of the draw played, the source's total notes
    /// (`IntegerPropertyFactory.java:562-576`): the chart's, under every
    /// status.
    pub notes: Option<u32>,
    /// Its judgment class, 0 the hardest to 4 the easiest, as play's
    /// [`ChartMetadata::judge_class`](crate::ChartMetadata::judge_class):
    /// from its own rank, whatever preset judged or views it; `None`
    /// also for a rank in no class. A value above 4 is none.
    pub judge_class: Option<u8>,
    /// Whether its stage file and back image are images, resolved before
    /// any result import, probe or view (options 190, 191, 194 and 195).
    pub images: ImageFacts,
}

/// Whether a chart's stage file and back image are images: each `true`
/// exactly when the attempt's read of it decoded (S10-12, the result's
/// availability). Static for the attempt's result, as the source's
/// options are there (`BooleanPropertyFactory.java:219`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ImageFacts {
    pub stage: bool,
    pub back: bool,
}

/// A chart's tempo range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultBpm {
    /// The lowest and highest tempos in floored hundredths of a BPM.
    pub min_x100: u32,
    pub max_x100: u32,
    /// Whether the tempo changes, as the player decides it.
    pub changes: bool,
}

/// One view's result: what its evaluation counted and what follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultScore {
    pub tally: Tally,
    /// The emphasized gauge's end value, in ‰.
    pub gauge_permille: u16,
    pub clear: LampKey,
    /// Whether the view cleared: its lamp is not failed (options 90 and 91).
    pub cleared: bool,
    /// `None` when the largest EX score is 0.
    pub rate: Option<ResultRate>,
    /// The EX score to the next grade, or to the largest at the top grade.
    pub next_rank: u32,
    /// The mean absolute duration of the result's notes in µs, the
    /// source's penalized mean (numbers 372 and 373); `None` with no note.
    pub duration_us: Option<u32>,
}

impl Default for ResultScore {
    fn default() -> Self {
        Self {
            tally: Tally::default(),
            gauge_permille: 0,
            clear: LampKey::NoPlay,
            cleared: false,
            rate: None,
            next_rank: 0,
            duration_us: None,
        }
    }
}

/// A score's rate over the largest EX score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultRate {
    /// `⌊10 000 · ex / max_ex⌋`.
    pub rate_bp: u32,
    /// 0 (F) to 7 (AAA).
    pub grade: u8,
}

/// A best value that is known and may have nothing to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultNumber {
    /// Drawn; a value beyond ±([`i32::MAX`] − 1) reads at that bound.
    Shown(i32),
    /// Known with nothing to draw: it reads `i32::MIN`.
    Hidden,
}

/// The best before this play, as the view compares with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestView {
    /// Not read yet: every best value is unavailable. A best read as not
    /// known is the source's empty score, [`Known`](Self::Known) (design.md
    /// "Personal Bests", skin projections).
    Unavailable,
    /// Known, an empty best included; its values as the comparison gives
    /// them.
    Known(BestFigures),
}

/// The best values of a known best.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BestFigures {
    pub ex: u32,
    /// The EX score minus the best's.
    pub ex_diff: i32,
    pub bp: ResultNumber,
    /// The BP minus the best's.
    pub bp_diff: ResultNumber,
    pub clear: LampKey,
    /// The best's max combo, of any counted play, 0 included; hidden
    /// while no play counts for the best.
    pub max_combo: ResultNumber,
    /// The view's max combo minus it; hidden as `max_combo`.
    pub max_combo_diff: ResultNumber,
    pub ex_updated: bool,
    pub bp_updated: bool,
    /// `None` when the best's rate is not known.
    pub rate: Option<BestRate>,
}

/// A best's rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BestRate {
    pub rate_bp: u32,
    /// 0 (F) to 7 (AAA).
    pub grade: u8,
    /// Whether the view's rate beat it.
    pub updated: bool,
}

/// The timing statistics of one view, over its samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingStats {
    /// The samples, at least one.
    pub samples: u32,
    /// The mean in hundredths of a ms, truncated toward 0, early positive.
    pub mean_x100: i32,
    /// The population standard deviation in hundredths of a ms, rounded
    /// down.
    pub sd_x100: u32,
}

/// The judgments of one view over chart time, in bins of one width, and
/// the judged notes in each bin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timelines {
    width: TimeUs,
    bins: Box<[[u32; TIMELINE_CHANNELS]]>,
    notes: Box<[u32]>,
    uncovered_from: Option<u16>,
}

/// Why [`Timelines::new`] refused its bins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimelinesError {
    #[error("{n} bins, not 1 to {MAX_TIMELINE_BINS}")]
    Bins { n: usize },
    #[error("the bin width is not positive")]
    Width,
    #[error("the first uncovered bin is past the bins")]
    Uncovered,
    #[error("{notes} note counts for {bins} bins")]
    Notes { notes: usize, bins: usize },
    #[error("bin {bin} counts more judgments than its notes")]
    Overcounted { bin: usize },
}

impl Timelines {
    /// Timelines of `bins`, each `width` of chart time, holding `notes`
    /// judged notes each, the bins from `uncovered_from` on past what the
    /// input covers.
    ///
    /// # Errors
    ///
    /// No bin or more than [`MAX_TIMELINE_BINS`], not one note count per
    /// bin, a bin whose counted judgments exceed its notes (each counted
    /// judgment is one of its bin's notes), a width that is not positive,
    /// or `uncovered_from` past the bins.
    pub fn new(
        width: TimeUs,
        bins: Vec<[u32; TIMELINE_CHANNELS]>,
        notes: Vec<u32>,
        uncovered_from: Option<u16>,
    ) -> Result<Self, TimelinesError> {
        let n = bins.len();
        if n == 0 || n > MAX_TIMELINE_BINS {
            return Err(TimelinesError::Bins { n });
        }
        if notes.len() != n {
            return Err(TimelinesError::Notes {
                notes: notes.len(),
                bins: n,
            });
        }
        if let Some(bin) = bins
            .iter()
            .zip(&notes)
            .position(|(b, &notes)| b.iter().map(|&c| u64::from(c)).sum::<u64>() > u64::from(notes))
        {
            return Err(TimelinesError::Overcounted { bin });
        }
        if width.0 <= 0 {
            return Err(TimelinesError::Width);
        }
        if uncovered_from.is_some_and(|u| usize::from(u) > n) {
            return Err(TimelinesError::Uncovered);
        }
        Ok(Self {
            width,
            bins: bins.into_boxed_slice(),
            notes: notes.into_boxed_slice(),
            uncovered_from,
        })
    }

    /// Each bin's width of chart time.
    #[must_use]
    pub fn width(&self) -> TimeUs {
        self.width
    }

    /// The bins, each its channels in [`TIMELINE_CHANNELS`]' order.
    #[must_use]
    pub fn bins(&self) -> &[[u32; TIMELINE_CHANNELS]] {
        &self.bins
    }

    /// The judged notes of the attempt in each bin, whether judged or not.
    #[must_use]
    pub fn notes(&self) -> &[u32] {
        &self.notes
    }

    /// The first bin past what the input covers; `None` when every bin is
    /// covered.
    #[must_use]
    pub fn uncovered_from(&self) -> Option<u16> {
        self.uncovered_from
    }
}

/// A chart's header strings a result skin's text shows, each as stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultLabels {
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub subartist: String,
    pub genre: String,
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{MAX_TIMELINE_BINS, Timelines, TimelinesError};

    /// The bins are bounded at the cap, each has one note count, and a
    /// width and the uncovered mark are checked.
    #[test]
    fn timelines_hold_at_most_the_cap() {
        let w = TimeUs(1_000_000);
        let at_cap = Timelines::new(
            w,
            vec![[1; 9]; MAX_TIMELINE_BINS],
            vec![9; MAX_TIMELINE_BINS],
            Some(512),
        )
        .unwrap();
        assert_eq!(at_cap.bins().len(), 512);
        assert_eq!(at_cap.notes(), &[9; MAX_TIMELINE_BINS][..]);
        assert_eq!(
            Timelines::new(
                w,
                vec![[0; 9]; MAX_TIMELINE_BINS + 1],
                vec![0; MAX_TIMELINE_BINS + 1],
                None
            ),
            Err(TimelinesError::Bins { n: 513 })
        );
        assert_eq!(
            Timelines::new(w, Vec::new(), Vec::new(), None),
            Err(TimelinesError::Bins { n: 0 })
        );
        for notes in [2, 4] {
            assert_eq!(
                Timelines::new(w, vec![[0; 9]; 3], vec![0; notes], None),
                Err(TimelinesError::Notes { notes, bins: 3 })
            );
        }
        // Each bin's counted judgments are some of its notes: 9 of 9
        // holds, 9 of 8 does not, nor a sum past `u32`.
        assert!(Timelines::new(w, vec![[1; 9]; 2], vec![9, 10], None).is_ok());
        assert_eq!(
            Timelines::new(w, vec![[0; 9], [1; 9]], vec![0, 8], None),
            Err(TimelinesError::Overcounted { bin: 1 })
        );
        assert_eq!(
            Timelines::new(w, vec![[u32::MAX; 9]], vec![u32::MAX], None),
            Err(TimelinesError::Overcounted { bin: 0 })
        );
        assert_eq!(
            Timelines::new(TimeUs(0), vec![[0; 9]], vec![0], None),
            Err(TimelinesError::Width)
        );
        assert_eq!(
            Timelines::new(w, vec![[0; 9]; 3], vec![0; 3], Some(4)),
            Err(TimelinesError::Uncovered)
        );
    }
}
