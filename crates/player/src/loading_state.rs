//! The loading lifetime's state on the producers' side (design.md
//! "Rendering and Skins", loading state): what a loading frame shows of
//! the load accumulates here, before the slot the render thread takes it
//! from. Typed inputs ([`LoadInput`]) update one state per lifetime
//! ([`LifetimeState`]), and each update publishes the whole state with its
//! sequence number ([`LoadingUpdate`]), so a newer publication always
//! carries everything an older one did: the bank's completion and the
//! readiness are state, never notices a later update could overwrite.
//! Everything here runs before recording opens, where allocation is
//! allowed.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use bms_model::TimeUs;
use rules::RulePreset;
use skin_model::{GaugeDesc, LoadingGauge};

use crate::loading_text::{ChosenChart, LoadingText, Stage};

/// One typed input to a loading lifetime's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadInput {
    /// What the load is doing; the sounds' stage carries their count when
    /// it is known ([`Stage::known_sounds`]).
    Stage(Stage),
    /// The sound bank is complete, an empty one included.
    BankComplete,
    /// The attempt is ready: its preparation, a round's restore check and
    /// the last look for a quit or a failure passed, before the commit and
    /// the end of loading.
    Ready,
    /// The shown gauge's descriptor, replacing the one before whole;
    /// `None` when the preset has no such gauge.
    Gauge(Option<LoadingGauge>),
    /// The chosen chart's fields, replacing those before; a change takes
    /// the next text revision.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "only fixtures report later fields until the selection's producer does"
        )
    )]
    Chart(ChosenChart),
}

/// The whole state of a loading lifetime as one publication carries it
/// (design.md "Play Lifecycle", who loads): bound to the lifetime it was
/// published in, and ordered by its sequence number. Nothing reads these
/// back out of the formatted lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadingUpdate {
    /// The publication's number in its lifetime, from 1: a later
    /// publication has a larger one.
    pub seq: u64,
    /// The lifetime's origin on the loading clock.
    pub origin: TimeUs,
    /// What the load is doing.
    pub stage: Stage,
    /// The progress, 0 to 100 ([`LifetimeState::percent`]).
    pub percent: u8,
    /// The shown gauge's descriptor; `None` leaves what depends on it
    /// undrawn.
    pub gauge: Option<LoadingGauge>,
    /// The chosen chart's fields, each `None` while unavailable.
    pub chart: Arc<ChosenChart>,
    /// The revision of `chart`, from 1: it changes whenever the fields
    /// do.
    pub revision: u64,
    /// The play preset's name.
    pub preset: String,
    /// Whether it is a practice's Start.
    pub practice: bool,
}

/// One loading lifetime's state, accumulated from its typed inputs.
#[derive(Clone, Debug)]
pub struct LifetimeState {
    lifetime: u64,
    seq: u64,
    origin: TimeUs,
    stage: Stage,
    /// The sounds done and the files to decode, as last reported with a
    /// known count.
    sounds: Option<(usize, usize)>,
    bank: bool,
    ready: bool,
    gauge: Option<LoadingGauge>,
    chart: Arc<ChosenChart>,
    revision: u64,
    preset: String,
    practice: bool,
}

impl LifetimeState {
    /// The state of lifetime `lifetime` begun at `origin` on the loading
    /// clock, for the chart and preset `text` names, with the shown
    /// gauge's descriptor `gauge`: reading the chart, progress 0, the
    /// chart's fields at revision 1, nothing published yet.
    pub fn new(
        lifetime: u64,
        origin: TimeUs,
        text: &LoadingText,
        gauge: Option<LoadingGauge>,
    ) -> Self {
        Self {
            lifetime,
            seq: 0,
            origin,
            stage: Stage::Chart,
            sounds: None,
            bank: false,
            ready: false,
            gauge,
            chart: Arc::new(text.chart.clone()),
            revision: 1,
            preset: text.preset.clone(),
            practice: text.practice,
        }
    }

    /// The lifetime it belongs to.
    pub const fn lifetime(&self) -> u64 {
        self.lifetime
    }

    /// Applies `input` and publishes the whole state under the next
    /// sequence number.
    pub fn apply(&mut self, input: LoadInput) -> LoadingUpdate {
        match input {
            LoadInput::Stage(stage) => {
                self.stage = stage;
                if let Some(known) = stage.known_sounds() {
                    self.sounds = Some(known);
                }
            }
            LoadInput::BankComplete => self.bank = true,
            LoadInput::Ready => self.ready = true,
            LoadInput::Gauge(gauge) => self.gauge = gauge,
            LoadInput::Chart(chart) => {
                if *self.chart != chart {
                    self.chart = Arc::new(chart);
                    self.revision = self.revision.saturating_add(1);
                }
            }
        }
        self.seq = self.seq.saturating_add(1);
        LoadingUpdate {
            seq: self.seq,
            origin: self.origin,
            stage: self.stage,
            percent: self.percent(),
            gauge: self.gauge,
            chart: Arc::clone(&self.chart),
            revision: self.revision,
            preset: self.preset.clone(),
            practice: self.practice,
        }
    }

    /// The progress in %, the one mapping of the state: 100 once ready,
    /// else 90 once the bank is complete, else `⌊90 · min(done, files) /
    /// files⌋` of the sounds' last known count, computed in 128-bit
    /// integers, else 0. A count not known yet and every other stage
    /// leave it where it is.
    pub fn percent(&self) -> u8 {
        if self.ready {
            return 100;
        }
        if self.bank {
            return 90;
        }
        let Some((done, files)) = self.sounds else {
            return 0;
        };
        let wide = |n: usize| u128::try_from(n).unwrap_or(u128::MAX);
        let files = wide(files).max(1);
        let done = wide(done).min(files);
        u8::try_from(90 * done / files).unwrap_or(90)
    }
}

/// The descriptor of gauge `index` of `preset`, the one the player shows:
/// its look, maximum, border and index, never its start; `None` when the
/// preset has no such gauge.
pub fn shown_gauge(preset: &RulePreset, index: usize) -> Option<LoadingGauge> {
    preset
        .gauges
        .get(index)
        .map(|rule| LoadingGauge::of(index, &GaugeDesc::from_rule(rule)))
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{LifetimeState, LoadInput, shown_gauge};
    use crate::loading_text::{ChosenChart, LoadingText, Stage};

    fn state() -> LifetimeState {
        let text = LoadingText {
            title: "B".to_owned(),
            chart: ChosenChart {
                title: Some("B".to_owned()),
                ..ChosenChart::default()
            },
            ..LoadingText::default()
        };
        LifetimeState::new(3, TimeUs(1_000), &text, None)
    }

    /// The progress starts at 0; an unknown count of sounds and the
    /// loudness's wait before decoding claim no sound work done; a known
    /// count gives `⌊90 · done / files⌋`, 40 at 4 of 9; the other stages
    /// keep it; the bank's completion gives 90, an empty bank's too, and
    /// only readiness gives 100.
    #[test]
    fn progress_follows_the_load() {
        let mut s = state();
        assert_eq!(s.percent(), 0);
        let percent = |s: &mut LifetimeState, input| s.apply(input).percent;
        assert_eq!(percent(&mut s, LoadInput::Stage(Stage::Loudness)), 0);
        let unknown = Stage::Sounds { done: 0, files: 0 };
        assert_eq!(percent(&mut s, LoadInput::Stage(unknown)), 0);
        let four = Stage::Sounds { done: 4, files: 9 };
        assert_eq!(percent(&mut s, LoadInput::Stage(four)), 40);
        assert_eq!(percent(&mut s, LoadInput::Stage(unknown)), 40);
        assert_eq!(percent(&mut s, LoadInput::Stage(Stage::Loudness)), 40);
        let past = Stage::Sounds { done: 12, files: 9 };
        assert_eq!(percent(&mut s, LoadInput::Stage(past)), 90);
        let all = Stage::Sounds {
            done: usize::MAX,
            files: usize::MAX,
        };
        assert_eq!(percent(&mut s, LoadInput::Stage(all)), 90);
        assert_eq!(percent(&mut s, LoadInput::Stage(Stage::Screen)), 90);
        assert_eq!(percent(&mut s, LoadInput::Ready), 100);
        assert_eq!(percent(&mut s, LoadInput::Stage(Stage::Scan)), 100);

        let mut empty = state();
        assert_eq!(percent(&mut empty, LoadInput::Stage(unknown)), 0);
        assert_eq!(percent(&mut empty, LoadInput::BankComplete), 90);
        assert_eq!(percent(&mut empty, LoadInput::Stage(Stage::Loudness)), 90);
        assert_eq!(percent(&mut empty, LoadInput::Stage(Stage::Screen)), 90);
        assert_eq!(percent(&mut empty, LoadInput::Ready), 100);
    }

    /// Every publication carries the whole state under the next number:
    /// the origin, the descriptor until replaced, and the chart's fields
    /// with a revision that changes only with them.
    #[test]
    fn each_publication_is_the_whole_state() {
        let mut s = state();
        let first = s.apply(LoadInput::Stage(Stage::Chart));
        assert_eq!(
            (first.seq, first.origin, first.revision),
            (1, TimeUs(1_000), 1)
        );
        assert_eq!(first.chart.title.as_deref(), Some("B"));
        let hard = shown_gauge(&rules::tapestry(), 0);
        assert!(hard.is_some());
        assert_eq!(shown_gauge(&rules::tapestry(), 99), None);
        s.apply(LoadInput::Gauge(hard));
        let same = s.apply(LoadInput::Chart(ChosenChart {
            title: Some("B".to_owned()),
            ..ChosenChart::default()
        }));
        assert_eq!((same.seq, same.revision, same.gauge), (3, 1, hard));
        let changed = s.apply(LoadInput::Chart(ChosenChart {
            title: Some("B".to_owned()),
            artist: Some("b".to_owned()),
            ..ChosenChart::default()
        }));
        assert_eq!((changed.seq, changed.revision), (4, 2));
        let later = s.apply(LoadInput::BankComplete);
        assert_eq!((later.seq, later.revision, later.gauge), (5, 2, hard));
        assert_eq!(later.chart.artist.as_deref(), Some("b"));
        assert_eq!(later.origin, TimeUs(1_000));
        assert_eq!(s.lifetime(), 3);
    }
}
