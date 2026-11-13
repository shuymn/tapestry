//! A play skin's score figures and bars (stage 3, S10-10): the best B and
//! the target T a play reads, their rates and the EX score's differences
//! from their paced values, the rank flags, the 351/352 conditions and the
//! six score-rate bars, each through `Skin::new_play` → `prepare` →
//! `build` (design.md "Rendering and Skins", the play's score figures).
//!
//! The expected values are worked out by hand, apart from the code, on a
//! chart of n = 7 targets (2n = 14) with B = 11 and T = 13, after p = 3
//! judged targets with ex = 5:
//! - paced best `⌊33/7⌋ = 4`, so `best.diff` = 1; paced target
//!   `⌊39/7⌋ = 5`, so `target.diff` = 0;
//! - B's rate 7857 bp, T's 9285 bp;
//! - bars 5/14, 5/6, 4/14, 11/14, 5/14 and 13/14;
//! - the current band over 5/6 (`27 · 5 = 135` against `6 · lo`: 144 >
//!   135 ≥ 126) is AA, `rank.current.1`; the whole ranks over 5/14 (`135`
//!   against `14 · i`) reach i = 9 (126) but not 12 (168): `rank.whole.5`
//!   to `.7`.

mod graph_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use eval::{Section, Tally};
use rules::PerJudgment;
use skin_model::{
    Atlas, Attempt, ElementDef, PlayContext, PlayDefs, PlayScore, RatioBarDef, RatioForm,
    RatioSource, ScoreBasis, ScoreRate, Skin, Source, TimerRef, Variant, field, timer,
};

use play_support::{GAUGES, Harness, MS, bool_program, def, full, lane, white_sheet};

/// The bars' length: 168 reference pixels, a multiple of 14 and of 6.
const LONG: f32 = 168.0;

/// The ratio bar of `rate`, cut from the bottom when `vertical`.
fn bar(rate: ScoreRate, vertical: bool) -> RatioBarDef {
    RatioBarDef {
        image: 0,
        form: RatioForm::Bar { vertical },
        source: RatioSource::Score(rate),
    }
}

/// Horizontal bar `i` (rate `ScoreRate::ALL[i]`) at y = 100 + 30i, 168
/// wide; vertical bar `i` at x = 400 + 30i, 168 tall.
fn bar_rect(i: usize, vertical: bool) -> [i32; 4] {
    let at = 30 * i32::try_from(i).unwrap();
    if vertical {
        [400 + at, 100, 20, 168]
    } else {
        [100, 100 + at, 168, 20]
    }
}

/// An element on `screen` drawing `source` over `r`.
fn screen(source: Source, r: [i32; 4]) -> ElementDef {
    graph_support::source_element(source, r, 255, timer::SCREEN)
}

/// A play skin of the six score bars, horizontal then vertical (bar `i`
/// and `6 + i`), then three markers: shown while `best.valid` at x = 700,
/// and on the `when` timers 0 (`best.reached`) at x = 720 and 1
/// (`target.reached`) at x = 740.
fn skin() -> Skin {
    let mut elements = Vec::new();
    let mut bars = Vec::new();
    for vertical in [false, true] {
        for (i, rate) in ScoreRate::ALL.into_iter().enumerate() {
            let n = u32::try_from(bars.len()).unwrap();
            bars.push(bar(rate, vertical));
            elements.push(screen(Source::RatioBar(n), bar_rect(i, vertical)));
        }
    }
    elements.push(ElementDef {
        visible: Some(bool_program("best.valid")),
        ..screen(Source::Image(0), [700, 100, 10, 10])
    });
    for (w, x) in [(0, 720), (1, 740)] {
        elements.push(ElementDef {
            timer: TimerRef::When(w),
            ..screen(Source::Image(0), [x, 100, 10, 10])
        });
    }
    let whens = vec![bool_program("best.reached"), bool_program("target.reached")];
    Skin::new_play(
        def(whens, white_sheet(), elements),
        PlayDefs {
            ratio_bars: bars,
            ..PlayDefs::default()
        },
    )
    .unwrap()
}

/// A chart of `n` taps, 100 ms apart from 1 s; `random` records one
/// `#RANDOM` draw.
fn chart(n: usize, random: bool) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..n {
        let at = 1_000 + 100 * i64::try_from(i).unwrap();
        b = b.tap(lane(1), TimeUs(at * MS));
    }
    let mut chart = b.build().unwrap();
    if random {
        chart.identity.random_results = vec![1];
    }
    chart
}

/// The whole chart played with `basis`.
fn ctx(basis: ScoreBasis) -> PlayContext<'static> {
    PlayContext {
        score: PlayScore {
            basis,
            ..PlayScore::NONE
        },
        ..full(0)
    }
}

/// B = 11 and T = 13, the best known.
const KNOWN: ScoreBasis = ScoreBasis {
    best: 11,
    best_known: true,
    target: 13,
};

/// A tally of `perfect` PERFECTs, `great` GREATs, `poor` POORs and
/// `empty_poor` empty POORs (not counted as POORs).
fn tally(perfect: u32, great: u32, poor: u32, empty_poor: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect,
            great,
            good: 0,
            bad: 0,
            poor,
        },
        ex_score: 2 * perfect + great,
        empty_poor,
        ..play_support::tally(0)
    }
}

/// A skin's play over its own atlas, so the frame can be composited.
struct Fx {
    h: Harness,
    atlas: Atlas,
}

impl Fx {
    fn new(chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        let skin = skin();
        let atlas = skin
            .sheet()
            .atlas(Variant::Lo, graph_support::LIMITS, &|| false)
            .unwrap();
        let uvs = atlas.uvs().clone();
        let mut h = Harness::new(skin, chart, ctx);
        h.uvs = uvs;
        Self { h, atlas }
    }

    /// Builds the frame at `ms` over `t`.
    fn frame(&mut self, ms: i64, t: Tally) {
        self.h.tally = t;
        self.h.frame(ms, &[]);
    }

    fn field(&self, slot: u16) -> i32 {
        self.h.field(slot)
    }

    /// Whether the frame lights canvas point `[x, y]` with the white
    /// image.
    fn lit(&self, x: f32, y: f32) -> bool {
        let (rgb, covered) =
            graph_support::composite(self.h.batch.quads(), &self.atlas, x, y, None);
        if covered == 0 {
            return false;
        }
        graph_support::near(rgb, [255.0; 3], 0.5);
        true
    }

    /// Bar `i` (of [`ScoreRate::ALL`]) shows exactly `n / d` of its box,
    /// at most all of it, in both orientations: horizontal from the left,
    /// vertical from the bottom, half a pixel inside its edge lit and half
    /// a pixel past it not. `None` draws neither.
    #[track_caller]
    fn bar(&self, i: usize, ratio: Option<(u32, u32)>) {
        let [hx, hy, ..] = bar_rect(i, false).map(|v| v as f32);
        let [vx, vy, ..] = bar_rect(i, true).map(|v| v as f32);
        let Some((n, d)) = ratio else {
            assert!(!self.lit(hx + 0.5, hy + 10.0), "bar {i} drawn");
            assert!(!self.lit(vx + 10.0, vy + LONG - 0.5), "bar {i} drawn");
            return;
        };
        let len = LONG * n.min(d) as f32 / d as f32;
        if n > 0 {
            assert!(
                self.lit(hx + len - 0.5, hy + 10.0),
                "bar {i}: {n}/{d} short"
            );
            assert!(
                self.lit(vx + 10.0, vy + LONG - len + 0.5),
                "bar {i}: {n}/{d} short (vertical)"
            );
        }
        if n < d {
            assert!(
                !self.lit(hx + len + 0.5, hy + 10.0),
                "bar {i}: {n}/{d} long"
            );
            assert!(
                !self.lit(vx + 10.0, vy + LONG - len - 0.5),
                "bar {i}: {n}/{d} long (vertical)"
            );
        }
        // The cut keeps the left end, or the bottom: never the far end.
        if n > 0 && n < d {
            assert!(
                !self.lit(hx + LONG - 0.5, hy + 10.0),
                "bar {i} from the right"
            );
            assert!(!self.lit(vx + 10.0, vy + 0.5), "bar {i} from the top");
        }
    }

    /// Every score field from `best.ex` on.
    fn figures(&self) -> Figures {
        let on = |base: u16| -> Vec<usize> {
            (0..8)
                .filter(|&k| self.field(base + k) != 0)
                .map(usize::from)
                .collect()
        };
        Figures {
            best: self.field(field::BEST_EX),
            target: self.field(field::TARGET_EX),
            best_diff: self.field(field::BEST_DIFF),
            target_diff: self.field(field::TARGET_DIFF),
            best_bp: self.field(field::BEST_RATE_BP),
            target_bp: self.field(field::TARGET_RATE_BP),
            valid: self.field(field::BEST_VALID) != 0,
            best_ppm: self.field(field::BEST_RATE_PPM),
            best_reached: self.field(field::BEST_REACHED) != 0,
            target_reached: self.field(field::TARGET_REACHED) != 0,
            current: on(field::RANK_CURRENT),
            whole: on(field::RANK_WHOLE),
        }
    }
}

/// The score fields as one value.
#[derive(Debug, PartialEq, Eq)]
struct Figures {
    best: i32,
    target: i32,
    best_diff: i32,
    target_diff: i32,
    best_bp: i32,
    target_bp: i32,
    valid: bool,
    best_ppm: i32,
    best_reached: bool,
    target_reached: bool,
    /// The `rank.current.K` flags on.
    current: Vec<usize>,
    /// The `rank.whole.K` flags on.
    whole: Vec<usize>,
}

/// The figures with B = 11, T = 13 and n = 7 before any judgment.
const fn before() -> Figures {
    Figures {
        best: 11,
        target: 13,
        best_diff: 0,
        target_diff: 0,
        best_bp: 7_857,
        target_bp: 9_285,
        valid: true,
        best_ppm: 785_714,
        best_reached: false,
        target_reached: false,
        current: Vec::new(),
        whole: Vec::new(),
    }
}

/// 150, 151/121, 152, 153, 122/123 and 183/184: B, T, the differences
/// from their linearly paced values and their exact rates, at p = 0, 3 and
/// 7 of the hand-worked chart.
#[test]
fn the_figures_follow_the_best_and_the_target() {
    let mut fx = Fx::new(&chart(7, false), &ctx(KNOWN));
    fx.frame(0, tally(0, 0, 0, 0));
    assert_eq!(fx.figures(), before());
    // p = 3, ex = 5.
    fx.frame(1_300, tally(2, 1, 0, 0));
    assert_eq!(
        fx.figures(),
        Figures {
            best_diff: 1,
            target_diff: 0,
            current: vec![1],
            whole: vec![5, 6, 7],
            ..before()
        }
    );
    // p = 7, ex = 12: 12 − 11 and 12 − 13; 12/14 is AA by both rates.
    fx.frame(1_700, tally(5, 2, 0, 0));
    assert_eq!(
        fx.figures(),
        Figures {
            best_diff: 1,
            target_diff: -1,
            best_reached: true,
            current: vec![1],
            whole: vec![1, 2, 3, 4, 5, 6, 7],
            ..before()
        }
    );
    // p = 7, ex = 7, POORs among them: the paced values are B and T.
    fx.frame(1_800, tally(3, 1, 3, 0));
    assert_eq!(fx.figures().best_diff, 7 - 11);
    assert_eq!(fx.figures().target_diff, 7 - 13);
}

/// A chart that declares `#RANDOM` hides what compares against its best:
/// `best.valid` is false, the best's rate in ppm (Lua `float_number(113)`)
/// is 0, and bars 112 and 113 draw nothing. B, T and the target's figures
/// stay.
#[test]
fn random_hides_the_comparisons_with_the_best() {
    let mut fx = Fx::new(&chart(7, true), &ctx(KNOWN));
    fx.frame(1_300, tally(2, 1, 0, 0));
    let f = fx.figures();
    assert!(!f.valid);
    assert_eq!(f.best_ppm, 0);
    assert_eq!((f.best, f.target, f.target_bp), (11, 13, 9_285));
    // The marker shown while `best.valid` is not drawn.
    assert!(!fx.lit(705.0, 105.0));
    fx.bar(2, None);
    fx.bar(3, None);
    fx.bar(0, Some((5, 14)));
    fx.bar(4, Some((5, 14)));
    fx.bar(5, Some((13, 14)));
    // The same chart without the draw shows them.
    let mut fx = Fx::new(&chart(7, false), &ctx(KNOWN));
    fx.frame(1_300, tally(2, 1, 0, 0));
    assert!(fx.figures().valid);
    assert!(fx.lit(705.0, 105.0));
    fx.bar(2, Some((4, 14)));
    fx.bar(3, Some((11, 14)));
}

/// An unavailable best (no library, a failed read) is the empty score,
/// shown: 150 is 0, 183/184 are 0.00, bar 113 is empty and 152 is the EX
/// score, and the ppm rate is 0; and with no target, 122/123 read 0, not
/// 100 %.
#[test]
fn an_unavailable_best_shows_as_the_empty_score() {
    // No library and a failed read give the same basis, each with the
    // target its choice gives: max (14) and a rank (13).
    for target in [14, 13] {
        let basis = ScoreBasis {
            best: 0,
            best_known: false,
            target,
        };
        let mut fx = Fx::new(&chart(7, false), &ctx(basis));
        fx.frame(1_300, tally(2, 1, 0, 0));
        let f = fx.figures();
        assert_eq!(
            (f.best, f.best_bp, f.best_ppm, f.best_diff, f.valid),
            (0, 0, 0, 5, true),
            "{target}"
        );
        assert!(fx.lit(705.0, 105.0));
        fx.bar(3, Some((0, 14)));
        fx.bar(2, Some((0, 14)));
    }
    // n = 0: every rate over the chart is 0, never 100 %.
    let mut fx = Fx::new(&chart(0, false), &ctx(KNOWN));
    fx.frame(0, tally(0, 0, 0, 0));
    let f = fx.figures();
    assert_eq!((f.best_bp, f.target_bp, f.best_ppm), (0, 0, 0));
    assert_eq!((f.best_diff, f.target_diff), (0, 0));
    for i in 0..6 {
        fx.bar(i, Some((0, 1)));
    }
    // Nor does a chart without targets rank, even once updated.
    fx.frame(900, tally(0, 0, 0, 1));
    assert!(fx.figures().current.is_empty());
    assert!(fx.figures().whole.is_empty());
}

/// 200-207 hold the one band of the rate over the judged targets once
/// the attempt has a judgment, the rate 1 while none is judged, and
/// nothing before; 220-227 each rank the whole-chart rate reached, nothing
/// before: exactly at each threshold, and not one EX below it.
#[test]
fn the_ranks_switch_at_their_edges() {
    let lows = [24, 21, 18, 15, 12, 9, 6, 0];
    // n = 27 targets, all judged: a rate of ex/54 is at least i/27
    // exactly when ex ≥ 2i.
    let mut fx = Fx::new(&chart(27, false), &ctx(KNOWN));
    fx.frame(0, tally(0, 0, 0, 0));
    assert!(fx.figures().current.is_empty());
    assert!(fx.figures().whole.is_empty());
    for (k, &lo) in lows.iter().enumerate() {
        // `lo` PERFECTs and the rest POORs: p stays 27.
        let t = tally(lo, 0, 27 - lo, 0);
        assert_eq!(t.ex_score, 2 * lo);
        fx.frame(2_000, t);
        assert_eq!(fx.figures().current, vec![k], "at {lo}/27");
        assert_eq!(fx.figures().whole, (k..8).collect::<Vec<_>>(), "at {lo}/27");
        if lo > 0 {
            // One EX below: the band below, and that rank not reached.
            let t = tally(lo - 1, 1, 27 - lo, 0);
            assert_eq!(t.ex_score, 2 * lo - 1);
            fx.frame(2_000, t);
            assert_eq!(fx.figures().current, vec![k + 1], "below {lo}/27");
            assert_eq!(
                fx.figures().whole,
                (k + 1..8).collect::<Vec<_>>(),
                "below {lo}/27"
            );
        }
    }
    // An empty POOR first: updated with nothing judged, the current rate
    // is 1 (200, AAA) and only 227 (F) is reached.
    let mut fx = Fx::new(&chart(7, false), &ctx(KNOWN));
    fx.frame(900, tally(0, 0, 0, 1));
    assert_eq!(fx.figures().current, vec![0]);
    assert_eq!(fx.figures().whole, vec![7]);
}

/// Timers 351 and 352 (`when best.reached`, `when target.reached`) start
/// at the first update where the EX score reaches B or T: with B = 0 at
/// the first judgment, an empty POOR, and at exactly B and T.
#[test]
fn the_pass_timers_start_where_the_score_reaches() {
    let basis = ScoreBasis {
        best: 0,
        best_known: true,
        target: 13,
    };
    let when = |fx: &Fx, w: u16| fx.h.start_ms(timer::BUILTIN + w);
    let mut fx = Fx::new(&chart(7, false), &ctx(basis));
    fx.frame(500, tally(0, 0, 0, 0));
    assert_eq!((when(&fx, 0), when(&fx, 1)), (None, None));
    fx.frame(900, tally(0, 0, 0, 1));
    assert_eq!((when(&fx, 0), when(&fx, 1)), (Some(900), None));
    assert!(fx.lit(725.0, 105.0));
    assert!(!fx.lit(745.0, 105.0));
    fx.frame(1_600, tally(6, 0, 0, 1));
    assert_eq!(when(&fx, 1), None);
    fx.frame(1_700, tally(6, 1, 0, 1));
    assert_eq!((when(&fx, 0), when(&fx, 1)), (Some(900), Some(1_700)));
    assert!(fx.lit(745.0, 105.0));
    // B = 11, reached at exactly 11.
    let mut fx = Fx::new(&chart(7, false), &ctx(KNOWN));
    fx.frame(1_500, tally(5, 0, 0, 0));
    assert_eq!(when(&fx, 0), None);
    fx.frame(1_600, tally(5, 1, 0, 0));
    assert_eq!(when(&fx, 0), Some(1_600));
}

/// Bars 110-115 cut to their exact rates, horizontal from the left and
/// vertical from the bottom; 111 is 0 before the first judgment and full
/// after an empty POOR with nothing judged.
#[test]
fn the_bars_cut_to_the_exact_rates() {
    let mut fx = Fx::new(&chart(7, false), &ctx(KNOWN));
    fx.frame(0, tally(0, 0, 0, 0));
    let ready = [(0, 14), (0, 1), (0, 14), (11, 14), (0, 14), (13, 14)];
    for (i, r) in ready.into_iter().enumerate() {
        fx.bar(i, Some(r));
    }
    fx.frame(900, tally(0, 0, 0, 1));
    fx.bar(1, Some((1, 1)));
    fx.frame(1_300, tally(2, 1, 0, 0));
    let mid = [(5, 14), (5, 6), (4, 14), (11, 14), (5, 14), (13, 14)];
    for (i, r) in mid.into_iter().enumerate() {
        fx.bar(i, Some(r));
    }
    // The twelve bars and the `best.valid` marker; neither pass timer.
    assert_eq!(fx.h.batch.quads().len(), 12 + 1);
}

/// A practice round's attempt starts its projections fresh: the second
/// round's READY frame shows every value as before the first judgment,
/// whatever the first round reached, as a rearm does.
#[test]
fn a_new_round_starts_its_projections_fresh() {
    let chart = chart(7, false);
    let section = Section {
        start: TimeUs(900 * MS),
        end: TimeUs(2_000 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: GAUGES.iter().map(|g| g.start).collect(),
    };
    let practice = PlayContext {
        attempt: Attempt::Section(&section),
        ..ctx(ScoreBasis::default())
    };
    let mut fx = Fx::new(&chart, &practice);
    fx.frame(1_300, tally(2, 1, 0, 0));
    assert_eq!(fx.figures().current, vec![1]);
    fx.bar(1, Some((5, 6)));
    // The next round, prepared fresh.
    let mut next = fx.h.skin.prepare(&chart, &practice).unwrap();
    std::mem::swap(&mut fx.h.play, &mut next);
    fx.frame(400, tally(0, 0, 0, 0));
    let fresh = Figures {
        best: 0,
        target: 0,
        best_bp: 0,
        target_bp: 0,
        best_ppm: 0,
        ..before()
    };
    assert_eq!(fx.figures(), fresh);
    for i in 0..6 {
        let r = if i == 1 { (0, 1) } else { (0, 14) };
        fx.bar(i, Some(r));
    }
    // A rearm alike.
    fx.frame(1_300, tally(2, 1, 0, 0));
    fx.h.play.rearm();
    fx.frame(400, tally(0, 0, 0, 0));
    assert_eq!(fx.figures(), fresh);
    fx.bar(1, Some((0, 1)));
}

/// While the first play loads, the best and the target and what reads
/// them are not drawn (bars 113 and 115, an element reading `best.ex`,
/// the markers on `best.valid` and on both pass timers), and the values
/// that wait for the first judgment show their READY value: the other
/// four bars at 0, and an element reading `best.diff` at 0.
#[test]
fn the_loading_frames_leave_out_the_best_and_the_target() {
    let more = |src: &str, x| ElementDef {
        visible: Some(bool_program(src)),
        ..screen(Source::Image(0), [x, 300, 10, 10])
    };
    let mut d = def(
        vec![bool_program("best.reached"), bool_program("target.reached")],
        white_sheet(),
        Vec::new(),
    );
    let mut bars = Vec::new();
    for vertical in [false, true] {
        for (i, rate) in ScoreRate::ALL.into_iter().enumerate() {
            let n = u32::try_from(bars.len()).unwrap();
            bars.push(bar(rate, vertical));
            d.elements
                .push(screen(Source::RatioBar(n), bar_rect(i, vertical)));
        }
    }
    d.elements.push(more("best.valid", 700));
    d.elements.push(more("best.ex >= 0", 720));
    d.elements.push(more("best.diff == 0", 740));
    d.elements
        .push(more("rank.current.0 || !rank.whole.7", 760));
    for (w, x) in [(0, 780), (1, 800)] {
        d.elements.push(ElementDef {
            timer: TimerRef::When(w),
            ..screen(Source::Image(0), [x, 300, 10, 10])
        });
    }
    let skin = Skin::new_play(
        d,
        PlayDefs {
            ratio_bars: bars,
            ..PlayDefs::default()
        },
    )
    .unwrap();
    let atlas = skin
        .sheet()
        .atlas(Variant::Lo, graph_support::LIMITS, &|| false)
        .unwrap();
    let mut loading = skin.prepare_loading(skin_model::Side::Left);
    let mut batch = skin_model::QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = skin_model::Layout::AUTHOR;
    let frame = skin_model::LoadingFrame {
        app: skin_model::AppReadouts::LAUNCH,
        display_offset: TimeUs(0),
        fix_hispeed: 0,
        difficulty: None,
        bga: None,
        length: None,
        autoplay: false,
        now: TimeUs(500 * MS),
        load: skin_model::LoadState {
            since: TimeUs(0),
            percent: 40,
        },
        target: [1_920, 1_080],
        layout: &layout,
        turntable: 0,
        lanes: play_support::VIEW,
        covers: play_support::readouts_for(play_support::VIEW).covers,
        gauge: None,
        play_level: None,
    };
    skin.build_loading(&mut loading, &frame, atlas.uvs(), &mut batch)
        .unwrap();
    // Bars 110, 111, 112 and 114 in both orientations, each cut to 0 (an
    // empty quad); the `best.diff` and rank markers.
    let quads = batch.quads();
    assert_eq!(quads.len(), 8 + 2, "{quads:?}");
    let lit = |x: f32, y: f32| graph_support::composite(quads, &atlas, x, y, None).1 > 0;
    for (i, r) in ScoreRate::ALL.into_iter().enumerate() {
        let [x, y, ..] = bar_rect(i, false).map(|v| v as f32);
        assert!(!lit(x + 0.5, y + 10.0), "{r:?}");
        let [x, y, ..] = bar_rect(i, true).map(|v| v as f32);
        assert!(!lit(x + 10.0, y + LONG - 0.5), "{r:?}");
    }
    for (x, shown) in [
        (700.0, false),
        (720.0, false),
        (740.0, true),
        (760.0, true),
        (780.0, false),
        (800.0, false),
    ] {
        assert_eq!(lit(x + 5.0, 305.0), shown, "marker at {x}");
    }
}
