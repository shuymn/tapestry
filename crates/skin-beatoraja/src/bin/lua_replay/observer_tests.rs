//! Construction-time observers in the reference executor (design.md
//! "beatoraja Lua Skins", the reference executor): the host answers an
//! observer's function with the source's state machine, its start carried
//! across the rounds' references, and the comparator checks each observer
//! occurrence against production's fold and its destination's own
//! elements' ages on `screen`.

use crate::inputs::{FrameInputs, Host};
use crate::observed::Observed;
use crate::rounds_tests::{pieces, rounds_of};
use crate::script::{Phase, Round, Script};
use crate::tests::{dst, folder, options};
use skin_beatoraja::lua::replay::Called;

/// A true observer `on` (image `i` at x 0), a false one `off` (image `j`
/// at x 100), and an untimed plain image at x 200 whose `screen` samples
/// are not the observer's.
pub(crate) fn twins() -> String {
    format!(
        "local tu = require(\"timer_util\")\n\
         local on = tu.timer_observe_boolean(function() return true end)\n\
         local off = tu.timer_observe_boolean(function() return false end)\n\
         {}{}{}",
        dst("\"i\"", 0, "timer = on,"),
        dst("\"j\"", 100, "timer = off,"),
        dst("\"i\"", 200, "")
    )
}

/// The observer occurrences' starts on frame `i`, by destination: the
/// timers of destinations 0 (`on`) and 1 (`off`).
fn observer_starts(obs: &Observed, i: usize) -> [Option<Option<i64>>; 2] {
    let at = |d: usize| {
        obs.frames[i]
            .timers
            .iter()
            .find(|t| t.key.dest == d)
            .map(|t| t.start.filter(|_| t.on))
    };
    [at(0), at(1)]
}

/// A practice session: the true observer starts at the play state's first
/// frame (the first loading frame) and keeps that start through the page,
/// every round's fresh reference and its rearmed READY; the false one is
/// off throughout; production's own elements agree on every loading and
/// play frame, with no difference from the reference's age (the screen
/// origin is the play state's first frame).
#[test]
fn an_observer_s_ages_agree_across_a_practice_session() {
    let (r, obs, inputs) = pieces(&twins(), Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let first = inputs[0].state_us();
    for i in 0..inputs.len() {
        assert_eq!(
            observer_starts(&obs, i),
            [Some(Some(first)), Some(None)],
            "frame {i}"
        );
    }
    assert_eq!(rounds_of(&inputs).len(), 3);
    for kind in ["loading", "page", "ready", "play"] {
        for b in [true, false] {
            assert!(
                r.observers.get(&(kind, b)).copied().unwrap_or(0) > 0,
                "{kind} {b}: {}",
                r.text(0, true, true)
            );
        }
    }
    assert_eq!(r.observer_offsets.keys().copied().collect::<Vec<_>>(), [0]);
}

/// The host answers an observer's function with the source's state
/// machine: a true predicate starts it at the frame's time if it is off
/// and keeps that start while it stays true, a false one turns it off,
/// and the next true one starts it again.
#[test]
fn the_observer_host_starts_on_true_and_stops_on_false() {
    let body = format!(
        "local tu = require(\"timer_util\")\n\
         skin.probe = tu.timer_observe_boolean(function()\n\
           local t = ms.time() return (t >= 2000 and t < 4000) or t >= 5000\n\
         end)\n{}",
        dst("\"i\"", 0, "")
    );
    let d = folder(&body);
    let o = options(Script::full());
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let (out, _, vm) = h.import_reference(&config, &o.facts, &cancel);
    assert!(
        matches!(out, Ok(skin_beatoraja::LuaOutcome::Imported(_))),
        "{out:?}"
    );
    let vm = vm.unwrap();
    let probe: mlua::Function = vm.skin().as_table().unwrap().get("probe").unwrap();
    assert!(vm.observer(&probe).is_some());
    let off = i64::MIN as f64;
    let mut got = Vec::new();
    for t in [1000, 2000, 3000, 4000, 5000, 6000] {
        vm.set_frame(&[], &[], t);
        got.push((t, vm.call(&probe)));
    }
    let want = [off, 2000.0, 2000.0, off, 5000.0, 5000.0];
    assert_eq!(
        got,
        [1000, 2000, 3000, 4000, 5000, 6000]
            .into_iter()
            .zip(want)
            .map(|(t, v)| (t, Called::Value(mlua::Value::Number(v))))
            .collect::<Vec<_>>()
    );
}

/// A true observer's track that draws nothing for a while is no finding:
/// production's age is its element's timer age, recorded whether or not
/// the track gives a sample, so a finite track past its end, a first key
/// after the start and a loop wrapping into the gap before its first key
/// agree on every frame as a track drawn throughout does.
#[test]
fn an_observer_s_track_gaps_are_no_finding() {
    let tu = "local tu = require(\"timer_util\")\n\
              local on = tu.timer_observe_boolean(function() return true end)\n";
    for (what, fields, keys) in [
        (
            "finite",
            "loop = -1,",
            "{ time = 0, x = 0, y = 0, w = 64, h = 64 }, { time = 100 }",
        ),
        (
            "delayed",
            "",
            "{ time = 500, x = 0, y = 0, w = 64, h = 64 }, { time = 600 }",
        ),
        (
            "looping gap",
            "loop = 0,",
            "{ time = 200, x = 0, y = 0, w = 64, h = 64 }, { time = 300 }",
        ),
    ] {
        let body = format!(
            "{tu}table.insert(skin.destination, {{ id = \"i\", timer = on, {fields} dst = {{ {keys} }} }})\n"
        );
        let (r, _, _) = pieces(&body, Script::rounds());
        assert!(r.passes(), "{what}: {}", r.text(0, true, true));
        assert!(
            r.observers.get(&("play", true)).copied().unwrap_or(0) > 0,
            "{what}: {}",
            r.text(0, true, true)
        );
    }
}

/// A round-only script has no loading: production's `screen` starts the
/// skin's loading minimum (2 500 ms declared) before READY, so its age
/// exceeds the reference's by exactly that.
#[test]
fn a_round_only_observer_is_ahead_by_the_loading_minimum() {
    let body = format!("skin.loadend = 2500\n{}", twins());
    let (r, _, _) = pieces(&body, Script::full());
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(
        r.observer_offsets.keys().copied().collect::<Vec<_>>(),
        [2_500_000]
    );
}

/// A later Start's loading frames run `screen` from their own lifetime
/// (docs/stage3.md D3): there the difference is the play state's first
/// frame less that lifetime's start, negative; on every play frame it is
/// 0.
#[test]
fn a_later_loading_lifetime_s_observer_follows_its_own_origin() {
    let since = 3_210_000;
    let script = Script {
        name: "screen",
        notes: 7,
        origin: since,
        phases: vec![
            Phase::Loading { len_us: 1_234_000 },
            Phase::Page { len_us: 500_000 },
            Phase::Round(Round::on_time(7, true)),
            Phase::Page { len_us: 500_000 },
            Phase::Loading { len_us: 700_000 },
            Phase::Round(Round::on_time(7, true)),
            Phase::Page { len_us: 500_000 },
        ],
        fix_hispeed: 0,
        tempo: Vec::new(),
    };
    let (r, _, inputs) = pieces(&twins(), script);
    assert!(r.passes(), "{}", r.text(0, true, true));
    let loading = |f: &FrameInputs| matches!(f.host(), Host::Outside { page: None, .. });
    // Each loading lifetime's first frame.
    let lifetimes: Vec<i64> = inputs
        .iter()
        .enumerate()
        .filter(|&(i, f)| loading(f) && (i == 0 || !loading(&inputs[i - 1])))
        .map(|(_, f)| f.state_us())
        .collect();
    let [first, second] = lifetimes[..] else {
        panic!("loading lifetimes {lifetimes:?}");
    };
    assert_eq!(first, since);
    assert_eq!(
        r.observer_offsets.keys().copied().collect::<Vec<_>>(),
        [first - second, 0]
    );
}
