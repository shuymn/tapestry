//! Rounds of a play state and the frames production builds no play
//! frame for (s7-06e-2-design.md §8): R20 and the e-2 claims. Every
//! fixture uses system and plain images only (selector 0), no judge and
//! no pruning, so none depends on the program's per-kind gates.

use crate::Options;
use crate::inputs::FrameInputs;
use crate::isolation;
use crate::observed::Observed;
use crate::report::Report;
use crate::script::{Phase, Round, Script};
use crate::tests::{LATCH, dst, folder, options, run_with};

/// Both imports, the production frames and the reference over `script`:
/// the report, what the reference observed, and the frames' inputs.
fn pieces(body: &str, script: Script) -> (Report, Observed, Vec<FrameInputs>) {
    pieces_with(body, &options(script))
}

/// [`pieces`] under `o`, its test controls included.
fn pieces_with(body: &str, o: &Options) -> (Report, Observed, Vec<FrameInputs>) {
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
            Ok(crate::reference::Fresh {
                vm,
                items: rec.items,
                sources: rec.sources,
            })
        },
        o.page_point,
    )
    .unwrap();
    obs.root = d.path().to_path_buf();
    let report = crate::production::compare::compare(&run, &obs, &[]);
    (report, obs, inputs)
}

/// Each round's frame indices, in order: the runs of play frames.
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

/// The reference's timer starts on frame `i`, on timers only.
fn starts(obs: &Observed, i: usize) -> Vec<Option<i64>> {
    obs.frames[i]
        .timers
        .iter()
        .map(|t| t.start.filter(|_| t.on))
        .collect()
}

/// GenericTheme's shape, written fresh: the latch as the timer of a
/// system image and of a plain image.
fn latch_pair() -> String {
    format!(
        "{LATCH}{}{}",
        dst("-111", 0, "timer = latch, loop = -1,"),
        dst("\"i\"", 100, "timer = latch, loop = -1,")
    )
}

/// A latch whose return reads the max combo, which no outside host
/// answers.
const MAX_COMBO_LATCH: &str = "local latch = (function()
       local state = ms.timer_off_value
       return function()
         if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end
         if ms.number(75) >= 0 then return state end
         return ms.timer_off_value
       end
     end)()
";

/// Each round starts fresh on both sides (design.md "beatoraja Lua
/// Skins", the reference executor): every round's first frame is dark;
/// rounds 1 and 3, every note on time, light the latch at their own full
/// score; round 2, quit after note 3, stays dark throughout, its rearmed
/// READY included; production and the reference agree.
#[test]
fn rounds_start_fresh() {
    let (r, obs, inputs) = pieces(&latch_pair(), Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.compiled.len(), 2);
    let rounds = rounds_of(&inputs);
    assert_eq!(rounds.len(), 3);
    for (n, round) in rounds.iter().enumerate() {
        assert_eq!(starts(&obs, round[0]), [None, None], "round {}", n + 1);
    }
    for n in [0, 2] {
        let set = rounds[n]
            .iter()
            .copied()
            .find(|&i| starts(&obs, i).iter().any(Option::is_some))
            .unwrap_or_else(|| panic!("the latch sets in round {}", n + 1));
        let at = inputs[set].state_us();
        assert_eq!(starts(&obs, set), [Some(at), Some(at)], "round {}", n + 1);
        assert!(
            rounds[n]
                .iter()
                .filter(|&&i| i >= set)
                .all(|&i| starts(&obs, i) == [Some(at), Some(at)]),
            "round {}",
            n + 1
        );
    }
    assert!(
        rounds[1]
            .iter()
            .all(|&i| starts(&obs, i).iter().all(Option::is_none))
    );
    assert!(r.kinds.get("rearmed ready").is_some_and(|k| k.1 > 0));
}

/// R20, C3: READY, a rearmed READY and the tail are play frames,
/// compared; loading and the page are outside frames, bucketed.
#[test]
fn ready_frames_are_play_frames() {
    let body = dst(
        "-111",
        0,
        "timer = function() return ms.time() end, loop = -1,",
    );
    let (r, _, _) = pieces(&body, Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.compiled.len(), 1);
    assert_eq!(r.kinds.get("loading").map(|k| k.0), Some(120));
    assert_eq!(r.kinds.get("page").map(|k| k.0), Some(480));
    for kind in ["ready", "rearmed ready", "play", "tail"] {
        let (frames, agreed) = r.kinds.get(kind).copied().unwrap_or_default();
        assert!(
            frames > 0 && agreed > 0,
            "{kind}: {}",
            r.text(0, true, true)
        );
    }
    // One timer invocation an outside frame.
    assert_eq!(r.buckets.get("outside (loading)"), Some(&120));
    assert_eq!(r.buckets.get("outside (page)"), Some(&480));
}

/// O3, C3: FAILED and Closing frames are play frames, compared.
#[test]
fn failed_and_closing_frames_are_play_frames() {
    let body = format!(
        "{LATCH}{}{}",
        dst(
            "-111",
            0,
            "timer = function() return ms.time() end, loop = -1,"
        ),
        dst("\"i\"", 100, "timer = latch, loop = -1,")
    );
    let (r, _, _) = pieces(&body, Script::closing());
    assert!(r.passes(), "{}", r.text(0, true, true));
    for kind in ["failed", "closing"] {
        let (frames, agreed) = r.kinds.get(kind).copied().unwrap_or_default();
        assert!(
            frames > 0 && agreed > 0,
            "{kind}: {}",
            r.text(0, true, true)
        );
    }
}

/// A slotted root stopped on an outside frame (a read no outside host
/// answers) is a finding no entry waives.
#[test]
fn a_slotted_root_stopped_outside_is_found() {
    let body = format!(
        "{MAX_COMBO_LATCH}{}",
        dst("-111", 0, "timer = latch, loop = -1,")
    );
    let (r, _, _) = pieces(&body, Script::rounds());
    assert!(!r.findings.is_empty());
    assert!(
        r.findings.iter().all(|f| f
            .what
            .starts_with("a slotted root stopped on an outside frame")
            && (f.place == "loading" || f.place == "page")),
        "{}",
        r.text(0, true, true)
    );
}

/// A latch written on the page (its point perturbed there) stays in the
/// reference of the round before it: the next round's fresh reference
/// starts dark, and production agrees, so the run passes (design.md
/// "beatoraja Lua Skins", the reference executor: an outside frame's
/// write reaches no later round).
#[test]
fn a_page_write_does_not_leak_into_the_next_round() {
    let mut r1 = Round::on_time(7, true);
    r1.presses[6] = Some(30_000);
    let script = Script {
        name: "page-write",
        notes: 7,
        origin: 0,
        phases: vec![
            Phase::Round(r1),
            Phase::Page { len_us: 500_000 },
            Phase::Round(Round::on_time(7, true)),
        ],
        fix_hispeed: 0,
        tempo: Vec::new(),
    };
    let body = format!("{LATCH}{}", dst("-111", 0, "timer = latch, loop = -1,"));
    let mut o = options(script);
    o.page_point = Some((0, 200_000));
    let (r, obs, inputs) = pieces_with(&body, &o);
    assert!(r.passes(), "{}", r.text(0, true, true));
    let rounds = rounds_of(&inputs);
    // The control: the page wrote the latch, on its first frame.
    let page = rounds[0].last().unwrap() + 1;
    assert!(!inputs[page].is_play());
    let last_page = rounds[1][0] - 1;
    assert_eq!(starts(&obs, last_page), [Some(inputs[page].state_us())]);
    // Round 2 starts dark and lights at its own full score.
    assert_eq!(starts(&obs, rounds[1][0]), [None]);
    assert!(
        rounds[1]
            .iter()
            .any(|&i| starts(&obs, i).iter().all(Option::is_some))
    );
}

/// The draws of `outside_hosts_answer_only_their_fields`, one destination
/// (and so one line) each: the cover (a setting), the point, the max combo, the
/// loading timer through `option(80)`, the play level (its guard).
fn host_reads() -> String {
    [
        "ms.number(14) >= 0",
        "ms.number(100) >= 0",
        "ms.number(75) >= 0",
        "ms.option(80)",
        "ms.number(96) >= 0",
    ]
    .iter()
    .enumerate()
    .map(|(i, read)| {
        dst(
            "\"i\"",
            i32::try_from(i).unwrap() * 70,
            &format!("draw = function() return {read} end,"),
        )
    })
    .collect()
}

/// C5: loading answers what loading gives and the point (0); the page
/// only the point; a timer operand, a denied field and a guard on one are
/// unanswered, each stopped root needing a reviewed entry.
#[test]
fn outside_hosts_answer_only_their_fields() {
    let body = host_reads();
    let r = run_with(&body, &options(Script::rounds()));
    // Each read's line, in the order written, and its outside stops.
    let mut stops: Vec<(u32, &str, usize)> = r
        .stopped
        .iter()
        .map(|s| (s.site.1, s.kind, s.count))
        .collect();
    stops.sort_unstable();
    let loading = 120;
    let page = 480;
    let counts: Vec<usize> = stops.iter().map(|s| s.2).collect();
    assert!(
        stops.iter().all(|s| s.1 == "outside"),
        "{}",
        r.text(0, true, true)
    );
    // The cover and the play level on the page only; the max combo and
    // the loading timer on both; the point never.
    assert_eq!(
        counts,
        [page, loading + page, loading + page, page],
        "{}",
        r.text(0, true, true)
    );
    assert!(!r.passes());
    let entries: String = stops
        .iter()
        .map(|(line, _, _)| format!("root s.luaskin {line} writes\n"))
        .collect();
    let mut o = options(Script::rounds());
    o.isolation = isolation::parse(&entries).unwrap();
    let r = run_with(&body, &o);
    assert!(r.passes(), "{}", r.text(0, true, true));
}

/// C4: installation reads the first play frame's static facts: the
/// difficulty, which loading does not give, removes one object and keeps
/// the other; on a loading frame both would be unresolved.
#[test]
fn installation_reads_the_first_play_frame() {
    let body = format!(
        "{}{}",
        dst("-111", 0, "op = { 151 },"),
        dst("-112", 70, "op = { -151 },")
    );
    let (r, obs, _) = pieces(&body, Script::rounds());
    assert!(r.unresolved.is_empty(), "{}", r.text(0, true, true));
    assert_eq!(
        r.removed
            .iter()
            .map(|l| (l.dest, l.what.as_str()))
            .collect::<Vec<_>>(),
        [(0, "static condition")]
    );
    assert_eq!(obs.installed.len(), 1);
}

/// O1, C6: an entry covering a root's outside stops leaves its play stops
/// a finding, and no entry covers a stopped slotted root.
#[test]
fn outside_entries_waive_nothing_on_play_frames() {
    // A compiled draw reading the max combo, made unanswered on play frames
    // too.
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(75) / 3 >= 0 end,",
    );
    let mut o = options(Script::rounds());
    o.deny = vec![75];
    let r = run_with(&body, &o);
    let line = r
        .stopped
        .first()
        .map(|s| s.site.1)
        .expect("an outside stop");
    o.isolation = isolation::parse(&format!("root s.luaskin {line} writes\n")).unwrap();
    let r = run_with(&body, &o);
    assert!(
        r.stopped
            .iter()
            .all(|s| s.covered.is_ok() && s.kind == "outside")
    );
    assert!(
        r.findings
            .iter()
            .any(|f| f.place.starts_with("round") && f.what.contains("could not complete")),
        "{}",
        r.text(0, true, true)
    );
    assert!(!r.passes());
    // The latch reading the max combo, with an entry naming it.
    let body = format!(
        "{MAX_COMBO_LATCH}{}",
        dst("-111", 0, "timer = latch, loop = -1,")
    );
    let r = run_with(&body, &options(Script::rounds()));
    let line = r.compiled.first().map(|c| c.2.1).unwrap();
    let mut o = options(Script::rounds());
    o.isolation = isolation::parse(&format!("root s.luaskin {line} writes\n")).unwrap();
    let r = run_with(&body, &o);
    assert!(
        r.findings.iter().any(|f| f
            .what
            .starts_with("a slotted root stopped on an outside frame")),
        "{}",
        r.text(0, true, true)
    );
}

/// C9: a whole-chart script keeps e-1's frames: one round, its READY,
/// play and tail only, on the state clock e-1 used.
#[test]
fn whole_chart_scripts_keep_their_frames() {
    use crate::script::{LEAD, STATE_AT_ZERO};
    let (r, _, inputs) = pieces(&latch_pair(), Script::full());
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert!(inputs.iter().all(FrameInputs::is_play));
    assert_eq!(inputs[0].state_us(), STATE_AT_ZERO - LEAD);
    assert_eq!(
        r.kinds.keys().copied().collect::<Vec<_>>(),
        ["play", "ready", "tail"]
    );
    assert_eq!(r.rounds.len(), 1);
}

/// Every page reads the point 0, the next round's fresh score data,
/// after a full-score round as after a quit.
#[test]
fn pages_read_the_point_at_0() {
    let (r, _, inputs) = pieces(&latch_pair(), Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let point = usize::from(skin_model::field::POINT);
    let rounds = rounds_of(&inputs);
    for round in &rounds {
        let after = round.last().unwrap() + 1;
        assert!(!inputs[after].is_play());
        assert_eq!(inputs[after].fields()[point], 0);
    }
    assert!(
        rounds
            .first()
            .is_some_and(|r| inputs[*r.last().unwrap()].fields()[point] == 200_000)
    );
}

/// Each play frame's `screen` reading (ms) and state clock.
fn screens(inputs: &[FrameInputs]) -> Vec<(i32, i64)> {
    let slot = usize::from(skin_model::timer::SCREEN);
    inputs
        .iter()
        .filter(|f| f.is_play())
        .map(|f| (f.timers()[slot], f.state_us()))
        .collect()
}

/// A practice script's untimed elements run on the play state's screen
/// origin, as the player resolves it: the first loading phase's `since`
/// (the state clock at 3 210 ms, 1 234 ms of loading), kept through a
/// rearmed round and over a later loading phase into the next round.
#[test]
fn a_practice_script_s_rounds_run_on_the_loading_origin() {
    use crate::script::LEAD;
    let mut r1 = Round::on_time(7, true);
    r1.rearm_at = Some(-LEAD / 2);
    let since = 3_210_000;
    let script = Script {
        name: "screen",
        notes: 7,
        origin: since,
        phases: vec![
            Phase::Loading { len_us: 1_234_000 },
            Phase::Page { len_us: 500_000 },
            Phase::Round(r1),
            Phase::Page { len_us: 500_000 },
            Phase::Loading { len_us: 700_000 },
            Phase::Round(Round::on_time(7, true)),
            Phase::Page { len_us: 500_000 },
        ],
        fix_hispeed: 0,
        tempo: Vec::new(),
    };
    let body = dst("\"i\"", 0, "");
    let (r, _, inputs) = pieces(&body, script);
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(rounds_of(&inputs).len(), 2);
    let read = screens(&inputs);
    assert!(read.len() > 100);
    for (ms, state_us) in read {
        assert_eq!(i64::from(ms), (state_us - since) / 1_000, "at {state_us}");
    }
}

/// A round-only script has no loading phase: its screen is anchored as
/// the player anchors it, reading the skin's loading minimum (2 500 ms
/// declared) when READY opens.
#[test]
fn a_round_only_script_anchors_the_screen() {
    let body = format!("skin.loadend = 2500\n{}", dst("\"i\"", 0, ""));
    let (r, _, inputs) = pieces(&body, Script::full());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let read = screens(&inputs);
    assert_eq!(read[0].0, 2_500);
    let ready = read[0].1;
    for (ms, state_us) in read {
        assert_eq!(i64::from(ms), (state_us - ready + 2_500_000) / 1_000);
    }
}

/// The skin's declared ending: a 3 s finish margin and a 1 s fadeout.
const ENDING: &str = "skin.finishmargin = 3000\nskin.fadeout = 1000\n";

/// Each play frame's `fadeout` timer, off (-1) or its start.
fn fade_timers(inputs: &[FrameInputs]) -> Vec<i32> {
    let slot = usize::from(skin_model::timer::FADEOUT);
    inputs
        .iter()
        .filter(|f| f.is_play())
        .map(|f| f.timers()[slot])
        .collect()
}

/// A full play's frames run through its hold and its fade-out: timer 2,
/// read by a draw callback, is off before the hold's end and on from it
/// through the 1 s fade-out, in frames labelled `fadeout` that the reference VM
/// answers alike; no frame of a failing script's Closing has it on.
#[test]
fn fade_out_frames_start_timer_2() {
    let body = format!(
        "{ENDING}{}",
        dst(
            "\"i\"",
            0,
            "draw = function() return ms.timer(2) ~= ms.timer_off_value end,"
        )
    );
    let (r, _, inputs) = pieces(&body, Script::full());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let (frames, agreed) = r.kinds.get("fadeout").copied().unwrap_or_default();
    assert!(frames > 0 && agreed > 0, "{}", r.text(0, true, true));
    let timers = fade_timers(&inputs);
    // Read as its age in ms: 0 at the hold's end, then rising with the
    // frames, never off again.
    let first = timers.iter().position(|&t| t != -1).unwrap();
    assert!(timers[..first].iter().all(|&t| t == -1));
    assert_eq!(timers[first], 0);
    assert!(timers[first..].windows(2).all(|w| w[0] <= w[1]));
    assert!(*timers.last().unwrap() >= 995, "{timers:?}");
    assert_eq!(timers.len() - first, frames);
    // Without the declaration the frames end at the 5 s hold, unfaded.
    let (plain, _, plain_inputs) = pieces(
        &dst(
            "\"i\"",
            0,
            "draw = function() return ms.timer(2) ~= ms.timer_off_value end,",
        ),
        Script::full(),
    );
    assert!(plain.passes(), "{}", plain.text(0, true, true));
    assert_eq!(plain.kinds.get("fadeout"), None);
    assert!(fade_timers(&plain_inputs).iter().all(|&t| t == -1));
    assert!(plain_inputs.len() < inputs.len());
    // A failing practice script fades no frame. Its skin reads nothing in
    // Lua, which the harness would stop on the page's outside frames.
    let declaring = format!("{ENDING}{}", dst("\"i\"", 0, ""));
    let (failing, _, failing_inputs) = pieces(&declaring, Script::closing());
    assert!(failing.passes(), "{}", failing.text(0, true, true));
    assert_eq!(failing.kinds.get("fadeout"), None);
    assert!(fade_timers(&failing_inputs).iter().all(|&t| t == -1));
}

/// The longest declared ending, `finishmargin` and `fadeout` both at
/// `i32::MAX` ms, which the player honours whole, is replayed only to 30 s
/// past the last note: the hold's frames, not faded, as many as a 30 s
/// ending gives, with the round's report saying its ending was cut; a
/// declared ending that fits is not cut.
#[test]
fn the_longest_declared_ending_is_replayed_to_its_cap() {
    let body = |decl: &str| format!("{decl}{}", dst("\"i\"", 0, ""));
    let max = "skin.finishmargin = 2147483647\nskin.fadeout = 2147483647\n";
    let (cut, _, inputs) = pieces(&body(max), Script::full());
    assert!(cut.passes(), "{}", cut.text(0, true, true));
    assert!(
        cut.rounds[0].contains("ending replayed 30 s"),
        "{:?}",
        cut.rounds
    );
    assert_eq!(cut.kinds.get("fadeout"), None);
    assert!(fade_timers(&inputs).iter().all(|&t| t == -1));
    // A 25 s hold and a 5 s fade-out end exactly 30 s past the last note:
    // the same frame times, the last 5 s faded, and nothing cut.
    let fits = "skin.finishmargin = 20000\nskin.fadeout = 5000\n";
    let (whole, _, whole_inputs) = pieces(&body(fits), Script::full());
    assert!(whole.passes(), "{}", whole.text(0, true, true));
    assert!(
        !whole.rounds[0].contains("ending replayed"),
        "{:?}",
        whole.rounds
    );
    assert!(whole.kinds.get("fadeout").is_some_and(|k| k.0 > 0));
    let times = |i: &[FrameInputs]| i.iter().map(FrameInputs::state_us).collect::<Vec<_>>();
    let (cut_times, whole_times) = (times(&inputs), times(&whole_inputs));
    // The fade-out's start adds one frame time of its own.
    assert_eq!(cut_times.len() + 1, whole_times.len());
    assert_eq!(cut_times.last(), whole_times.last());
}

/// A practice round never fades whatever the skin declares: no frame of
/// the rounds script has timer 2 on or is labelled `fadeout`, and its
/// frames are those of the same script with no declaration.
#[test]
fn a_practice_rounds_frames_never_fade() {
    let body = dst("\"i\"", 0, "");
    let (declaring, _, inputs) = pieces(&format!("{ENDING}{body}"), Script::rounds());
    assert!(declaring.passes(), "{}", declaring.text(0, true, true));
    assert_eq!(declaring.kinds.get("fadeout"), None);
    assert!(fade_timers(&inputs).iter().all(|&t| t == -1));
    let (_, _, plain) = pieces(&body, Script::rounds());
    let times = |i: &[FrameInputs]| i.iter().map(FrameInputs::state_us).collect::<Vec<_>>();
    assert_eq!(times(&inputs), times(&plain));
}
