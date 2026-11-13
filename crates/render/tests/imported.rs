//! An imported skin on WARP, from self-made JSON, a bitmap font and page
//! images through the importer, its atlas and bundle, and the real
//! installation ([`json_support`]): each fresh attempt's bitmap labels;
//! the chosen chart's bitmap loading text staged, installed at the next
//! loading frame, changed field by field and refused when stale;
//! LIFT's paint cut at its trim line by the run's inline clip, with an
//! image following the lift, under changing covers; and a Lua skin's
//! second practice round on one installation, prepared fresh.
//!
//! The skin is a fixed 1920×1080 canvas on a 1920×1080 target, so canvas
//! pixels are target pixels.

mod common;
mod json_support;

use std::sync::Arc;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, context, input, install, loading, offscreen, read_back, view};
use json_support::{
    ANIMATION, BACKGROUND, FOLLOWER, LANE_LENGTH, LIFT_PAINT, MAGENTA, RED, TRIM_LINE, bundle,
    centre, colour, rgb, text_at,
};
use render::text::MenuText;
use render::{AttemptInput, LoadingFrame, LoadingTextError, Renderer, SkinBundle};
use skin_model::{ChartText, ScreenKind, TextField, TextSlots, timer};

const BLACK: [u8; 3] = [0; 3];
const MS: i64 = 1_000;

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// The chart labels `title`, `artist` and `genre`, no subtitle.
fn labels<'a>(title: &'a str, artist: &'a str, genre: &'a str) -> ChartText<'a> {
    ChartText {
        title,
        subtitle: "",
        artist,
        subartist: "",
        genre,
    }
}

/// Stages `b` with a fresh attempt of `chart` labelled `text` for
/// `request`, commits it, and draws and reads back a play frame.
fn attempt_frame(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    chart: &Chart,
    text: &ChartText<'_>,
    request: u64,
) -> Vec<u8> {
    let ctx = context(&GAUGES, false);
    let a = AttemptInput {
        chart,
        context: &ctx,
        text,
        target: r.target(),
    };
    let token = r.stage(b, &a, request).unwrap();
    r.commit(token).unwrap();
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    read_back(r)
}

/// Each fresh attempt lays its own labels out in the skin's bitmap font
/// at its preparation (`DeclSkin::prepare`'s bitmap attachment): the
/// first attempt's title, artist and genre paint their letters'
/// colours, the full title its title's; the next attempt, of other
/// labels on the same installation, paints those, never the first's.
/// Without the attachment every square stays black.
#[test]
fn each_fresh_attempt_draws_its_own_bitmap_labels() {
    let mut r = offscreen(1_920, 1_080);
    let (_dir, b) = bundle(&r);
    let chart = chart();
    let first = attempt_frame(&mut r, &b, &chart, &labels("A", "D", "C"), 1);
    assert_eq!(
        text_at(&first),
        [colour('A'), colour('D'), colour('C'), colour('A')]
    );
    let next = attempt_frame(&mut r, &b, &chart, &labels("B", "E", "A"), 2);
    assert_eq!(
        text_at(&next),
        [colour('B'), colour('E'), colour('A'), colour('B')]
    );
}

/// A loading frame of lifetime `lifetime` at `now` ms on the loading
/// clock from origin 0, with the lift at `lift` ‰ (on) and text revision
/// `text`.
fn loading_at(lifetime: u64, now: i64, lift: u16, text: u64) -> LoadingFrame<'static> {
    let mut f = common::loading_frame(lifetime, TimeUs(now * MS), TimeUs(0));
    f.lanes.lift = lift;
    f.covers.lift = lift;
    f.covers.lift_enabled = true;
    f.text = text;
    f
}

/// The chosen chart's bitmap text, with no rasterized-text producer: laid
/// out when prepared and held staged, drawing nothing until the next
/// frame's start installs it; counted in the renderer's memory while
/// staged, its glyphs included, and no longer once installed. A chart
/// lacking its genre leaves that slot empty. A changed artist clears its
/// slot at once, the other slots kept, and draws its replacement once
/// installed; the animation goes on and the `screen` timer keeps its
/// start throughout. A preparation replaced before its install never
/// draws. Without the staged bitmap lines no square ever paints.
#[test]
fn bitmap_loading_text_installs_on_the_next_frame() {
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (_dir, b) = bundle(&r);
    install(&mut r, &b, 1);
    r.enter_loading(1);

    let first = labels("A", "D", "");
    r.prepare_loading_text(1, 1, TextSlots::ALL, &first)
        .unwrap();
    let staged = r.memory().staged_text;
    let glyphs = b
        .skin()
        .loading_bitmap_lines(TextSlots::ALL, &first)
        .heap_bytes();
    assert!(glyphs >= 3 * 64, "three glyphs laid out: {glyphs}");
    assert!(staged >= u64::try_from(glyphs).unwrap(), "{staged}");
    let p = loading(&mut r, &text, &loading_at(1, 0, 0, 0));
    assert_eq!(text_at(&p), [BLACK; 4], "staged, not installed");
    assert_eq!(centre(&p, BACKGROUND), RED);
    let started = screen(&r);
    assert!(started.is_some());

    assert_eq!(r.install_staged_loading_text(), Ok(true));
    assert_eq!(r.memory().staged_text, 0);
    let p = loading(&mut r, &text, &loading_at(1, 100, 0, 1));
    assert_eq!(
        text_at(&p),
        [colour('A'), colour('D'), BLACK, colour('A')],
        "the genre unavailable"
    );

    // The artist changes: cleared at once, the others kept.
    let artist = TextSlots::field(TextField::Artist);
    r.clear_loading_text(artist);
    let p = loading(&mut r, &text, &loading_at(1, 200, 0, 1));
    assert_eq!(text_at(&p), [colour('A'), BLACK, BLACK, colour('A')]);
    // A preparation replaced before its install never draws.
    r.prepare_loading_text(1, 2, artist, &labels("A", "B", ""))
        .unwrap();
    r.prepare_loading_text(1, 3, artist, &labels("A", "E", ""))
        .unwrap();
    assert_eq!(r.staged_loading_text(), Some((1, 3)));
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    let p = loading(&mut r, &text, &loading_at(1, 300, 0, 3));
    assert_eq!(text_at(&p), [colour('A'), colour('E'), BLACK, colour('A')]);
    // The animation moved 300 canvas pixels; the timer kept its start.
    assert_eq!(centre(&p, ANIMATION), BLACK);
    assert_eq!(rgb(&p, (550, 950)), RED);
    assert_eq!(screen(&r), started);
}

/// Bitmap text staged for what has passed installs nothing: text of an
/// older revision than the one installed, of another lifetime than the
/// one entered, and of an installation replaced by another bundle's
/// commit; the slots keep what they held, or the new installation's stay
/// empty.
#[test]
fn stale_bitmap_text_installs_nothing() {
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (_dir, b) = bundle(&r);
    install(&mut r, &b, 1);
    r.enter_loading(1);
    r.prepare_loading_text(1, 2, TextSlots::ALL, &labels("A", "D", "C"))
        .unwrap();
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    // Older.
    r.prepare_loading_text(1, 1, TextSlots::ALL, &labels("B", "B", "B"))
        .unwrap();
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Older {
            installed: 2,
            revision: 1
        })
    );
    let p = loading(&mut r, &text, &loading_at(1, 0, 0, 2));
    assert_eq!(
        text_at(&p),
        [colour('A'), colour('D'), colour('C'), colour('A')]
    );
    // Another lifetime: entered after the text was prepared for the last.
    r.prepare_loading_text(1, 3, TextSlots::ALL, &labels("B", "B", "B"))
        .unwrap();
    r.enter_loading(2);
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Lifetime)
    );
    let p = loading(&mut r, &text, &loading_at(2, 100, 0, 0));
    assert_eq!(text_at(&p), [BLACK; 4]);
    // Another bundle committed within the lifetime.
    r.prepare_loading_text(2, 1, TextSlots::ALL, &labels("B", "B", "B"))
        .unwrap();
    let (_other_dir, other) = bundle(&r);
    install(&mut r, &other, 2);
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Stale)
    );
    let p = loading(&mut r, &text, &loading_at(2, 200, 0, 0));
    assert_eq!(text_at(&p), [BLACK; 4]);
    assert_eq!(centre(&p, BACKGROUND), RED);
}

/// LIFT's paint and its follower under the loading frame's covers,
/// changing frame to frame. At rest the paint's top lies on its trim line
/// (the lanes' bottom, 880) and draws nothing, the follower at its place.
/// At 300 ‰ (180 px) both rise 180: the paint's 700..900 crosses the
/// line, so a scissor cuts it there, magenta above and black below in its
/// own columns, its quad upright; the follower moved off its rest. At
/// 500 ‰ (300 px) the paint's 580..780 lies wholly above the line and
/// draws whole. Without the run's inline clip the 300 ‰ paint would draw
/// below the line.
#[test]
fn lift_paint_is_cut_at_its_trim_line_as_the_covers_change() {
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (_dir, b) = bundle(&r);
    install(&mut r, &b, 1);
    r.enter_loading(1);
    let [x, y, w, _] = LIFT_PAINT;
    let column = u32::try_from(x + w / 2).unwrap();
    let row = |v: i32| (column, u32::try_from(v).unwrap());
    let rise = |lift: i32| lift * LANE_LENGTH / 1_000;
    let follower_at = |dy: i32| {
        let [fx, fy, fw, fh] = FOLLOWER;
        centre_of([fx, fy - dy, fw, fh])
    };

    let rest = loading(&mut r, &text, &loading_at(1, 0, 0, 0));
    assert_eq!(rgb(&rest, row(TRIM_LINE - 10)), BLACK);
    assert_eq!(rgb(&rest, row(TRIM_LINE + 10)), BLACK);
    assert_eq!(rgb(&rest, follower_at(0)), RED);

    let lifted = loading(&mut r, &text, &loading_at(1, 16, 300, 0));
    let top = y - rise(300);
    assert_eq!(top, 700);
    assert_eq!(rgb(&lifted, row(top + 5)), MAGENTA);
    assert_eq!(rgb(&lifted, row(TRIM_LINE - 1)), MAGENTA);
    assert_eq!(rgb(&lifted, row(TRIM_LINE)), BLACK, "cut at the line");
    assert_eq!(rgb(&lifted, row(TRIM_LINE + 10)), BLACK);
    assert_eq!(rgb(&lifted, row(top - 5)), BLACK);
    // Upright: the cut spans the paint's whole width at the line.
    for dx in [5, w / 2, w - 5] {
        let at = (
            u32::try_from(x + dx).unwrap(),
            u32::try_from(TRIM_LINE - 1).unwrap(),
        );
        assert_eq!(rgb(&lifted, at), MAGENTA, "{dx}");
    }
    assert_eq!(rgb(&lifted, follower_at(0)), BLACK);
    assert_eq!(rgb(&lifted, follower_at(rise(300))), RED);

    let high = loading(&mut r, &text, &loading_at(1, 32, 500, 0));
    let top = y - rise(500);
    assert_eq!(rgb(&high, row(top + 5)), MAGENTA);
    assert_eq!(rgb(&high, row(top + 195)), MAGENTA);
    assert_eq!(rgb(&high, row(top + 205)), BLACK);
    assert_eq!(rgb(&high, follower_at(rise(500))), RED);
}

/// The installed presentation's `screen` timer start.
fn screen(r: &Renderer) -> Option<TimeUs> {
    r.installation(ScreenKind::Play)
        .unwrap()
        .loading()
        .timer(timer::SCREEN)
}

/// The centre of canvas rect `rect`, as a pixel.
fn centre_of([x, y, w, h]: [i32; 4]) -> (u32, u32) {
    (
        u32::try_from(x + w / 2).unwrap(),
        u32::try_from(y + h / 2).unwrap(),
    )
}

/// The control's canvas rect: drawn on every play frame.
const CONTROL: [i32; 4] = [0, 0, 100, 100];
/// The MAX line's canvas rect: drawn on the latch's timer.
const MAX_LINE: [i32; 4] = [200, 0, 100, 100];

/// A Lua play skin through the real importer, as a bundle for `r`: a
/// red control image, and the same image on a latch that stores the state
/// clock once the point reaches 200 000 (a full score) and keeps it, the
/// MAX line's shape.
fn latch_bundle(r: &Renderer) -> (tempfile::TempDir, Arc<SkinBundle>) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let at = |[x, y, w, h]: [i32; 4]| format!("x = {x}, y = {}, w = {w}, h = {h}", 1_080 - y - h);
    std::fs::write(
        d.join("s.luaskin"),
        format!(
            "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             local latch = (function()\n\
               local state = ms.timer_off_value\n\
               return function()\n\
                 if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
                 return state\n\
               end\n\
             end)()\n\
             skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
             skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 16, h = 16 }} }}\n\
             skin.destination = {{\n\
               {{ id = \"i\", dst = {{ {{ {} }} }} }},\n\
               {{ id = \"i\", timer = latch, dst = {{ {{ {} }} }} }},\n\
             }}\n\
             return skin\n",
            at(CONTROL),
            at(MAX_LINE)
        ),
    )
    .unwrap();
    json_support::png(&d.join("a.png"), 16, 16, |_, _| [255, 0, 0, 255]);
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d, "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let facts = skin_beatoraja::ImportFacts::unavailable(ScreenKind::Play);
    let skin_beatoraja::LuaOutcome::Imported(i) = h.import(&config, &facts, &cancel).unwrap()
    else {
        panic!("pending");
    };
    let skin = i.imported.skin;
    let variant = skin.variant([1_920, 1_080], &skin_model::Layout::AUTHOR);
    let atlas = skin.atlas(variant, r.atlas_limits(), &|| false).unwrap();
    let b = SkinBundle::new(
        Arc::new(skin),
        atlas,
        render::ClipTable::default(),
        skin_model::Side::Left,
    )
    .unwrap();
    (dir, Arc::new(b))
}

/// Draws a play frame at state clock `state_us` over `view`, which must
/// present, and reads it back.
fn latch_frame(r: &mut Renderer, view: &game::PlayView, state_us: i64) -> Vec<u8> {
    let mut f = input(view, TimeUs(500 * MS), TimeUs(0), &[]);
    f.state_us = state_us;
    let timing = r.frame(&f).unwrap();
    assert!(timing.presented);
    read_back(r)
}

/// Two practice rounds on one installation, each prepared fresh
/// (design.md "beatoraja Lua Skins", differences from the source, program
/// state and score data across rounds): the MAX line round 1 lit at its
/// full score is off at round 2's READY, its control drawn; round 2's own
/// full score lights it again.
#[test]
fn a_second_round_starts_fresh() {
    let mut r = offscreen(1_920, 1_080);
    let (_dir, b) = latch_bundle(&r);
    let lane = bms_model::Lane::new(1).unwrap();
    let chart = (1..=4)
        .fold(ChartBuilder::new(120_000), |c, k| {
            c.tap(lane, TimeUs(k * 1_000 * MS))
        })
        .build()
        .unwrap();
    let section = eval::Section {
        start: TimeUs(0),
        end: TimeUs(60_000 * MS),
        lead_in: TimeUs(0),
        initial_gauges: GAUGES.iter().map(|g| g.start).collect(),
    };
    let ctx = skin_model::PlayContext {
        attempt: skin_model::Attempt::Section(&section),
        ..context(&GAUGES, false)
    };
    let text = labels("", "", "");
    let a = AttemptInput {
        chart: &chart,
        context: &ctx,
        text: &text,
        target: r.target(),
    };
    let mut full = view(4, 8);
    full.tally = eval::Tally {
        counts: rules::PerJudgment {
            perfect: 4,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 8,
        combo: 4,
        max_combo: 4,
        ..Default::default()
    };
    let mut none = view(0, 0);
    none.tally = eval::Tally::default();
    // Round 1, staged and committed: the line lights at its full score.
    let token = r.stage(&b, &a, 1).unwrap();
    r.commit(token).unwrap();
    let px = latch_frame(&mut r, &full, 7_000 * MS);
    assert_eq!(centre(&px, CONTROL), RED);
    assert_eq!(centre(&px, MAX_LINE), RED);
    // Round 2 on the same installation: READY presents, the control drawn,
    // the line off.
    r.prepare_attempt(&a).unwrap();
    let px = latch_frame(&mut r, &none, 9_000 * MS);
    assert_eq!(centre(&px, CONTROL), RED);
    assert_eq!(centre(&px, MAX_LINE), BLACK);
    // Round 2's own full score lights it again.
    let px = latch_frame(&mut r, &full, 9_500 * MS);
    assert_eq!(centre(&px, MAX_LINE), RED);
}
