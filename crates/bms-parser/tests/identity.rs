//! Object identity on exact positions (design.md "What is rounded"):
//! objects at distinct positions stay distinct even when they floor to one
//! µs or quantize to one display position, and objects at one position
//! conflict as they always have.

use bms_model::{BEAT, Note, NoteKind, Position, SoundId, TimeUs};
use bms_parser::{LoadOptions, Loaded, WarningKind, load_bms};

fn load(text: &str) -> Loaded {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).unwrap();
    loaded
        .warnings
        .retain(|w| w.kind != WarningKind::MissingSoundFile);
    loaded
}

fn kinds(loaded: &Loaded) -> Vec<WarningKind> {
    loaded.warnings.iter().map(|w| w.kind.clone()).collect()
}

/// `slots` object pairs with `id` at `index` and `00` elsewhere.
fn line(slots: usize, index: usize, id: &str) -> String {
    (0..slots)
        .map(|i| if i == index { id } else { "00" })
        .collect()
}

/// `floor(2^80 × n / d)` beats, for small `n` and `d`.
fn units(n: i128, d: i128) -> Position {
    Position(BEAT * n / d)
}

const HEAD: &str = "#BPM 120\n#WAV01 a.wav\n#WAV02 b.wav\n";

#[test]
fn taps_at_distinct_positions_in_one_microsecond_both_stay() {
    // Measure 0 is 4e-6 beat, 2 µs: taps at 1/4 (0.5 µs) and 1/8 (0.25 µs)
    // floor to µs 0, each at its own beat, in position order.
    let text = format!(
        "{HEAD}#00002:0.000001\n#00011:{}\n#00011:{}\n",
        line(4, 1, "01"),
        line(8, 1, "02")
    );
    let loaded = load(&text);
    let tap = |pos, sound| Note {
        time: TimeUs(0),
        pos,
        sound: Some(SoundId(sound)),
        kind: NoteKind::Tap,
    };
    assert_eq!(
        loaded.chart.lanes[1],
        [tap(units(4, 8_000_000), 1), tap(units(4, 4_000_000), 0)]
    );
    assert_eq!(kinds(&loaded), []);
}

#[test]
fn positions_that_quantize_together_stay_distinct() {
    // Measure 0 is 4e-18 beat: its 1/4096 and 1/4097 both sit at 0 µs and
    // at display position floor(2^80 × 4e-18 / 4096) = 1180 =
    // floor(2^80 × 4e-18 / 4097), yet they are distinct positions.
    let a = line(4096, 1, "01");
    let b = line(4097, 1, "02");
    for (first, second) in [(&a, &b), (&b, &a)] {
        let text = format!("{HEAD}#00002:0.000000000000000001\n#00011:{first}\n#00011:{second}\n");
        let loaded = load(&text);
        let lane = &loaded.chart.lanes[1];
        // Position order: 1/4097 (sound 02) before 1/4096 (sound 01).
        let got: Vec<_> = lane.iter().map(|n| (n.time, n.pos, n.sound)).collect();
        assert_eq!(
            got,
            [
                (TimeUs(0), Position(1180), Some(SoundId(1))),
                (TimeUs(0), Position(1180), Some(SoundId(0))),
            ]
        );
        assert_eq!(kinds(&loaded), []);
    }
}

#[test]
fn long_notes_may_start_and_end_in_one_microsecond() {
    // Measure 0 is 2 µs. On key 1 an #LNTYPE 1 long note from 0 to 1/4
    // (0.5 µs); on key 2 an #LNOBJ long note over the same span.
    let text = format!(
        "{HEAD}#LNTYPE 1\n#LNOBJ ZZ\n#00002:0.000001\n#00051:{}\n#00012:{}\n#00012:{}\n",
        "01010000",
        line(4, 0, "01"),
        line(4, 1, "ZZ"),
    );
    let loaded = load(&text);
    for l in [1, 2] {
        let lane = &loaded.chart.lanes[l];
        assert_eq!(lane.len(), 1, "key {l}");
        assert_eq!(lane[0].time, TimeUs(0));
        assert_eq!(lane[0].pos, Position(0));
        assert!(matches!(
            lane[0].kind,
            NoteKind::Long { end: TimeUs(0), end_pos, .. } if end_pos == units(1, 1_000_000)
        ));
    }
    assert_eq!(kinds(&loaded), []);
}

#[test]
fn a_head_may_start_at_the_microsecond_the_previous_long_note_ends() {
    // Measure 0 is 2 µs: a long note on key 3 from 0 to 1/8 (0.25 µs), a
    // tap at 1/4 (0.5 µs), both ends in µs 0.
    let text = format!(
        "{HEAD}#LNTYPE 1\n#00002:0.000001\n#00053:{}\n#00013:{}\n",
        "0101000000000000",
        line(8, 2, "02")
    );
    let loaded = load(&text);
    let lane = &loaded.chart.lanes[3];
    assert_eq!(lane.len(), 2);
    assert!(matches!(
        lane[0].kind,
        NoteKind::Long { end: TimeUs(0), .. }
    ));
    assert_eq!((lane[1].time, lane[1].kind), (TimeUs(0), NoteKind::Tap));
    assert_eq!(lane[1].pos, units(1, 1_000_000));
    assert_eq!(kinds(&loaded), []);
}

#[test]
fn a_mine_may_share_a_microsecond_with_a_tap() {
    // A tap at 0 and a mine of damage 10 at 1/4 of a 2 µs measure.
    let text = format!(
        "{HEAD}#00002:0.000001\n#00014:{}\n#000D4:{}\n",
        line(4, 0, "01"),
        line(4, 1, "0A")
    );
    let loaded = load(&text);
    let lane: Vec<_> = loaded.chart.lanes[4]
        .iter()
        .map(|n| (n.time, n.kind))
        .collect();
    assert_eq!(
        lane,
        [
            (TimeUs(0), NoteKind::Tap),
            (TimeUs(0), NoteKind::Mine { damage: 10 })
        ]
    );
    assert_eq!(kinds(&loaded), []);
}

#[test]
fn objects_at_one_position_still_conflict() {
    // 1/2 and 2/4 are one position: the later line wins, with a warning.
    let text = format!(
        "{HEAD}#00011:{}\n#00011:{}\n",
        line(2, 1, "01"),
        line(4, 2, "02")
    );
    let loaded = load(&text);
    let lane = &loaded.chart.lanes[1];
    assert_eq!(lane.len(), 1);
    assert_eq!(lane[0].sound, Some(SoundId(1)));
    assert_eq!(kinds(&loaded), [WarningKind::SamePositionNoteDropped]);

    // Two taps on a STOP's position share its start: one is kept, sounding
    // at the STOP's start.
    let text = format!("{HEAD}#STOP01 96\n#00109:01\n#00111:01\n#00111:02\n#00211:01\n",);
    let loaded = load(&text);
    let lane = &loaded.chart.lanes[1];
    assert_eq!(lane.len(), 2);
    assert_eq!(
        (lane[0].time, lane[0].sound),
        (TimeUs(2_000_000), Some(SoundId(1)))
    );
    assert_eq!(kinds(&loaded), [WarningKind::SamePositionNoteDropped]);
}

#[test]
fn notes_split_by_a_bar_line_only_position_stay_distinct() {
    // Measures 0 and 1 are 4e-12 beat each, 2e-6 µs: a tap at the last
    // quarter of measure 0 and one at the start of measure 2, with only
    // measure 1's bar line between them, both floor to µs 0 and stay
    // apart.
    let text = format!(
        "{HEAD}#00002:0.000000000001\n#00102:0.000000000001\n#00011:{}\n#00211:01\n",
        line(4, 3, "01")
    );
    let loaded = load(&text);
    let lane = &loaded.chart.lanes[1];
    assert_eq!(lane.len(), 2);
    assert_eq!((lane[0].time, lane[1].time), (TimeUs(0), TimeUs(0)));
    assert_eq!(loaded.chart.bar_lines.len(), 3);
    assert_eq!(kinds(&loaded), []);
}
