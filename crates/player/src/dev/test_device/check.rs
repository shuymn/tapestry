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
];

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
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RUN, "close")],
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
            // The inner's recorder ends with its input or with the inner's
            // job, either way with code 0 (a job's kill-on-close gives 0).
            ExitRule {
                role: SINGLE,
                process: "inner-recorder",
                spawn: Some(Spawned {
                    role: INNER,
                    name: "recorder",
                }),
                exit: Exit::Code(0),
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
            pass("n2.quiet", &[link], &[ev(SINGLE, LINK, "ans")]),
            pass("n2.children-gone", &[run], &[ev(SINGLE, RUN, "exit")]),
        ],
        negative: None,
    }
}

/// The expectation of `row` among `rows`.
fn expectation(rows: &[Row], row: &str) -> Option<Expectation> {
    rows.iter().find(|(name, _)| *name == row).map(|(_, e)| e())
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
}
