//! Bar groups: each group's authored rect moved by the covers it follows
//! and by its bar's travel from the chart position, its own clock, colour,
//! blend and placement, full quads over the lanes' band, chart-then-group
//! order before the notes on the lanes' shared budgets, the load checks,
//! the variant and the per-attempt work. Parts are told apart by their
//! uv.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use lanes_support::{
    BAR, COVER, LINE, NOTE, WHITE, bar_group, image, lanes_element, lanes_skin, quads_of, rects,
    simple, skin_with, source_with, still,
};
use play_support::{Harness, MS, REGIONS, bool_program, full, lane, readouts_for, uvs};
use skin_model::{
    BarGroupDef, BarGroupError, Bind, Blend, BuildError, Ease, End, Fill, Follow, KeyDef,
    LANE_QUADS, LANE_VISITS, LanePart, LanesDef, LanesError, Layout, LoadError, MAX_BAR_GROUPS,
    MAX_COORD, MAX_SKIN_KEYS, Native, Place, Region, TimerRef, Track, Variant, timer,
};

/// 150 BPM, in thousandths: 800 px over the default 600 ms.
const BPM: i64 = 150_000;

/// A rect track `[x, y, w, h]` with `alpha` (0–255) holding from 0.
fn rect_alpha([x, y, w, h]: [i32; 4], alpha: i32) -> Track {
    still([
        Some(x),
        Some(y),
        Some(w),
        Some(h),
        Some(alpha),
        None,
        None,
        None,
        None,
    ])
}

/// A group of `image` over `r` in the playfield, following `follow`, on
/// `ready`, unbound.
fn group(image: u32, r: [i32; 4], follow: Follow) -> BarGroupDef {
    BarGroupDef {
        image,
        track: rect_alpha(r, 255),
        timer: TimerRef::Builtin(timer::READY),
        visible: None,
        blend: Blend::Alpha,
        place: Place {
            region: Region::Playfield,
            follow,
            ..Place::SCREEN
        },
    }
}

/// `def` with `groups` as its bar groups.
fn with_groups(mut def: LanesDef, groups: Vec<BarGroupDef>) -> LanesDef {
    def.bar_groups = groups;
    def
}

/// A chart with bar lines at `ms` and a tap on key 3 at 5 s.
fn chart_with_bars(ms: &[i64]) -> Chart {
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    chart.bar_lines = ms
        .iter()
        .map(|&t| chart.scroll.position_at(TimeUs(t * MS)))
        .collect();
    chart
}

fn harness(def: LanesDef, chart: &Chart) -> Harness {
    Harness::new(lanes_skin(def), chart, &full(0))
}

#[test]
fn a_bar_at_the_line_sits_where_its_group_is_authored() {
    // Whatever the notes' height or the source's offset: no note-centre
    // anchoring and no lane-wide width.
    let chart = chart_with_bars(&[1_000]);
    let g = group(BAR, [100, 500, 300, 7], Follow::None);
    let mut tall = simple();
    tall.note_h = 40;
    let offset = source_with(|s| s.offset_y = 30.0);
    for def in [simple(), tall, offset] {
        let mut h = harness(with_groups(def, vec![g.clone()]), &chart);
        h.frame(1_000, &[]);
        assert_eq!(rects(&h, BAR), [[100.0, 500.0, 300.0, 7.0]]);
    }
}

#[test]
fn travel_and_the_lift_each_move_a_bar_once() {
    // A lane 600 long (top 80, line 680) lifted 250 ‰: L = 150, the 450
    // px left scrolled over 450 ms, so a bar 100 ms ahead travels −100.
    let mut def = simple();
    def.line = 680;
    let def = with_groups(
        def,
        vec![
            group(BAR, [100, 500, 300, 2], Follow::Lift),
            group(WHITE, [100, 400, 300, 2], Follow::None),
        ],
    );
    let chart = chart_with_bars(&[1_100]);
    let mut h = harness(def, &chart);
    h.view.visible_time = TimeUs(450 * MS);
    h.view.lift = 250;
    h.frame(1_000, &[]);
    assert_eq!(rects(&h, BAR), [[100.0, 250.0, 300.0, 2.0]]);
    assert_eq!(rects(&h, WHITE), [[100.0, 300.0, 300.0, 2.0]]);
}

/// A group on `timer` fading in over 10 s, alpha 0 to 250.
fn fading(image: u32, r: [i32; 4], timer: u16) -> BarGroupDef {
    let key = |at: i64, a: i32| KeyDef {
        at: TimeUs(at * MS),
        props: [
            Some(r[0]),
            Some(r[1]),
            Some(r[2]),
            Some(r[3]),
            Some(a),
            None,
            None,
            None,
            None,
        ],
    };
    BarGroupDef {
        track: Track::new(&[key(0, 0), key(10_000, 250)], Ease::Linear, End::Hold).unwrap(),
        timer: TimerRef::Builtin(timer),
        ..group(image, r, Follow::None)
    }
}

#[test]
fn a_stop_holds_a_bar_while_its_animation_runs() {
    // A bar at the STOP's start (1 s, held 1 s): its time passes while the
    // chart position holds, and it stays on the line, fading in on the
    // frame's clock.
    let mut chart = ChartBuilder::new(BPM)
        .stop(TimeUs(1_000 * MS), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
    let def = with_groups(
        simple(),
        vec![fading(BAR, [160, 879, 544, 2], timer::READY)],
    );
    let mut h = harness(def, &chart);
    let mut seen = Vec::new();
    for ms in [1_100, 1_500, 1_900] {
        h.frame(ms, &[]);
        let q = quads_of(&h, BAR);
        assert_eq!(q.len(), 1, "{ms}");
        seen.push((q[0].rect, q[0].color[3]));
    }
    assert!(seen.iter().all(|(r, _)| *r == [160.0, 879.0, 544.0, 2.0]));
    assert!(seen.windows(2).all(|w| w[0].1 < w[1].1), "{seen:?}");
}

#[test]
fn lead_in_and_display_offset_keep_decorative_time() {
    // A 2 s lead-in: `ready` runs, `play` is off. Over 3 s visible, the
    // bar at 0 is 1 s ahead; a display offset moves it but not the fade.
    let chart = chart_with_bars(&[0]);
    let def = with_groups(
        simple(),
        vec![
            fading(BAR, [160, 879, 544, 2], timer::READY),
            fading(WHITE, [160, 879, 544, 2], timer::PLAY),
        ],
    );
    let mut h = Harness::new(lanes_skin(def.clone()), &chart, &full(2_000));
    h.view.visible_time = TimeUs(3_000 * MS);
    h.frame(-1_000, &[]);
    let at = quads_of(&h, BAR);
    assert_eq!(at.len(), 1);
    assert!(rects(&h, WHITE).is_empty(), "`play` is off");
    let ready = h.start(timer::READY).unwrap();
    let alpha = 250.0 * ((-1_000 * MS - ready) as f32 / 1e7) / 255.0;
    assert!((at[0].color[3] - alpha).abs() < 1e-3, "{alpha} {:?}", at[0]);
    // 150 ms of display offset: the bar is 150 ms nearer, 40 px lower.
    let mut later = Harness::new(lanes_skin(def), &chart, &full(2_000));
    later.view.visible_time = TimeUs(3_000 * MS);
    later.offset = TimeUs(150 * MS);
    later.frame(-1_000, &[]);
    let moved = quads_of(&later, BAR);
    assert_eq!(moved[0].color, at[0].color);
    assert!((moved[0].rect[1] - at[0].rect[1] - 40.0).abs() < 1e-3);
}

/// The draw order of `h`'s quads as images, by uv.
fn order(h: &Harness, images: &[u32]) -> Vec<u32> {
    let keys: Vec<(u32, [u32; 2], u16)> = images
        .iter()
        .map(|&i| {
            let f = h.uvs.get(i).unwrap();
            (i, [f.uv[0].to_bits(), f.uv[2].to_bits()], f.layer)
        })
        .collect();
    h.batch
        .quads()
        .iter()
        .filter_map(|q| {
            keys.iter()
                .find(|(_, k, l)| *k == [q.uv[0].to_bits(), q.uv[2].to_bits()] && *l == q.layer)
                .map(|(i, ..)| *i)
        })
        .collect()
}

#[test]
fn groups_keep_their_own_look_in_chart_then_group_order_before_the_notes() {
    let mut add = group(WHITE, [300, 860, 20, 40], Follow::None);
    add.blend = Blend::Add;
    add.track = rect_alpha([300, 860, 20, 40], 128);
    let defs = vec![group(BAR, [160, 879, 544, 2], Follow::None), add];
    // Two bars 300 ms apart from the line and the tap on it.
    let chart = chart_with_bars(&[5_000, 5_300]);
    let mut h = harness(with_groups(simple(), defs), &chart);
    h.frame(5_000, &[]);
    assert_eq!(
        order(&h, &[BAR, WHITE, LINE, NOTE]),
        [BAR, WHITE, BAR, WHITE, LINE, NOTE]
    );
    let white = quads_of(&h, WHITE);
    assert_eq!(white[0].rect, [300.0, 860.0, 20.0, 40.0]);
    assert!((white[0].color[3] - 128.0 / 255.0).abs() < 1e-6);
    let runs: Vec<Blend> = h.batch.runs().iter().map(|r| r.key.blend).collect();
    assert_eq!(
        &runs[..4],
        [Blend::Alpha, Blend::Add, Blend::Alpha, Blend::Add]
    );
}

#[test]
fn a_tall_displaced_turned_or_native_bar_draws_whole() {
    let chart = chart_with_bars(&[1_000]);
    // Reaching 10 px into the top of the lanes from above it.
    let tall = group(BAR, [160, -310, 544, 400], Follow::None);
    let mut h = harness(with_groups(simple(), vec![tall]), &chart);
    h.frame(1_000, &[]);
    assert_eq!(rects(&h, BAR), [[160.0, -310.0, 544.0, 400.0]]);
    // Touching the top (80) draws nothing.
    let touching = group(BAR, [160, -320, 544, 400], Follow::None);
    let mut h = harness(with_groups(simple(), vec![touching]), &chart);
    h.frame(1_000, &[]);
    assert!(rects(&h, BAR).is_empty());
    // Turned a quarter about its centre, its 2 px are 544 tall.
    let mut turned = group(BAR, [160, 40, 544, 2], Follow::None);
    turned.track = still([
        Some(160),
        Some(40),
        Some(544),
        Some(2),
        None,
        None,
        None,
        None,
        Some(90),
    ]);
    turned.place.pivot = skin_model::Anchor::Center;
    let mut h = harness(with_groups(simple(), vec![turned]), &chart);
    h.frame(1_000, &[]);
    let q = quads_of(&h, BAR);
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].rect, [160.0, 40.0, 544.0, 2.0]);
    assert!(q[0].angle > 1.5);
    // A native 4×300 frame centred on a 2 px rect at the line.
    let mut native = group(BAR, [400, 879, 10, 2], Follow::None);
    native.place.native = Some(Native::Exact([4, 300]));
    let mut h = harness(with_groups(simple(), vec![native]), &chart);
    h.frame(1_000, &[]);
    assert_eq!(rects(&h, BAR), [[403.0, 730.0, 4.0, 300.0]]);
}

#[test]
fn bars_stop_with_no_scroll_left() {
    let chart = chart_with_bars(&[1_000]);
    let mut h = harness(simple(), &chart);
    h.view.cover = 1_000;
    h.frame(1_000, &[]);
    assert!(rects(&h, BAR).is_empty());
    assert_eq!(rects(&h, LINE).len(), 1);
}

/// `n` bar lines at 5 s, where the tap is.
fn many_bars(n: usize) -> Chart {
    let mut chart = chart_with_bars(&[]);
    chart.bar_lines = vec![chart.scroll.position_at(TimeUs(5_000 * MS)); n];
    chart
}

#[test]
fn hidden_and_off_groups_spend_no_visits() {
    // One drawn group at 8,191 bars spends 8,191 visits and quads; the
    // tap takes the last of each. Fifty hidden, fifty `play`-timed groups
    // before the play and fifty without height spend none.
    let mut groups = vec![group(BAR, [160, 879, 544, 2], Follow::None)];
    for _ in 0..50 {
        groups.push(BarGroupDef {
            visible: Some(bool_program("false")),
            ..group(WHITE, [160, 879, 544, 2], Follow::None)
        });
        groups.push(BarGroupDef {
            timer: TimerRef::Builtin(timer::PLAY),
            ..group(WHITE, [160, 879, 544, 2], Follow::None)
        });
        groups.push(group(WHITE, [160, 879, 544, 0], Follow::None));
    }
    let n = usize::try_from(LANE_QUADS).unwrap() - 1;
    let mut h = Harness::new(
        lanes_skin(with_groups(simple(), groups)),
        &many_bars(n),
        &full(6_000),
    );
    h.frame(-1_000, &[]);
    assert!(h.start(timer::PLAY).is_none());
    // The tap at 5 s is 6 s ahead, above the lanes: move the frame on.
    h.offset = TimeUs(6_000 * MS);
    h.frame(-1_000, &[]);
    assert_eq!(rects(&h, BAR).len(), n);
    assert_eq!(rects(&h, NOTE).len(), 1);
    assert_eq!(h.batch.dropped(), 0);
}

#[test]
fn bars_and_notes_share_one_stop() {
    // 8,192 drawn bars spend every quad: the tap is dropped once, the line
    // and an image after the lanes still draw.
    let skin = skin_with(
        REGIONS,
        Some(simple()),
        vec![lanes_element(), image(WHITE, [0, 0, 10, 10], Place::SCREEN)],
    )
    .unwrap();
    let mut h = Harness::new(
        skin,
        &many_bars(usize::try_from(LANE_QUADS).unwrap()),
        &full(0),
    );
    h.frame(5_000, &[]);
    assert_eq!(rects(&h, BAR).len(), usize::try_from(LANE_QUADS).unwrap());
    assert!(rects(&h, NOTE).is_empty());
    assert_eq!(rects(&h, LINE).len(), 1);
    assert_eq!(rects(&h, WHITE), [[0.0, 0.0, 10.0, 10.0]]);
    assert_eq!(h.batch.dropped(), 1);
}

#[test]
fn rejected_candidates_spend_the_visits() {
    // Every bar's quad touches the lanes' top: each a candidate, none
    // drawn. The visits run out before the tap; the line and the image
    // after the lanes still draw.
    let skin = skin_with(
        REGIONS,
        Some(with_groups(
            simple(),
            vec![group(BAR, [160, 70, 544, 10], Follow::None)],
        )),
        vec![lanes_element(), image(WHITE, [0, 0, 10, 10], Place::SCREEN)],
    )
    .unwrap();
    let mut h = Harness::new(
        skin,
        &many_bars(usize::try_from(LANE_VISITS).unwrap()),
        &full(0),
    );
    h.frame(5_000, &[]);
    assert!(rects(&h, BAR).is_empty());
    assert!(rects(&h, NOTE).is_empty());
    assert_eq!(rects(&h, LINE).len(), 1);
    assert_eq!(rects(&h, WHITE), [[0.0, 0.0, 10.0, 10.0]]);
    assert_eq!(h.batch.dropped(), 1);
}

#[test]
fn the_group_table_is_capped_before_it_is_built() {
    let one = bool_program("cover.enabled");
    let g = BarGroupDef {
        visible: Some(one),
        ..group(BAR, [160, 879, 544, 2], Follow::None)
    };
    let at_cap = with_groups(simple(), vec![g.clone(); MAX_BAR_GROUPS]);
    let skin = lanes_skin(at_cap.clone());
    assert_eq!(skin.bar_group_count(), MAX_BAR_GROUPS);
    let mut h = Harness::new(skin, &many_bars(1), &full(0));
    h.frame(5_000, &[]);
    assert_eq!(rects(&h, BAR).len(), usize::try_from(LANE_QUADS).unwrap());
    assert_eq!(h.batch.dropped(), 1);
    let mut over = at_cap;
    over.bar_groups.push(g);
    let err = skin_with(REGIONS, Some(over), vec![lanes_element()]).unwrap_err();
    assert!(matches!(
        err,
        LoadError::Lanes(LanesError::TooManyBarGroups { n }) if n == MAX_BAR_GROUPS + 1
    ));
}

fn lanes_err(def: LanesDef) -> LanesError {
    match skin_with(REGIONS, Some(def), vec![lanes_element()]).unwrap_err() {
        LoadError::Lanes(e) => e,
        e => panic!("{e:?}"),
    }
}

#[test]
fn groups_are_checked_at_load() {
    let ok = || group(BAR, [160, 879, 544, 2], Follow::None);
    let at = |g: BarGroupDef| with_groups(simple(), vec![ok(), g]);
    let bad = |error| LanesError::BarGroup { group: 1, error };
    assert_eq!(
        lanes_err(at(BarGroupDef { image: 99, ..ok() })),
        LanesError::UnknownPart {
            part: LanePart::BarGroup(1)
        }
    );
    assert_eq!(
        lanes_err(at(BarGroupDef {
            timer: TimerRef::When(0),
            ..ok()
        })),
        bad(BarGroupError::UnknownTimer)
    );
    assert_eq!(
        lanes_err(at(BarGroupDef {
            visible: Some(
                skin_model::compile(
                    "combo",
                    &skin_model::Scope::new(&skin_model::PLAY),
                    skin_model::Ty::Int
                )
                .unwrap()
            ),
            ..ok()
        })),
        bad(BarGroupError::VisibleNotBool)
    );
    assert_eq!(
        lanes_err(at(BarGroupDef {
            track: rect_alpha([0, MAX_COORD + 1, 1, 1], 255),
            ..ok()
        })),
        bad(BarGroupError::Coordinates)
    );
    let mut screen = ok();
    screen.place.region = Region::Screen;
    assert_eq!(lanes_err(at(screen)), bad(BarGroupError::Region));
    let mut filled = ok();
    filled.place.fill = Fill::Cover;
    assert_eq!(lanes_err(at(filled)), bad(BarGroupError::Fill));
    // A follower moved the lane's length (800) either way stays inside.
    let follower = |y| BarGroupDef {
        track: rect_alpha([0, y, 1, 1], 255),
        ..group(BAR, [0; 4], Follow::Cover)
    };
    assert!(
        skin_with(
            REGIONS,
            Some(at(follower(MAX_COORD - 800))),
            vec![lanes_element()]
        )
        .is_ok()
    );
    assert_eq!(
        lanes_err(at(follower(MAX_COORD - 799))),
        bad(BarGroupError::Coordinates)
    );
    // Groups' keys count toward the skin's.
    let keys: Vec<KeyDef> = (0..256)
        .map(|i| KeyDef {
            at: TimeUs(i * MS),
            props: [Some(0); 9],
        })
        .collect();
    let heavy = BarGroupDef {
        track: Track::new(&keys, Ease::Linear, End::Hold).unwrap(),
        ..ok()
    };
    let n = MAX_SKIN_KEYS / 256;
    let full_keys = with_groups(simple(), vec![heavy; n]);
    let err = skin_with(REGIONS, Some(full_keys), vec![lanes_element()]).unwrap_err();
    assert!(matches!(err, LoadError::TooManyKeys { .. }), "{err:?}");
}

#[test]
fn each_group_visibility_is_charged_once() {
    let p = bool_program("combo > 3");
    let groups = vec![
        BarGroupDef {
            visible: Some(p.clone()),
            ..group(BAR, [160, 879, 544, 2], Follow::None)
        };
        3
    ];
    let without = lanes_skin(with_groups(simple(), Vec::new()));
    let with = lanes_skin(with_groups(simple(), groups));
    assert_eq!(with.ops().total() - without.ops().total(), 3 * p.op_count());
}

#[test]
fn an_unbound_group_has_no_lane_width() {
    // At 3840×2160 (×2) and 50 % lanes: the lanes and a bound group are
    // 2 × 50 % = 1, `Lo`; an unbound group is 2, `Hi`, unless hidden.
    let width = {
        let mut l = Layout::AUTHOR;
        l.set_lane_width(50).unwrap();
        l
    };
    let bound = bar_group(879, 2);
    let mut unbound = bar_group(879, 2);
    unbound.place.bind = Bind::None;
    let variant =
        |groups| lanes_skin(with_groups(simple(), groups)).variant([3_840, 2_160], &width);
    assert_eq!(variant(vec![bound.clone()]), Variant::Lo);
    assert_eq!(variant(vec![bound.clone(), unbound.clone()]), Variant::Hi);
    let hidden = BarGroupDef {
        visible: Some(bool_program("false")),
        ..unbound.clone()
    };
    assert_eq!(variant(vec![bound, hidden]), Variant::Lo);
    // A hidden lanes element hides its groups.
    let mut lanes = lanes_element();
    lanes.visible = Some(bool_program("false"));
    let skin = skin_with(
        REGIONS,
        Some(with_groups(simple(), vec![unbound])),
        vec![lanes],
    )
    .unwrap();
    assert_eq!(skin.variant([3_840, 2_160], &width), Variant::Lo);
}

#[test]
fn a_play_of_another_group_count_draws_nothing() {
    let chart = chart_with_bars(&[1_000]);
    let one = lanes_skin(simple());
    let mut two_def = simple();
    two_def
        .bar_groups
        .push(group(COVER, [0, 0, 5, 5], Follow::None));
    let two = lanes_skin(two_def);
    let mut h = Harness::new(one, &chart, &full(0));
    h.uvs = uvs(&two);
    h.skin = two.clone();
    h.frame(1_000, &[]);
    assert!(h.batch.quads().is_empty());
    // Not filled: an invalid frame is not refused, as with any wrong
    // argument.
    let mut bad = readouts_for(h.view);
    bad.covers.cover = 500;
    h.readouts = Some(bad);
    assert_eq!(h.try_frame_us(1_000 * MS, &[]), Ok(()));
    // A poisoned play keeps its error whatever the skin.
    let mut p = Harness::new(lanes_skin(simple()), &chart, &full(0));
    p.readouts = Some(bad);
    assert_eq!(p.try_frame_us(0, &[]), Err(BuildError::InvalidReadouts));
    p.skin = two.clone();
    p.uvs = uvs(&two);
    assert_eq!(p.try_frame_us(0, &[]), Err(BuildError::InvalidReadouts));
    // Its own skin draws it.
    let mut own = Harness::new(two, &chart, &full(0));
    own.frame(1_000, &[]);
    assert_eq!(rects(&own, BAR).len(), 1);
}
