//! The expression-lowered consumers against their closures
//! (s7-06e-3-design.md §3): L1–L11. Every fixture uses system images,
//! plain images, image sets and numbers only, no judge outside L6's
//! listed one, and no pruning.

use crate::inputs::FrameInputs;
use crate::isolation;
use crate::observed::{Cause, Observed, Outcome, Point};
use crate::report::Report;
use crate::script::Script;
use crate::tests::{dst, folder, options, run_with};
use crate::{Options, Stop, replay};

/// Both imports, the production frames and the reference over `o`: the
/// report, what the reference observed and the frames' inputs.
fn pieces(body: &str, o: &Options) -> (Report, Observed, Vec<FrameInputs>) {
    let d = folder(body);
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let (out, rec) = h.import_recorded(&config, &o.facts, &cancel);
    let skin_beatoraja::LuaOutcome::Imported(i) = out.unwrap() else {
        panic!("pending");
    };
    let (run, inputs) =
        crate::production::record(&i.imported.skin, rec, &o.script, Some(12)).unwrap();
    let mut obs = crate::reference::run_rounds(
        &inputs,
        || {
            let (_, rec, vm) = h.import_reference(&config, &o.facts, &cancel);
            let vm = vm.unwrap();
            for &(id, delta) in &o.perturb {
                vm.perturb(id, delta);
            }
            Ok(crate::reference::Fresh {
                vm,
                items: rec.items,
                sources: rec.sources,
            })
        },
        None,
    )
    .unwrap();
    obs.root = d.path().to_path_buf();
    let report = crate::production::compare::compare(&run, &obs, &o.isolation);
    (report, obs, inputs)
}

/// `o` with a reviewed `writes` entry for every root `body` stops on
/// loading or page frames: the fixtures' reads the outside hosts do not
/// answer, each a pure return (e-2's O1).
fn covered(body: &str, mut o: Options) -> Options {
    let r = run_with(body, &o);
    let entries: String = r
        .stopped
        .iter()
        .filter(|s| s.kind == "outside")
        .map(|s| format!("root s.luaskin {} writes\n", s.site.1))
        .collect();
    o.isolation = isolation::parse(&entries).unwrap();
    o
}

/// A number `id` over the source `a`, its value `value`, on a destination.
fn number(id: &str, value: &str, x: i32) -> String {
    format!(
        "table.insert(skin.value, {{ id = \"{id}\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, value = {value} }})\n{}",
        dst(&format!("\"{id}\""), x, "")
    )
}

/// The skin's `value` list, before any [`number`].
const VALUES: &str = "skin.value = {}\n";

/// Each round's frame indices: the runs of play frames.
fn rounds_of(inputs: &[FrameInputs]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut open = false;
    for (i, f) in inputs.iter().enumerate() {
        if f.is_play() {
            if !open {
                out.push(Vec::new());
            }
            open = true;
            if let Some(r) = out.last_mut() {
                r.push(i);
            }
        } else {
            open = false;
        }
    }
    out
}

/// The reference's converted value of every `value` invocation on frame
/// `i`, in schedule order.
fn values_on(obs: &Observed, i: usize) -> Vec<i32> {
    obs.frames[i]
        .points
        .iter()
        .filter_map(|p| match p {
            Point::Ran {
                key,
                outcome: Outcome::Value(r),
            } if key.field == "value" => Some(crate::production::compare::toint(*r)),
            _ => None,
        })
        .collect()
}

fn lowered(r: &Report, field: &str) -> usize {
    r.lowered
        .iter()
        .filter(|((_, f), _)| *f == field)
        .map(|(_, n)| n)
        .sum()
}

/// L1: one callable consumed as an `op` (a truthiness, `true`) and as a
/// number's value (an integer, the PGREAT count) is two occurrences, each with
/// its own expression: the `op` comes first, so an expression looked up by
/// callable would give the value the `op`'s 1 where the count is 0.
#[test]
fn a_lowered_record_carries_its_expression() {
    let body = format!(
        "local f = function() return ms.number(110) end\n{VALUES}{}{}",
        dst("\"i\"", 100, "op = { f },"),
        number("m", "f", 0)
    );
    let r = run_with(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(lowered(&r, "value"), r.frames, "{}", r.text(0, true, true));
    assert_eq!(lowered(&r, "op"), r.frames, "{}", r.text(0, true, true));
    assert!(!r.buckets.contains_key("lowered"));
}

/// L2: a lowered `draw`, an unguarded one and one whose guard is false on
/// every frame (the chart's total, unknown to the harness), agrees.
#[test]
fn a_lowered_draw_agrees() {
    let body = format!(
        "{}{}",
        dst(
            "\"i\"",
            0,
            "draw = function() return ms.number(100) >= 0 end,"
        ),
        dst(
            "\"i\"",
            100,
            "draw = function() return ms.number(368) > 0 end,"
        )
    );
    let r = run_with(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(
        lowered(&r, "draw"),
        r.frames * 2,
        "{}",
        r.text(0, true, true)
    );
}

/// L2: two different lowered `op` entries of one element, each compared
/// at its own index.
#[test]
fn a_lowered_op_keeps_its_index() {
    let body = dst(
        "\"i\"",
        0,
        "op = { function() return ms.number(100) >= 0 end, function() return ms.number(100) < 0 end },",
    );
    let r = run_with(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(lowered(&r, "op"), r.frames * 2, "{}", r.text(0, true, true));
}

/// L3: a value whose guard is false is unavailable on both sides.
#[test]
fn a_guarded_value_is_unavailable() {
    let body = format!(
        "{VALUES}{}",
        number("m", "function() return ms.number(368) end", 0)
    );
    let r = run_with(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(lowered(&r, "value"), r.frames, "{}", r.text(0, true, true));
}

/// L3: an ordinary value, `i32::MAX` and `i32::MIN` convert alike: 5
/// (shown), MAX and MIN (hidden).
#[test]
fn lowered_values_convert_as_the_source() {
    let body = format!(
        "{VALUES}{}{}{}",
        number("a1", "function() return ms.number(100) * 0 + 5 end", 0),
        number(
            "a2",
            "function() return ms.number(100) * 0 + 2147483647 end",
            70
        ),
        number(
            "a3",
            "function() return ms.number(100) * 0 - 2147483648 end",
            140
        )
    );
    let (r, obs, _) = pieces(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    // `i32::MIN` is past the expressions' literals: that one is compiled
    // into the program and compared there; the other two lower.
    assert_eq!(
        lowered(&r, "value"),
        r.frames * 2,
        "{}",
        r.text(0, true, true)
    );
    assert_eq!(values_on(&obs, 0), [5, i32::MAX, i32::MIN]);
}

/// L4: a lowered image-set selector agrees: the gauge's type, and a
/// guarded read hidden (−1) while its guard is false.
#[test]
fn a_lowered_selector_agrees() {
    let body = "skin.imageset = {\n\
           { id = \"s\", images = { \"i\", \"j\" }, value = function() return ms.gauge_type() end },\n\
           { id = \"t\", images = { \"i\", \"j\" }, value = function() return ms.number(368) end } }\n"
        .to_owned()
        + &dst("\"s\"", 0, "")
        + &dst("\"t\"", 100, "");
    let r = run_with(&body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(
        lowered(&r, "value"),
        r.frames * 2,
        "{}",
        r.text(0, true, true)
    );
}

/// L5: a lowered consumer whose closure cannot complete on a play frame is
/// a finding, which an isolation entry does not waive.
#[test]
fn an_inconclusive_lowered_consumer_is_found() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(75) >= 0 end,",
    );
    let mut o = options(Script::full());
    o.deny = vec![75];
    let first = run_with(&body, &o);
    let line = first.findings.first().map(|f| f.site.1).expect("a finding");
    o.isolation = isolation::parse(&format!("root s.luaskin {line} writes\n")).unwrap();
    let r = run_with(&body, &o);
    assert!(r.stopped.is_empty(), "{}", r.text(0, true, true));
    assert_eq!(r.findings.len(), r.frames, "{}", r.text(0, true, true));
    assert!(
        r.findings
            .iter()
            .all(|f| f.what.starts_with("reference could not complete")),
        "{}",
        r.text(0, true, true)
    );
}

/// L6: a lowered consumer of an unmodelled kind stays listed and counted.
#[test]
fn unmodelled_lowered_stays_listed() {
    let body = "skin.judge = { { id = \"jd\", index = 0, images = {\n\
           { id = \"i\", dst = { { x = 0, y = 0, w = 6, h = 6 } } } }, numbers = {} } }\n\
         table.insert(skin.destination, { id = \"jd\", draw = function() return ms.number(100) >= 0 end, dst = { { x = 0, y = 0, w = 6, h = 6 } } })\n";
    let r = run_with(body, &options(Script::full()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(
        r.buckets.get("lowered on a unmodelled object (e-3)"),
        Some(&1),
        "{}",
        r.text(0, true, true)
    );
    assert!(r.lowered.is_empty());
}

/// L7: comparisons on the frame's own state over the rounds: a threshold
/// crossing (the first PGREAT), the rearmed READY and the quit's frame.
#[test]
fn lowered_consumers_follow_the_rounds() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(110) >= 1 end,",
    );
    let (r, obs, inputs) = pieces(&body, &covered(&body, options(Script::rounds())));
    assert!(r.passes(), "{}", r.text(0, true, true));
    for kind in ["ready", "rearmed ready", "play", "tail"] {
        assert!(
            r.lowered.get(&(kind, "draw")).is_some_and(|n| *n > 0),
            "{kind}"
        );
    }
    // The quit is round 2's last frame, compared there.
    let quit = *rounds_of(&inputs)[1].last().unwrap();
    assert!(
        obs.frames[quit]
            .points
            .iter()
            .any(|p| matches!(p, Point::Ran { key, .. } if key.field == "draw"))
    );
}

/// L7: FAILED and Closing frames, the latter in round 2, are compared
/// too.
#[test]
fn lowered_consumers_follow_failed_and_closing() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(100) >= 0 end,",
    );
    let r = run_with(&body, &options(Script::closing()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    for kind in ["failed", "closing"] {
        assert!(
            r.lowered.get(&(kind, "draw")).is_some_and(|n| *n > 0),
            "{kind}: {}",
            r.text(0, true, true)
        );
    }
}

/// L8: on loading and page frames a lowered invocation is bucketed, not
/// compared.
#[test]
fn outside_frames_leave_lowered_uncompared() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(100) >= 0 end,",
    );
    let r = run_with(&body, &options(Script::rounds()));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert!(
        r.lowered
            .keys()
            .all(|(kind, _)| !matches!(*kind, "loading" | "page"))
    );
    assert_eq!(r.buckets.get("outside (loading)"), Some(&120));
    assert_eq!(r.buckets.get("outside (page)"), Some(&480));
}

/// L9: a perturbation that turns a lowered draw false (the point −1
/// before the first judgment) is found there.
#[test]
fn a_perturbed_lowered_read_is_found() {
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(100) >= 0 end,",
    );
    let mut o = options(Script::full());
    o.perturb = vec![(100, -1)];
    let r = run_with(&body, &o);
    assert!(!r.findings.is_empty());
    assert!(
        r.findings
            .iter()
            .all(|f| f.what.starts_with("lowered: production 1, reference 0")),
        "{}",
        r.text(0, true, true)
    );
}

/// The three-way reading of the EX score: 1 without score data, 2 at 0,
/// 3 above.
const THREE_WAY: &str = "function() local n = ms.number(71) if n < 0 then return 1 elseif n == 0 then return 2 end return 3 end";

/// L10: the score data starts afresh at each round: `MIN_VALUE` before
/// the round's first judgment on the source's side, so a number whose
/// `ref` is the EX score skips its conditions there in every round, and
/// production's own value for no judgment is a finding there only (the
/// known difference, "Score data before the first judgment").
#[test]
fn score_data_starts_at_each_round() {
    let body = format!(
        "{VALUES}{}table.insert(skin.value, {{ id = \"e\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, ref = 71 }})
{}",
        number("m", THREE_WAY, 0),
        dst(
            "\"e\"",
            70,
            "draw = function() return ms.number(100) >= 0 end,"
        )
    );
    let (r, obs, inputs) = pieces(&body, &options(Script::rounds()));
    let rounds = rounds_of(&inputs);
    let sentinel = |i: usize| {
        obs.frames[i].points.iter().any(
            |p| matches!(p, Point::NotReached { key, why: Cause::Sentinel } if key.field == "draw"),
        )
    };
    for (n, round) in rounds.iter().enumerate() {
        assert_eq!(
            values_on(&obs, round[0]).first(),
            Some(&1),
            "round {}",
            n + 1
        );
        assert!(sentinel(round[0]), "round {}", n + 1);
        assert_eq!(
            values_on(&obs, *round.last().unwrap()).first(),
            Some(&3),
            "round {}",
            n + 1
        );
        assert!(
            r.findings
                .iter()
                .any(|f| f.place.starts_with(&format!("round {} ", n + 1))),
            "round {}: {}",
            n + 1,
            r.text(0, true, true)
        );
    }
    assert!(
        r.findings.iter().all(|f| f.key.field == "value"),
        "{}",
        r.text(0, true, true)
    );
    assert!(r.buckets.keys().all(|b| !b.contains("score")));
}

/// L10: the eight score properties' exact answers through Lua reads:
/// `MIN_VALUE` on each round's first READY frame, then the round's own
/// values at its last frame (round 1 every note on time: EX 14; round 2
/// quit after note 3: EX 6).
#[test]
fn score_ids_read_exact_values() {
    let ids = [71, 101, 102, 103, 115, 116, 155, 156];
    let body = ids
        .iter()
        .enumerate()
        .fold(VALUES.to_owned(), |b, (k, id)| {
            b + &number(
                &format!("n{id}"),
                &format!("function() return ms.number({id}) end"),
                i32::try_from(k).unwrap() * 70,
            )
        });
    let min = i32::MIN;
    let (_, obs, inputs) = pieces(&body, &options(Script::rounds()));
    let rounds = rounds_of(&inputs);
    for round in &rounds {
        assert_eq!(values_on(&obs, round[0]), [min; 8]);
    }
    assert_eq!(
        values_on(&obs, *rounds[0].last().unwrap()),
        [14, 14, 100, 0, 100, 0, 100, 0]
    );
    assert_eq!(
        values_on(&obs, *rounds[1].last().unwrap()),
        [6, 6, 100, 0, 42, 85, 42, 85]
    );
}

/// s7-06s S8: the score readings agree in every round (Rm's
/// `getDummyNumber` shape): each round's score data is its own on both
/// sides, so the run passes with nothing attributed.
#[test]
fn a_score_reading_agrees_across_rounds() {
    let dummy = |n: i32| {
        format!("function() if ms.number(101) < 1 then return 0 else return ms.number({n}) end end")
    };
    let body = format!(
        "{VALUES}{}{}",
        number("r", &dummy(102), 0),
        number("s", &dummy(101), 70)
    );
    let r = run_with(&body, &covered(&body, options(Script::rounds())));
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert!(
        r.buckets.keys().all(|b| !b.contains("score")),
        "{}",
        r.text(0, true, true)
    );
}

/// A score input that disagrees with a judged frame's fields (a wrong
/// model, injected: EX + 2) is refused, so the run never compares on it.
#[test]
fn a_mismatched_score_input_is_refused() {
    let body = format!(
        "{VALUES}{}",
        number("m", "function() return ms.number(71) end", 0)
    );
    let d = folder(&body);
    crate::production::SCORE_SKEW.with(|s| s.set(2));
    let got = replay(d.path(), "s.luaskin", &options(Script::rounds()));
    crate::production::SCORE_SKEW.with(|s| s.set(0));
    match got {
        Err(Stop::Failed(e)) => assert!(e.contains("score input"), "{e}"),
        Ok(_) => panic!("accepted"),
        Err(Stop::Differs) => panic!("the imports differ"),
    }
}

/// The replay refuses a run whose score input disagrees with a play
/// frame's score fields (its self-check, s7-06s-design.md S7): reached
/// only through a broken model or round, so this checks the replay's
/// normal path, later rounds' READY included, stays accepted.
#[test]
fn the_score_input_matches_every_play_frame() {
    let body = format!(
        "{VALUES}{}",
        number("m", "function() return ms.number(71) end", 0)
    );
    for s in [Script::rounds(), Script::closing()] {
        let d = folder(&body);
        match replay(d.path(), "s.luaskin", &options(s)) {
            Ok(_) => {}
            Err(Stop::Failed(e)) => panic!("refused: {e}"),
            Err(Stop::Differs) => panic!("the imports differ"),
        }
    }
}

/// s10-10-design.md C28: `float_number(113)` read against 0 lowers, and
/// the reference host answers it as the source's float rate, `(float) B /
/// (n · 2)`: with a best of 11 the draw against 0 holds on both sides and
/// with none it does not, compared on every frame it is reached.
#[test]
fn the_best_rate_agrees_with_the_source_s_float() {
    let body = format!(
        "{}{}",
        dst(
            "\"i\"",
            0,
            "draw = function() return ms.float_number(113) ~= 0 end,"
        ),
        dst(
            "\"i\"",
            100,
            "draw = function() return ms.float_number(113) == 0 end,"
        )
    );
    for basis in [Some((11, 13)), None] {
        crate::production::SCORE_BASIS.with(|s| s.set(basis));
        let r = run_with(&body, &options(Script::full()));
        crate::production::SCORE_BASIS.with(|s| s.set(None));
        assert!(r.passes(), "{basis:?}: {}", r.text(0, true, true));
        assert_eq!(
            lowered(&r, "draw"),
            r.frames * 2,
            "{basis:?}: {}",
            r.text(0, true, true)
        );
    }
}
