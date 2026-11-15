//! The play's judgment bins through the logic step (design.md "Rendering
//! and Skins", the note snapshot's judgment bins): each judged note counted
//! once in its head row's bin of the play's 1× profile and in the result
//! timelines' channel, whatever effects are lost; nothing else counts;
//! every view buffer carries the counts of the step that wrote it; a
//! rearm clears them; a chart whose rows are not the profile's leaves
//! them unavailable; and the play's last counts agree with the result's
//! timelines in their per-channel totals.

mod common;

use bms_model::{
    Chart, ChartBuilder, ChartProfile, ChartSummary, ChartTimeline, Lane, NoteKind, Speed, TimeUs,
    TimelinePoint,
};
use common::{MS, device, judgments, lane, offset_taps, press, release, tap_time};
use eval::{InputStart, JudgeEvent, JudgeEventKind, PhysicalInputEvent};
use game::{JUDGE_CHANNELS, JudgeBins, Logic, LogicConfig, RenderSide, ResultBasis, Timelines};
use input::{SourceReader, SourceWriter, source_queue};
use replay::verify::Coverage;
use rules::{Judgment, RulePreset};

type Channels = [u32; JUDGE_CHANNELS];

/// `chart` with its 1× length recorded, as the loader records it.
fn with_length(mut chart: Chart) -> Chart {
    chart.meta.length = ChartSummary::of(&chart).length;
    chart
}

/// One play: the logic step, its render side, the source's writer, the
/// inputs not yet pushed and every event emitted so far.
struct Play<'a> {
    logic: Logic<'a, SourceReader>,
    side: RenderSide,
    writer: SourceWriter,
    inputs: Vec<PhysicalInputEvent>,
    stream: Vec<JudgeEvent>,
}

impl<'a> Play<'a> {
    fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        dev: &'a eval::DeviceProfile,
        section: Option<&eval::Section>,
        effects: usize,
        mut inputs: Vec<PhysicalInputEvent>,
    ) -> Self {
        let (writer, reader) = source_queue(4_096);
        let (logic, side) = Logic::with_section(
            chart,
            rule,
            dev,
            &InputStart::default(),
            section,
            vec![reader],
            LogicConfig {
                inputs: 4_096,
                effects,
            },
            Vec::new(),
        )
        .unwrap();
        inputs.sort_by_key(|e| e.received);
        Self {
            logic,
            side,
            writer,
            inputs,
            stream: Vec::new(),
        }
    }

    /// Pushes the inputs received at or before `until`, publishes the source
    /// through `until` and steps there.
    fn step(&mut self, until: i64) {
        let n = self
            .inputs
            .iter()
            .take_while(|e| e.received.0 <= until)
            .count();
        for ev in self.inputs.drain(..n) {
            self.writer.push(ev).unwrap();
        }
        self.writer.publish(TimeUs(until));
        self.logic.advance_to(TimeUs(until));
        self.stream.extend_from_slice(self.logic.events());
    }

    /// The newest view's bins.
    fn bins(&mut self) -> JudgeBins {
        (*self.side.view().judge_bins).clone()
    }
}

/// Every bin's channels summed.
fn totals(bins: &[Channels]) -> Channels {
    let mut t = [0; JUDGE_CHANNELS];
    for b in bins {
        for (t, &c) in t.iter_mut().zip(b) {
            *t += c;
        }
    }
    t
}

fn sum(c: &Channels) -> u32 {
    c.iter().sum()
}

// T2's fixture: times in ms at 1×.
fn t2_chart() -> Chart {
    let mut b = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_200 * MS))
        .tap(lane(2), TimeUs(1_500 * MS))
        .tap(lane(3), TimeUs(1_800 * MS));
    for i in 0..23_i64 {
        let l = lane(u8::try_from(i % 7).unwrap() + 1);
        b = b.tap(l, TimeUs((2_000 + 40 * i) * MS));
    }
    with_length(
        b.long(lane(4), TimeUs(3_200 * MS), TimeUs(3_800 * MS))
            .tap(lane(5), TimeUs(3_500 * MS))
            .mine(lane(6), TimeUs(3_400 * MS), 3)
            .build()
            .unwrap(),
    )
}

/// T2's inputs at 1×, each time divided by `num / den` for a play at that
/// speed.
fn t2_inputs(num: i64, den: i64) -> Vec<PhysicalInputEvent> {
    let at = |ms: i64| ms * MS * den / num;
    let mut v = Vec::new();
    for (l, p, r) in [
        (1, 1_200, 1_250),
        (2, 1_470, 1_550),
        (3, 1_950, 2_000),
        (1, 2_070, 2_090),
        (3, 2_110, 2_130),
        (5, 3_200, 3_230),
        (4, 3_225, 3_680),
        (6, 3_390, 3_420),
    ] {
        v.push(press(at(p), 0, lane(l)));
        v.push(release(at(r), 0, lane(l)));
    }
    v
}

const PG: usize = 0;
const GR_E: usize = 1;
const GD_E: usize = 2;
const BD_E: usize = 3;
const GR_L: usize = 5;
const GD_L: usize = 6;
const BD_L: usize = 7;
const PR_L: usize = 8;

fn ch(pairs: &[(usize, u32)]) -> Channels {
    let mut c = [0; JUDGE_CHANNELS];
    for &(i, n) in pairs {
        c[i] = n;
    }
    c
}

/// T2: the logic step's own events, then the bins: one count per judged
/// note in its second's bin and its channel (a miss late, an LN by the
/// farther of head and release), none for the two empty POORs and the
/// mine hit; at 3 500 ms the held LN and the pending tap count nothing
/// yet; the end's sum is the tally's judged count.
#[test]
fn each_judged_note_counts_once_in_its_bin_and_channel() {
    let chart = t2_chart();
    let profile = ChartProfile::of(&chart, &[]);
    assert_eq!(profile.bins().len(), 5);
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut p = Play::new(&chart, &rule, &dev, None, 1_024, t2_inputs(1, 1));
    assert!(p.logic.bin_judgments(&chart, &profile));
    // READY: available and empty, before any step.
    let ready = p.bins();
    assert!(ready.available);
    assert_eq!(totals(&ready.bins), [0; JUDGE_CHANNELS]);

    for until in (100..=2_300).step_by(100) {
        p.step(until * MS);
    }
    let at_2300 = p.bins();
    assert_eq!(at_2300.bins[0], [0; JUDGE_CHANNELS]);
    assert_eq!(at_2300.bins[1], ch(&[(PG, 1), (GR_E, 1), (PR_L, 1)]));
    assert_eq!(at_2300.bins[2], ch(&[(BD_E, 1), (GD_L, 1), (PR_L, 2)]));
    assert_eq!(at_2300.bins[3], [0; JUDGE_CHANNELS]);
    assert_eq!(at_2300.bins[4], [0; JUDGE_CHANNELS]);

    for until in (2_400..=3_500).step_by(100) {
        p.step(until * MS);
    }
    let at_3500 = p.bins();
    assert_eq!(
        at_3500.bins[3], [0; JUDGE_CHANNELS],
        "the LN held, the tap pending"
    );

    for until in (3_600..=5_000).step_by(100) {
        p.step(until * MS);
    }
    let end = p.bins();
    assert_eq!(end.bins[1], ch(&[(PG, 1), (GR_E, 1), (PR_L, 1)]));
    assert_eq!(end.bins[2], ch(&[(BD_E, 1), (GD_L, 1), (PR_L, 21)]));
    assert_eq!(end.bins[3], ch(&[(GR_E, 1), (PR_L, 1)]));
    assert!(end.bins[5..].iter().all(|b| *b == [0; JUDGE_CHANNELS]));
    let judged = p.logic.view().tally.counts;
    let tally = judged.perfect + judged.great + judged.good + judged.bad + judged.poor;
    assert_eq!(sum(&totals(&end.bins)), 28);
    assert_eq!(tally, 28);

    // The events the counts came from.
    let mut got = judgments(&p.stream);
    got.sort_by_key(|&(l, n, ..)| (l, n));
    let expect_head = [
        (1, 0, Judgment::Perfect, Some(0)),
        (1, 1, Judgment::Good, Some(70 * MS)),
        (2, 0, Judgment::Great, Some(-30 * MS)),
        (3, 0, Judgment::Poor, None),
        (3, 1, Judgment::Bad, Some(-130 * MS)),
    ];
    for e in expect_head {
        assert!(got.contains(&e), "{e:?} not in {got:?}");
    }
    let ln = p
        .stream
        .iter()
        .find_map(|e| match e.kind {
            JudgeEventKind::Judged {
                lane: l,
                judgment,
                deviation,
                release,
                ..
            } if l == lane(4) && judgment != Judgment::Poor => Some((judgment, deviation, release)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        ln,
        (
            Judgment::Great,
            Some(TimeUs(25 * MS)),
            Some(TimeUs(-120 * MS))
        )
    );
    let empty: Vec<i64> = p
        .stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
        .map(|e| e.time.0 / MS)
        .collect();
    assert_eq!(empty, [2_110, 3_200]);
    let mines = p
        .stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::MineHit { .. }))
        .count();
    assert_eq!(mines, 1);
}

/// T2b: the side is the tally's, in µs: under a preset whose PERFECT
/// window is ±0.4 ms, a tap pressed 500 µs late is GREAT late (truncated
/// to ms it would read 0, early); a BAD long-note head counts at its
/// press, in its head's bin, while the note is still held.
#[test]
fn the_side_is_the_tallys_and_a_bad_head_counts_at_its_press() {
    let chart = with_length(
        ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .long(lane(2), TimeUs(2_000 * MS), TimeUs(4_000 * MS))
            .build()
            .unwrap(),
    );
    let profile = ChartProfile::of(&chart, &[]);
    let mut rule = rules::tapestry();
    rule.tables.keys.windows.perfect = rules::Window::symmetric(400);
    rule.validate().unwrap();
    let dev = device(&[0]);
    let inputs = vec![
        press(1_000 * MS + 500, 0, lane(1)),
        release(1_100 * MS, 0, lane(1)),
        press(2_150 * MS, 0, lane(2)),
        release(3_650 * MS, 0, lane(2)),
    ];
    let mut p = Play::new(&chart, &rule, &dev, None, 64, inputs);
    assert!(p.logic.bin_judgments(&chart, &profile));
    for until in (100..=1_500).step_by(100) {
        p.step(until * MS);
    }
    assert_eq!(judgments(&p.stream), [(1, 0, Judgment::Great, Some(500))]);
    assert_eq!(p.bins().bins[1], ch(&[(GR_L, 1)]));
    // The head pressed 150 ms late: BAD, counted from the press's step on.
    p.step(2_200 * MS);
    assert_eq!(p.bins().bins[2], ch(&[(BD_L, 1)]));
    // Still held 1.3 s later: the same count, nothing more.
    p.step(3_500 * MS);
    assert!(p.logic.view().input.lane(lane(2)).held);
    assert_eq!(p.bins().bins[2], ch(&[(BD_L, 1)]));
    p.step(5_000 * MS);
    assert_eq!(p.bins().bins[2], ch(&[(BD_L, 1)]));
    assert_eq!(sum(&totals(&p.bins().bins)), 2);
}

/// T3: with the effect queue holding one effect, all 130 judgments count;
/// each view buffer carries the counts of the step that wrote it, an idle
/// step after a judging one bringing a stale buffer up to date; the
/// terminal view equals the last publication; a rearm empties the bins
/// and keeps them available.
#[test]
fn no_count_is_lost_every_buffer_is_current_and_a_rearm_empties_them() {
    let (chart, all) = offset_taps(130);
    let chart = with_length(chart);
    let profile = ChartProfile::of(&chart, &[]);
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut p = Play::new(&chart, &rule, &dev, None, 1, all);
    assert!(p.logic.bin_judgments(&chart, &profile));

    // One judging step, then reads interleaved with idle steps: each idle
    // publication goes to another buffer.
    p.step(tap_time(9) + 50 * MS);
    let first = p.bins();
    assert_eq!(sum(&totals(&first.bins)), 10);
    for _ in 0..3 {
        p.logic.advance_to(TimeUs(tap_time(9) + 50 * MS));
        let again = p.bins();
        assert_eq!(again.bins, first.bins);
        assert_eq!(again.revision, first.revision);
    }

    p.step(tap_time(129) + 200 * MS);
    assert!(p.side.dropped() > 0, "the effect queue never filled");
    let full = p.bins();
    assert_eq!(sum(&totals(&full.bins)), 130);
    // 100 ms apart from 1 s on: ten notes a second, PERFECT but the
    // 20.4 ms late one, GREAT.
    assert_eq!(full.bins[1], ch(&[(PG, 9), (GR_L, 1)]));
    assert_eq!(sum(&full.bins[13]), 10);
    assert_eq!(*p.logic.view().judge_bins, full);

    drop(p.logic.lend_readers());
    let (writer, reader) = source_queue(16);
    p.logic.restore_readers(vec![reader]);
    p.writer = writer;
    p.logic.rearm(&InputStart::default(), TimeUs(20_000 * MS));
    let rearmed = p.bins();
    assert!(rearmed.available);
    assert!(rearmed.revision > full.revision);
    assert_eq!(totals(&rearmed.bins), [0; JUDGE_CHANNELS]);
}

/// A BMS text at 120 BPM, a measure 2 s long in 100 slots of 20 ms, with
/// one tap per `(lane, ms)` and the tempo changes and STOPs given.
fn bms(taps: &[(u8, i64)], extra: &str) -> String {
    let channel = |l: u8| match l {
        0 => "16",
        1 => "11",
        2 => "12",
        3 => "13",
        4 => "14",
        5 => "15",
        6 => "18",
        _ => "19",
    };
    let mut lines = String::from("#PLAYER 1\n#BPM 120\n#WAV01 a.wav\n");
    lines.push_str(extra);
    for &(l, ms) in taps {
        assert_eq!(ms % 20, 0);
        let slot = usize::try_from(ms / 20).unwrap();
        let (measure, at) = (slot / 100, slot % 100);
        let mut data = vec!["00"; 100];
        data[at] = "01";
        lines.push_str(&format!("#{measure:03}{}:{}\n", channel(l), data.concat()));
    }
    lines
}

fn load(text: &str, speed: Speed) -> bms_parser::Loaded {
    let dir = tempfile::tempdir().unwrap();
    let opts = bms_parser::LoadOptions {
        speed,
        ..bms_parser::LoadOptions::default()
    };
    bms_parser::load_bms(text.as_bytes(), dir.path(), &opts).unwrap()
}

/// Presses every note of `chart` at its time, released 10 ms later.
fn autoplay(chart: &Chart) -> Vec<PhysicalInputEvent> {
    let mut v = Vec::new();
    for l in Lane::all() {
        for n in &chart.lanes[l.index()] {
            if matches!(n.kind, NoteKind::Tap) {
                v.push(press(n.time.0, 0, l));
                v.push(release(n.time.0 + 10 * MS, 0, l));
            }
        }
    }
    v
}

/// The bins of a play of `chart` pressing every note on time, mapped by
/// `profile`.
fn played_bins(chart: &Chart, profile: &ChartProfile, until: i64) -> JudgeBins {
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut p = Play::new(chart, &rule, &dev, None, 1_024, autoplay(chart));
    assert!(p.logic.bin_judgments(chart, profile));
    p.step(until);
    p.bins()
}

/// T4: a note's bin is its head row's 1× second, whatever the practice
/// speed: at ×1.5 the note at 1× 1 000 000 µs plays at 666 666 µs and
/// counts in bin 1; across a STOP's rows and a tempo change every note
/// keeps the 1× bin it has at 1×.
#[test]
fn a_notes_bin_is_its_head_rows_1x_second_at_any_speed() {
    let text = bms(&[(1, 500), (2, 1_000), (3, 1_960)], "");
    let fast = load(&text, Speed::from_twentieths(30).unwrap());
    let n = fast.chart.lanes[2][0];
    assert_eq!(n.time, TimeUs(666_666));
    let bins = played_bins(&fast.chart, &fast.profile, 5_000 * MS);
    // 1× 0.5 s in bin 0; 1× 1 s and 1.96 s in bin 1, though the first
    // plays at 0.67 s.
    assert_eq!(bins.bins[0], ch(&[(PG, 1)]), "{:?}", &bins.bins[..4]);
    assert_eq!(bins.bins[1], ch(&[(PG, 2)]));

    // A STOP of one beat at 1 s, where a note sits, then 240 BPM from the
    // second measure.
    let extra = "#STOP01 48\n#00009:00000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000\n#00103:F0\n";
    let taps = [
        (1, 500),
        (2, 1_000),
        (3, 1_500),
        (4, 2_000),
        (5, 3_000),
        (6, 3_900),
    ];
    let text = bms(&taps, extra);
    let normal = load(&text, Speed::NORMAL);
    let expected: Vec<usize> = Lane::all()
        .flat_map(|l| normal.chart.lanes[l.index()].iter())
        .map(|n| usize::try_from(n.time.0 / 1_000_000).unwrap())
        .collect();
    let mut want = [[0_u32; JUDGE_CHANNELS]; 6];
    for &s in &expected {
        want[s][PG] += 1;
    }
    // The STOP and the tempo change spread the notes over more seconds
    // than their positions.
    assert!(expected.iter().any(|&s| s >= 3), "{expected:?}");
    for speed in [
        Speed::NORMAL,
        Speed::from_twentieths(30).unwrap(),
        Speed::from_twentieths(10).unwrap(),
    ] {
        let loaded = load(&text, speed);
        let bins = played_bins(&loaded.chart, &loaded.profile, 20_000 * MS);
        assert_eq!(&bins.bins[..6], &want, "at {}/20", speed.twentieths());
    }
}

/// T4: from 511 s of length the bins widen to whole seconds that keep
/// them within 512, and a widened bin sums the judgments of its seconds.
#[test]
fn widened_bins_hold_their_seconds_judgments_summed() {
    for (last, used, bin) in [(511, 257, 255), (512, 257, 256), (1_024, 342, 341)] {
        let chart = with_length(
            ChartBuilder::new(150_000)
                .tap(lane(1), TimeUs(last * 1_000 * MS))
                .build()
                .unwrap(),
        );
        let profile = ChartProfile::of(&chart, &[]);
        assert_eq!(profile.bins().len(), used, "{last} s");
        let bins = played_bins(&chart, &profile, (last + 1) * 1_000 * MS);
        assert_eq!(bins.bins[bin], ch(&[(PG, 1)]), "{last} s");
        assert_eq!(sum(&totals(&bins.bins)), 1);
    }
    let chart = with_length(
        ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(510_000 * MS))
            .tap(lane(2), TimeUs(511_000 * MS))
            .build()
            .unwrap(),
    );
    let profile = ChartProfile::of(&chart, &[]);
    assert_eq!(profile.bin_seconds(), 2);
    let bins = played_bins(&chart, &profile, 512_000 * MS);
    assert_eq!(bins.bins[255], ch(&[(PG, 2)]));
}

/// A chart of three lanes with different notes, its profile, and the
/// chart with lanes 1 and 2 swapped, as a lane permutation leaves it.
fn permuted() -> (Chart, ChartProfile, Chart) {
    let chart = with_length(
        ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_200 * MS))
            .tap(lane(1), TimeUs(3_400 * MS))
            .tap(lane(2), TimeUs(2_500 * MS))
            .tap(lane(3), TimeUs(4_600 * MS))
            .build()
            .unwrap(),
    );
    let profile = ChartProfile::of(&chart, &[]);
    let mut swapped = chart.clone();
    swapped.lanes.swap(1, 2);
    swapped.validate_presentation_links().unwrap();
    (chart, profile, swapped)
}

/// T4: a lane permutation keeps each note in its own bin, by the
/// attempt's identities, not the ingested chart's; a note moved onto
/// another existing row takes that row's bin.
#[test]
fn a_permuted_or_moved_note_counts_in_its_own_rows_bin() {
    let (_, profile, swapped) = permuted();
    let bins = played_bins(&swapped, &profile, 6_000 * MS);
    for (s, n) in [(1, 1), (2, 1), (3, 1), (4, 1)] {
        assert_eq!(bins.bins[s], ch(&[(PG, n)]), "second {s}");
    }
    // Each note's own second, by lane: lane 1 now holds the 2.5 s note.
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut p = Play::new(
        &swapped,
        &rule,
        &dev,
        None,
        64,
        vec![
            press(2_500 * MS, 0, lane(1)),
            release(2_510 * MS, 0, lane(1)),
        ],
    );
    assert!(p.logic.bin_judgments(&swapped, &profile));
    p.step(2_600 * MS);
    assert_eq!(p.bins().bins[2], ch(&[(PG, 1)]));
    // Lane 2's 1.2 s note, never pressed: a miss in its own bin.
    assert_eq!(p.bins().bins[1], ch(&[(PR_L, 1)]));

    // Lane 3's 4.6 s note moved onto the 2.5 s note's row.
    let mut moved = swapped.clone();
    let target = moved.lanes[1][0];
    let note = &mut moved.lanes[3][0];
    note.time = target.time;
    note.pos = target.pos;
    note.anchors = target.anchors;
    moved.validate_presentation_links().unwrap();
    let bins = played_bins(&moved, &profile, 6_000 * MS);
    assert_eq!(bins.bins[2], ch(&[(PG, 2)]));
    assert_eq!(bins.bins[4], [0; JUDGE_CHANNELS]);
}

/// `chart` with its timeline rebuilt from `points`.
fn retimed(chart: &Chart, points: Vec<TimelinePoint>) -> Chart {
    let t = &chart.timeline;
    let mut c = chart.clone();
    c.timeline = ChartTimeline::new(
        points,
        t.bar_ids().to_vec(),
        t.meta().initial,
        t.meta().speed,
    )
    .unwrap();
    c
}

/// T4: a transform that rebuilds the rows, at the same count or not,
/// leaves the bins unavailable: nothing counts, and the view says so.
#[test]
fn rebuilt_rows_leave_the_bins_unavailable() {
    let (chart, profile, _) = permuted();
    let points = chart.timeline.points().to_vec();
    // The same count, one row's position moved and the note on it moved
    // with it.
    let mut same = points.clone();
    let last = same.len() - 1;
    same[last].pos.0 += 1;
    let mut same_count = retimed(&chart, same);
    let head = same_count.lanes[3][0].anchors.head;
    assert_eq!(head.index(), last);
    same_count.lanes[3][0].pos.0 += 1;
    same_count.validate_presentation_links().unwrap();
    // One row more, past the last.
    let mut more = points;
    let extra = TimelinePoint {
        at: TimeUs(more[last].at.0 + 1),
        pos: more[last].pos,
        tempo: more[last].tempo,
    };
    more.push(extra);
    let added = retimed(&chart, more);
    for c in [&same_count, &added] {
        let rule = rules::tapestry();
        let dev = device(&[0]);
        let mut p = Play::new(c, &rule, &dev, None, 64, autoplay(c));
        assert!(!p.logic.bin_judgments(c, &profile));
        p.step(6_000 * MS);
        let bins = p.bins();
        assert!(!bins.available);
        assert_eq!(totals(&bins.bins), [0; JUDGE_CHANNELS]);
        assert_eq!(p.logic.view().tally.counts.perfect, 4);
    }
    // A preview's profile maps nothing.
    let preview = ChartProfile::preview(&chart, &[], &bms_model::PreviewInputs::default());
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let (mut logic, _) = Logic::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        vec![source_queue(4).1],
        LogicConfig {
            inputs: 16,
            effects: 16,
        },
        Vec::new(),
    )
    .unwrap();
    assert!(!logic.bin_judgments(&chart, &preview));
}

/// The result's timelines of `stream` over `chart`, summed per channel.
fn result_totals(
    stream: &[JudgeEvent],
    chart: &Chart,
    section: Option<(TimeUs, TimeUs)>,
    coverage: Coverage,
) -> Channels {
    let basis = ResultBasis::of(chart, section);
    let mut tl = Timelines::EMPTY;
    tl.fill(stream, chart, basis.timeline, coverage);
    totals(tl.used())
}

/// T11: the play's last bins and the result's timelines agree in their
/// per-channel totals, each over its own axis: T2's play, a practice round
/// of seconds 2 to 3 and a play ended at 3 500 ms with the LN held (×1.5
/// below).
#[test]
fn the_plays_last_bins_and_the_results_timelines_agree_in_totals() {
    let rule = rules::tapestry();
    let dev = device(&[0]);
    // At 1×, from the logic step's chart and the loader's profile alike.
    let chart = t2_chart();
    let profile = ChartProfile::of(&chart, &[]);
    let mut p = Play::new(&chart, &rule, &dev, None, 1_024, t2_inputs(1, 1));
    assert!(p.logic.bin_judgments(&chart, &profile));
    p.step(6_000 * MS);
    let play = totals(&p.logic.view().judge_bins.bins);
    assert_eq!(
        play,
        result_totals(&p.stream, &chart, None, Coverage::Recorded)
    );
    assert_eq!(play[GR_E], 2, "the LN's GREAT is early");

    // Ended at 3 500 ms, the LN held.
    let mut p = Play::new(&chart, &rule, &dev, None, 1_024, t2_inputs(1, 1));
    assert!(p.logic.bin_judgments(&chart, &profile));
    p.step(3_500 * MS);
    let play = totals(&p.logic.view().judge_bins.bins);
    assert_eq!(sum(&play), 26);
    assert_eq!(
        play,
        result_totals(
            &p.stream,
            &chart,
            None,
            Coverage::EndedMidway(TimeUs(3_500 * MS))
        )
    );

    // A practice round of seconds 2 to 3.
    let section = eval::Section {
        start: TimeUs(2_000 * MS),
        end: TimeUs(3_000 * MS),
        lead_in: TimeUs(1_000 * MS),
        initial_gauges: rule.gauges.iter().map(|g| g.start).collect(),
    };
    let mut p = Play::new(&chart, &rule, &dev, Some(&section), 1_024, t2_inputs(1, 1));
    assert!(p.logic.bin_judgments(&chart, &profile));
    p.step(6_000 * MS);
    let play = totals(&p.logic.view().judge_bins.bins);
    assert_eq!(sum(&play), 23);
    assert_eq!(
        play,
        result_totals(
            &p.stream,
            &chart,
            Some((section.start, section.end)),
            Coverage::Recorded
        )
    );
    // The notes outside the section are never judged.
    assert_eq!(p.logic.view().judge_bins.bins[1], [0; JUDGE_CHANNELS]);
}

/// T11 at ×1.5: T2's play, its long note included, loaded as the loader
/// loads it at ×1.5, agrees with the result's timelines in its per-channel
/// totals, and the long note counts on its release's side: released 120 ms
/// early at play time, farther than its head's 26.7 ms late, its GREAT is
/// early.
#[test]
fn at_1_5x_the_plays_last_bins_and_the_results_timelines_agree_in_totals() {
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut taps: Vec<(u8, i64)> = vec![(1, 1_200), (2, 1_500), (3, 1_800)];
    for i in 0..23_i64 {
        taps.push((u8::try_from(i % 7).unwrap() + 1, 2_000 + 40 * i));
    }
    taps.push((5, 3_500));
    // The long note on lane 4 from 3.2 to 3.8 s: measure 1, slots 60 and
    // 90 of 100.
    let mut ln = vec!["00"; 100];
    (ln[60], ln[90]) = ("01", "01");
    let extra = format!("#LNTYPE 1\n#00154:{}\n", ln.concat());
    let fast = load(&bms(&taps, &extra), Speed::from_twentieths(30).unwrap());
    let longs = fast.chart.lanes.iter().flatten();
    let longs = longs.filter(|n| matches!(n.kind, NoteKind::Long { .. }));
    assert_eq!(longs.count(), 1);
    // T2's inputs at ×1.5, but the long note's head pressed 40 ms late and
    // released 180 ms early at 1× (26.7 and 120 ms at play time): a GREAT
    // whose release lies farther.
    let at = |ms: i64| ms * MS * 2 / 3;
    let mut inputs: Vec<PhysicalInputEvent> = t2_inputs(3, 2)
        .into_iter()
        .filter(|e| e.received != TimeUs(at(3_225)) && e.received != TimeUs(at(3_680)))
        .collect();
    inputs.push(press(at(3_240), 0, lane(4)));
    inputs.push(release(at(3_620), 0, lane(4)));
    assert_eq!(inputs.len(), 16);
    inputs.sort_by_key(|e| e.received);
    let mut p = Play::new(&fast.chart, &rule, &dev, None, 1_024, inputs);
    assert!(p.logic.bin_judgments(&fast.chart, &fast.profile));
    p.step(6_000 * MS);
    let play = totals(&p.logic.view().judge_bins.bins);
    assert_eq!(sum(&play), 28);
    assert_eq!(
        play,
        result_totals(&p.stream, &fast.chart, None, Coverage::Recorded)
    );
    assert_eq!(play[GR_E], 1, "the long note's GREAT is early");
    assert_eq!(play[GR_L], 0);
}
