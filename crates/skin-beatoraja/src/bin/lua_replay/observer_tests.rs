//! Construction-time observers in the reference executor (design.md
//! "beatoraja Lua Skins", the reference executor): the host answers an
//! observer's function with the source's state machine.

use crate::script::Script;
use crate::tests::{dst, folder, options};
use skin_beatoraja::lua::replay::Called;

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
