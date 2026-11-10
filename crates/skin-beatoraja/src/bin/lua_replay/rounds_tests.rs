//! Rounds of a play state and the frames production builds no play
//! frame for (s7-06e-2-design.md §8): R20 and the e-2 claims. Every
//! fixture uses system and plain images only (selector 0), no judge and
//! no pruning, so none depends on the program's per-kind gates.

use crate::Options;
use crate::inputs::FrameInputs;
use crate::isolation;
use crate::observed::{Observed, Point};
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
    let (_, rec, vm) = h.import_reference(&config, &o.facts, &cancel);
    let vm = vm.unwrap();
    let mut reference = crate::reference::Reference::new(&vm, &rec.items, &rec.sources);
    if let Some((page, point)) = o.page_point {
        reference = reference.page_point(page, point);
    }
    let mut obs = reference.run(&inputs);
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

/// A latch the carry cannot run: its return reads the combo
/// (`lua_callbacks.rs`, `a_program_reading_other_fields_does_not_carry`).
const COMBO_LATCH: &str = "local latch = (function()\n\
       local state = ms.timer_off_value\n\
       return function()\n\
         if state == ms.timer_off_value and ms.number(100) == 200000 then state = ms.time() end\n\
         if ms.number(71) >= 0 then return state end\n\
         return ms.timer_off_value\n\
       end\n\
     end)()\n";

/// R20, C1: the carried round shows the latch round 1 set, from its
/// first READY frame, across its rearm.
#[test]
fn rounds_carry_the_latch() {
    let (r, obs, inputs) = pieces(&latch_pair(), Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(r.compiled.len(), 2);
    assert!(
        r.slots_off.keys().all(|&(round, _)| round == 3),
        "{:?}",
        r.slots_off
    );
    let rounds = rounds_of(&inputs);
    assert_eq!(rounds.len(), 3);
    // Round 1: off until the frame the point reaches 200 000.
    let set = rounds[0]
        .iter()
        .copied()
        .find(|&i| starts(&obs, i).iter().any(Option::is_some))
        .expect("the latch sets in round 1");
    let at = inputs[set].state_us();
    assert!(starts(&obs, set).iter().all(|s| *s == Some(at)));
    // Round 2's first READY frame: both on, round 1's start, agreed.
    let first = rounds[1][0];
    assert_eq!(starts(&obs, first), [Some(at), Some(at)]);
    assert_eq!(r.first_agreed.get(1), Some(&4), "{}", r.text(0, true, true));
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
    assert_eq!(r.buckets.get("outside (loading)"), Some(&240));
    assert_eq!(r.buckets.get("outside (page)"), Some(&960));
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

/// R20, C2, C7: the round after a quit, and every round after an
/// unwitnessed end, refuses the slots; a program that is not carry-safe
/// refuses them on any carried round.
#[test]
fn a_quit_round_refuses_the_slots() {
    // The quit: round 3's invocations of both callbacks, all attributed.
    let (r, obs, inputs) = pieces(&latch_pair(), Script::rounds());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let rounds = rounds_of(&inputs);
    let ran: usize = rounds[2]
        .iter()
        .map(|&i| {
            obs.frames[i]
                .points
                .iter()
                .filter(|p| matches!(p, Point::Ran { .. }))
                .count()
        })
        .sum();
    assert!(ran > 0);
    assert_eq!(
        r.slots_off.iter().collect::<Vec<_>>(),
        [(&(3, "invalid bank"), &ran)]
    );
    // An unwitnessed round 1: the reference latches on the page after it,
    // rounds 2 and 3 are refused.
    let (r, obs, inputs) = pieces(&latch_pair(), Script::rounds_unwitnessed());
    assert!(r.passes(), "{}", r.text(0, true, true));
    let rounds = rounds_of(&inputs);
    assert!(
        rounds[0]
            .iter()
            .all(|&i| starts(&obs, i).iter().all(Option::is_none))
    );
    let page = rounds[0].last().unwrap() + 1;
    assert!(!inputs[page].is_play());
    let at = inputs[page].state_us();
    assert_eq!(starts(&obs, rounds[1][0]), [Some(at), Some(at)]);
    assert_eq!(
        r.slots_off.keys().copied().collect::<Vec<_>>(),
        [(2, "invalid bank"), (3, "invalid bank")]
    );
    // Not carry-safe: round 2 refused for it, round 3 for the bank round
    // 2 could not witness. Its read of the combo stops it on every outside
    // frame, a finding no entry waives.
    let body = format!(
        "{COMBO_LATCH}{}",
        dst("-111", 0, "timer = latch, loop = -1,")
    );
    let (r, _, _) = pieces(&body, Script::rounds());
    assert!(!r.carry_safe);
    assert_eq!(
        r.slots_off.keys().copied().collect::<Vec<_>>(),
        [(2, "not carry-safe"), (3, "invalid bank")]
    );
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

/// R20, C4: a latch written on the page (its point perturbed there) is
/// found on the next round's first READY frame; round 1 stays clean.
#[test]
fn an_outside_frame_write_is_found() {
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
    };
    let body = format!("{LATCH}{}", dst("-111", 0, "timer = latch, loop = -1,"));
    let clean = run_with(&body, &options(script.clone()));
    assert!(clean.passes(), "{}", clean.text(0, true, true));
    let mut o = options(script);
    o.page_point = Some((0, 200_000));
    let (r, obs, inputs) = pieces_with(&body, &o);
    assert!(
        r.findings.iter().all(|f| !f.place.starts_with("round 1")),
        "{}",
        r.text(0, true, true)
    );
    let first = r
        .findings
        .iter()
        .find(|f| f.place.starts_with("round 2"))
        .expect("a finding in round 2");
    assert_eq!(first.place, "round 2 ready");
    let rounds = rounds_of(&inputs);
    assert_eq!(first.frame, rounds[1][0]);
    let page = rounds[0].last().unwrap() + 1;
    assert_eq!(starts(&obs, first.frame), [Some(inputs[page].state_us())]);
    assert!(
        r.findings
            .iter()
            .any(|f| f.frame == first.frame && f.what.starts_with("timer: production (false")),
        "{}",
        r.text(0, true, true)
    );
}

/// The draws of `outside_hosts_answer_only_their_fields`, one destination
/// (and so one line) each: the cover (a setting), the point, the combo, the
/// loading timer through `option(80)`, the play level (its guard).
fn host_reads() -> String {
    [
        "ms.number(14) >= 0",
        "ms.number(100) >= 0",
        "ms.number(71) >= 0",
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
    // The cover and the play level on the page only; the combo and
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
    // A compiled draw reading the combo, made unanswered on play frames
    // too.
    let body = dst(
        "\"i\"",
        0,
        "draw = function() return ms.number(71) / 3 >= 0 end,",
    );
    let mut o = options(Script::rounds());
    o.deny = vec![71];
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
    // The latch reading the combo, with an entry naming it.
    let body = format!(
        "{COMBO_LATCH}{}",
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

/// C2: a quit is its own instant: a round quit exactly at its first
/// note's miss deadline settles with that POOR, so the next page reads
/// the point 0, not the 200 000 round 1 left.
#[test]
fn a_quit_settles_the_judgment_due_at_it() {
    let r1 = Round::on_time(7, true);
    let mut r2 = Round::on_time(7, true);
    r2.presses[0] = None;
    r2.end = crate::script::End::Quit(Script::note_time(0) + 180_000);
    let script = Script::practice("quit-at-a-miss", vec![r1, r2]);
    let (r, _, inputs) = pieces(&latch_pair(), script);
    assert!(r.passes(), "{}", r.text(0, true, true));
    let point = usize::from(skin_model::field::POINT);
    let rounds = rounds_of(&inputs);
    let after = |round: usize| rounds[round].last().unwrap() + 1;
    assert_eq!(inputs[after(0)].fields()[point], 200_000);
    assert_eq!(inputs[after(1)].fields()[point], 0);
}
