//! Building charts in code, for tests across the workspace.

use crate::{
    BgmEvent, Chart, ChartIdentity, ChartMeta, LANE_COUNT, Lane, ModelError, Note, NoteKind,
    ScrollChange, ScrollMap, SoundId, SoundRef, TimeUs,
};

/// Builds a [`Chart`] from notes, sounds and scroll changes given in any
/// order. Notes and BGM are sorted by time, and the result is validated.
#[derive(Clone, Debug)]
pub struct ChartBuilder {
    bpm_milli: i64,
    lanes: [Vec<Note>; LANE_COUNT],
    bgm: Vec<BgmEvent>,
    sounds: Vec<SoundRef>,
    scroll: Vec<ScrollChange>,
}

impl ChartBuilder {
    /// Starts a chart with an initial BPM in thousandths.
    #[must_use]
    pub fn new(bpm_milli: i64) -> Self {
        Self {
            bpm_milli,
            lanes: Default::default(),
            bgm: Vec::new(),
            sounds: Vec::new(),
            scroll: Vec::new(),
        }
    }

    fn note(mut self, lane: Lane, time: TimeUs, kind: NoteKind) -> Self {
        self.lanes[lane.index()].push(Note {
            time,
            sound: None,
            kind,
        });
        self
    }

    /// Adds a tap note.
    #[must_use]
    pub fn tap(self, lane: Lane, at: TimeUs) -> Self {
        self.note(lane, at, NoteKind::Tap)
    }

    /// Adds a long note from `at` to `end`.
    #[must_use]
    pub fn long(self, lane: Lane, at: TimeUs, end: TimeUs) -> Self {
        self.note(
            lane,
            at,
            NoteKind::Long {
                end,
                end_sound: None,
            },
        )
    }

    /// Adds a mine.
    #[must_use]
    pub fn mine(self, lane: Lane, at: TimeUs, damage: i32) -> Self {
        self.note(lane, at, NoteKind::Mine { damage })
    }

    /// Appends `name` to the sound table; the first sound is `SoundId(0)`.
    #[must_use]
    pub fn sound(mut self, name: &str) -> Self {
        self.sounds.push(SoundRef {
            name: name.to_owned(),
            path: None,
        });
        self
    }

    /// Adds a BGM event.
    #[must_use]
    pub fn bgm(mut self, at: TimeUs, sound: SoundId) -> Self {
        self.bgm.push(BgmEvent { time: at, sound });
        self
    }

    /// Changes the BPM at `at`.
    #[must_use]
    pub fn bpm(mut self, at: TimeUs, bpm_milli: i64) -> Self {
        self.scroll.push(ScrollChange::Bpm { at, bpm_milli });
        self
    }

    /// Adds a STOP.
    #[must_use]
    pub fn stop(mut self, at: TimeUs, len: TimeUs) -> Self {
        self.scroll.push(ScrollChange::Stop { at, len });
        self
    }

    /// Builds and validates the chart.
    ///
    /// # Errors
    ///
    /// Returns the error of [`ScrollMap::new`] or [`Chart::validate`].
    pub fn build(mut self) -> Result<Chart, ModelError> {
        for notes in &mut self.lanes {
            notes.sort_by_key(|n| n.time);
        }
        self.bgm.sort_by_key(|e| e.time);
        let chart = Chart {
            lanes: self.lanes,
            bgm: self.bgm,
            sounds: self.sounds,
            bar_lines: Vec::new(),
            scroll: ScrollMap::new(self.bpm_milli, &self.scroll)?,
            meta: ChartMeta::default(),
            identity: ChartIdentity::default(),
        };
        chart.validate()?;
        Ok(chart)
    }
}

#[cfg(test)]
mod tests {
    use super::ChartBuilder;
    use crate::{Lane, ModelError, NoteKind, Position, SoundId, TimeUs};

    const BPM_150: i64 = 150_000;

    fn key(n: u8) -> Lane {
        Lane::new(n).unwrap()
    }

    #[test]
    fn builder_sorts_lanes_and_keeps_kinds() {
        let chart = ChartBuilder::new(BPM_150)
            .sound("kick.wav")
            .tap(key(1), TimeUs(2_000))
            .long(key(1), TimeUs(500), TimeUs(1_500))
            .mine(Lane::SCRATCH, TimeUs(100), 20)
            .bgm(TimeUs(300), SoundId(0))
            .bgm(TimeUs(0), SoundId(0))
            .stop(TimeUs(1_000), TimeUs(250))
            .build()
            .unwrap();
        let lane1 = &chart.lanes[1];
        assert_eq!(lane1.len(), 2);
        assert_eq!(lane1[0].time, TimeUs(500));
        assert_eq!(
            lane1[0].kind,
            NoteKind::Long {
                end: TimeUs(1_500),
                end_sound: None
            }
        );
        assert_eq!(lane1[1].kind, NoteKind::Tap);
        assert_eq!(chart.lanes[0][0].kind, NoteKind::Mine { damage: 20 });
        assert_eq!(chart.bgm.len(), 2);
        assert_eq!(chart.bgm[0].time, TimeUs(0));
        assert_eq!(chart.sounds[0].name, "kick.wav");
        assert_eq!(
            chart.scroll.position_at(TimeUs(1_000)),
            chart.scroll.position_at(TimeUs(1_250))
        );
        assert_eq!(chart.scroll.position_at(TimeUs(1_000)), Position(2_500));
    }

    #[test]
    fn validate_rejects_overlapping_long_notes() {
        let lane = key(3);
        let overlapping = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .long(lane, TimeUs(1_500), TimeUs(2_500))
            .build();
        assert_eq!(overlapping, Err(ModelError::Overlap { lane, index: 1 }));

        let touching = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .tap(lane, TimeUs(2_000))
            .build();
        assert_eq!(touching, Err(ModelError::Overlap { lane, index: 1 }));

        let apart = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .long(lane, TimeUs(2_001), TimeUs(2_500))
            .long(key(4), TimeUs(1_500), TimeUs(2_500))
            .build()
            .unwrap();
        assert_eq!(apart.lanes[lane.index()].len(), 2);
    }

    #[test]
    fn validate_rejects_long_notes_ending_at_or_before_start() {
        let lane = key(2);
        for end in [999, 1_000] {
            let chart = ChartBuilder::new(BPM_150)
                .long(lane, TimeUs(1_000), TimeUs(end))
                .build();
            assert_eq!(
                chart,
                Err(ModelError::LongEndNotAfterStart { lane, index: 0 })
            );
        }
    }

    #[test]
    fn validate_rejects_unsorted_notes_and_unknown_sounds() {
        let mut chart = ChartBuilder::new(BPM_150)
            .sound("a.wav")
            .tap(key(5), TimeUs(10))
            .tap(key(5), TimeUs(20))
            .build()
            .unwrap();
        chart.lanes[5].swap(0, 1);
        assert_eq!(
            chart.validate(),
            Err(ModelError::Unsorted {
                lane: key(5),
                index: 1
            })
        );
        chart.lanes[5].swap(0, 1);
        chart.lanes[5][0].sound = Some(SoundId(1));
        assert_eq!(
            chart.validate(),
            Err(ModelError::UnknownSound { sound: SoundId(1) })
        );

        let bgm = ChartBuilder::new(BPM_150)
            .bgm(TimeUs(0), SoundId(0))
            .build();
        assert_eq!(bgm, Err(ModelError::UnknownSound { sound: SoundId(0) }));
    }

    #[test]
    fn lanes_are_scratch_and_seven_keys() {
        assert_eq!(Lane::SCRATCH.index(), 0);
        assert_eq!(Lane::new(7).map(Lane::index), Some(7));
        assert_eq!(Lane::new(8), None);
        assert_eq!(Lane::all().count(), 8);
    }
}
