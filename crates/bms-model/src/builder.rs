//! Building charts in code, for tests across the workspace.

use crate::{
    BgmEvent, Chart, ChartIdentity, ChartMeta, ChartTimeline, LANE_COUNT, Lane, ModelError, Note,
    NoteAnchors, NoteKind, Position, ScrollChange, ScrollMap, SoundId, SoundRef, Speed, Tempo,
    TimeUs, TimelineId, TimelinePoint,
};

/// Builds a [`Chart`] from notes, sounds, bar lines and scroll changes given
/// in any order. Notes and BGM are sorted by time, each note head, long-note
/// end and bar line is placed where the scroll map puts its time, and the
/// result is validated. Its timeline has one row per distinct time of the
/// origin, a note endpoint, a bar line, a BPM change, a STOP's start or a
/// STOP's end, each with the tempo in effect from it; every endpoint and bar
/// line names the row of its time.
#[derive(Clone, Debug)]
pub struct ChartBuilder {
    bpm_milli: i64,
    /// Scroll changes, with BPMs still in thousandths.
    scroll: Vec<ScrollChange>,
    lanes: [Vec<Note>; LANE_COUNT],
    bgm: Vec<BgmEvent>,
    sounds: Vec<SoundRef>,
    bars: Vec<TimeUs>,
}

impl ChartBuilder {
    /// Starts a chart with an initial BPM in thousandths. The builder takes
    /// BPMs in thousandths for convenience; [`ScrollMap::new`] takes
    /// millionths.
    #[must_use]
    pub fn new(bpm_milli: i64) -> Self {
        Self {
            bpm_milli,
            lanes: Default::default(),
            bgm: Vec::new(),
            sounds: Vec::new(),
            scroll: Vec::new(),
            bars: Vec::new(),
        }
    }

    fn note(mut self, lane: Lane, time: TimeUs, kind: NoteKind) -> Self {
        self.lanes[lane.index()].push(Note {
            time,
            pos: Position(0),
            sound: None,
            kind,
            anchors: NoteAnchors::default(),
        });
        self
    }

    /// Adds a bar line at `at`.
    #[must_use]
    pub fn bar(mut self, at: TimeUs) -> Self {
        self.bars.push(at);
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
                end_pos: Position(0),
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

    /// Changes the BPM, in thousandths, at `at`.
    #[must_use]
    pub fn bpm(mut self, at: TimeUs, bpm_milli: i64) -> Self {
        self.scroll.push(ScrollChange::Bpm {
            at,
            bpm_micro: bpm_milli,
        });
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
    /// Returns the error of [`ScrollMap::new`], [`Chart::validate`] or the
    /// timeline ([`ModelError::Timeline`], for a time before 0); a BPM
    /// outside the tempo range is [`ModelError::InvalidBpm`].
    pub fn build(mut self) -> Result<Chart, ModelError> {
        for notes in &mut self.lanes {
            notes.sort_by_key(|n| n.time);
        }
        self.bgm.sort_by_key(|e| e.time);
        self.bars.sort_unstable();
        let to_micro = |milli: i64| milli.checked_mul(1_000).ok_or(ModelError::Overflow);
        // Each BPM change's time and BPM in thousandths, in the order given;
        // stable sorting keeps the later of two at one time last.
        let mut changes = Vec::new();
        for change in &mut self.scroll {
            if let ScrollChange::Bpm { at, bpm_micro } = change {
                changes.push((*at, *bpm_micro));
                *bpm_micro = to_micro(*bpm_micro)?;
            }
        }
        let scroll = ScrollMap::new(to_micro(self.bpm_milli)?, &self.scroll)?;
        let initial = tempo(TimeUs(0), self.bpm_milli)?;
        let mut tempos = Vec::with_capacity(changes.len());
        for (at, milli) in changes {
            tempos.push((at, tempo(at, milli)?));
        }
        tempos.sort_by_key(|&(at, _)| at);

        let mut times = vec![TimeUs(0)];
        times.extend(self.bars.iter().copied());
        for note in self.lanes.iter().flatten() {
            times.push(note.time);
            times.push(note.end());
        }
        for change in &self.scroll {
            match *change {
                ScrollChange::Bpm { at, .. } => times.push(at),
                ScrollChange::Stop { at, len } => {
                    times.push(at);
                    times.push(at.checked_add(len).ok_or(ModelError::Overflow)?);
                }
            }
        }
        times.sort_unstable();
        times.dedup();
        let row = |at: TimeUs| -> Result<TimelineId, ModelError> {
            let i = times.binary_search(&at).unwrap_or(0);
            u32::try_from(i)
                .map(TimelineId)
                .map_err(|_| ModelError::Overflow)
        };
        let mut points = Vec::with_capacity(times.len());
        for &at in &times {
            let changed = tempos.partition_point(|&(t, _)| t <= at);
            points.push(TimelinePoint {
                at,
                pos: scroll.position_at(at),
                tempo: changed.checked_sub(1).map_or(initial, |i| tempos[i].1),
            });
        }
        for note in self.lanes.iter_mut().flatten() {
            note.pos = scroll.position_at(note.time);
            let end = note.end();
            if let NoteKind::Long { end_pos, .. } = &mut note.kind {
                *end_pos = scroll.position_at(end);
            }
            note.anchors = NoteAnchors {
                head: row(note.time)?,
                end: row(end)?,
            };
        }
        let bar_lines = self.bars.iter().map(|&at| scroll.position_at(at)).collect();
        let bar_ids = self
            .bars
            .iter()
            .map(|&at| row(at))
            .collect::<Result<_, _>>()?;
        let timeline = ChartTimeline::new(points, bar_ids, initial, Speed::NORMAL)
            .map_err(ModelError::Timeline)?;
        let chart = Chart {
            lanes: self.lanes,
            bgm: self.bgm,
            sounds: self.sounds,
            bar_lines,
            scroll,
            timeline,
            meta: ChartMeta::default(),
            identity: ChartIdentity::default(),
        };
        // The links hold whenever this passes: each endpoint names the row
        // of its own time, and rows follow time.
        chart.validate()?;
        Ok(chart)
    }
}

/// The tempo of `milli` thousandths of a BPM, the change at `at`.
fn tempo(at: TimeUs, milli: i64) -> Result<Tempo, ModelError> {
    Tempo::from_milli(milli).ok_or(ModelError::InvalidBpm {
        at,
        bpm_micro: milli.saturating_mul(1_000),
    })
}

#[cfg(test)]
mod tests {
    use super::ChartBuilder;
    use crate::{BEAT, Lane, ModelError, NoteKind, Position, SoundId, TimeUs};

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
                end_pos: chart.scroll.position_at(TimeUs(1_500)),
                end_sound: None
            }
        );
        // Each note sits where the map puts its time.
        assert_eq!(lane1[0].pos, chart.scroll.position_at(TimeUs(500)));
        assert_eq!(lane1[1].pos, chart.scroll.position_at(TimeUs(2_000)));
        assert_eq!(lane1[1].kind, NoteKind::Tap);
        assert_eq!(chart.lanes[0][0].kind, NoteKind::Mine { damage: 20 });
        assert_eq!(chart.bgm.len(), 2);
        assert_eq!(chart.bgm[0].time, TimeUs(0));
        assert_eq!(chart.sounds[0].name, "kick.wav");
        assert_eq!(
            chart.scroll.position_at(TimeUs(1_000)),
            chart.scroll.position_at(TimeUs(1_250))
        );
        // 1 ms at 150 BPM: 0.0025 beat.
        assert_eq!(
            chart.scroll.position_at(TimeUs(1_000)),
            Position(BEAT / 400)
        );
    }

    #[test]
    fn validate_rejects_overlapping_long_notes() {
        let lane = key(3);
        let overlapping = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .long(lane, TimeUs(1_500), TimeUs(2_500))
            .build();
        assert_eq!(overlapping, Err(ModelError::Overlap { lane, index: 1 }));

        let early = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .tap(lane, TimeUs(1_999))
            .build();
        assert_eq!(early, Err(ModelError::Overlap { lane, index: 1 }));

        // A note may start at the µs the previous one ends, and several
        // notes may share a µs.
        let touching = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .tap(lane, TimeUs(2_000))
            .tap(lane, TimeUs(2_000))
            .build()
            .unwrap();
        assert_eq!(touching.lanes[lane.index()].len(), 3);

        let apart = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .long(lane, TimeUs(2_001), TimeUs(2_500))
            .long(key(4), TimeUs(1_500), TimeUs(2_500))
            .build()
            .unwrap();
        assert_eq!(apart.lanes[lane.index()].len(), 2);
    }

    #[test]
    fn validate_rejects_long_notes_ending_before_start() {
        let lane = key(2);
        let chart = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(999))
            .build();
        assert_eq!(
            chart,
            Err(ModelError::LongEndBeforeStart { lane, index: 0 })
        );
        // A long note may start and end in one µs.
        let one_us = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(1_000))
            .build()
            .unwrap();
        assert_eq!(one_us.lanes[lane.index()][0].end(), TimeUs(1_000));
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
    fn validate_checks_long_note_end_sounds() {
        let mut chart = ChartBuilder::new(BPM_150)
            .sound("a.wav")
            .long(key(6), TimeUs(10), TimeUs(20))
            .build()
            .unwrap();
        for (end_sound, expected) in [
            (SoundId(0), Ok(())),
            (
                SoundId(1),
                Err(ModelError::UnknownSound { sound: SoundId(1) }),
            ),
        ] {
            chart.lanes[6][0].kind = NoteKind::Long {
                end: TimeUs(20),
                end_pos: chart.lanes[6][0].end_pos(),
                end_sound: Some(end_sound),
            };
            assert_eq!(chart.validate(), expected);
        }
    }

    #[test]
    fn validate_rejects_positions_before_the_previous_end() {
        let lane = key(4);
        let mut chart = ChartBuilder::new(BPM_150)
            .long(lane, TimeUs(1_000), TimeUs(2_000))
            .tap(lane, TimeUs(3_000))
            .build()
            .unwrap();
        let end = chart.lanes[4][0].end_pos();
        // Equal positions are kept (a STOP holds them); a head before the
        // previous end, or an end before its head, is not.
        chart.lanes[4][1].pos = end;
        assert_eq!(chart.validate(), Ok(()));
        chart.lanes[4][1].pos = Position(end.0 - 1);
        assert_eq!(
            chart.validate(),
            Err(ModelError::PositionsUnsorted { lane, index: 1 })
        );
        chart.lanes[4][1].pos = end;
        let head = chart.lanes[4][0].pos;
        for (end_pos, expected) in [
            (head, Ok(())),
            (
                Position(head.0 - 1),
                Err(ModelError::PositionsUnsorted { lane, index: 0 }),
            ),
        ] {
            chart.lanes[4][0].kind = NoteKind::Long {
                end: TimeUs(2_000),
                end_pos,
                end_sound: None,
            };
            assert_eq!(chart.validate(), expected);
        }
    }

    #[test]
    fn validate_rejects_unsorted_bgm_and_bar_lines() {
        let mut chart = ChartBuilder::new(BPM_150)
            .sound("a.wav")
            .bgm(TimeUs(20), SoundId(0))
            .bgm(TimeUs(10), SoundId(0))
            .build()
            .unwrap();
        chart.bgm.swap(0, 1);
        assert_eq!(chart.validate(), Err(ModelError::BgmUnsorted { index: 1 }));
        chart.bgm.swap(0, 1);

        chart.bar_lines = vec![Position(10), Position(10), Position(20)];
        assert_eq!(chart.validate(), Ok(()));
        chart.bar_lines = vec![Position(10), Position(30), Position(20)];
        assert_eq!(
            chart.validate(),
            Err(ModelError::BarLinesUnsorted { index: 2 })
        );
    }

    #[test]
    fn builder_bpm_changes_take_thousandths() {
        // 150 BPM: 2.5 millionths of a beat per microsecond; then 60.5 BPM.
        let chart = ChartBuilder::new(BPM_150)
            .bpm(TimeUs(1_000), 60_500)
            .build()
            .unwrap();
        // floor(2^80 × 2.5e-6) units per µs.
        assert_eq!(chart.scroll.initial_rate(), BEAT / 400_000);
        assert_eq!(
            chart.scroll.position_at(TimeUs(1_000)),
            Position(BEAT / 400)
        );
        // 120_000 µs at 60.5 BPM is 0.121 beat, 0.1235 beat in all; the
        // map stays at or below it, within a unit per µs since the change.
        let exact = BEAT * 247 / 2_000;
        let d = exact - chart.scroll.position_at(TimeUs(121_000)).0;
        assert!((0..=120_000).contains(&d), "{d}");
        assert_eq!(
            ChartBuilder::new(i64::MAX).build(),
            Err(ModelError::Overflow)
        );
        assert_eq!(
            ChartBuilder::new(BPM_150).bpm(TimeUs(1), i64::MIN).build(),
            Err(ModelError::Overflow)
        );
    }

    #[test]
    fn lanes_are_scratch_and_seven_keys() {
        assert_eq!(Lane::SCRATCH.index(), 0);
        assert_eq!(Lane::new(7).map(Lane::index), Some(7));
        assert_eq!(Lane::new(8), None);
        assert_eq!(Lane::all().count(), 8);
    }
}
