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

/// A runner's cut code: `0x40000000` plus bits of its run id.
fn cut(run: &str) -> i32 {
    0x4000_0000 + i32::from_str_radix(&run[..6], 16).unwrap()
}

/// One run's folder: its children, its files (each sealed), its children's
/// exits, and its end (none when it is killed).
struct RunSpec {
    run: &'static str,
    row: &'static str,
    role: &'static str,
    process: (u32, u64),
    /// Its children's `spawn` records: name and process.
    spawns: Vec<(&'static str, (u32, u64))>,
    /// Its cut code.
    cut: i32,
    set: Option<&'static str>,
    inner: Option<Value>,
    files: Vec<(&'static str, Vec<u8>)>,
    /// Files written without the runner's seal.
    unsealed: Vec<(&'static str, Vec<u8>)>,
    exits: Vec<Value>,
    /// Records after the exits (a survivor's seals).
    after: Vec<Value>,
    end: Option<Value>,
}

impl RunSpec {
    fn new(run: &'static str, row: &'static str, role: &'static str) -> Self {
        Self {
            run,
            row,
            role,
            process: (100, 1000),
            spawns: vec![("recorder", (200, 2000))],
            cut: cut(run),
            set: None,
            inner: None,
            files: vec![
                ("link.jsonl", link(run, (100, 1000))),
                ("recorder.jsonl", recorder(run, 200, &[])),
                ("shot-a.png", format!("png of {run}").into_bytes()),
            ],
            unsealed: vec![],
            exits: vec![exit("recorder", (200, 2000), 0)],
            after: vec![],
            end: Some(json!({"t": "end", "exit": 0, "errors": []})),
        }
    }

    fn write(&self, folder: &Path) {
        let mut out = line(&json!({
            "t": "start", "run": self.run, "row": self.row, "role": self.role, "build": BUILD,
            "process": {"pid": self.process.0, "created": self.process.1}, "cut": self.cut,
            "args": ["--build", BUILD], "set": self.set, "inner": self.inner,
        }));
        for (name, process) in &self.spawns {
            out += &line(&spawn(name, *process));
        }
        for (path, bytes) in &self.files {
            write(&folder.join(path), bytes);
            out += &line(&json!({
                "t": "seal", "run": self.run, "role": self.role, "path": path, "sha256": sha(bytes),
            }));
        }
        for (path, bytes) in &self.unsealed {
            write(&folder.join(path), bytes);
        }
        for x in self.exits.iter().chain(&self.after) {
            out += &line(x);
        }
        if let Some(end) = &self.end {
            out += &line(end);
        }
        write(&folder.join(RUN), out.as_bytes());
    }
}

/// A link's `open`, naming its run and its writer.
fn open(run: &str, (pid, created): (u32, u64)) -> Value {
    json!({"t": "open", "q": 1, "run": run, "process": {"pid": pid, "created": created}, "freq": 10_000_000, "location": "L", "com": "COM4", "path": "P"})
}

fn link(run: &str, writer: (u32, u64)) -> Vec<u8> {
    [
        open(run, writer),
        json!({"t": "ans", "q": 2, "line": HELLO}),
        json!({"t": "end", "q": 3, "problems": []}),
    ]
    .iter()
    .map(line)
    .collect::<String>()
    .into_bytes()
}

/// A recorder's `ready`, naming its run and itself.
fn ready(run: &str, pid: u32) -> Value {
    json!({"t": "ready", "q": 1, "run": run, "process": {"pid": pid, "created": u64::from(pid) * 10}})
}

fn recorder(run: &str, pid: u32, errors: &[(i64, i64)]) -> Vec<u8> {
    let mut out = line(&ready(run, pid));
    for (from, to) in errors {
        out +=
            &line(&json!({"t": "error", "from": from, "to": to, "what": "GetRawInputData failed"}));
    }
    out += &line(&json!({"t": "end", "q": 9}));
    out.into_bytes()
}

fn survivor_seal(run: &str, role: &str, path: &str, bytes: &[u8]) -> Value {
    json!({"t": "seal", "run": run, "role": role, "path": path, "sha256": sha(bytes)})
}

fn exit(name: &str, (pid, created): (u32, u64), code: i32) -> Value {
    json!({"t": "exit", "q": 100, "name": name, "process": {"pid": pid, "created": created}, "code": code})
}

fn kill(name: &str, (pid, created): (u32, u64)) -> Value {
    json!({"t": "kill", "q": 90, "name": name, "process": {"pid": pid, "created": created}})
}

fn spawn(name: &str, (pid, created): (u32, u64)) -> Value {
    json!({"t": "spawn", "q": 0, "name": name, "process": {"pid": pid, "created": created}})
}

const fn spawned(role: &'static str, name: &'static str) -> Option<Spawned> {
    Some(Spawned { role, name })
}

/// A verdict line reading `inputs` (role, path, file) with the files'
/// current digests.
fn verdict(
    run: &str,
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
        "row": row, "run": run, "check": check, "outcome": outcome, "detail": "",
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
                    spawn: None,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "recorder",
                    spawn: spawned(SINGLE, "recorder"),
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
                RUN_A,
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
        RUN_A,
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
    let extra = verdict(RUN_A, "sink", "n1.extra", "pass", "judge", &[]);
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
    s.spec.exits = vec![exit("recorder", (200, 2000), 3)];
    s.write();
    mismatch(&s.check(), "role `run`'s recorder exits Code(0)");
    s.spec.exits.push(exit("player", (300, 3000), 0));
    s.write();
    mismatch(
        &s.check(),
        "observed the exit of `player`, which the row does not declare",
    );
}

#[test]
fn a_missing_record_fails() {
    let mut s = Single::new();
    let end = json!({"t": "end", "q": 3, "problems": []});
    s.spec.files[0].1 = format!("{}{}", line(&open(RUN_A, (100, 1000))), line(&end)).into_bytes();
    s.write();
    mismatch(
        &s.check(),
        "link.jsonl holds 0 `ans` records, the row expects AtLeast(1)",
    );
}

#[test]
fn a_check_reading_an_errored_stretch_is_incomplete() {
    let mut s = Single::new();
    s.spec.files[1].1 = recorder(RUN_A, 200, &[(5, 7)]);
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
    mismatch(
        &s.check(),
        "run/link.jsonl is not evidence: its bytes have sha256",
    );
    mismatch(&s.check(), "unlike its seal");
}

#[test]
fn an_input_outside_the_declared_files_fails() {
    let s = Single::new();
    let mut out = verdict(
        RUN_A,
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
        RUN_A,
        "sink",
        "n1.neutral",
        "pass",
        "judge",
        &[(SINGLE, RECORDER, &s.path(RECORDER))],
    );
    out += &verdict(
        RUN_A,
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

/// The inner runner's process.
const INNER_PROCESS: (u32, u64) = (500, 5000);

/// The inner's link, as its runner wrote it.
fn inner_link() -> Vec<u8> {
    link(RUN_B, INNER_PROCESS)
}

/// An outer run of row `sink-kill` that spawned, then killed (its job
/// terminated), its inner `sink`.
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
                    spawn: None,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "recorder",
                    spawn: spawned(SINGLE, "recorder"),
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: SINGLE,
                    process: "inner",
                    spawn: spawned(SINGLE, "inner"),
                    exit: Exit::Killed,
                },
                ExitRule {
                    role: INNER,
                    process: RUNNER,
                    spawn: None,
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
        outer.spawns.push((INNER, INNER_PROCESS));
        outer.exits.push(kill("inner", INNER_PROCESS));
        outer.exits.push(exit("inner", INNER_PROCESS, cut(RUN_A)));
        let mut inner = RunSpec::new(RUN_B, "sink", INNER);
        inner.process = INNER_PROCESS;
        inner.spawns = vec![("recorder", (600, 6000))];
        inner.set = Some(RUN_A);
        // Killed, the inner seals nothing: the outer seals its link after
        // it saw it end.
        inner.unsealed = vec![("link.jsonl", inner_link())];
        inner.files.clear();
        inner.exits.clear();
        inner.end = None;
        outer
            .after
            .push(survivor_seal(RUN_B, INNER, LINK, &inner_link()));
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
            RUN_A,
            "sink-kill",
            "provenance",
            "pass",
            "judge",
            &[(SINGLE, LINK, &self.dir.path().join(LINK))],
        );
        out += &verdict(
            RUN_A,
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
        "role `inner`'s runner must be killed, and no other runner observed it end with that runner's cut code after its kill",
    );
    mismatch(
        &report,
        "role `inner`: run.jsonl: the stream has no end record, and its writer was not cut short",
    );
    // An exit observed with another code than the observer's cut code is
    // the process ending by itself, after the kill or not.
    o.outer.exits.push(kill("inner", INNER_PROCESS));
    o.outer.exits.push(exit("inner", INNER_PROCESS, 1));
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `inner`'s runner must be killed",
    );
    // Nor does an exit with the observer's cut code stand for a code the
    // process ended with by itself.
    let o = Outer::new();
    let mut e = Outer::expectation();
    for r in &mut e.exits {
        if r.role == SINGLE && r.process == "inner" {
            r.exit = Exit::Code(cut(RUN_A));
        }
    }
    mismatch(&check(&e, o.dir.path()), "role `run`'s inner exits Code(");
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
                    spawn: None,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "A1",
                    process: "recorder",
                    spawn: spawned("A1", "recorder"),
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "R1",
                    process: RUNNER,
                    spawn: None,
                    exit: Exit::Code(0),
                },
                ExitRule {
                    role: "R1",
                    process: "recorder",
                    spawn: spawned("R1", "recorder"),
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
            self.set,
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
            self.set,
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
    s.spec.files[1].1 = line(&ready(RUN_A, 200)).into_bytes();
    s.write();
    mismatch(
        &s.check(),
        "role `run`: recorder.jsonl: the stream has no end record, and its writer was not cut short",
    );
    // Killed by its runner, it may stop short; the dropped line is noted.
    // The runner seals it after it observed the end.
    let mut cut_short = line(&ready(RUN_A, 200));
    cut_short += r#"{"t":"end""#;
    let bytes = cut_short.into_bytes();
    s.spec.files.remove(1);
    s.spec.unsealed = vec![(RECORDER, bytes.clone())];
    s.spec.exits = vec![
        kill("recorder", (200, 2000)),
        exit("recorder", (200, 2000), cut(RUN_A)),
    ];
    s.spec.after = vec![survivor_seal(RUN_A, SINGLE, RECORDER, &bytes)];
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
    o.inner.exits = vec![exit("inner", INNER_PROCESS, cut(RUN_B))];
    o.write();
    let mut e = Outer::expectation();
    e.exits
        .retain(|r| !(r.role == SINGLE && r.process == "inner"));
    e.exits.push(ExitRule {
        role: INNER,
        process: "inner",
        spawn: spawned(SINGLE, "inner"),
        exit: Exit::Killed,
    });
    let report = check(&e, o.dir.path());
    mismatch(
        &report,
        "role `inner`'s runner must be killed, and no other runner observed it end with that runner's cut code after its kill",
    );
    mismatch(
        &report,
        "role `inner`'s runner records its own exit as `inner`",
    );
    mismatch(
        &report,
        "role `inner`: run.jsonl: the stream stops short and its writer's end is ambiguous",
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
    s.spec.files[1].1 = recorder(RUN_A, 200, &[(5, 7)]);
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

/// A stream counts for a record rule or an event only when sealed: a
/// complete recorder stream left in the folder without its seal (another
/// run's, or one the runner never finished) supplies no `ready`.
#[test]
fn a_stream_counts_only_when_sealed() {
    let s = Single::new();
    let mut e = Single::expectation();
    e.records.push(RecordRule {
        role: SINGLE,
        stream: RECORDER,
        kind: "ready",
        count: Count::Exactly(1),
    });
    e.checks[1].inputs.clear();
    s.write_with(&[
        ("provenance", "pass"),
        ("n1.neutral", "pass"),
        ("n1.shot", "pass"),
    ]);
    // The verdict reads no file now; the event still reads the recorder.
    let verdicts = fs::read_to_string(s.path("verdicts-judge.jsonl")).unwrap();
    let rewritten: String = verdicts
        .lines()
        .map(|l| {
            if l.contains("n1.neutral") {
                let mut v: Value = serde_json::from_str(l).unwrap();
                v["inputs"] = json!([]);
                format!("{v}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    write(&s.path("verdicts-judge.jsonl"), rewritten.as_bytes());
    // The control: sealed, it counts.
    let report = check(&e, s.dir.path());
    assert!(report.ok(), "{report:#?}");
    let text = fs::read_to_string(s.path(RUN)).unwrap();
    let kept: String = text
        .lines()
        .filter(|l| !l.contains("recorder.jsonl"))
        .map(|l| format!("{l}\n"))
        .collect();
    write(&s.path(RUN), kept.as_bytes());
    let report = check(&e, s.dir.path());
    mismatch(
        &report,
        "role `run`'s recorder.jsonl is not evidence (unsealed), the row expects Exactly(1) `ready` records",
    );
    mismatch(
        &report,
        "check `n1.neutral` is Pass, but its evidence is incomplete: role `run`'s recorder.jsonl is not evidence: unsealed",
    );
}

/// A verdict names the run or set it judges: one copied from another set
/// whose files are byte-identical does not count.
#[test]
fn a_verdict_of_another_run_fails() {
    let s = SetDir::new("set-1", RUN_A, RUN_B);
    let other = SetDir::new("set-2", RUN_A, RUN_B);
    fs::copy(
        other.dir.path().join("verdicts-compare.jsonl"),
        s.dir.path().join("verdicts-compare.jsonl"),
    )
    .unwrap();
    mismatch(
        &s.check(),
        "verdicts-compare.jsonl: check `mask-17` judges run `set-2`, not this run's `set-1`",
    );
}

/// The outer seals the killed inner's link only after it saw the inner
/// end; every arrangement of the outer's kill record and its seal, with the
/// seal's digest and role right or wrong, against an oracle.
#[test]
fn a_survivor_seals_only_what_it_saw_end() {
    for seal_after_kill in [true, false] {
        for digest_right in [true, false] {
            for role_right in [true, false] {
                let mut o = Outer::new();
                let bytes = if digest_right {
                    inner_link()
                } else {
                    b"other".to_vec()
                };
                let role = if role_right { INNER } else { SINGLE };
                let seal = survivor_seal(RUN_B, role, LINK, &bytes);
                o.outer.after.clear();
                if seal_after_kill {
                    o.outer.after.push(seal);
                } else {
                    // Before its observation of the inner's end.
                    o.outer.exits.insert(1, seal);
                }
                o.write();
                let report = check(&Outer::expectation(), o.dir.path());
                let want = seal_after_kill && digest_right && role_right;
                assert_eq!(
                    report.ok(),
                    want,
                    "after kill {seal_after_kill}, digest {digest_right}, role {role_right}: {report:#?}"
                );
                if !want {
                    assert!(
                        report
                            .mismatches
                            .iter()
                            .any(|m| m.contains("inner/link.jsonl is not evidence")
                                || m.contains("a seal not of this run's own files")),
                        "{report:#?}"
                    );
                }
            }
        }
    }
    // Nor does a survivor seal the file of a run nobody killed.
    let mut o = Outer::new();
    o.outer.exits.retain(|x| x["name"] != "inner");
    o.outer.exits.push(exit("inner", INNER_PROCESS, 0));
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "a seal not of this run's own files",
    );
}

/// The inner's recorder writes `ready` as process 600 and no `end`.
fn cut_recorder() -> Vec<u8> {
    line(&ready(RUN_B, 600)).into_bytes()
}

/// A recorder its own runner saw end by itself, without its `end`, is not
/// excused by the runner's later kill, nor by the killer's observation.
#[test]
fn a_recorder_its_runner_saw_end_is_not_excused_by_a_later_kill() {
    let mut o = Outer::new();
    o.inner.unsealed.push((RECORDER, cut_recorder()));
    o.inner.exits = vec![exit("recorder", (600, 6000), 0)];
    o.outer.exits.push(exit("inner-recorder", (600, 6000), 0));
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `inner`: recorder.jsonl: the stream has no end record, and its writer was not cut short",
    );
}

/// An exit rule's process is the writer its stream names: an exit of
/// another process under the recorder's name does not stand for it.
#[test]
fn an_exit_of_another_process_does_not_match() {
    let mut s = Single::new();
    s.spec.exits = vec![exit("recorder", (201, 2010), 0)];
    s.write();
    mismatch(
        &s.check(),
        "role `run`'s recorder's exit names process Process { pid: 201, created: 2010 }, not the process role `run` spawned as `recorder`",
    );
}

/// A sealed link that recorded an error fails the run though the runner's
/// `end` names none: each kind of error, and none as the control.
#[test]
fn a_link_error_fails_the_run() {
    let open = open(RUN_A, (100, 1000));
    let ans = json!({"t": "ans", "q": 2, "line": HELLO});
    let cases = [
        (None, json!({"t": "end", "q": 3, "problems": []}), None),
        (
            Some(json!({"t": "lost", "q": 3, "error": "gone"})),
            json!({"t": "end", "q": 4, "problems": []}),
            Some("lost: gone"),
        ),
        (
            Some(json!({"t": "refused", "q": 3, "error": "build"})),
            json!({"t": "end", "q": 4, "problems": []}),
            Some("refused: build"),
        ),
        (
            None,
            json!({"t": "end", "q": 3, "problems": ["a ping went unanswered"]}),
            Some("problems at its end: a ping went unanswered"),
        ),
    ];
    for (error, end, want) in cases {
        let mut s = Single::new();
        let mut text = line(&open) + &line(&ans);
        if let Some(e) = &error {
            text += &line(e);
        }
        text += &line(&end);
        s.spec.files[0].1 = text.into_bytes();
        s.write();
        let report = s.check();
        match want {
            None => assert!(report.ok(), "{report:#?}"),
            Some(w) => mismatch(
                &report,
                &format!("role `run`: link.jsonl records an error: {w}"),
            ),
        }
    }
}

/// A stream counts only as its own run's: its first record names the run
/// and the writer its runner set. A complete link of the same build left by
/// another run, or written by another process, is not the inner's, though
/// the outer seals it after the kill.
#[test]
fn a_stream_of_another_run_or_writer_is_not_evidence() {
    for (run, writer, want) in [
        (RUN_C, INNER_PROCESS, "names run"),
        (RUN_B, (501, 5010), "names process"),
    ] {
        let mut o = Outer::new();
        let bytes = link(run, writer);
        o.inner.unsealed = vec![(LINK, bytes.clone())];
        o.outer.after = vec![survivor_seal(RUN_B, INNER, LINK, &bytes)];
        o.write();
        let report = check(&Outer::expectation(), o.dir.path());
        mismatch(
            &report,
            &format!("role `inner`: link.jsonl is not this run's: its first record {want}"),
        );
        mismatch(&report, "inner/link.jsonl is not evidence");
    }
    // The control: the inner's own link, sealed by the outer.
    let o = Outer::new();
    let report = check(&Outer::expectation(), o.dir.path());
    assert!(report.ok(), "{report:#?}");
}

/// Only the runner that cut a writer short seals what that writer wrote,
/// after it observed the end: not the killed run itself, nor another
/// runner than the one that cut it.
#[test]
fn only_the_runner_that_cut_a_writer_seals_its_stream() {
    // The inner sealed its own link, then the outer killed it.
    let mut o = Outer::new();
    o.inner.unsealed.clear();
    o.inner.files = vec![(LINK, inner_link())];
    o.outer.after.clear();
    o.write();
    let report = check(&Outer::expectation(), o.dir.path());
    mismatch(&report, "role `inner`: a seal not of this run's own files");
    mismatch(&report, "inner/link.jsonl is not evidence");
    // The inner cut its recorder short; the outer seals the recorder's
    // stream.
    let mut o = Outer::new();
    let bytes = cut_recorder();
    o.inner.unsealed.push((RECORDER, bytes.clone()));
    o.inner.exits = vec![
        kill("recorder", (600, 6000)),
        exit("recorder", (600, 6000), cut(RUN_B)),
    ];
    o.outer
        .after
        .push(survivor_seal(RUN_B, INNER, RECORDER, &bytes));
    o.write();
    let mut e = Outer::expectation();
    e.exits.push(ExitRule {
        role: INNER,
        process: "recorder",
        spawn: spawned(INNER, "recorder"),
        exit: Exit::Killed,
    });
    mismatch(
        &check(&e, o.dir.path()),
        "role `run`: a seal not of this run's own files",
    );
}

/// An end observed before its writer's last record cannot be: the outer's
/// exit of the inner at counter 0, before the inner's link opened at 1,
/// leaves the inner's end ambiguous, so neither its kill nor its cut
/// stream stands.
#[test]
fn an_end_observed_before_the_writer_s_last_record_does_not_stand() {
    let mut o = Outer::new();
    for x in &mut o.outer.exits {
        if x["name"] == "inner" {
            x["q"] = json!(0);
        }
    }
    o.write();
    let report = check(&Outer::expectation(), o.dir.path());
    mismatch(&report, "role `inner`'s runner must be killed");
    mismatch(
        &report,
        "role `inner`: run.jsonl: the stream stops short and its writer's end is ambiguous: process 500's end is observed at 0, before its last record at 3",
    );
    // After the writer's last record, but before the kill it follows.
    let mut o = Outer::new();
    for x in &mut o.outer.exits {
        if x["t"] == "exit" && x["name"] == "inner" {
            x["q"] = json!(50);
        }
    }
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `run` observed process 500 end at 50, before its kill at 90",
    );
}

/// The roles' runs are bound to each other: the inner's runner is the
/// process its outer spawned as `inner`, and a runner kills only a child it
/// spawned, by the name it spawned it under.
#[test]
fn a_run_kills_and_hosts_only_its_own_children() {
    let mut o = Outer::new();
    o.outer.spawns = vec![("recorder", (200, 2000)), (INNER, (501, 5010))];
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `inner`'s runner is process Process { pid: 500, created: 5000 }, not the process role `run` spawned as `inner`",
    );
    let mut o = Outer::new();
    o.outer.exits.insert(0, kill("recorder", INNER_PROCESS));
    o.write();
    mismatch(
        &check(&Outer::expectation(), o.dir.path()),
        "role `run` kills `recorder` as process Process { pid: 500, created: 5000 }, not a child it spawned under that name",
    );
}

/// Each runner has a cut code of its own form, and no two runners share
/// one, so a cut code names the runner that cut.
#[test]
fn runners_have_cut_codes_of_their_own() {
    let mut s = SetDir::new("set-1", RUN_A, RUN_B);
    s.r1.cut = s.a1.cut;
    s.write();
    mismatch(
        &s.check(),
        "role `R1`'s cut code 0x40012345 is another runner's too",
    );
    s.r1.cut = 1;
    s.write();
    mismatch(
        &s.check(),
        "role `R1`: the cut code 0x1 is not 0x40000000 plus 30 bits",
    );
}

/// A stream whose first record names another run supplies nothing, its
/// writer's exit included: the exit binds to the run's `spawn` record.
#[test]
fn a_stream_header_of_another_run_supplies_nothing() {
    let mut s = Single::new();
    s.spec.files[1].1 = recorder(RUN_C, 200, &[]);
    s.write();
    let report = s.check();
    mismatch(
        &report,
        "role `run`: recorder.jsonl is not this run's: its first record names run",
    );
    // Its exit is still the spawned recorder's, and matches.
    assert!(
        !report
            .mismatches
            .iter()
            .any(|m| m.contains("recorder exits")),
        "{report:#?}"
    );
}

/// A child that writes no stream (the player) binds by its `spawn` record.
#[test]
fn a_child_without_a_stream_binds_by_its_spawn() {
    let mut s = Single::new();
    s.spec.spawns.push(("player", (300, 3000)));
    s.spec.exits.push(exit("player", (300, 3000), 0));
    s.write();
    let mut e = Single::expectation();
    e.exits.push(ExitRule {
        role: SINGLE,
        process: "player",
        spawn: spawned(SINGLE, "player"),
        exit: Exit::Code(0),
    });
    let report = check(&e, s.dir.path());
    assert!(report.ok(), "{report:#?}");
    s.spec.exits.pop();
    s.spec.exits.push(exit("player", (301, 3010), 0));
    s.write();
    mismatch(
        &check(&e, s.dir.path()),
        "not the process role `run` spawned as `player`",
    );
}

/// Two runners observe one end: the inner terminated its recorder (its
/// cut code), and the outer, after it killed the inner, saw the same code.
/// They agree by the code, and the end is the inner's cut; a code that
/// differs leaves the end ambiguous.
#[test]
fn observers_agree_by_the_exit_code() {
    for other in [false, true] {
        let mut o = Outer::new();
        let bytes = cut_recorder();
        o.inner.unsealed.push((RECORDER, bytes.clone()));
        o.inner.exits = vec![
            kill("recorder", (600, 6000)),
            exit("recorder", (600, 6000), cut(RUN_B)),
            survivor_seal(RUN_B, INNER, RECORDER, &bytes),
        ];
        let seen_by_outer = if other { 1 } else { cut(RUN_B) };
        o.outer
            .exits
            .push(exit("inner-recorder", (600, 6000), seen_by_outer));
        o.write();
        let mut e = Outer::expectation();
        e.exits.push(ExitRule {
            role: INNER,
            process: "recorder",
            spawn: spawned(INNER, "recorder"),
            exit: Exit::Killed,
        });
        e.exits.push(ExitRule {
            role: SINGLE,
            process: "inner-recorder",
            spawn: spawned(INNER, "recorder"),
            exit: Exit::Code(cut(RUN_B)),
        });
        e.records.push(RecordRule {
            role: INNER,
            stream: RECORDER,
            kind: "ready",
            count: Count::Exactly(1),
        });
        let report = check(&e, o.dir.path());
        if other {
            mismatch(&report, "the exit codes recorded for process 600 disagree");
        } else {
            assert!(report.ok(), "{report:#?}");
            // The outer did not cut it: its observation is not its kill.
            let last = e.exits.len() - 1;
            e.exits[last].exit = Exit::Killed;
            mismatch(
                &check(&e, o.dir.path()),
                "role `run`'s inner-recorder exits Killed",
            );
        }
    }
}

/// Every way the inner's recorder may truly have ended (its `end` written
/// and exit 0, exit 3 without `end`, or still running when the outer
/// terminated the inner's job), against every way that end may be
/// recorded: the outer's observation before or after its kill, at a
/// counter after or (impossibly) before the recorder's last record; the
/// inner's own observation (none, the same code, another code); the seal
/// (by the outer after its observation, by the outer before it, by the
/// inner, none); and the stream's first record naming this run or another.
/// The observations follow from the true end: a runner reads its own cut
/// code only when its termination ended the process, and nobody sees a
/// running process end before the termination. The oracle judges from the
/// true end and order alone; the full contract (the recorder's exit
/// declared as the truth makes it, and a rule reading the stream) matches
/// exactly when the oracle accepts. Its positive controls: the recorder
/// cut short by the job, and the recorder that ended by itself.
#[test]
fn a_stream_cut_short_is_excused_only_by_the_runner_that_ended_it() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fate {
        Ended,
        Crashed,
        Running,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Sealer {
        OuterAfter,
        OuterBefore,
        Inner,
        Nobody,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InnerSaw {
        Nothing,
        Same,
        Other,
    }
    const REC: (u32, u64) = (600, 6000);
    let mut controls = (false, false);
    for fate in [Fate::Ended, Fate::Crashed, Fate::Running] {
        for before_kill in [false, true] {
            for late in [true, false] {
                for sealer in [
                    Sealer::OuterAfter,
                    Sealer::OuterBefore,
                    Sealer::Inner,
                    Sealer::Nobody,
                ] {
                    for inner_saw in [InnerSaw::Nothing, InnerSaw::Same, InnerSaw::Other] {
                        for own_run in [true, false] {
                            // Nobody sees a running process end before the
                            // termination that ends it.
                            if fate == Fate::Running && inner_saw != InnerSaw::Nothing {
                                continue;
                            }
                            let code = match fate {
                                Fate::Ended => 0,
                                Fate::Crashed => 3,
                                Fate::Running => cut(RUN_A),
                            };
                            let run = if own_run { RUN_B } else { RUN_C };
                            let mut text = line(
                                &json!({"t": "ready", "q": 5, "run": run, "process": {"pid": 600, "created": 6000}}),
                            );
                            if fate == Fate::Ended {
                                text += &line(&json!({"t": "end", "q": 9}));
                            }
                            let bytes = text.into_bytes();
                            let mut o = Outer::new();
                            let mut seen = exit("inner-recorder", REC, code);
                            seen["q"] = json!(if late { 100 } else { 4 });
                            // The outer's exits: its recorder, its kill of
                            // the inner, the inner's end.
                            let at = if before_kill { 1 } else { 3 };
                            o.outer.exits.insert(at, seen);
                            let seal = survivor_seal(RUN_B, INNER, RECORDER, &bytes);
                            match sealer {
                                Sealer::OuterAfter => o.outer.after.push(seal),
                                Sealer::OuterBefore => o.outer.exits.insert(at, seal),
                                Sealer::Inner => o.inner.files.push((RECORDER, bytes.clone())),
                                Sealer::Nobody => {}
                            }
                            if sealer != Sealer::Inner {
                                o.inner.unsealed.push((RECORDER, bytes.clone()));
                            }
                            let mut e = Outer::expectation();
                            if inner_saw != InnerSaw::Nothing {
                                let mut x = exit(
                                    "recorder",
                                    REC,
                                    if inner_saw == InnerSaw::Same { code } else { 7 },
                                );
                                x["q"] = json!(20);
                                o.inner.exits = vec![x];
                                e.exits.push(ExitRule {
                                    role: INNER,
                                    process: "recorder",
                                    spawn: spawned(INNER, "recorder"),
                                    exit: Exit::Code(code),
                                });
                            }
                            e.exits.push(ExitRule {
                                role: SINGLE,
                                process: "inner-recorder",
                                spawn: spawned(INNER, "recorder"),
                                exit: if fate == Fate::Running {
                                    Exit::Killed
                                } else {
                                    Exit::Code(code)
                                },
                            });
                            e.records.push(RecordRule {
                                role: INNER,
                                stream: RECORDER,
                                kind: "ready",
                                count: Count::Exactly(1),
                            });
                            o.write();
                            let report = check(&e, o.dir.path());
                            // The oracle, from the true end and order.
                            let accepted = own_run
                                && late
                                && match fate {
                                    Fate::Running => !before_kill && sealer == Sealer::OuterAfter,
                                    Fate::Ended => {
                                        sealer == Sealer::Inner && inner_saw != InnerSaw::Other
                                    }
                                    Fate::Crashed => false,
                                };
                            assert_eq!(
                                report.ok(),
                                accepted,
                                "{fate:?}, seen before the kill {before_kill}, late {late}, sealed {sealer:?}, inner saw {inner_saw:?}, own run {own_run}: {report:#?}"
                            );
                            if accepted {
                                match fate {
                                    Fate::Running => controls.0 = true,
                                    _ => controls.1 = true,
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(controls, (true, true), "a positive control never matched");
}
