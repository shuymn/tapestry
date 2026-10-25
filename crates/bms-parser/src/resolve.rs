//! Timed objects to lanes: long-note reconstruction and same-lane conflicts.

use std::collections::{BTreeMap, BTreeSet};

use bms_model::{BgmEvent, LANE_COUNT, Note, NoteKind, SoundId, TimeUs};

use crate::parse::ObjectKind;
use crate::timing::Timed;
use crate::{Warning, WarningKind};

/// A note before conflicts are settled.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    note: Note,
    /// Long note 2, tap 1, mine 0: the winner at one time and lane.
    priority: u8,
    /// File order of the object, the tie-breaker (later wins).
    order: usize,
    line: usize,
}

impl Candidate {
    fn new(note: Note, order: usize, line: usize) -> Self {
        let priority = match note.kind {
            NoteKind::Long { .. } => 2,
            NoteKind::Tap => 1,
            NoteKind::Mine { .. } => 0,
        };
        Self {
            note,
            priority,
            order,
            line,
        }
    }
}

/// Maps `#WAV` ids to the dense sound table, warning once per undefined id.
pub struct Sounds<'a> {
    pub ids: &'a BTreeMap<u16, SoundId>,
    pub warned: Vec<u16>,
}

impl Sounds<'_> {
    fn get(&mut self, id: u16, line: usize, warnings: &mut Vec<Warning>) -> Option<SoundId> {
        let sound = self.ids.get(&id).copied();
        if sound.is_none() && !self.warned.contains(&id) {
            self.warned.push(id);
            warnings.push(Warning {
                line: Some(line),
                kind: WarningKind::UndefinedReference,
            });
        }
        sound
    }
}

/// The resolved notes per lane and the BGM.
pub struct Resolved {
    pub lanes: [Vec<Note>; LANE_COUNT],
    pub bgm: Vec<BgmEvent>,
}

/// A `#LNTYPE 1` endpoint before pairing.
#[derive(Clone, Copy)]
struct Endpoint {
    time: TimeUs,
    id: u16,
    seq: usize,
    line: usize,
}

/// Builds lanes from objects sorted by time.
pub fn resolve(
    objects: &[Timed],
    lnobj: &BTreeSet<u16>,
    sounds: &mut Sounds<'_>,
    warnings: &mut Vec<Warning>,
) -> Resolved {
    let mut candidates: [Vec<Candidate>; LANE_COUNT] = Default::default();
    let mut bgm = Vec::new();
    // Per lane: the last plain note, which an #LNOBJ end may turn into a
    // long note; and an open #LNTYPE 1 start.
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
                }
            }
            ObjectKind::Note(lane, id) if lnobj.contains(&id) => {
                let l = lane.index();
                let start = last_plain[l]
                    .take()
                    .map(|i| &mut candidates[l][i])
                    .filter(|c| c.note.time < obj.time);
                match start {
                    Some(start) => {
                        start.note.kind = NoteKind::Long {
                            end: obj.time,
                            end_sound: None,
                        };
                        start.priority = 2;
                    }
                    None => warn(warnings, WarningKind::UnpairedLongNoteEnd),
                }
            }
            ObjectKind::Note(lane, id) => {
                let sound = sounds.get(id, obj.line, warnings);
                let l = lane.index();
                last_plain[l] = Some(candidates[l].len());
                candidates[l].push(Candidate::new(
                    Note {
                        time: obj.time,
                        sound,
                        kind: NoteKind::Tap,
                    },
                    obj.seq,
                    obj.line,
                ));
            }
            ObjectKind::Long(lane, id) => endpoints[lane.index()].push(Endpoint {
                time: obj.time,
                id,
                seq: obj.seq,
                line: obj.line,
            }),
            ObjectKind::Mine(lane, id) => {
                candidates[lane.index()].push(Candidate::new(
                    Note {
                        time: obj.time,
                        sound: None,
                        kind: NoteKind::Mine {
                            damage: i32::from(id),
                        },
                    },
                    obj.seq,
                    obj.line,
                ));
            }
            ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_) => {}
        }
    }
    for (l, lane) in endpoints.into_iter().enumerate() {
        pair_long_notes(lane, &mut candidates[l], sounds, warnings);
    }

    let lanes = candidates.map(|lane| settle(lane, warnings));
    Resolved { lanes, bgm }
}

/// Pairs one lane's `#LNTYPE 1` endpoints in time order. Endpoints at one
/// time merge first, keeping the later object in the file, so a duplicated
/// line does not split a long note.
fn pair_long_notes(
    mut lane: Vec<Endpoint>,
    out: &mut Vec<Candidate>,
    sounds: &mut Sounds<'_>,
    warnings: &mut Vec<Warning>,
) {
    lane.sort_by_key(|e| (e.time, e.seq));
    let mut merged: Vec<Endpoint> = Vec::with_capacity(lane.len());
    for e in lane {
        if let Some(last) = merged.last_mut().filter(|last| last.time == e.time) {
            warnings.push(Warning {
                line: Some(last.line),
                kind: WarningKind::SameTimeNoteDropped,
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
        out.push(Candidate::new(
            Note {
                time: start.time,
                sound,
                kind: NoteKind::Long {
                    end: end.time,
                    end_sound,
                },
            },
            start.seq,
            start.line,
        ));
    }
    if let [open] = open {
        warnings.push(Warning {
            line: Some(open.line),
            kind: WarningKind::UnpairedLongNoteStart,
        });
    }
}

/// Settles one lane: at one time the highest priority wins, then the later
/// object in the file; a note at or inside an earlier long note's span is
/// dropped.
fn settle(mut lane: Vec<Candidate>, warnings: &mut Vec<Warning>) -> Vec<Note> {
    lane.sort_by_key(|c| (c.note.time, c.priority, c.order));
    let mut kept: Vec<Note> = Vec::with_capacity(lane.len());
    for (i, c) in lane.iter().enumerate() {
        let drop = |warnings: &mut Vec<Warning>, kind| {
            warnings.push(Warning {
                line: Some(c.line),
                kind,
            });
        };
        if lane
            .get(i + 1)
            .is_some_and(|next| next.note.time == c.note.time)
        {
            drop(warnings, WarningKind::SameTimeNoteDropped);
        } else if kept.last().is_some_and(|prev| c.note.time <= prev.end()) {
            drop(warnings, WarningKind::NoteInsideLongNoteDropped);
        } else {
            kept.push(c.note);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bms_model::{Lane, NoteKind, SoundId, TimeUs};

    use super::{Sounds, resolve};
    use crate::WarningKind;
    use crate::parse::ObjectKind;
    use crate::timing::Timed;

    fn at(time: i64, kind: ObjectKind) -> Timed {
        Timed {
            time: TimeUs(time),
            kind,
            line: usize::try_from(time).unwrap(),
            seq: 0,
        }
    }

    type Row = (i64, Option<SoundId>, NoteKind);

    fn run(objects: &[Timed]) -> (Vec<Row>, Vec<WarningKind>) {
        let ids = BTreeMap::from([(1, SoundId(0)), (2, SoundId(1))]);
        let mut sounds = Sounds {
            ids: &ids,
            warned: Vec::new(),
        };
        let mut warnings = Vec::new();
        // File order is the order given.
        let objects: Vec<Timed> = (0..)
            .zip(objects)
            .map(|(seq, o)| Timed { seq, ..*o })
            .collect();
        let resolved = resolve(&objects, &BTreeSet::new(), &mut sounds, &mut warnings);
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
    fn same_time_notes_keep_long_then_tap_then_later() {
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
        assert_eq!(warnings, vec![WarningKind::SameTimeNoteDropped; 3]);
    }
}
