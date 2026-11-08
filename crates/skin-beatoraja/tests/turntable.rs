//! Offset 1, the turntable (docs/stage3.md §3 "Offsets"): a Lua skin's
//! image, image at an angle and number listing it turn with the play
//! state's clock, its angle negated as every source angle is; a text
//! listing it stays unrotated, as the source draws text; the same angle
//! reaches the loading frame.

mod common;

use std::f32::consts::TAU;

use common::play::{Drawn, Loader};
use common::{font, png, write};
use game::{ScratchInput, Turn, TurntableClock};
use skin_beatoraja::{Cancel, Config, read_lua_header};

const MS: i64 = 1_000;

/// An image at angle 0 and at 10°, a number and a text, each listing
/// offset 1, and an image that does not; the text at its own angle 0.
const LUA: &str = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
    if not skin_config then return skin end\n\
    skin.source = { { id = \"a\", path = \"a.png\" }, { id = \"n\", path = \"n.png\" } }\n\
    skin.font = { { id = \"f\", path = \"f.ttf\" } }\n\
    skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
    skin.value = { { id = \"v\", src = \"n\", x = 0, y = 0, w = 100, h = 10, divx = 10, digit = 1, ref = 71 } }\n\
    skin.text = { { id = \"t\", font = \"f\", size = 24, constantText = \"hi\" } }\n\
    skin.destination = {\n\
      { id = \"i\", offset = 1, dst = { { x = 100, y = 100, w = 10, h = 10 } } },\n\
      { id = \"i\", offsets = { 1 }, dst = { { x = 300, y = 100, w = 10, h = 10, angle = 10 } } },\n\
      { id = \"v\", offset = 1, dst = { { x = 500, y = 100, w = 10, h = 20 } } },\n\
      { id = \"t\", offset = 1, dst = { { x = 700, y = 100, w = 100, h = 30 } } },\n\
      { id = \"i\", dst = { { x = 900, y = 100, w = 10, h = 10 } } } }\n\
    return skin\n";

/// The skin, with the folder its files stay in while it is drawn.
fn imported() -> (tempfile::TempDir, skin_model::Skin) {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "s.luaskin", LUA.as_bytes());
    png(d.path(), "a.png", 64, 64);
    png(d.path(), "n.png", 100, 10);
    font(d.path(), "f.ttf");
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    assert!(
        i.imported.report.notes.is_empty(),
        "{:?}",
        i.imported.report.notes
    );
    (d, i.imported.skin)
}

/// `a` in degrees, within [0, 360).
fn degrees(radians: f32) -> f32 {
    radians.rem_euclid(TAU).to_degrees().rem_euclid(360.0)
}

fn close(a: f32, b: f32) -> bool {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d) < 1e-3
}

/// The angles drawn, in degrees, of the frame with the clock's angle:
/// the two images, the number, the text and the image without the
/// offset, in draw order.
fn angles(d: &mut Drawn, clock: &TurntableClock) -> Vec<f32> {
    d.turntable = clock.angle();
    d.frame(0, &[]).iter().map(|q| degrees(q.angle)).collect()
}

fn clock() -> TurntableClock {
    let mut c = TurntableClock::default();
    c.adopt(1);
    c
}

/// After 600 ms idle the source's angle is −100°: the follower adds
/// +100°, after the source angle's negation (the 10° image reads −10°
/// plus it).
#[test]
fn idle_600_ms_turns_the_mark_by_plus_100() {
    let (_dir, skin) = imported();
    let mut d = Drawn::new(skin, 1);
    d.attach_lines(1.0, 1.0);
    let mut c = clock();
    let at_rest = angles(&mut d, &c);
    assert_eq!(at_rest.len(), 5, "{at_rest:?}");
    assert!(close(at_rest[0], 0.0));
    assert!(close(at_rest[1], -10.0));
    for ms in (0..=600).step_by(10) {
        c.tick(TimeUs(ms * MS), Turn::Idle);
    }
    let a = angles(&mut d, &c);
    assert!(close(a[0], 100.0), "{a:?}");
    assert!(close(a[1], 90.0), "{a:?}");
    assert!(close(a[2], 100.0), "the number turns: {a:?}");
    assert!(close(a[3], 0.0), "the text stays unrotated: {a:?}");
    assert!(close(a[4], 0.0), "no offset, no turn: {a:?}");
}

/// Positive held 300 ms turns the source −150° (the follower +150°),
/// then Negative +50° per 300 ms (−50°), then both: Negative wins.
#[test]
fn positive_then_negative_then_both() {
    let (_dir, skin) = imported();
    let mut d = Drawn::new(skin, 1);
    let mut c = clock();
    let held = |positive, negative| ScratchInput {
        positive,
        negative,
        analog: None,
    };
    let mut seen = Vec::new();
    for (from, to, input) in [
        (0, 300, held(true, false)),
        (310, 600, held(false, true)),
        (610, 900, held(true, true)),
    ] {
        for ms in (from..=to).step_by(10) {
            c.tick(
                TimeUs(ms * MS),
                input.direction(TimeUs(ms * MS), [false; 2]),
            );
        }
        seen.push(angles(&mut d, &c)[0]);
    }
    for (got, want) in seen.iter().zip([150.0, 100.0, 50.0]) {
        assert!(close(*got, want), "{seen:?}");
    }
}

/// An analog press read without a drain: Negative until its deadline,
/// then the digital Positive held.
#[test]
fn an_analog_release_at_its_deadline() {
    let (_dir, skin) = imported();
    let mut d = Drawn::new(skin, 1);
    let mut c = clock();
    let input = ScratchInput {
        positive: true,
        negative: false,
        analog: Some(game::AnalogTurn {
            negative: true,
            until: TimeUs(300 * MS),
        }),
    };
    for ms in (0..=600).step_by(10) {
        c.tick(
            TimeUs(ms * MS),
            input.direction(TimeUs(ms * MS), [false; 2]),
        );
    }
    // Each tick turns by the direction its frame reads: Negative for the
    // 29 ticks before the deadline (+290/6°), Positive from it on, the
    // tick at 300 ms included (−3 × 310/6°): −640/6° in the source, drawn
    // negated.
    assert!(close(angles(&mut d, &c)[0], 640.0 / 6.0));
}

/// The loading frame turns the same mark by the same angle.
#[test]
fn the_loading_frame_turns_alike() {
    let (_dir, skin) = imported();
    let mut l = Loader::new(skin);
    let mut c = clock();
    for ms in (0..=600).step_by(10) {
        c.tick(TimeUs(ms * MS), Turn::Idle);
    }
    l.turntable = c.angle();
    let q = l.frame(0);
    assert!(close(degrees(q[0].angle), 100.0), "{q:?}");
}

use bms_model::TimeUs;
