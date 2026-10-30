//! Timed objects to lanes: long-note reconstruction and same-lane conflicts.

use std::collections::{BTreeMap, BTreeSet};

use bms_model::{BgmEvent, LANE_COUNT, Note, NoteKind, Position, SoundId, TimeUs};

use crate::parse::ObjectKind;
use crate::timing::Timed;
use crate::{Warning, WarningKind};

/// A note before conflicts are settled.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    note: Note,
    /// The real times of its head and its end ([`Timed::real`]).
    real: (TimeUs, TimeUs),
    /// The exact position of the note's head ([`Timed::rank`]).
    rank: usize,
    /// The exact position of its end: a long note's end, else `rank`.
    end_rank: usize,
    /// File order of the object, the tie-breaker (later wins).
    seq: usize,
    line: usize,
}

/// Long note 2, tap 1, mine 0: the winner at one position and lane.
const fn priority(kind: NoteKind) -> u8 {
    match kind {
        NoteKind::Long { .. } => 2,
        NoteKind::Tap => 1,
        NoteKind::Mine { .. } => 0,
    }
}

/// Maps `#WAV` ids to the dense sound table, warning once per undefined id.
pub struct Sounds<'a> {
    pub ids: &'a BTreeMap<u16, SoundId>,
    pub warned: BTreeSet<u16>,
}

impl Sounds<'_> {
    fn get(&mut self, id: u16, line: usize, warnings: &mut Vec<Warning>) -> Option<SoundId> {
        let sound = self.ids.get(&id).copied();
        if sound.is_none() && self.warned.insert(id) {
            warnings.push(Warning {
                line: Some(line),
                kind: WarningKind::UndefinedReference,
            });
        }
        sound
    }
}

/// The resolved notes per lane and the BGM, at 1×.
pub struct Resolved {
    pub lanes: [Vec<Note>; LANE_COUNT],
    pub bgm: Vec<BgmEvent>,
    /// At another speed, the real times of the same objects in the same
    /// order: each note's head and end, and each BGM event's.
    pub real: Option<RealTimes>,
}

/// The real times of [`Resolved`]'s objects.
pub struct RealTimes {
    pub lanes: [Vec<(TimeUs, TimeUs)>; LANE_COUNT],
    pub bgm: Vec<TimeUs>,
}

/// A `#LNTYPE 1` endpoint before pairing.
#[derive(Clone, Copy)]
struct Endpoint {
    time: TimeUs,
    real: TimeUs,
    pos: Position,
    rank: usize,
    id: u16,
    seq: usize,
    line: usize,
}

/// Builds lanes from objects sorted by position. Identity, conflicts and
/// pairing are decided on exact positions ([`Timed::rank`]), so objects at
/// distinct positions that floor to one µs stay distinct. With `mapped`,
/// the real times are kept beside them.
pub fn resolve(
    objects: &[Timed],
    mapped: bool,
    lnobj: &BTreeSet<u16>,
    sounds: &mut Sounds<'_>,
    warnings: &mut Vec<Warning>,
) -> Resolved {
    let mut candidates: [Vec<Candidate>; LANE_COUNT] = Default::default();
    let mut bgm = Vec::new();
    let mut real_bgm = Vec::new();
    // Per lane: the last plain note, which an #LNOBJ end may turn into a
    // long note.
    let mut last_plain: [Option<usize>; LANE_COUNT] = [None; LANE_COUNT];
    let mut endpoints: [Vec<Endpoint>; LANE_COUNT] = Default::default();

    for obj in objects {
        let warn = |warnings: &mut Vec<Warning>, kind| {
            warnings.push(Warning {
                line: Some(obj.line),
                kind,
            });
        };
        match obj.kind {
            ObjectKind::Bgm(id) => {
                if let Some(sound) = sounds.get(id, obj.line, warnings) {
                    bgm.push(BgmEvent {
                        time: obj.time,
                        sound,
                    });
                    if mapped {
                        real_bgm.push(obj.real);
                    }
                }
            }
            ObjectKind::Note(lane, id) if lnobj.contains(&id) => {
                let l = lane.index();
                let start = last_plain[l]
                    .take()
                    .map(|i| &mut candidates[l][i])
                    .filter(|c| c.rank < obj.rank);
                match start {
                    Some(start) => {
                        start.note.kind = NoteKind::Long {
                            end: obj.time,
                            end_pos: obj.pos,
                            end_sound: None,
                        };
                        start.end_rank = obj.rank;
                        start.real.1 = obj.real;
                    }
                    None => warn(warnings, WarningKind::UnpairedLongNoteEnd),
                }
            }
            ObjectKind::Note(lane, id) => {
                let sound = sounds.get(id, obj.line, warnings);
                let l = lane.index();
                last_plain[l] = Some(candidates[l].len());
                candidates[l].push(Candidate {
                    note: Note {
                        time: obj.time,
                        pos: obj.pos,
                        sound,
                        kind: NoteKind::Tap,
                    },
                    real: (obj.real, obj.real),
                    rank: obj.rank,
                    end_rank: obj.rank,
                    seq: obj.seq,
                    line: obj.line,
                });
            }
            ObjectKind::Long(lane, id) => endpoints[lane.index()].push(Endpoint {
                time: obj.time,
                real: obj.real,
                pos: obj.pos,
                rank: obj.rank,
                id,
                seq: obj.seq,
                line: obj.line,
            }),
            ObjectKind::Mine(lane, id) => {
                candidates[lane.index()].push(Candidate {
                    note: Note {
                        time: obj.time,
                        pos: obj.pos,
                        sound: None,
                        kind: NoteKind::Mine {
                            damage: i32::from(id),
                        },
                    },
                    real: (obj.real, obj.real),
                    rank: obj.rank,
                    end_rank: obj.rank,
                    seq: obj.seq,
                    line: obj.line,
                });
            }
            ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_) => {}
        }
    }
    for (l, lane) in endpoints.into_iter().enumerate() {
        pair_long_notes(lane, &mut candidates[l], sounds, warnings);
    }

    let mut lanes: [Vec<Note>; LANE_COUNT] = Default::default();
    let mut real_lanes: [Vec<(TimeUs, TimeUs)>; LANE_COUNT] = Default::default();
    for ((lane, notes), real) in candidates.into_iter().zip(&mut lanes).zip(&mut real_lanes) {
        (*notes, *real) = settle(lane, mapped, warnings);
    }
    let real = mapped.then_some(RealTimes {
        lanes: real_lanes,
        bgm: real_bgm,
    });
    Resolved { lanes, bgm, real }
}

/// Pairs one lane's `#LNTYPE 1` endpoints in position order. Endpoints at
/// one position merge first, keeping the later object in the file, so a
/// duplicated line does not split a long note.
fn pair_long_notes(
    mut lane: Vec<Endpoint>,
    out: &mut Vec<Candidate>,
    sounds: &mut Sounds<'_>,
    warnings: &mut Vec<Warning>,
) {
    lane.sort_unstable_by_key(|e| (e.rank, e.seq));
    let mut merged: Vec<Endpoint> = Vec::with_capacity(lane.len());
    for e in lane {
        if let Some(last) = merged.last_mut().filter(|last| last.rank == e.rank) {
            warnings.push(Warning {
                line: Some(last.line),
                kind: WarningKind::SamePositionNoteDropped,
            });
            *last = e;
        } else {
            merged.push(e);
        }
    }
    let (pairs, open) = merged.as_chunks::<2>();
    for &[start, end] in pairs {
        let sound = sounds.get(start.id, start.line, warnings);
        let end_sound = if end.id == start.id {
            None
        } else {
            sounds.get(end.id, end.line, warnings)
        };
        out.push(Candidate {
            note: Note {
                time: start.time,
                pos: start.pos,
                sound,
                kind: NoteKind::Long {
                    end: end.time,
                    end_pos: end.pos,
                    end_sound,
                },
            },
            real: (start.real, end.real),
            rank: start.rank,
            end_rank: end.rank,
            seq: start.seq,
            line: start.line,
        });
    }
    if let [open] = open {
        warnings.push(Warning {
            line: Some(open.line),
            kind: WarningKind::UnpairedLongNoteStart,
        });
    }
}

/// Settles one lane: at one position the highest priority wins, then the
/// later object in the file; a note at or inside an earlier long note's span
/// is dropped. With `mapped`, the kept notes' real times come beside them;
/// otherwise that list stays empty.
fn settle(
    mut lane: Vec<Candidate>,
    mapped: bool,
    warnings: &mut Vec<Warning>,
) -> (Vec<Note>, Vec<(TimeUs, TimeUs)>) {
    lane.sort_unstable_by_key(|c| (c.rank, priority(c.note.kind), c.seq));
    let mut kept: Vec<Note> = Vec::with_capacity(lane.len());
    let mut real = Vec::with_capacity(if mapped { lane.len() } else { 0 });
    // The end position of the last note kept.
    let mut last_end_rank: Option<usize> = None;
    for (i, c) in lane.iter().enumerate() {
        let drop = |warnings: &mut Vec<Warning>, kind| {
            warnings.push(Warning {
                line: Some(c.line),
                kind,
            });
        };
        if lane.get(i + 1).is_some_and(|next| next.rank == c.rank) {
            drop(warnings, WarningKind::SamePositionNoteDropped);
        } else if last_end_rank.is_some_and(|end| c.rank <= end) {
            drop(warnings, WarningKind::NoteInsideLongNoteDropped);
        } else {
            kept.push(c.note);
            if mapped {
                real.push(c.real);
            }
            last_end_rank = Some(c.end_rank);
        }
    }
    (kept, real)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bms_model::{Lane, NoteKind, Position, SoundId, TimeUs};

    use super::{Sounds, resolve};
    use crate::WarningKind;
    use crate::parse::ObjectKind;
    use crate::timing::Timed;

    fn at(time: i64, kind: ObjectKind) -> Timed {
        Timed {
            time: TimeUs(time),
            real: TimeUs(time),
            pos: Position(i128::from(time)),
            kind,
            line: usize::try_from(time).unwrap(),
            seq: 0,
            rank: usize::try_from(time).unwrap(),
        }
    }

    type Row = (i64, Option<SoundId>, NoteKind);

    fn run(objects: &[Timed]) -> (Vec<Row>, Vec<WarningKind>) {
        let ids = BTreeMap::from([(1, SoundId(0)), (2, SoundId(1))]);
        let mut sounds = Sounds {
            ids: &ids,
            warned: BTreeSet::new(),
        };
        let mut warnings = Vec::new();
        // File order is the order given.
        let objects: Vec<Timed> = (0..)
            .zip(objects)
            .map(|(seq, o)| Timed { seq, ..*o })
            .collect();
        let resolved = resolve(
            &objects,
            false,
            &BTreeSet::new(),
            &mut sounds,
            &mut warnings,
        );
        let notes = resolved.lanes[1]
            .iter()
            .map(|n| (n.time.0, n.sound, n.kind))
            .collect();
        (notes, warnings.into_iter().map(|w| w.kind).collect())
    }

    const K1: Lane = Lane::new(1).unwrap();

    #[test]
    fn mine_inside_long_note_is_dropped() {
        let (notes, warnings) = run(&[
            at(10, ObjectKind::Long(K1, 1)),
            at(15, ObjectKind::Mine(K1, 5)),
            at(20, ObjectKind::Long(K1, 1)),
            at(20, ObjectKind::Mine(K1, 5)),
            at(30, ObjectKind::Mine(K1, 5)),
        ]);
        assert_eq!(
            notes,
            vec![
                (
                    10,
                    Some(SoundId(0)),
                    NoteKind::Long {
                        end: TimeUs(20),
                        end_pos: Position(20),
                        end_sound: None
                    }
                ),
                (30, None, NoteKind::Mine { damage: 5 }),
            ]
        );
        assert_eq!(
            warnings,
            vec![
                WarningKind::NoteInsideLongNoteDropped,
                WarningKind::NoteInsideLongNoteDropped
            ]
        );
    }

    #[test]
    fn same_position_notes_keep_long_then_tap_then_later() {
        let (notes, warnings) = run(&[
            // A tap and a long-note start at one time: the long note wins.
            at(10, ObjectKind::Note(K1, 2)),
            at(10, ObjectKind::Long(K1, 1)),
            at(20, ObjectKind::Long(K1, 1)),
            // Two taps: the later in the file wins.
            at(30, ObjectKind::Note(K1, 1)),
            at(30, ObjectKind::Note(K1, 2)),
            // A tap and a mine: the tap wins.
            at(40, ObjectKind::Note(K1, 1)),
            at(40, ObjectKind::Mine(K1, 9)),
        ]);
        assert_eq!(notes.len(), 3);
        assert!(matches!(
            notes[0],
            (10, Some(SoundId(0)), NoteKind::Long { .. })
        ));
        assert_eq!(notes[1], (30, Some(SoundId(1)), NoteKind::Tap));
        assert_eq!(notes[2], (40, Some(SoundId(0)), NoteKind::Tap));
        assert_eq!(warnings, vec![WarningKind::SamePositionNoteDropped; 3]);
    }
}
