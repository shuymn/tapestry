//! `tapestry test-device check <row> <dir>` (docs/test-device.md
//! "Verdicts"): the contract for the run, set or outer run in `<dir>`,
//! against the row's expectation compiled into this build.

use std::io::Write;
use std::path::Path;

use anyhow::bail;
use test_device::contract::{
    self, Binding, CheckRule, Count, Event, Exit, ExitRule, Expectation, File, INNER, LINK,
    Negative, RECORDER, RUN, RUNNER, RecordRule, Role, SINGLE, Spawned,
};
use test_device::evidence::Outcome;

use super::judge::PRODUCER;

/// A row's name and its expectation.
type Row = (&'static str, fn() -> Expectation);

/// Each row's expectation, by the row's name.
const ROWS: &[Row] = &[
    ("sink", sink),
    ("sink-early-close", sink_early_close),
    ("sink-kill", sink_kill),
    ("p1", p1),
    ("p4", p4),
    ("p5", p5),
    ("p1-kill", p1_kill),
    ("p1-kill-escape", p1_kill_escape),
];

/// The private panel judge's name: it writes `verdicts-panel.jsonl`.
pub const PANEL: &str = "panel";

const fn file(role: &'static str, path: &'static str) -> File {
    File { role, path }
}

const fn ev(role: &'static str, stream: &'static str, kind: &'static str) -> Event {
    Event { role, stream, kind }
}

const fn rule(
    role: &'static str,
    stream: &'static str,
    kind: &'static str,
    count: Count,
) -> RecordRule {
    RecordRule {
        role,
        stream,
        kind,
        count,
    }
}

fn check_rule(id: &'static str, outcome: Outcome, inputs: &[File], events: &[Event]) -> CheckRule {
    CheckRule {
        id,
        producer: PRODUCER,
        outcome,
        inputs: inputs.to_vec(),
        events: events.to_vec(),
    }
}

const fn exit_rule(
    role: &'static str,
    process: &'static str,
    spawn: Option<(&'static str, &'static str)>,
    exit: Exit,
) -> ExitRule {
    ExitRule {
        role,
        process,
        spawn: match spawn {
            Some((role, name)) => Some(Spawned { role, name }),
            None => None,
        },
        exit,
    }
}

/// What the runner of a row with a sink writes.
fn sink_records() -> Vec<RecordRule> {
    use Count::{AtLeast, Exactly};
    let role = SINGLE;
    vec![
        rule(role, RUN, "spawn", Exactly(1)),
        rule(role, RUN, "sink", Exactly(1)),
        rule(role, RUN, "call", AtLeast(2)),
        rule(role, RUN, "held", Exactly(1)),
        rule(role, RUN, "stop", Exactly(1)),
        rule(role, RUN, "quiet", Exactly(1)),
        rule(role, RUN, "close", Exactly(1)),
        rule(role, RUN, "exit", Exactly(1)),
        rule(role, RUN, "seal", Exactly(2)),
        rule(role, LINK, "open", Exactly(1)),
        rule(role, LINK, "fence", Exactly(1)),
        rule(role, LINK, "check", AtLeast(1)),
    ]
}

/// N1 (`sink`), and with `failing` its negative's trigger, the one check
/// that fails.
fn n1(row: &'static str, failing: Option<&'static str>) -> Expectation {
    let (run, link, rec) = (
        file(SINGLE, RUN),
        file(SINGLE, LINK),
        file(SINGLE, RECORDER),
    );
    let outcome = |id| {
        if failing == Some(id) {
            Outcome::Fail
        } else {
            Outcome::Pass
        }
    };
    let check = |id, inputs: &[File], events: &[Event]| check_rule(id, outcome(id), inputs, events);
    Expectation {
        row,
        binding: Binding::Run,
        roles: vec![Role { name: SINGLE, row }],
        exits: vec![
            ExitRule {
                role: SINGLE,
                process: RUNNER,
                spawn: None,
                exit: Exit::Code(0),
            },
            ExitRule {
                role: SINGLE,
                process: "recorder",
                spawn: Some(Spawned {
                    role: SINGLE,
                    name: "recorder",
                }),
                exit: Exit::Code(0),
            },
        ],
        records: sink_records(),
        checks: vec![
            check("provenance", &[link], &[ev(SINGLE, LINK, "ans")]),
            check(
                "n1.grants",
                &[link],
                &[ev(SINGLE, LINK, "check"), ev(SINGLE, LINK, "ping")],
            ),
            check(
                "n1.press",
                &[run, rec],
                &[
                    ev(SINGLE, RUN, "held"),
                    ev(SINGLE, RECORDER, "pad"),
                    ev(SINGLE, RECORDER, "key"),
                ],
            ),
            check(
                "n1.release-requested",
                &[run, link],
                &[ev(SINGLE, RUN, "stop"), ev(SINGLE, LINK, "ans")],
            ),
            check(
                "n1.neutral",
                &[run, rec],
                &[ev(SINGLE, RECORDER, "pad"), ev(SINGLE, RECORDER, "key")],
            ),
            check(
                "n1.quiet-before-close",
                &[run, link],
                &[ev(SINGLE, RUN, "close"), ev(SINGLE, LINK, "fence")],
            ),
            check(
                "n1.no-foreign",
                &[run, rec],
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RUN, "quiet")],
            ),
        ],
        negative: failing.map(|trigger| Negative {
            mutation: "the end closes the sink straight after the release request",
            trigger,
        }),
    }
}

fn sink() -> Expectation {
    n1("sink", None)
}

/// N1's negative: exit 0 (the end still reads `quiet 1` after the close),
/// failing exactly `n1.quiet-before-close`.
fn sink_early_close() -> Expectation {
    n1("sink-early-close", Some("n1.quiet-before-close"))
}

/// N2 (`sink-kill`): the outer reserves and kills the inner `sink`.
fn sink_kill() -> Expectation {
    use Count::{AtLeast, Exactly};
    let (run, link, rec) = (
        file(SINGLE, RUN),
        file(SINGLE, LINK),
        file(SINGLE, RECORDER),
    );
    let (inner_run, inner_link) = (file(INNER, RUN), file(INNER, LINK));
    let pass =
        |id, inputs: &[File], events: &[Event]| check_rule(id, Outcome::Pass, inputs, events);
    let records = vec![
        rule(SINGLE, RUN, "spawn", Exactly(2)),
        rule(SINGLE, RUN, "exit", Exactly(3)),
        rule(SINGLE, RUN, "seal", Exactly(4)),
        rule(SINGLE, LINK, "open", Exactly(1)),
        rule(INNER, RUN, "held", Exactly(1)),
        rule(INNER, RUN, "spawn", Exactly(1)),
        rule(INNER, LINK, "open", Exactly(1)),
        rule(INNER, LINK, "check", AtLeast(1)),
        rule(SINGLE, RUN, "quiet", Exactly(1)),
    ];
    Expectation {
        row: "sink-kill",
        binding: Binding::Run,
        roles: vec![
            Role {
                name: SINGLE,
                row: "sink-kill",
            },
            Role {
                name: INNER,
                row: "sink",
            },
        ],
        exits: vec![
            ExitRule {
                role: SINGLE,
                process: RUNNER,
                spawn: None,
                exit: Exit::Code(0),
            },
            ExitRule {
                role: SINGLE,
                process: "recorder",
                spawn: Some(Spawned {
                    role: SINGLE,
                    name: "recorder",
                }),
                exit: Exit::Code(0),
            },
            ExitRule {
                role: INNER,
                process: RUNNER,
                spawn: None,
                exit: Exit::Killed,
            },
            // The outer's record of that kill.
            ExitRule {
                role: SINGLE,
                process: "inner",
                spawn: Some(Spawned {
                    role: SINGLE,
                    name: INNER,
                }),
                exit: Exit::Killed,
            },
            // The inner's recorder ends with the inner's job, which the
            // outer terminates with its cut code.
            ExitRule {
                role: SINGLE,
                process: "inner-recorder",
                spawn: Some(Spawned {
                    role: INNER,
                    name: "recorder",
                }),
                exit: Exit::Killed,
            },
        ],
        records,
        checks: vec![
            pass(
                "provenance",
                &[link, inner_link],
                &[ev(SINGLE, LINK, "ans"), ev(INNER, LINK, "ans")],
            ),
            pass(
                "n2.abnormal",
                &[inner_run, inner_link],
                &[ev(INNER, RUN, "held")],
            ),
            pass(
                "n2.neutral",
                &[rec],
                &[ev(SINGLE, RECORDER, "pad"), ev(SINGLE, RECORDER, "key")],
            ),
            pass(
                "n2.quiet",
                &[run, link, inner_link],
                &[
                    ev(SINGLE, RUN, "kill"),
                    ev(SINGLE, LINK, "ans"),
                    ev(INNER, LINK, "ans"),
                ],
            ),
            pass(
                "n2.no-foreign",
                &[run, rec, inner_run],
                &[ev(INNER, RUN, "call"), ev(SINGLE, RUN, "exit")],
            ),
            pass("n2.children-gone", &[run], &[ev(SINGLE, RUN, "exit")]),
        ],
        negative: None,
    }
}

/// A panel row (P1, and its negatives P4 and P5): `failing` the checks
/// that must fail, and the negative's mutation and trigger.
fn panel(row: &'static str, failing: &[&str], negative: Option<Negative>) -> Expectation {
    use super::launch::{DEVICE_AFTER, DEVICE_BEFORE, PLAYER_AFTER, PLAYER_BEFORE};
    use super::run::{MODE_LINK, SHOT_A, SHOT_B};
    use Count::{AtLeast, Exactly};
    let f = |path| file(SINGLE, path);
    let (run, link, rec) = (f(RUN), f(LINK), f(RECORDER));
    let outcome = |id: &str| {
        if failing.contains(&id) {
            Outcome::Fail
        } else {
            Outcome::Pass
        }
    };
    let judge = |id, inputs: &[File], events: &[Event]| check_rule(id, outcome(id), inputs, events);
    let private = |id, inputs: &[File]| CheckRule {
        id,
        producer: PANEL,
        outcome: outcome(id),
        inputs: inputs.to_vec(),
        events: vec![ev(SINGLE, RUN, "shot")],
    };
    let role = SINGLE;
    Expectation {
        row,
        binding: Binding::Run,
        roles: vec![Role { name: SINGLE, row }],
        exits: vec![
            exit_rule(SINGLE, RUNNER, None, Exit::Code(0)),
            exit_rule(
                SINGLE,
                "recorder",
                Some((SINGLE, "recorder")),
                Exit::Code(0),
            ),
            exit_rule(SINGLE, "player", Some((SINGLE, "player")), Exit::Code(0)),
        ],
        records: vec![
            rule(role, RUN, "spawn", Exactly(2)),
            rule(role, RUN, "mode", Exactly(1)),
            rule(role, RUN, "player", Exactly(1)),
            rule(role, RUN, "sink", Exactly(1)),
            rule(role, RUN, "window", Exactly(1)),
            rule(role, RUN, "call", AtLeast(9)),
            rule(role, RUN, "held", Exactly(1)),
            rule(role, RUN, "shot", Exactly(2)),
            rule(role, RUN, "barrier", Exactly(0)),
            rule(role, RUN, "stop", Exactly(1)),
            rule(role, RUN, "quiet", Exactly(1)),
            rule(role, RUN, "close", Exactly(1)),
            rule(role, RUN, "kill", Exactly(0)),
            rule(role, RUN, "exit", Exactly(2)),
            // The link, the recorder, the mode link, the settings before
            // and after, the player's output, the two shots.
            rule(role, RUN, "seal", Exactly(11)),
            rule(role, LINK, "open", Exactly(1)),
            rule(role, LINK, "fence", Exactly(1)),
            rule(role, LINK, "check", AtLeast(1)),
        ],
        checks: vec![
            judge(
                "provenance",
                &[link, f(MODE_LINK)],
                &[ev(SINGLE, LINK, "ans")],
            ),
            judge(
                "p1.grants",
                &[link],
                &[ev(SINGLE, LINK, "check"), ev(SINGLE, LINK, "ping")],
            ),
            judge("p1.volumes", &[f(DEVICE_AFTER)], &[ev(SINGLE, RUN, "exit")]),
            judge(
                "p1.device-toml",
                &[f(DEVICE_BEFORE), f(DEVICE_AFTER)],
                &[ev(SINGLE, RUN, "exit")],
            ),
            judge(
                "p1.player-toml",
                &[f(PLAYER_BEFORE), f(PLAYER_AFTER)],
                &[ev(SINGLE, RUN, "exit")],
            ),
            judge(
                "p1.panel-keys",
                &[run, rec],
                &[ev(SINGLE, RECORDER, "pad"), ev(SINGLE, RUN, "quiet")],
            ),
            judge(
                "p1.clicks",
                &[run, rec],
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RECORDER, "mouse")],
            ),
            judge(
                "p1.no-foreign",
                &[run, rec],
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RUN, "quiet")],
            ),
            judge(
                "p1.buttons-released",
                &[run, rec],
                &[ev(SINGLE, RECORDER, "pad")],
            ),
            judge(
                "p1.release-requested",
                &[run, link],
                &[ev(SINGLE, RUN, "stop"), ev(SINGLE, LINK, "ans")],
            ),
            judge(
                "p1.quiet-before-close",
                &[run, link],
                &[ev(SINGLE, RUN, "close"), ev(SINGLE, LINK, "fence")],
            ),
            private("panel.drawn-a", &[f(SHOT_A)]),
            private("panel.knobs", &[f(SHOT_A)]),
            private("panel.graphs", &[f(SHOT_A)]),
            private("panel.numbers", &[f(SHOT_A)]),
            private("panel.closed-b", &[f(SHOT_B)]),
        ],
        negative,
    }
}

/// P1: panel 3 by START and SELECT held on the device's pad, the volumes set
/// by clicks.
fn p1() -> Expectation {
    panel("p1", &[], None)
}

/// P4: START alone opens panel 1, so nothing of panel 3 is drawn and the
/// clicks set nothing.
fn p4() -> Expectation {
    panel(
        "p4",
        &[
            "p1.volumes",
            "panel.drawn-a",
            "panel.knobs",
            "panel.graphs",
            "panel.numbers",
        ],
        Some(Negative {
            mutation: "SELECT is never held: START alone opens panel 1",
            trigger: "panel.drawn-a",
        }),
    )
}

/// P5: P1's steps against a local player whose volume arm drops the value:
/// panel 3 is drawn, the volumes stay at 100 %.
fn p5() -> Expectation {
    panel(
        "p5",
        &["p1.volumes", "panel.knobs", "panel.graphs", "panel.numbers"],
        Some(Negative {
            mutation: "the player's pointer volume arm drops the value (a local build)",
            trigger: "p1.volumes",
        }),
    )
}

/// N2p (`p1-kill`), and with `escape` its negative: the outer reserves and
/// kills an inner `p1` at its kill barrier. The negative's runner ends the
/// inner's process alone, not its job: the inner's player outlives it (the
/// outer then ends it), and the inner's recorder ends by itself at its
/// input's end, so both of those checks fail. A player can leave the job
/// the outer ends in no other way: it is in the inner's job from its
/// creation, and the job refuses breakaway.
fn n2p(row: &'static str, escape: bool) -> Expectation {
    use Count::{AtLeast, Exactly};
    let (run, link, rec) = (
        file(SINGLE, RUN),
        file(SINGLE, LINK),
        file(SINGLE, RECORDER),
    );
    let (inner_run, inner_link) = (file(INNER, RUN), file(INNER, LINK));
    let check = |id, inputs: &[File], events: &[Event]| {
        let outcome = if escape && ["n2p.player-gone", "n2p.children-gone"].contains(&id) {
            Outcome::Fail
        } else {
            Outcome::Pass
        };
        check_rule(id, outcome, inputs, events)
    };
    Expectation {
        row,
        binding: Binding::Run,
        roles: vec![
            Role { name: SINGLE, row },
            Role {
                name: INNER,
                row: "p1",
            },
        ],
        exits: vec![
            exit_rule(SINGLE, RUNNER, None, Exit::Code(0)),
            exit_rule(
                SINGLE,
                "recorder",
                Some((SINGLE, "recorder")),
                Exit::Code(0),
            ),
            exit_rule(INNER, RUNNER, None, Exit::Killed),
            exit_rule(SINGLE, "inner", Some((SINGLE, INNER)), Exit::Killed),
            exit_rule(
                SINGLE,
                "inner-recorder",
                Some((INNER, "recorder")),
                if escape { Exit::Code(0) } else { Exit::Killed },
            ),
            exit_rule(
                SINGLE,
                "inner-player",
                Some((INNER, "player")),
                Exit::Killed,
            ),
        ],
        records: vec![
            rule(SINGLE, RUN, "spawn", Exactly(2)),
            rule(SINGLE, RUN, "kill", Exactly(if escape { 2 } else { 1 })),
            rule(SINGLE, RUN, "exit", Exactly(4)),
            rule(SINGLE, RUN, "seal", Exactly(4)),
            rule(SINGLE, RUN, "quiet", Exactly(1)),
            rule(SINGLE, LINK, "open", Exactly(1)),
            rule(INNER, RUN, "held", Exactly(1)),
            rule(INNER, RUN, "barrier", Exactly(1)),
            rule(INNER, RUN, "spawn", Exactly(2)),
            rule(INNER, RUN, "stop", Exactly(0)),
            rule(INNER, LINK, "open", Exactly(1)),
            rule(INNER, LINK, "check", AtLeast(1)),
        ],
        checks: vec![
            check(
                "provenance",
                &[link, inner_link],
                &[ev(SINGLE, LINK, "ans"), ev(INNER, LINK, "ans")],
            ),
            check(
                "n2p.abnormal",
                &[inner_run, inner_link],
                &[ev(INNER, RUN, "barrier")],
            ),
            check("n2p.neutral", &[rec], &[ev(SINGLE, RECORDER, "pad")]),
            check(
                "n2p.quiet",
                &[run, link, inner_link],
                &[
                    ev(SINGLE, RUN, "kill"),
                    ev(SINGLE, LINK, "ans"),
                    ev(INNER, LINK, "ans"),
                ],
            ),
            check(
                "n2p.no-foreign",
                &[run, rec, inner_run],
                &[ev(INNER, RUN, "call"), ev(SINGLE, RUN, "exit")],
            ),
            check("n2p.children-gone", &[run], &[ev(SINGLE, RUN, "exit")]),
            check(
                "n2p.player-gone",
                &[run],
                &[ev(SINGLE, RUN, "kill"), ev(SINGLE, RUN, "exit")],
            ),
        ],
        negative: escape.then_some(Negative {
            mutation: "the outer ends the inner's process alone, not its job (a local build)",
            trigger: "n2p.player-gone",
        }),
    }
}

fn p1_kill() -> Expectation {
    n2p("p1-kill", false)
}

fn p1_kill_escape() -> Expectation {
    n2p("p1-kill-escape", true)
}

/// The expectation of `row` among `rows`.
fn expectation(rows: &[Row], row: &str) -> Option<Expectation> {
    rows.iter().find(|(name, _)| *name == row).map(|(_, e)| e())
}

/// The expectation compiled into this build for `row`.
pub fn expectation_of(row: &str) -> Option<Expectation> {
    expectation(ROWS, row)
}

/// Prints every note and mismatch, then `CONTRACT <row> ok` or
/// `CONTRACT <row> mismatch <n>`, on standard output only.
///
/// # Errors
///
/// An unknown row, or any mismatch.
pub fn check(row: &str, dir: &Path) -> anyhow::Result<()> {
    run(ROWS, row, dir, &mut std::io::stdout().lock())
}

/// As [`check`], printing to `out`.
fn run(rows: &[Row], row: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(e) = expectation(rows, row) else {
        bail!("no row `{row}`: this build compiles no expectation for it");
    };
    let report = contract::check(&e, dir, super::COMMIT);
    for n in &report.notes {
        writeln!(out, "note: {n}")?;
    }
    for m in &report.mismatches {
        writeln!(out, "{m}")?;
    }
    writeln!(out, "{}", report.last_line(row))?;
    if report.ok() {
        Ok(())
    } else {
        bail!("the run does not match row `{row}`'s expectation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare() -> Expectation {
        Expectation {
            row: "bare",
            binding: Binding::Run,
            roles: vec![Role {
                name: SINGLE,
                row: "bare",
            }],
            exits: vec![ExitRule {
                role: SINGLE,
                process: RUNNER,
                spawn: None,
                exit: Exit::Code(0),
            }],
            records: vec![],
            checks: vec![CheckRule {
                id: "provenance",
                producer: "judge",
                outcome: Outcome::Incomplete,
                inputs: vec![],
                events: vec![],
            }],
            negative: None,
        }
    }

    #[test]
    fn an_unknown_row_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let e = check("p9", dir.path()).unwrap_err().to_string();
        assert!(e.contains("no row `p9`"), "{e}");
    }

    /// Every row's expectation is usable, and the negative fails exactly
    /// its trigger.
    #[test]
    fn every_row_s_expectation_is_usable() {
        for (name, e) in ROWS {
            let e = e();
            assert_eq!(e.row, *name);
            assert!(e.problems().is_empty(), "{name}: {:?}", e.problems());
        }
        // Each negative fails exactly its declared set, and its checks are
        // its positive row's, in the same order.
        let ids = |e: &Expectation| e.checks.iter().map(|c| c.id).collect::<Vec<_>>();
        let failing = |e: &Expectation| {
            e.checks
                .iter()
                .filter(|c| c.outcome == Outcome::Fail)
                .map(|c| c.id)
                .collect::<Vec<_>>()
        };
        assert!(failing(&p1()).is_empty());
        assert_eq!(
            failing(&p4()),
            [
                "p1.volumes",
                "panel.drawn-a",
                "panel.knobs",
                "panel.graphs",
                "panel.numbers"
            ]
        );
        assert_eq!(
            failing(&p5()),
            ["p1.volumes", "panel.knobs", "panel.graphs", "panel.numbers"]
        );
        assert_eq!(
            failing(&p1_kill_escape()),
            ["n2p.children-gone", "n2p.player-gone"]
        );
        assert!(failing(&p1_kill()).is_empty());
        for (neg, pos) in [(p4(), p1()), (p5(), p1()), (p1_kill_escape(), p1_kill())] {
            assert_eq!(ids(&neg), ids(&pos));
        }
        let neg = sink_early_close();
        let failing: Vec<&str> = neg
            .checks
            .iter()
            .filter(|c| c.outcome == Outcome::Fail)
            .map(|c| c.id)
            .collect();
        assert_eq!(failing, ["n1.quiet-before-close"]);
        // The negative's checks are the row's, in the same order.
        let ids = |e: Expectation| e.checks.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(neg), ids(sink()));
    }

    /// Writes an N1 run folder as the runner writes it, from the judge's
    /// good N1 records in their full form, its link and recorder sealed;
    /// with `early`, the sink closed before the release evidence, as N1's
    /// negative would. Then the judge's verdicts.
    fn n1_folder(dir: &std::path::Path, row: &str, early: bool) {
        n1_files(dir, row, early, false);
        super::super::judge::judge(row, dir).unwrap();
    }

    /// As [`n1_folder`] without the verdicts; with `torn`, the recorder's
    /// last line (its `end`) is cut short though it exited 0, and sealed so.
    fn n1_files(dir: &std::path::Path, row: &str, early: bool, torn: bool) {
        use serde_json::{Value, json};
        let run_id = "0123456789abcdef0123456789abcdef";
        let me = json!({"pid": 1, "created": 2});
        let rec = json!({"pid": 7, "created": 70});
        let f = super::super::judge::tests::good();
        let mut run = f.run.records.unwrap();
        let mut link = f.link.records.unwrap();
        let mut recorder = f.recorder.records.unwrap();
        run[0] = json!({
            "t": "start", "run": run_id, "row": row, "role": SINGLE, "build": "0123456789ab",
            "process": me, "commit": super::super::COMMIT, "tag": 0xABCD, "cut": 0x4000_0123,
            "args": [], "set": null, "inner": null,
        });
        run.insert(
            1,
            json!({"t": "spawn", "q": 40, "name": "recorder", "process": rec}),
        );
        let end = run.pop().unwrap();
        if early {
            let close = run.iter().position(|v| v["t"] == "close").unwrap();
            let quiet = run.iter().position(|v| v["t"] == "quiet").unwrap();
            let c = run.remove(close);
            run.insert(quiet, c);
        }
        run.push(json!({"t": "exit", "q": 6100, "name": "recorder", "process": rec, "code": 0}));
        link[0] = json!({
            "t": "open", "q": 10, "run": run_id, "process": me, "freq": 10_000_000,
            "location": "L", "com": "COM9", "path": "p",
        });
        link.push(json!({"t": "end", "q": 5700, "problems": []}));
        recorder[0] = json!({
            "t": "ready", "q": 50, "run": run_id, "process": rec, "window": 9,
            "location": "L", "keyboard": 1, "pad": 2,
        });
        let write = |name: &str, records: &[Value], torn: bool| {
            let mut text: String = records.iter().map(|v| format!("{v}\n")).collect();
            if torn {
                text.truncate(text.len() - 3);
            }
            std::fs::write(dir.join(name), text).unwrap();
        };
        write(LINK, &link, false);
        write(RECORDER, &recorder, torn);
        for name in [LINK, RECORDER] {
            run.push(super::super::owner::seal(dir, &dir.join(name), run_id, SINGLE).unwrap());
        }
        run.push(end);
        write(RUN, &run, false);
    }

    /// The judge judges only streams the checker takes: a recorder that
    /// exited 0 with its `end` cut short is no evidence, so every check
    /// reading it is `incomplete` rather than judged on the prefix; the
    /// whole stream (the control) passes them.
    #[test]
    fn the_judge_reads_only_streams_the_checker_takes() {
        let outcomes = |torn: bool| {
            let dir = tempfile::tempdir().unwrap();
            n1_files(dir.path(), "sink", false, torn);
            super::super::judge::judge("sink", dir.path()).unwrap();
            let text = std::fs::read_to_string(dir.path().join("verdicts-judge.jsonl")).unwrap();
            text.lines()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .filter(|v| {
                    v["inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|i| i["path"] == RECORDER)
                })
                .map(|v| v["outcome"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let whole = outcomes(false);
        assert!(
            !whole.is_empty() && whole.iter().all(|o| o == "pass"),
            "{whole:?}"
        );
        let torn = outcomes(true);
        assert_eq!(torn.len(), whole.len());
        assert!(torn.iter().all(|o| o == "incomplete"), "{torn:?}");
    }

    /// N1's negative is judged and checked on a run folder, never run: the
    /// early close fails exactly `n1.quiet-before-close`, which matches the
    /// negative's expectation and not N1's; the run without it matches
    /// N1's (the positive control).
    #[test]
    fn the_early_close_negative_is_checked_on_its_records() {
        let verdict = |row: &str, early: bool| {
            let dir = tempfile::tempdir().unwrap();
            n1_folder(dir.path(), row, early);
            let mut out = Vec::new();
            let r = run(ROWS, row, dir.path(), &mut out);
            (r.is_ok(), String::from_utf8(out).unwrap())
        };
        let (ok, out) = verdict("sink", false);
        assert!(ok, "{out}");
        let (ok, out) = verdict("sink-early-close", true);
        assert!(ok, "{out}");
        assert!(out.ends_with("CONTRACT sink-early-close ok\n"), "{out}");
        let (ok, out) = verdict("sink", true);
        assert!(!ok, "{out}");
        assert!(out.contains("n1.quiet-before-close"), "{out}");
        let (ok, out) = verdict("sink-early-close", false);
        assert!(!ok, "{out}");
    }

    #[test]
    fn the_last_line_names_the_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let rows: &[Row] = &[("bare", bare)];
        // An empty folder: no run.jsonl.
        let mut out = Vec::new();
        let e = run(rows, "bare", dir.path(), &mut out)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "the run does not match row `bare`'s expectation");
        let printed = String::from_utf8(out).unwrap();
        // The last line is printed once, last; the error does not repeat it.
        assert_eq!(printed.matches("CONTRACT").count(), 1, "{printed}");
        assert!(printed.ends_with("CONTRACT bare mismatch 1\n"), "{printed}");
        assert!(!e.contains("CONTRACT"), "{e}");
        let start = format!(
            r#"{{"t":"start","run":"0123456789abcdef0123456789abcdef","row":"bare","role":"run","build":"0123456789ab","process":{{"pid":1,"created":2}},"commit":"{}","tag":7,"cut":1073741825,"args":[]}}"#,
            super::super::COMMIT
        );
        let end = r#"{"t":"end","exit":0,"errors":[]}"#;
        std::fs::write(dir.path().join("run.jsonl"), format!("{start}\n{end}\n")).unwrap();
        let report = test_device::contract::check(&bare(), dir.path(), super::super::COMMIT);
        assert_eq!(
            report.last_line("bare"),
            "CONTRACT bare mismatch 1",
            "the provenance check is missing: {report:?}"
        );
        let verdict = r#"{"row":"bare","run":"0123456789abcdef0123456789abcdef","check":"provenance","outcome":"incomplete","detail":"no hello recorded","inputs":[],"producer":{"name":"judge","version":"0123456789ab"}}"#;
        std::fs::write(
            dir.path().join("verdicts-judge.jsonl"),
            format!("{verdict}\n"),
        )
        .unwrap();
        let mut out = Vec::new();
        run(rows, "bare", dir.path(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "CONTRACT bare ok\n");
    }

    /// Writes a panel row's run folder as the runner writes it, from the
    /// judge's good panel records in their full form, every file sealed;
    /// with `volumes_left`, the player kept the volumes at 1.0 (as P5's
    /// player does). Then the judge's verdicts, and the panel judge's
    /// verdict lines with `failing` failed.
    fn panel_folder(dir: &std::path::Path, row: &str, volumes_left: bool, failing: &[&str]) {
        use serde_json::{Value, json};

        use super::super::launch::{
            DEVICE_AFTER, DEVICE_BEFORE, PLAYER_AFTER, PLAYER_BEFORE, STDERR, STDOUT,
        };
        use super::super::run::{MODE_LINK, SHOT_A, SHOT_B};
        let run_id = "0123456789abcdef0123456789abcdef";
        let me = json!({"pid": 1, "created": 2});
        let rec = json!({"pid": 7, "created": 70});
        let player = json!({"pid": 8, "created": 80});
        let (f, x) = super::super::judge::tests::panel_good();
        let mut run = f.run.records.unwrap();
        let mut link = f.link.records.unwrap();
        let mut recorder = f.recorder.records.unwrap();
        run[0] = json!({
            "t": "start", "run": run_id, "row": row, "role": SINGLE, "build": "0123456789ab",
            "process": me, "commit": super::super::COMMIT, "tag": 0xABCD, "cut": 0x4000_0123,
            "args": [], "set": null, "inner": null,
        });
        let end = run.pop().unwrap();
        let hello = recs_line(&x.mode);
        run.splice(
            1..1,
            [
                json!({"t": "mode", "q": 20, "line": hello}),
                json!({"t": "spawn", "q": 40, "name": "recorder", "process": rec}),
                json!({"t": "player", "q": 45, "path": "t.exe", "sha256": "00", "commit": "c", "modified": 1}),
            ],
        );
        let at = run.iter().position(|v| v["t"] == "call").unwrap();
        run.splice(
            at + 2..at + 2,
            [
                json!({"t": "spawn", "q": 215, "name": "player", "process": player}),
                json!({"t": "window", "q": 216, "window": 99}),
            ],
        );
        run.push(json!({"t": "exit", "q": 6100, "name": "recorder", "process": rec, "code": 0}));
        run.push(json!({"t": "exit", "q": 6200, "name": "player", "process": player, "code": 0}));
        link[0] = json!({
            "t": "open", "q": 10, "run": run_id, "process": me, "freq": 10_000_000,
            "location": "L", "com": "COM9", "path": "p",
        });
        link.push(json!({"t": "end", "q": 5700, "problems": []}));
        recorder[0] = json!({
            "t": "ready", "q": 50, "run": run_id, "process": rec, "window": 9,
            "location": "L", "keyboard": 1, "pad": 2,
        });
        let lines =
            |records: &[Value]| -> String { records.iter().map(|v| format!("{v}\n")).collect() };
        let mode = [
            json!({
                "t": "open", "q": 5, "run": run_id, "process": me, "freq": 10_000_000,
                "location": "L", "com": "COM9", "path": "p",
            }),
            json!({"t": "cmd", "q": 6, "line": "hello"}),
            json!({"t": "ans", "q": 7, "sent": 6, "cmd": "hello", "line": hello}),
            json!({"t": "end", "q": 8, "problems": []}),
        ];
        let mut device_after = x.device_after.unwrap();
        if volumes_left {
            device_after = x.device_before.clone().unwrap();
        }
        let files: Vec<(&str, Vec<u8>)> = vec![
            (LINK, lines(&link).into_bytes()),
            (RECORDER, lines(&recorder).into_bytes()),
            (MODE_LINK, lines(&mode).into_bytes()),
            (DEVICE_BEFORE, x.device_before.unwrap()),
            (PLAYER_BEFORE, x.player_before.unwrap()),
            (DEVICE_AFTER, device_after),
            (PLAYER_AFTER, x.player_after.unwrap()),
            (STDOUT, Vec::new()),
            (STDERR, Vec::new()),
            (SHOT_A, b"a".to_vec()),
            (SHOT_B, b"b".to_vec()),
        ];
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
            run.push(super::super::owner::seal(dir, &dir.join(name), run_id, SINGLE).unwrap());
        }
        run.push(end);
        std::fs::write(dir.join(RUN), lines(&run)).unwrap();
        super::super::judge::judge(row, dir).unwrap();
        let sha = |p: &str| super::super::owner::digest(&dir.join(p)).unwrap();
        let panel: String = [
            ("panel.drawn-a", SHOT_A),
            ("panel.knobs", SHOT_A),
            ("panel.graphs", SHOT_A),
            ("panel.numbers", SHOT_A),
            ("panel.closed-b", SHOT_B),
        ]
        .iter()
        .map(|(check, path)| {
            let outcome = if failing.contains(check) {
                "fail"
            } else {
                "pass"
            };
            format!(
                "{}\n",
                json!({
                    "row": row, "run": run_id, "check": check, "outcome": outcome, "detail": "",
                    "inputs": [{"role": SINGLE, "path": path, "sha256": sha(path)}],
                    "producer": {"name": PANEL, "version": "0"},
                })
            )
        })
        .collect();
        std::fs::write(dir.join("verdicts-panel.jsonl"), panel).unwrap();
    }

    fn recs_line(s: &super::super::judge::Stream) -> String {
        s.records.as_ref().unwrap()[0]["line"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A panel run folder matches its row's expectation through the
    /// checker: a good P1 run matches `p1`; P5's (the volumes left, the
    /// knobs, graphs and numbers failed) matches `p5` and not `p1`; and P1's
    /// matches no `p5`.
    #[test]
    fn a_panel_run_folder_matches_its_row() {
        let verdict = |row: &str, left: bool, failing: &[&str]| {
            let dir = tempfile::tempdir().unwrap();
            panel_folder(dir.path(), row, left, failing);
            let mut out = Vec::new();
            let r = run(ROWS, row, dir.path(), &mut out);
            (r.is_ok(), String::from_utf8(out).unwrap())
        };
        let (ok, out) = verdict("p1", false, &[]);
        assert!(ok, "{out}");
        let p5 = ["panel.knobs", "panel.graphs", "panel.numbers"];
        let (ok, out) = verdict("p5", true, &p5);
        assert!(ok, "{out}");
        let (ok, out) = verdict("p1", true, &p5);
        assert!(!ok, "{out}");
        assert!(out.contains("p1.volumes"), "{out}");
        let (ok, out) = verdict("p5", false, &[]);
        assert!(!ok, "{out}");
    }
}
