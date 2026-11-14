//! A selection skin's notes graph (`judgegraph` type 0) and BPM graph
//! (`bpmgraph`) draw the cursor's chart as play's widgets draw the same
//! data (design.md "Rendering and Skins", the selection graphs): the same
//! quads at the same place, each graph absent where the chart's previews
//! are (design.md "Loading and Song Selection", Summaries).

mod common;

use std::sync::Arc;

use bms_model::{
    Chart, ChartBuilder, ChartProfile, ChartSummary, Lane, MAIN_HISTOGRAM_LIMIT, Tempo, TimeUs,
    main_tempo,
};
use common::play::{Drawn, MS};
use common::selection::{SET, Sel, TEXT_T, TITLE, chart, folder, folder_row, song_list, view};
use skin_model::{ChartGraphs, Filter, PlayScore, Quad};

/// The notes graph's and the BPM graph's destinations, apart from the
/// song list (left of x 400).
const NG: &str = r#"{"id": "ng", "dst": [{"x": 1000, "y": 100, "w": 300, "h": 100}]}"#;
const BG: &str = r#"{"id": "bg", "dst": [{"x": 1000, "y": 400, "w": 300, "h": 100}]}"#;

/// When the frames are drawn: both graphs revealed (500 ms by default).
const AT: i64 = 1_000;

/// A selection skin with both graphs over a minimal song list.
fn selection() -> Sel {
    let list = song_list(
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        0,
        TITLE,
    );
    let d = folder(&format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "judgegraph": [{{"id": "ng"}}], "bpmgraph": [{{"id": "bg"}}],
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {NG}, {BG}]"#
    ));
    Sel::of(d.path())
}

/// Play's frame at [`AT`] of the same two graphs over `chart`.
fn play(chart: &Chart, profile: &ChartProfile, main: Tempo) -> Vec<Quad> {
    let l = common::import_sized(
        [1_920, 1_080],
        &format!(
            r#""judgegraph": [{{"id": "ng"}}], "bpmgraph": [{{"id": "bg"}}], "destination": [{NG}, {BG}]"#
        ),
        &[],
    );
    let skin = match l.result {
        Ok(i) => i.skin,
        Err(e) => panic!("{e:?}"),
    };
    let score = PlayScore {
        profile: Some(profile),
        main: Some(main),
        ..PlayScore::NONE
    };
    Drawn::over_score(skin, chart, score).frame(AT, &[])
}

/// A quad's place and colour.
type Placed = ([f32; 4], [f32; 4]);

/// The notes graph's chips (its bars, from the chip grid) in `quads`.
fn chips(quads: &[Quad]) -> Vec<Placed> {
    quads
        .iter()
        .filter(|q| q.rect[0] >= 1_000.0 && q.filter == Filter::NearestBase)
        .filter(|q| q.rect[1] >= 1_080.0 - 200.0)
        .map(|q| (q.rect, q.color))
        .collect()
}

/// The BPM graph's quads in `quads`.
fn bpm(quads: &[Quad]) -> Vec<Placed> {
    quads
        .iter()
        .filter(|q| q.rect[0] >= 1_000.0 && q.rect[1] < 1_080.0 - 300.0)
        .map(|q| (q.rect, q.color))
        .collect()
}

/// Every quad in either graph's place.
fn graphs(quads: &[Quad]) -> Vec<Placed> {
    quads
        .iter()
        .filter(|q| q.rect[0] >= 1_000.0)
        .map(|q| (q.rect, q.color))
        .collect()
}

/// Chart C: 150.5 BPM, 200 from 1 s; ten taps at 150.5 (so MAIN, a
/// fractional tempo) and three at 200.
fn chart_c() -> (Chart, ChartProfile, Tempo) {
    let lane = |l| Lane::new(l).unwrap();
    let mut b = ChartBuilder::new(150_500).bpm(TimeUs(1_000 * MS), 200_000);
    for k in 1..=10 {
        b = b.tap(lane(1), TimeUs(90 * k * MS));
    }
    for k in 0..3 {
        b = b.tap(lane(2), TimeUs((1_500 + 200 * k) * MS));
    }
    let mut chart = b.build().unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let profile = ChartProfile::of(&chart, &[]);
    let main = main_tempo(&chart, |_| true, MAIN_HISTOGRAM_LIMIT)
        .unwrap()
        .unwrap();
    (chart, profile, main)
}

/// What the scan stores of `profile` with MAIN `main`.
fn stored(profile: &ChartProfile, main: Option<Option<Tempo>>) -> Arc<ChartGraphs> {
    Arc::new(ChartGraphs {
        bins: profile.bins().to_vec(),
        bin_seconds: profile.bin_seconds(),
        length_ms: profile.length_ms(),
        rows: Some(profile.speeds().clone()),
        main,
    })
}

/// The selection frame at [`AT`] with the cursor on a chart row whose
/// graphs are `graphs`.
fn on_chart(s: &mut Sel, graphs: Option<Arc<ChartGraphs>>) -> Vec<Quad> {
    let mut v = view(vec![chart("a"), folder_row("f", 3)], Some(0), None, None);
    v.selected.as_mut().unwrap().graphs = graphs;
    s.view(&v);
    s.frame(AT)
}

/// V8: the cursor's chart's graphs draw as play's: the notes graph's chips
/// and the BPM graph's strokes are play's quads for chart C, whose MAIN is
/// the fractional 150.5 (its rows 150.5, MAIN-coloured, and 200, the
/// highest). Reading MAIN back from 92's whole 150 moves the strokes, so
/// the exact MAIN is what draws.
#[test]
fn the_graphs_draw_as_plays_for_a_fractional_main() {
    let (chart, profile, main) = chart_c();
    assert_eq!(
        main,
        Tempo::from_units(150 * Tempo::UNITS_PER_BPM + Tempo::UNITS_PER_BPM / 2).unwrap()
    );
    assert_eq!(
        profile.length_ms(),
        u32::try_from(chart.meta.length.0 / 1_000).unwrap()
    );
    let want = play(&chart, &profile, main);
    let mut s = selection();
    let got = on_chart(&mut s, Some(stored(&profile, Some(Some(main)))));
    assert!(!chips(&want).is_empty());
    assert_eq!(chips(&got), chips(&want));
    assert!(bpm(&want).len() >= 2, "{:?}", bpm(&want));
    // The 150.5 row in the default MAIN colour, `00ff00`.
    assert!(bpm(&want).iter().any(|(_, c)| *c == [0.0, 1.0, 0.0, 1.0]));
    assert_eq!(bpm(&got), bpm(&want));
    // MAIN from 92: 150 in whole BPM.
    let whole = Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap();
    let mut s = selection();
    let from_92 = on_chart(&mut s, Some(stored(&profile, Some(Some(whole)))));
    assert_eq!(chips(&from_92), chips(&want));
    assert_ne!(bpm(&from_92), bpm(&want));
}

/// §6's graph rows: a chart without stored graphs, and a folder row, draw
/// neither graph; rows left out past the bounds, an unknown MAIN and a
/// chart whose notes weigh nothing (mines only, MAIN none) draw the notes
/// graph alone.
#[test]
fn each_graph_is_absent_as_its_data_is() {
    let (_, profile, main) = chart_c();
    let mut s = selection();
    let full = on_chart(&mut s, Some(stored(&profile, Some(Some(main)))));
    let notes = chips(&full);
    assert!(!notes.is_empty() && !bpm(&full).is_empty());
    assert_eq!(graphs(&on_chart(&mut s, None)), []);
    let mut v = view(vec![chart("a"), folder_row("f", 3)], Some(1), None, None);
    v.selected = None;
    s.view(&v);
    assert_eq!(graphs(&s.frame(AT)), []);
    let no_rows = ChartGraphs {
        rows: None,
        ..(*stored(&profile, Some(Some(main)))).clone()
    };
    for (what, g) in [
        ("rows left out", Arc::new(no_rows)),
        ("MAIN unknown", stored(&profile, None)),
        ("no weighted note", stored(&profile, Some(None))),
    ] {
        let q = on_chart(&mut s, Some(g));
        assert_eq!(chips(&q), notes, "{what}");
        assert_eq!(bpm(&q), [], "{what}");
    }
    // A chart of mines alone: its stored MAIN is none, so no BPM graph,
    // as play draws none for it.
    let lane = |l| Lane::new(l).unwrap();
    let mut mines = ChartBuilder::new(150_000)
        .mine(lane(1), TimeUs(100 * MS), 10)
        .mine(lane(2), TimeUs(200 * MS), 10)
        .mine(lane(3), TimeUs(300 * MS), 10)
        .build()
        .unwrap();
    mines.meta.length = ChartSummary::of(&mines).length;
    assert_eq!(
        main_tempo(&mines, |_| true, MAIN_HISTOGRAM_LIMIT).unwrap(),
        None
    );
    let profile = ChartProfile::of(&mines, &[]);
    let q = on_chart(&mut s, Some(stored(&profile, Some(None))));
    assert_eq!(bpm(&q), []);
    let fallback = Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap();
    assert_eq!(bpm(&play(&mines, &profile, fallback)), []);
}
