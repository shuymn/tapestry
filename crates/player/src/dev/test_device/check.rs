//! `tapestry test-device check <row> <dir>` (docs/test-device.md
//! "Verdicts"): the contract for the run, set or outer run in `<dir>`,
//! against the row's expectation compiled into this build.

use std::io::Write;
use std::path::Path;

use anyhow::bail;
use test_device::contract::{self, Expectation};

/// A row's name and its expectation.
type Row = (&'static str, fn() -> Expectation);

/// Each row's expectation, by the row's name. The rows arrive with their
/// runner.
const ROWS: &[Row] = &[];

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
    let report = contract::check(&e, dir);
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
    use test_device::contract::{Binding, CheckRule, Exit, ExitRule, RUNNER, Role, SINGLE};
    use test_device::evidence::Outcome;

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
        let e = check("sink", dir.path()).unwrap_err().to_string();
        assert!(e.contains("no row `sink`"), "{e}");
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
        let start = r#"{"t":"start","run":"0123456789abcdef0123456789abcdef","row":"bare","role":"run","build":"0123456789ab","process":{"pid":1,"created":2},"cut":1073741825,"args":[]}"#;
        let end = r#"{"t":"end","exit":0,"errors":[]}"#;
        std::fs::write(dir.path().join("run.jsonl"), format!("{start}\n{end}\n")).unwrap();
        let report = test_device::contract::check(&bare(), dir.path());
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
