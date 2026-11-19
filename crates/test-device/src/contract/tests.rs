//! The checker against run folders written in a temporary folder: a
//! matching single run, an outer run with its killed inner, and a set, each
//! then broken in one way.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

const BUILD: &str = "0123456789ab";
const RUN_A: &str = "0123456789abcdef0123456789abcdef";
const RUN_B: &str = "fedcba9876543210fedcba9876543210";
const RUN_C: &str = "00112233445566778899aabbccddeeff";
const HELLO: &str = "hello 2 build 0123456789ab mode 24 lease - armed 0 reason none boot forced count 0 \
                     identified 1 faults 0 quiet 1 session 0123456789abcdef00000002";

fn line(v: &Value) -> String {
    format!("{v}\n")
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// One run's folder: its files (each sealed), its children's exits, and
/// its end (none when it is killed).
struct RunSpec {
    run: &'static str,
    row: &'static str,
    role: &'static str,
    process: (u32, u64),
    set: Option<&'static str>,
    inner: Option<Value>,
    files: Vec<(&'static str, Vec<u8>)>,
    exits: Vec<Value>,
    end: Option<Value>,
}

impl RunSpec {
    fn new(run: &'static str, row: &'static str, role: &'static str) -> Self {
        Self {
            run,
            row,
            role,
            process: (100, 1000),
            set: None,
            inner: None,
            files: vec![
                ("link.jsonl", link()),
                ("recorder.jsonl", recorder(200, &[])),
                ("shot-a.png", format!("png of {run}").into_bytes()),
            ],
            exits: vec![exit("recorder", (200, 2000), 0, false)],
            end: Some(json!({"t": "end", "exit": 0, "errors": []})),
        }
    }

    fn write(&self, folder: &Path) {
        let mut out = line(&json!({
            "t": "start", "run": self.run, "row": self.row, "role": self.role, "build": BUILD,
            "process": {"pid": self.process.0, "created": self.process.1},
            "args": ["--build", BUILD], "set": self.set, "inner": self.inner,
        }));
        for (path, bytes) in &self.files {
            write(&folder.join(path), bytes);
            out += &line(&json!({
                "t": "seal", "run": self.run, "role": self.role, "path": path, "sha256": sha(bytes),
            }));
        }
        for x in &self.exits {
            out += &line(x);
        }
        if let Some(end) = &self.end {
            out += &line(end);
        }
        write(&folder.join(RUN), out.as_bytes());
    }
}

fn link() -> Vec<u8> {
    [
        json!({"t": "open", "q": 1, "freq": 10_000_000, "location": "L", "com": "COM4", "path": "P"}),
        json!({"t": "ans", "q": 2, "line": HELLO}),
        json!({"t": "end", "q": 3, "problems": []}),
    ]
    .iter()
    .map(line)
    .collect::<String>()
    .into_bytes()
}

fn recorder(pid: u32, errors: &[(i64, i64)]) -> Vec<u8> {
    let mut out = line(
        &json!({"t": "ready", "q": 1, "process": {"pid": pid, "created": u64::from(pid) * 10}}),
    );
    for (from, to) in errors {
        out +=
            &line(&json!({"t": "error", "from": from, "to": to, "what": "GetRawInputData failed"}));
    }
    out += &line(&json!({"t": "end", "q": 9}));
    out.into_bytes()
}

fn exit(name: &str, (pid, created): (u32, u64), code: i32, killed: bool) -> Value {
    json!({"t": "exit", "name": name, "process": {"pid": pid, "created": created}, "code": code, "killed": killed})
}

/// A verdict line reading `inputs` (role, path, file) with the files'
/// current digests.
fn verdict(
    row: &str,
    check: &str,
    outcome: &str,
    producer: &str,
    inputs: &[(&str, &str, &Path)],
) -> String {
    let inputs: Vec<Value> = inputs
        .iter()
        .map(|(role, path, file)| json!({"role": role, "path": path, "sha256": sha(&fs::read(file).unwrap())}))
        .collect();
    line(&json!({
        "row": row, "check": check, "outcome": outcome, "detail": "",
        "inputs": inputs, "producer": {"name": producer, "version": BUILD},
    }))
}

fn file(role: &'static str, path: &'static str) -> File {
    File { role, path }
}

fn rule(
    id: &'static str,
    producer: &'static str,
    outcome: Outcome,
    inputs: Vec<File>,
) -> CheckRule {
    CheckRule {
        id,
        producer,
        outcome,
        inputs,
        events: vec![],
    }
}

/// A single run of row `sink`: three checks by `judge`.
struct Single {
    dir: tempfile::TempDir,
    spec: RunSpec,
}

impl Single {
    fn expectation() -> Expectation {
        Expectation {
            row: "sink",
            binding: Binding::Run,
            roles: vec![Role {
                name: SINGLE,
                row: "sink",
            }],
            exits: vec![
                ExitRule {
                    role: SINGLE,
                    process: RUNNER,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "recorder",
                    exit: Exit::Code(0),
                },
            ],
            records: vec![RecordRule {
                role: SINGLE,
                stream: LINK,
                kind: "ans",
                count: Count::AtLeast(1),
            }],
            checks: vec![
                rule(
                    "provenance",
                    "judge",
                    Outcome::Pass,
                    vec![file(SINGLE, LINK)],
                ),
                CheckRule {
                    events: vec![Event {
                        role: SINGLE,
                        stream: RECORDER,
                        kind: "ready",
                    }],
                    ..rule(
                        "n1.neutral",
                        "judge",
                        Outcome::Pass,
                        vec![file(SINGLE, RECORDER)],
                    )
                },
                rule(
                    "n1.shot",
                    "judge",
                    Outcome::Pass,
                    vec![file(SINGLE, "shot-a.png")],
                ),
            ],
            negative: None,
        }
    }

    fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
            spec: RunSpec::new(RUN_A, "sink", SINGLE),
        };
        s.write();
        s
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Writes the run and the verdicts, each `(check, outcome)`, reading
    /// their declared files.
    fn write_with(&self, verdicts: &[(&str, &str)]) {
        self.spec.write(self.dir.path());
        let mut out = String::new();
        for (check, outcome) in verdicts {
            let path = match *check {
                "provenance" => LINK,
                "n1.neutral" => RECORDER,
                _ => "shot-a.png",
            };
            out += &verdict(
                "sink",
                check,
                outcome,
                "judge",
                &[(SINGLE, path, &self.path(path))],
            );
        }
        write(&self.path("verdicts-judge.jsonl"), out.as_bytes());
    }

    fn write(&self) {
        self.write_with(&[
            ("provenance", "pass"),
            ("n1.neutral", "pass"),
            ("n1.shot", "pass"),
        ]);
    }

    fn check(&self) -> Report {
        check(&Self::expectation(), self.dir.path())
    }
}

/// One mismatch containing `needle`, else a panic naming the report.
#[track_caller]
fn mismatch(report: &Report, needle: &str) {
    assert!(
        report.mismatches.iter().any(|m| m.contains(needle)),
        "no mismatch with {needle:?} in {report:#?}"
    );
}

#[test]
fn a_matching_run_matches() {
    let s = Single::new();
    let report = s.check();
    assert!(report.ok(), "{report:#?}");
    assert_eq!(report.last_line("sink"), "CONTRACT sink ok");
}

#[test]
fn a_missing_check_fails() {
    let s = Single::new();
    s.write_with(&[("provenance", "pass"), ("n1.neutral", "pass")]);
    let report = s.check();
    mismatch(&report, "check `n1.shot` is missing");
    assert_eq!(report.last_line("sink"), "CONTRACT sink mismatch 1");
}

#[test]
fn a_duplicate_check_fails() {
    let s = Single::new();
    s.write();
    let shot = verdict(
        "sink",
        "n1.shot",
        "pass",
        "judge",
        &[(SINGLE, "shot-a.png", &s.path("shot-a.png"))],
    );
    write(
        &s.path("verdicts-panel.jsonl"),
        shot.replace("\"judge\"", "\"panel\"").as_bytes(),
    );
    mismatch(&s.check(), "check `n1.shot` has 2 verdicts");
}

#[test]
fn an_extra_check_fails() {
    let s = Single::new();
    let extra = verdict("sink", "n1.extra", "pass", "judge", &[]);
    let mut bytes = fs::read(s.path("verdicts-judge.jsonl")).unwrap();
    bytes.extend_from_slice(extra.as_bytes());
    write(&s.path("verdicts-judge.jsonl"), &bytes);
    mismatch(&s.check(), "check `n1.extra` is not the row's");
}

#[test]
fn another_outcome_fails() {
    let s = Single::new();
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "fail"),
    ]);
    mismatch(&s.check(), "check `n1.shot` is Fail, the row expects Pass");
}

#[test]
fn another_exit_fails() {
    let mut s = Single::new();
    s.spec.end = Some(json!({"t": "end", "exit": 1, "errors": []}));
    s.write();
    mismatch(&s.check(), "role `run`'s runner exits Code(0)");
    s.spec.end = Some(json!({"t": "end", "exit": 0, "errors": []}));
    s.spec.exits = vec![exit("recorder", (200, 2000), 3, false)];
    s.write();
    mismatch(&s.check(), "role `run`'s recorder exits Code(0)");
    s.spec.exits.push(exit("player", (300, 3000), 0, false));
    s.write();
    mismatch(
        &s.check(),
        "observed the exit of `player`, which the row does not declare",
    );
}

#[test]
fn a_missing_record_fails() {
    let mut s = Single::new();
    let open = json!({"t": "open", "q": 1, "freq": 1, "location": "L", "com": "COM4", "path": "P"});
    let end = json!({"t": "end", "q": 3, "problems": []});
    s.spec.files[0].1 = format!("{}{}", line(&open), line(&end)).into_bytes();
    s.write();
    mismatch(
        &s.check(),
        "link.jsonl holds Some(0) `ans` records, the row expects AtLeast(1)",
    );
}

#[test]
fn a_check_reading_an_errored_stretch_is_incomplete() {
    let mut s = Single::new();
    s.spec.files[1].1 = recorder(200, &[(5, 7)]);
    s.write();
    mismatch(
        &s.check(),
        "check `n1.neutral` is Pass, but its evidence is incomplete: role `run`'s recorder.jsonl has 1 recorder errors",
    );
    // Its producer saying so does not pass the row's expectation either.
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "incomplete"),
        ("n1.shot", "pass"),
    ]);
    let report = s.check();
    mismatch(
        &report,
        "check `n1.neutral` is Incomplete, the row expects Pass",
    );
    assert_eq!(report.mismatches.len(), 1, "{report:#?}");
}

#[test]
fn a_missing_event_holds_its_check_incomplete() {
    let s = Single::new();
    let mut e = Single::expectation();
    e.checks[1].events = vec![Event {
        role: SINGLE,
        stream: RECORDER,
        kind: "error",
    }];
    mismatch(
        &check(&e, s.dir.path()),
        "but its evidence is incomplete: role `run`'s recorder.jsonl holds no `error` record",
    );
}

#[test]
fn a_file_unlike_its_seal_fails() {
    let s = Single::new();
    let replaced = fs::read_to_string(s.path(LINK))
        .unwrap()
        .replace("\"q\":2", "\"q\":4");
    write(&s.path(LINK), replaced.as_bytes());
    mismatch(&s.check(), "run/link.jsonl has sha256");
    mismatch(&s.check(), "unlike its seal");
}

#[test]
fn an_input_outside_the_declared_files_fails() {
    let s = Single::new();
    let mut out = verdict(
        "sink",
        "provenance",
        "pass",
        "judge",
        &[
            (SINGLE, LINK, &s.path(LINK)),
            (SINGLE, RECORDER, &s.path(RECORDER)),
        ],
    );
    out += &verdict(
        "sink",
        "n1.neutral",
        "pass",
        "judge",
        &[(SINGLE, RECORDER, &s.path(RECORDER))],
    );
    out += &verdict(
        "sink",
        "n1.shot",
        "pass",
        "judge",
        &[(SINGLE, "shot-a.png", &s.path("shot-a.png"))],
    );
    write(&s.path("verdicts-judge.jsonl"), out.as_bytes());
    mismatch(
        &s.check(),
        "check `provenance` read run/link.jsonl run/recorder.jsonl, not exactly its declared files run/link.jsonl",
    );
}

#[test]
fn an_input_whose_hash_differs_fails() {
    let s = Single::new();
    let text = fs::read_to_string(s.path("verdicts-judge.jsonl")).unwrap();
    let shot = sha(&fs::read(s.path("shot-a.png")).unwrap());
    write(
        &s.path("verdicts-judge.jsonl"),
        text.replace(&shot, &sha(b"another")).as_bytes(),
    );
    mismatch(
        &s.check(),
        "check `n1.shot` read run/shot-a.png with sha256",
    );
}

#[test]
fn an_unsealed_file_fails() {
    let s = Single::new();
    let text = fs::read_to_string(s.path(RUN)).unwrap();
    let kept: String = text
        .lines()
        .filter(|l| !l.contains("shot-a.png"))
        .map(|l| format!("{l}\n"))
        .collect();
    write(&s.path(RUN), kept.as_bytes());
    mismatch(&s.check(), "run/shot-a.png has no seal");
}

/// An outer run of row `sink-kill` that reserved and killed its inner
/// `sink`.
struct Outer {
    dir: tempfile::TempDir,
    outer: RunSpec,
    inner: RunSpec,
}

impl Outer {
    fn expectation() -> Expectation {
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
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "recorder",
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "inner",
                    exit: Exit::Killed,
                },
                ExitRule {
                    role: INNER,
                    process: RUNNER,
                    exit: Exit::Killed,
                },
            ],
            records: vec![],
            checks: vec![
                rule(
                    "provenance",
                    "judge",
                    Outcome::Pass,
                    vec![file(SINGLE, LINK)],
                ),
                rule(
                    "n2.neutral",
                    "judge",
                    Outcome::Pass,
                    vec![file(SINGLE, RECORDER), file(INNER, LINK)],
                ),
            ],
            negative: None,
        }
    }

    fn new() -> Self {
        let mut outer = RunSpec::new(RUN_A, "sink-kill", SINGLE);
        outer.inner = Some(json!({"role": INNER, "row": "sink", "run": RUN_B, "dir": INNER}));
        outer.exits.push(exit("inner", (500, 5000), 1, true));
        let mut inner = RunSpec::new(RUN_B, "sink", INNER);
        inner.process = (500, 5000);
        inner.set = Some(RUN_A);
        inner.files.truncate(1);
        inner.exits.clear();
        inner.end = None;
        let o = Self {
            dir: tempfile::tempdir().unwrap(),
            outer,
            inner,
        };
        o.write();
        o
    }

    fn write(&self) {
        self.outer.write(self.dir.path());
        self.inner.write(&self.dir.path().join(INNER));
        let mut out = verdict(
            "sink-kill",
            "provenance",
            "pass",
            "judge",
            &[(SINGLE, LINK, &self.dir.path().join(LINK))],
        );
        out += &verdict(
            "sink-kill",
            "n2.neutral",
            "pass",
            "judge",
            &[
                (SINGLE, RECORDER, &self.dir.path().join(RECORDER)),
                (INNER, LINK, &self.dir.path().join(INNER).join(LINK)),
            ],
        );
        write(
            &self.dir.path().join("verdicts-judge.jsonl"),
            out.as_bytes(),
        );
    }
}

#[test]
fn a_killed_inner_run_matches() {
    let o = Outer::new();
    let report = check(&Outer::expectation(), o.dir.path());
    assert!(report.ok(), "{report:#?}");
}

#[test]
fn a_killed_exit_needs_its_observer() {
    let mut o = Outer::new();
    o.outer.exits.retain(|x| x["name"] != "inner");
    o.write();
    let report = check(&Outer::expectation(), o.dir.path());
    mismatch(
        &report,
        "role `inner`'s runner must be killed, and no other process observed its exit as a kill",
    );
    mismatch(
        &report,
        "role `inner`: run.jsonl: the stream has no end record, and its writer was not cut short",
    );
    // An exit observed but not as a kill does not stand for one.
    o.outer.exits.push(exit("inner", (500, 5000), 1, false));
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `inner`'s runner must be killed",
    );
}

#[test]
fn a_negative_row_needs_its_exact_failing_set() {
    let s = Single::new();
    let mut e = Single::expectation();
    e.checks[2].outcome = Outcome::Fail;
    e.negative = Some(Negative {
        mutation: "close-early",
        trigger: "n1.shot",
    });
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "fail"),
    ]);
    let report = check(&e, s.dir.path());
    assert!(report.ok(), "{report:#?}");
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "fail"),
        ("n1.shot", "fail"),
    ]);
    mismatch(
        &check(&e, s.dir.path()),
        "check `n1.neutral` is Fail, the row expects Pass",
    );
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "pass"),
    ]);
    mismatch(
        &check(&e, s.dir.path()),
        "check `n1.shot` is Pass, the row expects Fail",
    );
    e.negative = Some(Negative {
        mutation: "close-early",
        trigger: "n1.neutral",
    });
    mismatch(
        &check(&e, s.dir.path()),
        "the expectation: the trigger `n1.neutral` of mutation `close-early` is not a check that must fail",
    );
}

#[test]
fn an_execution_error_never_matches_a_negative_row() {
    let mut s = Single::new();
    let mut e = Single::expectation();
    e.checks[2].outcome = Outcome::Fail;
    e.negative = Some(Negative {
        mutation: "close-early",
        trigger: "n1.shot",
    });
    s.spec.end = Some(json!({"t": "end", "exit": 0, "errors": ["the sink's close failed"]}));
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "fail"),
    ]);
    let report = check(&e, s.dir.path());
    mismatch(
        &report,
        "role `run`: the runner's error: the sink's close failed",
    );
    assert_eq!(report.mismatches.len(), 1, "{report:#?}");
    s.spec.end = Some(json!({"t": "end", "exit": 1, "errors": []}));
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "fail"),
    ]);
    mismatch(
        &check(&e, s.dir.path()),
        "role `run`'s runner exits Code(0)",
    );
}

/// A set of row `p1-compare`: roles A1 (`p1`) and R1 (`p1-ref`), one
/// check by `compare` reading both shots.
struct SetDir {
    dir: tempfile::TempDir,
    set: &'static str,
    a1: RunSpec,
    r1: RunSpec,
}

impl SetDir {
    fn expectation() -> Expectation {
        Expectation {
            row: "p1-compare",
            binding: Binding::Set,
            roles: vec![
                Role {
                    name: "A1",
                    row: "p1",
                },
                Role {
                    name: "R1",
                    row: "p1-ref",
                },
            ],
            exits: vec![
                ExitRule {
                    role: "A1",
                    process: RUNNER,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "A1",
                    process: "recorder",
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "R1",
                    process: RUNNER,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "R1",
                    process: "recorder",
                    exit: Exit::Code(0),
                },
            ],
            records: vec![],
            checks: vec![
                rule(
                    "provenance",
                    "judge",
                    Outcome::Pass,
                    vec![file("A1", LINK), file("R1", LINK)],
                ),
                rule(
                    "mask-17",
                    "compare",
                    Outcome::Pass,
                    vec![file("A1", "shot-a.png"), file("R1", "shot-a.png")],
                ),
            ],
            negative: None,
        }
    }

    fn new(set: &'static str, a1: &'static str, r1: &'static str) -> Self {
        let mut a = RunSpec::new(a1, "p1", "A1");
        a.set = Some(set);
        let mut r = RunSpec::new(r1, "p1-ref", "R1");
        r.set = Some(set);
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
            set,
            a1: a,
            r1: r,
        };
        s.write();
        s
    }

    fn write(&self) {
        let d = self.dir.path();
        let reserved = json!({
            "t": "reserved", "set": self.set, "row": "p1-compare", "build": BUILD,
            "roles": [
                {"role": "A1", "row": "p1", "run": self.a1.run, "dir": "A1"},
                {"role": "R1", "row": "p1-ref", "run": self.r1.run, "dir": "R1"},
            ],
        });
        write(&d.join(RESERVATION), line(&reserved).as_bytes());
        self.a1.write(&d.join("A1"));
        self.r1.write(&d.join("R1"));
        self.write_verdict();
    }

    fn write_verdict(&self) {
        let d = self.dir.path();
        let out = verdict(
            "p1-compare",
            "mask-17",
            "pass",
            "compare",
            &[
                ("A1", "shot-a.png", &d.join("A1/shot-a.png")),
                ("R1", "shot-a.png", &d.join("R1/shot-a.png")),
            ],
        );
        write(&d.join("verdicts-compare.jsonl"), out.as_bytes());
        let provenance = verdict(
            "p1-compare",
            "provenance",
            "pass",
            "judge",
            &[
                ("A1", LINK, &d.join("A1").join(LINK)),
                ("R1", LINK, &d.join("R1").join(LINK)),
            ],
        );
        write(&d.join("verdicts-judge.jsonl"), provenance.as_bytes());
    }

    fn check(&self) -> Report {
        check(&Self::expectation(), self.dir.path())
    }
}

#[test]
fn a_matching_set_matches() {
    let s = SetDir::new("set-1", RUN_A, RUN_B);
    let report = s.check();
    assert!(report.ok(), "{report:#?}");
}

#[test]
fn a_png_replaced_by_one_of_another_set_fails() {
    let s = SetDir::new("set-1", RUN_A, RUN_B);
    let other = SetDir::new("set-2", RUN_C, RUN_B);
    fs::copy(
        other.dir.path().join("A1/shot-a.png"),
        s.dir.path().join("A1/shot-a.png"),
    )
    .unwrap();
    // The producer read the replaced file as it is.
    s.write_verdict();
    let report = s.check();
    mismatch(&report, "A1/shot-a.png has sha256");
    mismatch(&report, "unlike its seal");
    mismatch(&report, "check `mask-17` read A1/shot-a.png with sha256");
}

#[test]
fn a_role_run_with_another_run_id_fails() {
    let mut s = SetDir::new("set-1", RUN_A, RUN_B);
    s.a1.run = RUN_C;
    s.a1.write(&s.dir.path().join("A1"));
    s.write_verdict();
    mismatch(
        &s.check(),
        &format!("role `A1`: A1/run.jsonl names run id `{RUN_C}`, the reservation `{RUN_A}`"),
    );
}

#[test]
fn a_role_run_of_another_row_fails() {
    let mut s = SetDir::new("set-1", RUN_A, RUN_B);
    s.a1.row = "p1-ref";
    s.a1.write(&s.dir.path().join("A1"));
    s.write_verdict();
    mismatch(
        &s.check(),
        "role `A1`: A1/run.jsonl names row `p1-ref`, the reservation `p1`",
    );
    // Nor can the reservation give a role another row.
    let text = fs::read_to_string(s.dir.path().join(RESERVATION)).unwrap();
    let swapped = text.replace(r#""role":"A1","row":"p1""#, r#""role":"A1","row":"p1-ref""#);
    assert_ne!(text, swapped);
    write(&s.dir.path().join(RESERVATION), swapped.as_bytes());
    mismatch(&s.check(), "reserves role `A1` for row `p1-ref`, not `p1`");
}

#[test]
fn a_consistent_capture_from_another_set_fails() {
    let s = SetDir::new("set-1", RUN_A, RUN_B);
    let other = SetDir::new("set-2", RUN_C, RUN_B);
    // Another set's complete, self-consistent A1 folder in place of this
    // set's, and the producer's verdict over it.
    fs::remove_dir_all(s.dir.path().join("A1")).unwrap();
    other.a1.write(&s.dir.path().join("A1"));
    s.write_verdict();
    let report = s.check();
    mismatch(
        &report,
        &format!("names run id `{RUN_C}`, the reservation `{RUN_A}`"),
    );
    mismatch(&report, "names set `set-2`, the reservation `set-1`");
}

#[test]
fn an_unbound_role_or_reservation_fails() {
    let s = SetDir::new("set-1", RUN_A, RUN_B);
    let text = fs::read_to_string(s.dir.path().join(RESERVATION)).unwrap();
    let escaped = text.replace(r#""dir":"R1""#, r#""dir":"../R1""#);
    write(&s.dir.path().join(RESERVATION), escaped.as_bytes());
    mismatch(
        &s.check(),
        "role `R1`'s folder `../R1` is not a folder name",
    );
    fs::remove_file(s.dir.path().join(RESERVATION)).unwrap();
    mismatch(&s.check(), "reservation.jsonl");
}

#[test]
fn an_unusable_expectation_is_named() {
    let mut e = Single::expectation();
    e.exits.remove(0);
    e.checks.push(e.checks[0].clone());
    e.records.push(RecordRule {
        role: "A1",
        stream: LINK,
        kind: "ans",
        count: Count::Exactly(1),
    });
    let problems = e.problems();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("role `run`'s runner has no declared exit")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("a check is declared twice")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("names role `A1`")),
        "{problems:?}"
    );
    let mut late = Single::expectation();
    late.checks.swap(0, 2);
    assert_eq!(late.problems(), ["the first check is not `provenance`"]);
}

#[test]
fn a_stream_cut_short_by_a_process_nobody_killed_fails() {
    let mut s = Single::new();
    // The recorder exited by itself, observed, without its end.
    let ready = line(&json!({"t": "ready", "q": 1, "process": {"pid": 200, "created": 2000}}));
    s.spec.files[1].1 = ready.into_bytes();
    s.write();
    mismatch(
        &s.check(),
        "role `run`: recorder.jsonl: the stream has no end record, and its writer was not cut short",
    );
    // Killed by its runner, it may stop short; the dropped line is noted.
    let mut cut = line(&json!({"t": "ready", "q": 1, "process": {"pid": 200, "created": 2000}}));
    cut += r#"{"t":"end""#;
    s.spec.files[1].1 = cut.into_bytes();
    s.spec.exits = vec![exit("recorder", (200, 2000), 1, true)];
    s.write();
    let mut e = Single::expectation();
    e.exits[1].exit = Exit::Killed;
    let report = check(&e, s.dir.path());
    assert!(report.ok(), "{report:#?}");
    assert_eq!(
        report.notes,
        [r#"role `run`: recorder.jsonl: dropped the unterminated last line "{\"t\":\"end\"""#]
    );
}

#[test]
fn a_process_cannot_observe_its_own_kill() {
    let mut o = Outer::new();
    o.outer.exits.retain(|x| x["name"] != "inner");
    o.inner.exits = vec![exit("inner", (500, 5000), 1, true)];
    o.write();
    let mut e = Outer::expectation();
    e.exits
        .retain(|r| !(r.role == SINGLE && r.process == "inner"));
    e.exits.push(ExitRule {
        role: INNER,
        process: "inner",
        exit: Exit::Killed,
    });
    let report = check(&e, o.dir.path());
    mismatch(
        &report,
        "role `inner`'s runner must be killed, and no other process observed its exit as a kill",
    );
    mismatch(
        &report,
        "role `inner`'s runner records its own exit as `inner`",
    );
    mismatch(
        &report,
        "role `inner`: run.jsonl: the stream has no end record",
    );
}

#[test]
fn a_runner_killed_after_its_end_counts_as_killed() {
    let mut o = Outer::new();
    o.inner.end = Some(json!({"t": "end", "exit": 0, "errors": []}));
    o.write();
    let report = check(&Outer::expectation(), o.dir.path());
    assert!(report.ok(), "{report:#?}");
}

#[test]
fn a_recorder_event_read_from_an_errored_stream_is_incomplete() {
    let mut s = Single::new();
    s.spec.files[1].1 = recorder(200, &[(5, 7)]);
    s.write();
    let mut e = Single::expectation();
    // `n1.shot` reads the recorder's `ready` without reading its file.
    e.checks[2].events = vec![Event {
        role: SINGLE,
        stream: RECORDER,
        kind: "ready",
    }];
    mismatch(
        &check(&e, s.dir.path()),
        "check `n1.shot` is Pass, but its evidence is incomplete: role `run`'s recorder.jsonl has 1 recorder errors",
    );
}
