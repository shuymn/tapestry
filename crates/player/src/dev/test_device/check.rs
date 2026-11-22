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
    ("p1-ref", p1_ref),
    ("p1-ref-75", p1_ref_75),
    ("p1-compare", p1_compare),
];

/// The private panel judge's name: it writes `verdicts-panel-exact.jsonl`.
/// The name is the judging policy's: each shot equal to its pinned
/// reference at every pixel, with no mask. A judge of another policy (an
/// earlier one that masked regions, `panel`) writes under its own name, and
/// its verdicts do not stand for these checks.
pub const PANEL: &str = "panel-exact";

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
        id: id.into(),
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
                &[run, rec, inner_run],
                &[
                    ev(SINGLE, RECORDER, "pad"),
                    ev(SINGLE, RECORDER, "key"),
                    ev(INNER, RUN, "held"),
                    ev(SINGLE, RUN, "kill"),
                ],
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

/// The private panel judge's checks, each a shot compared with a pinned
/// reference frame by exact equality at every pixel of the whole frame, no
/// region masked: shot A with P1's (panel 3 open, the sliders at their
/// targets), shot B with the closed screen's.
pub const FRAME_A: &str = "panel.frame-a";
pub const FRAME_B: &str = "panel.frame-b";
/// A negative's own declared frame, which must pass: shot A equal to the
/// row's reference (P4: panel 1 open, nothing of panel 3; P5: panel 3 open
/// with every slider at 100 %), so it fails [`FRAME_A`] for its declared
/// reason and no other.
pub const DECLARED_A: &str = "panel.declared-a";

/// A panel row (P1, and its negatives P4 and P5): `failing` the checks
/// that must fail, and the negative's mutation and trigger. A negative
/// also has checks of its own that must pass: the judge's (its volumes
/// all kept at 1.0; P5's player the declared mutated build) and the
/// private judge's [`DECLARED_A`].
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
    let private = |id: &'static str, inputs: &[File]| CheckRule {
        id: id.into(),
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
            rule(role, RUN, "released", Exactly(1)),
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
            judge("p1.player-bound", &[run], &[ev(SINGLE, RUN, "player")]),
            judge(
                "p1.volumes",
                &[f(DEVICE_BEFORE), f(DEVICE_AFTER)],
                &[ev(SINGLE, RUN, "exit")],
            ),
            judge(
                "p1.volumes-start",
                &[f(DEVICE_BEFORE)],
                &[ev(SINGLE, RUN, "exit")],
            ),
            judge("p1.volume-arm", &[run], &[ev(SINGLE, RUN, "shot")]),
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
            private(FRAME_A, &[run, rec, f(SHOT_A)]),
            private(FRAME_B, &[run, rec, f(SHOT_B)]),
        ]
        .into_iter()
        .chain(match row {
            "p4" => [
                judge(
                    "p4.volumes-kept",
                    &[f(DEVICE_AFTER)],
                    &[ev(SINGLE, RUN, "exit")],
                ),
                judge("p4.no-volume-arm", &[run], &[ev(SINGLE, RUN, "shot")]),
            ]
            .into_iter()
            .chain([private(DECLARED_A, &[run, rec, f(SHOT_A)])])
            .collect(),
            "p5" => [
                judge(
                    "p5.volumes-kept",
                    &[f(DEVICE_AFTER)],
                    &[ev(SINGLE, RUN, "exit")],
                ),
                judge("p5.mutation-build", &[run], &[ev(SINGLE, RUN, "player")]),
            ]
            .into_iter()
            .chain([private(DECLARED_A, &[run, rec, f(SHOT_A)])])
            .collect(),
            _ => Vec::new(),
        })
        .collect(),
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
        &["p1.volumes", "p1.volume-arm", FRAME_A],
        Some(Negative {
            mutation: "SELECT is never held: START alone opens panel 1",
            trigger: FRAME_A,
        }),
    )
}

/// P5: P1's steps against a local player whose volume arm drops the value:
/// panel 3 is drawn, the volumes stay at 100 %.
fn p5() -> Expectation {
    panel(
        "p5",
        &["p1.volumes", FRAME_A],
        Some(Negative {
            mutation: "the player's pointer volume arm drops the value (a local build)",
            trigger: "p1.volumes",
        }),
    )
}

/// N2p (`p1-kill`): the outer reserves and kills an inner `p1` at its kill
/// barrier. It has no run negative: no player can outlive the inner, which
/// starts it in the inner's own job (closed, and so killed, with the inner)
/// inside the job the outer ends, neither allowing breakaway; the judge's
/// tests hold the survivors' records instead (docs/test-device.md
/// "Verification", N2p).
fn p1_kill() -> Expectation {
    use super::run::MODE_LINK;
    use Count::{AtLeast, Exactly};
    let (run, link, rec) = (
        file(SINGLE, RUN),
        file(SINGLE, LINK),
        file(SINGLE, RECORDER),
    );
    let (inner_run, inner_link) = (file(INNER, RUN), file(INNER, LINK));
    let check =
        |id, inputs: &[File], events: &[Event]| check_rule(id, Outcome::Pass, inputs, events);
    Expectation {
        row: "p1-kill",
        binding: Binding::Run,
        roles: vec![
            Role {
                name: SINGLE,
                row: "p1-kill",
            },
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
                Exit::Killed,
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
            rule(SINGLE, RUN, "kill", Exactly(1)),
            rule(SINGLE, RUN, "exit", Exactly(4)),
            // The inner's link, recorder and mode link, the outer's link and
            // recorder.
            rule(SINGLE, RUN, "seal", Exactly(5)),
            rule(SINGLE, RUN, "quiet", Exactly(1)),
            rule(SINGLE, LINK, "open", Exactly(1)),
            rule(INNER, RUN, "held", Exactly(1)),
            rule(INNER, RUN, "barrier", Exactly(1)),
            rule(INNER, RUN, "mode", Exactly(1)),
            rule(INNER, RUN, "spawn", Exactly(2)),
            rule(INNER, RUN, "stop", Exactly(0)),
            rule(INNER, LINK, "open", Exactly(1)),
            rule(INNER, LINK, "check", AtLeast(1)),
        ],
        checks: vec![
            check(
                "provenance",
                &[link, inner_link, file(INNER, MODE_LINK)],
                &[ev(SINGLE, LINK, "ans"), ev(INNER, LINK, "ans")],
            ),
            check(
                "n2p.abnormal",
                &[inner_run, inner_link],
                &[ev(INNER, RUN, "barrier")],
            ),
            check(
                "n2p.neutral",
                &[run, rec, inner_run],
                &[
                    ev(SINGLE, RECORDER, "pad"),
                    ev(INNER, RUN, "barrier"),
                    ev(SINGLE, RUN, "kill"),
                ],
            ),
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
        negative: None,
    }
}

/// A reference row (`p1-ref`, `p1-ref-75`): the runner attaches to the
/// reference player's window, clicks the row's targets and captures shot A
/// twice; it starts no player and holds nothing on the device, so its link
/// makes no check and the release evidence's `hello` keeps its reason.
fn reference(row: &'static str) -> Expectation {
    use Count::{AtLeast, Exactly};
    let (run, link, rec) = (
        file(SINGLE, RUN),
        file(SINGLE, LINK),
        file(SINGLE, RECORDER),
    );
    let check =
        |id, inputs: &[File], events: &[Event]| check_rule(id, Outcome::Pass, inputs, events);
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
        ],
        records: vec![
            rule(role, RUN, "spawn", Exactly(1)),
            rule(role, RUN, "mode", Exactly(0)),
            rule(role, RUN, "player", Exactly(0)),
            rule(role, RUN, "sink", Exactly(1)),
            rule(role, RUN, "window", Exactly(0)),
            rule(role, RUN, "attach", Exactly(1)),
            rule(role, RUN, "call", AtLeast(9)),
            rule(role, RUN, "held", Exactly(0)),
            rule(role, RUN, "shot", Exactly(0)),
            rule(role, RUN, "capture", Exactly(1)),
            rule(role, RUN, "stop", Exactly(1)),
            rule(role, RUN, "quiet", Exactly(1)),
            rule(role, RUN, "close", Exactly(1)),
            rule(role, RUN, "exit", Exactly(1)),
            // The link, the recorder, the two captures.
            rule(role, RUN, "seal", Exactly(4)),
            rule(role, LINK, "open", Exactly(1)),
            rule(role, LINK, "fence", Exactly(1)),
            rule(role, LINK, "check", Exactly(0)),
        ],
        checks: vec![
            check("provenance", &[link], &[ev(SINGLE, LINK, "ans")]),
            check("ref.attached", &[run], &[ev(SINGLE, RUN, "attach")]),
            check(
                "p1.clicks",
                &[run, rec],
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RECORDER, "mouse")],
            ),
            check(
                "p1.no-foreign",
                &[run, rec],
                &[ev(SINGLE, RUN, "call"), ev(SINGLE, RUN, "quiet")],
            ),
            check(
                "ref.device-idle",
                &[link, rec],
                &[ev(SINGLE, LINK, "open"), ev(SINGLE, RUN, "quiet")],
            ),
            check(
                "ref.capture-bound",
                &[run, rec],
                &[ev(SINGLE, RUN, "capture"), ev(SINGLE, RECORDER, "mouse")],
            ),
            check(
                "p1.release-requested",
                &[run, link],
                &[ev(SINGLE, RUN, "stop"), ev(SINGLE, LINK, "ans")],
            ),
            check(
                "ref.quiet-before-close",
                &[run, link],
                &[ev(SINGLE, RUN, "close"), ev(SINGLE, LINK, "fence")],
            ),
        ],
        negative: None,
    }
}

/// `p1-ref`: the reference player's panel 3 with each volume target
/// clicked, as P1 clicks them.
fn p1_ref() -> Expectation {
    reference("p1-ref")
}

/// `p1-ref-75`: the comparison's control, slider 17 clicked at 75 %.
fn p1_ref_75() -> Expectation {
    reference("p1-ref-75")
}

/// The private comparison's name: it writes
/// `verdicts-panel-compare.jsonl` in a `p1-compare` set's folder.
pub const COMPARE: &str = "panel-compare";

/// The regions the comparison judges, per slider: its bar, its graph and
/// its number.
const COMPARED: [&str; 9] = [
    "band-17",
    "graph-17",
    "number-17",
    "band-18",
    "graph-18",
    "number-18",
    "band-19",
    "graph-19",
    "number-19",
];
/// Each slider's knob position, judged on its own.
const KNOBS: [&str; 3] = ["knob-17", "knob-18", "knob-19"];

/// `p1-compare`: tapestry's panel 3 against the reference player's, each
/// set by clicks on the test device. Its roles are two runs of P1 (`A1`,
/// `A2`), two of `p1-ref` (`R1`, `R2`) and one of `p1-ref-75` (`R75`),
/// reserved together; each role's run is held to its own row's
/// expectation (its checks named by the role), with one provenance over
/// every role's links. The private comparison judges each A run against
/// the R run of its pair (`A1` with `R1`, `A2` with `R2`) per region and
/// knob, the two R runs against each other, and two controls that must
/// fail: `A1` against `R75` (slider 17 differs, the others pass), and `A1`
/// against `R1` shifted 3 pixels (every slider).
fn p1_compare() -> Expectation {
    use super::run::{MODE_LINK, SHOT_A, SHOT_A_2};
    const ROLES: [(&str, &str); 5] = [
        ("A1", "p1"),
        ("A2", "p1"),
        ("R1", "p1-ref"),
        ("R2", "p1-ref"),
        ("R75", "p1-ref-75"),
    ];
    let second = SHOT_A_2;
    let mut exits = Vec::new();
    let mut records = Vec::new();
    let mut checks = vec![check_rule(
        "provenance",
        Outcome::Pass,
        &ROLES
            .iter()
            .flat_map(|&(role, row)| {
                std::iter::once(file(role, LINK))
                    .chain((row == "p1").then(|| file(role, MODE_LINK)))
            })
            .collect::<Vec<_>>(),
        &ROLES
            .iter()
            .map(|&(role, _)| ev(role, LINK, "ans"))
            .collect::<Vec<_>>(),
    )];
    for (role, row) in ROLES {
        let e = expectation_of(row).and_then(|e| e.as_role(role));
        let Some(r) = e else {
            continue;
        };
        exits.extend(r.exits);
        records.extend(r.records);
        checks.extend(r.checks);
    }
    let inputs = |a: &'static str, r: &'static str| -> Vec<File> {
        let mut v = vec![file(a, RUN), file(a, SHOT_A), file(r, RUN), file(r, SHOT_A)];
        if r.starts_with('R') {
            v.push(file(r, second));
        }
        if a.starts_with('R') {
            v.insert(2, file(a, second));
        }
        v
    };
    let events = |a: &'static str, r: &'static str| -> Vec<Event> {
        [a, r]
            .into_iter()
            .map(|x| {
                if x.starts_with('A') {
                    ev(x, RUN, "shot")
                } else {
                    ev(x, RUN, "capture")
                }
            })
            .collect()
    };
    let compare = |id: String, outcome, a, r| CheckRule {
        id: id.into(),
        producer: COMPARE,
        outcome,
        inputs: inputs(a, r),
        events: events(a, r),
    };
    for (pair, a, r) in [
        ("a1-r1", "A1", "R1"),
        ("a2-r2", "A2", "R2"),
        ("r1-r2", "R1", "R2"),
    ] {
        for m in COMPARED.iter().chain(&KNOBS) {
            checks.push(compare(format!("compare.{pair}.{m}"), Outcome::Pass, a, r));
        }
    }
    for m in COMPARED.iter().chain(&KNOBS) {
        let outcome = if m.ends_with("-17") {
            Outcome::Fail
        } else {
            Outcome::Pass
        };
        checks.push(compare(format!("compare.a1-r75.{m}"), outcome, "A1", "R75"));
    }
    for slider in [17, 18, 19] {
        checks.push(compare(
            format!("compare.a1-r1-shift3.slider-{slider}"),
            Outcome::Fail,
            "A1",
            "R1",
        ));
    }
    Expectation {
        row: "p1-compare",
        binding: Binding::Set,
        roles: ROLES
            .iter()
            .map(|&(name, row)| Role { name, row })
            .collect(),
        exits,
        records,
        checks,
        negative: None,
    }
}

/// A panel run's shots bound to the player and to their phases, from the
/// run's and the recorder's records alone, not from the runner's steps: the
/// run stream holds exactly one `shot` record of each shot, naming the
/// player's process (its `spawn` record) as the shot's maker and its pid as
/// the pipe's client, at the client size the rows run at, its frame
/// acquired after the request and presented after its acquisition, its
/// present advancing the swap chain's present count. Shot A is requested
/// after the recorder's button-up of the run's last click (the mouse report
/// tagged with the last `click` call's tag: the click landed, not only its
/// call logged), and its frame acquired after the recorder's first report
/// of the row's buttons
/// all held and presented before its next report that releases any of
/// them; shot B is requested and its frame acquired after the recorder's
/// first neutral report (no button) after that, and presented before the
/// run's end begins (its `stop`). Each mismatch, named; a stream that does
/// not read is the contract's to name.
fn shots_bound(dir: &Path, row: &str) -> Vec<String> {
    /// A recorder mouse report's left-button release edge.
    const LEFT_UP: u64 = 2;
    use super::run::{SHOT_A, SHOT_B, pad_buttons};
    let files = super::judge::Files::read(dir);
    let (Ok(records), Ok(reports)) = (
        files.run.records.as_deref(),
        files.recorder.records.as_deref(),
    ) else {
        return Vec::new();
    };
    let Some(mask) =
        super::judge::held_by(row).map(|b| b.iter().fold(0_u16, |m, &n| m | 1 << (n - 1)))
    else {
        return vec![format!("row {row} holds no buttons")];
    };
    let one =
        |kind: &str, pick: &dyn Fn(&serde_json::Value) -> bool| -> Option<serde_json::Value> {
            let all: Vec<_> = records
                .iter()
                .filter(|v| v["t"] == kind && pick(v))
                .collect();
            match all[..] {
                [v] => Some(v.clone()),
                _ => None,
            }
        };
    let player = one("spawn", &|v| v["name"] == "player").map(|v| v["process"].clone());
    // The device's own pad reports, in the recorder's order.
    let pads: Vec<(i64, u16)> = reports
        .iter()
        .filter(|v| v["t"] == "pad" && v["ours"] == true)
        .filter_map(|v| Some((v["q"].as_i64()?, pad_buttons(v["bytes"].as_str()?)?)))
        .collect();
    let held = pads.iter().position(|&(_, b)| b & mask == mask);
    let after = |pick: &dyn Fn(u16) -> bool| {
        held.and_then(|h| {
            pads.iter()
                .skip(h + 1)
                .find(|&&(_, b)| pick(b))
                .map(|&(q, _)| q)
        })
    };
    let held_q = held.and_then(|h| pads.get(h)).map(|&(q, _)| q);
    let release_q = after(&|b| b & mask != mask);
    let neutral_q = after(&|b| b == 0);
    // The recorder's button-ups of the run's last click.
    let last_click = records
        .iter()
        .filter(|v| v["t"] == "call" && v["what"] == "click")
        .filter_map(|v| Some((v["q"].as_i64()?, v["tag"].as_u64()?)))
        .max();
    let last_ups: Vec<i64> = last_click.map_or_else(Vec::new, |(_, tag)| {
        reports
            .iter()
            .filter(|v| {
                v["t"] == "mouse"
                    && v["extra"].as_u64() == Some(tag)
                    && v["buttons"].as_u64().is_some_and(|b| b & LEFT_UP != 0)
            })
            .filter_map(|v| v["q"].as_i64())
            .collect()
    });
    let stop_q = records
        .iter()
        .find(|v| v["t"] == "stop")
        .and_then(|v| v["q"].as_i64());
    let mut out = Vec::new();
    for (name, phase) in [(SHOT_A, "held"), (SHOT_B, "released")] {
        let Some(shot) = one("shot", &|v| v["what"] == name) else {
            out.push(format!("{name} has not exactly one `shot` record"));
            continue;
        };
        if player.as_ref() != Some(&shot["process"]) {
            out.push(format!(
                "{name} was made by process {}, not the player's {player:?}",
                shot["process"]
            ));
        }
        if player.as_ref().map(|p| &p["pid"]) != Some(&shot["client"]) {
            out.push(format!(
                "{name} came from pipe client {}, not the player's {player:?}",
                shot["client"]
            ));
        }
        if shot["size"] != serde_json::json!(test_device::panel::CLIENT) {
            out.push(format!(
                "{name} is {}, not the rows' client size {:?}",
                shot["size"],
                test_device::panel::CLIENT
            ));
        }
        let (Some(requested), Some(acquired), Some(presented)) = (
            shot["requested"].as_i64(),
            shot["frame"]["acquired"].as_i64(),
            shot["frame"]["presented"].as_i64(),
        ) else {
            out.push(format!("{name}'s record has no request or frame counters"));
            continue;
        };
        let counts = (
            shot["frame"]["previous"].as_u64(),
            shot["frame"]["present"].as_u64(),
        );
        if !matches!(counts, (Some(p), Some(c)) if c > p) {
            out.push(format!(
                "{name}'s present did not advance the swap chain's count: {counts:?}"
            ));
        }
        let mut order = |ok: bool, what: String| {
            if !ok {
                out.push(format!("{name} ({phase}): {what}"));
            }
        };
        order(
            acquired > requested,
            format!("its frame was acquired at {acquired}, not after its request at {requested}"),
        );
        order(
            presented >= acquired,
            format!("its frame was presented at {presented}, before its acquisition at {acquired}"),
        );
        if name == SHOT_A {
            order(
                matches!(last_ups[..], [up] if requested > up),
                format!(
                    "requested at {requested}, not after the last click's one recorded button-up {last_ups:?}"
                ),
            );
            order(
                held_q.is_some_and(|h| acquired > h),
                format!("acquired at {acquired}, not after the buttons held at {held_q:?}"),
            );
            order(
                release_q.is_some_and(|r| presented < r),
                format!("presented at {presented}, not before a button's release at {release_q:?}"),
            );
        } else {
            order(
                neutral_q.is_some_and(|n| requested > n && acquired > n),
                format!(
                    "requested at {requested} and acquired at {acquired}, not both after the neutral report at {neutral_q:?}"
                ),
            );
            order(
                stop_q.is_some_and(|s| presented < s),
                format!("presented at {presented}, not before the run's end at {stop_q:?}"),
            );
        }
    }
    out
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
    let mut report = contract::check(&e, dir, super::COMMIT);
    if super::judge::held_by(row).is_some() {
        report.mismatches.extend(shots_bound(dir, row));
    }
    // A set's panel roles: each role's shots, bound in its own folder.
    if e.binding == Binding::Set
        && let Ok(Some(set)) = super::owner::reservation(dir)
    {
        for role in &e.roles {
            let Some(r) = set.roles.iter().find(|r| r.role == role.name) else {
                continue;
            };
            if super::judge::held_by(role.row).is_some() && contract::one_segment(&r.dir) {
                report.mismatches.extend(
                    shots_bound(&dir.join(&r.dir), role.row)
                        .into_iter()
                        .map(|m| format!("role `{}`: {m}", role.name)),
                );
            }
        }
    }
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
                id: "provenance".into(),
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
        let ids = |e: &Expectation| e.checks.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        let failing = |e: &Expectation| {
            e.checks
                .iter()
                .filter(|c| c.outcome == Outcome::Fail)
                .map(|c| c.id.clone())
                .collect::<Vec<_>>()
        };
        assert!(failing(&p1()).is_empty());
        assert_eq!(failing(&p4()), ["p1.volumes", "p1.volume-arm", FRAME_A]);
        assert_eq!(failing(&p5()), ["p1.volumes", FRAME_A]);
        assert!(failing(&p1_kill()).is_empty());
        // A negative's checks are its positive row's, in the same order,
        // then its own, which pass.
        for (neg, own) in [
            (
                p4(),
                vec!["p4.volumes-kept", "p4.no-volume-arm", DECLARED_A],
            ),
            (
                p5(),
                vec!["p5.volumes-kept", "p5.mutation-build", DECLARED_A],
            ),
        ] {
            let all = ids(&neg);
            let base = ids(&p1());
            assert_eq!(all[..base.len()], base[..]);
            assert_eq!(all[base.len()..], own[..]);
        }
        let neg = sink_early_close();
        let failing: Vec<&str> = neg
            .checks
            .iter()
            .filter(|c| c.outcome == Outcome::Fail)
            .map(|c| &*c.id)
            .collect();
        assert_eq!(failing, ["n1.quiet-before-close"]);
        // The negative's checks are the row's, in the same order.
        assert_eq!(ids(&neg), ids(&sink()));
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

    /// How a test's panel run folder differs from a good P1 run's.
    #[derive(Default)]
    struct Folder<'a> {
        /// The player kept the volumes at 1.0 (as P4's and P5's do).
        left: bool,
        /// `device.toml` after the run, as given.
        after: Option<&'a str>,
        /// `device.toml` before the run, as given.
        before: Option<&'a str>,
        /// The private judge's name, in place of [`PANEL`].
        producer: Option<&'a str>,
        /// A normal row's held copy is P5's mutated build, not the runner's
        /// own it expected.
        other_player: bool,
        /// The private judge's checks that failed.
        failing: &'a [&'a str],
        /// The player's executable is another than the declared one.
        stale: bool,
        /// An edit of the run's records before they are written (the
        /// shot records).
        shots: Option<fn(&mut Vec<serde_json::Value>)>,
        /// An edit of the recorder's records before they are written.
        reports: Option<fn(&mut Vec<serde_json::Value>)>,
        /// The panel verdicts leave the run's stream out of their inputs.
        unbound: bool,
        /// The folder is a set's role: its reservation, and no verdicts of
        /// its own (the set's are in the set's folder).
        role: Option<SetRole<'a>>,
        /// The shots' bytes from this folder's files of their names, in
        /// place of a stand-in.
        shot_files: Option<&'a std::path::Path>,
    }

    /// A role of a set as its run's `start` names it.
    #[derive(Clone, Copy)]
    struct SetRole<'a> {
        role: &'a str,
        run: &'a str,
        set: &'a str,
        cut: u32,
    }

    /// Writes a panel row's run folder as the runner writes it, from the
    /// judge's good panel records in their full form, every file sealed, as
    /// `how` says (a P5 folder declares its player's digest). Then the
    /// judge's verdicts, and the panel judge's verdict lines for the row.
    fn panel_folder(dir: &std::path::Path, row: &str, how: &Folder<'_>) {
        let failing = how.failing;
        use serde_json::{Value, json};

        use super::super::launch::{
            DEVICE_AFTER, DEVICE_BEFORE, PLAYER_AFTER, PLAYER_BEFORE, STDERR, STDOUT,
        };
        use super::super::run::{MODE_LINK, SHOT_A, SHOT_B};
        let (run_id, role, set, cut) = how.role.map_or(
            (
                "0123456789abcdef0123456789abcdef",
                SINGLE,
                None,
                0x4000_0123,
            ),
            |r| (r.run, r.role, Some(r.set), r.cut),
        );
        let me = json!({"pid": 1, "created": 2});
        let rec = json!({"pid": 7, "created": 70});
        let player = json!({"pid": 8, "created": 80});
        let (f, x) = super::super::judge::tests::panel_good();
        let mut run = f.run.records.unwrap();
        // The folder's own `player` record replaces the judge fixture's.
        run.retain(|v| v["t"] != "player");
        let mut link = f.link.records.unwrap();
        let mut recorder = f.recorder.records.unwrap();
        if row == "p4" {
            // START alone.
            for v in recorder
                .iter_mut()
                .filter(|v| v["t"] == "pad" && v["q"] == 1500)
            {
                *v = super::super::judge::tests::panel_pad(1500, 1 << 8, 2, true);
            }
            // Its clicks reach no volume slider.
            for v in run.iter_mut().filter(|v| v["t"] == "shot") {
                v["volumes"] = json!([]);
            }
        }
        run[0] = json!({
            "t": "start", "run": run_id, "row": row, "role": role, "build": "0123456789ab",
            "process": me, "commit": super::super::COMMIT, "tag": 0xABCD, "cut": cut,
            "args": [], "set": set, "inner": null,
        });
        let end = run.pop().unwrap();
        let hello = recs_line(&x.mode);
        let (mutated, head) = ("5a".repeat(32), "4e".repeat(32));
        let player_record = if row == "p5" {
            let sha = if how.stale {
                "3c".repeat(32)
            } else {
                mutated.clone()
            };
            json!({"t": "player", "q": 45, "path": "d/player-bin/t.exe", "files": [{"name": "tapestry.exe", "sha256": sha}], "source": "p5/t.exe", "sha256": sha, "expected": mutated, "declared": mutated,
                   "runner_sha256": head, "commit": "nogit", "modified": 1})
        } else {
            let sha = if how.other_player {
                mutated.clone()
            } else {
                head.clone()
            };
            json!({"t": "player", "q": 45, "path": "d/player-bin/t.exe", "files": [{"name": "tapestry.exe", "sha256": sha}], "source": "t.exe", "sha256": sha, "expected": head, "declared": null,
                   "runner_sha256": head, "commit": "nogit", "modified": 1})
        };
        run.splice(
            1..1,
            [
                json!({"t": "mode", "q": 20, "line": hello}),
                json!({"t": "spawn", "q": 40, "name": "recorder", "process": rec}),
                player_record,
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
        if let Some(edit) = how.reports {
            edit(&mut recorder);
        }
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
        if how.left {
            device_after = x.device_before.clone().unwrap();
        }
        if let Some(a) = how.after {
            device_after = a.as_bytes().to_vec();
        }
        let files: Vec<(&str, Vec<u8>)> = vec![
            (LINK, lines(&link).into_bytes()),
            (RECORDER, lines(&recorder).into_bytes()),
            (MODE_LINK, lines(&mode).into_bytes()),
            (
                DEVICE_BEFORE,
                how.before
                    .map_or_else(|| x.device_before.unwrap(), |b| b.as_bytes().to_vec()),
            ),
            (PLAYER_BEFORE, x.player_before.unwrap()),
            (DEVICE_AFTER, device_after),
            (PLAYER_AFTER, x.player_after.unwrap()),
            (STDOUT, Vec::new()),
            (STDERR, Vec::new()),
            (SHOT_A, b"a".to_vec()),
            (SHOT_B, b"b".to_vec()),
        ];
        for (name, bytes) in &files {
            let bytes = match (name, how.shot_files) {
                (&(SHOT_A | SHOT_B), Some(from)) => std::fs::read(from.join(name)).unwrap(),
                _ => bytes.clone(),
            };
            std::fs::write(dir.join(name), bytes).unwrap();
            run.push(super::super::owner::seal(dir, &dir.join(name), run_id, role).unwrap());
        }
        run.push(end);
        if let Some(edit) = how.shots {
            edit(&mut run);
        }
        std::fs::write(dir.join(RUN), lines(&run)).unwrap();
        if how.role.is_some() {
            return;
        }
        super::super::judge::judge(row, dir).unwrap();
        let sha = |p: &str| super::super::owner::digest(&dir.join(p)).unwrap();
        // The panel judge's lines, as that judge writes them for the row
        // (its own table, not the expectation under test): each check
        // reading the run's stream, the recorder's and its shot (the run's
        // stream left out when `unbound`).
        let producer = how.producer.unwrap_or(PANEL);
        let panel: String = panel_judge_checks(row)
            .into_iter()
            .map(|(id, shot)| {
                let outcome = if failing.contains(&id) {
                    "fail"
                } else {
                    "pass"
                };
                let inputs: Vec<serde_json::Value> = ["run.jsonl", "recorder.jsonl", shot]
                    .into_iter()
                    .filter(|p| !(how.unbound && *p == "run.jsonl"))
                    .map(|p| json!({"role": "run", "path": p, "sha256": sha(p)}))
                    .collect();
                format!(
                    "{}\n",
                    json!({
                        "row": row, "run": run_id, "check": id, "outcome": outcome, "detail": "",
                        "inputs": inputs, "producer": {"name": producer, "version": "0"},
                    })
                )
            })
            .collect();
        std::fs::write(dir.join(format!("verdicts-{producer}.jsonl")), panel).unwrap();
    }

    /// Writes a reference row's run folder for role `r` of a set, from the
    /// judge's good reference records in their full form, every file
    /// sealed; its captures' bytes from `shots` when given.
    fn ref_folder(
        dir: &std::path::Path,
        row: &str,
        r: SetRole<'_>,
        shots: Option<&std::path::Path>,
    ) {
        use serde_json::{Value, json};

        use super::super::run::{SHOT_A, SHOT_A_2};
        let me = json!({"pid": 3, "created": 4});
        let rec = json!({"pid": 9, "created": 90});
        let f = super::super::judge::tests::ref_good(row);
        let mut run = f.run.records.unwrap();
        let mut link = f.link.records.unwrap();
        let mut recorder = f.recorder.records.unwrap();
        run[0] = json!({
            "t": "start", "run": r.run, "row": row, "role": r.role, "build": "0123456789ab",
            "process": me, "commit": super::super::COMMIT, "tag": 0xABCD, "cut": r.cut,
            "args": [], "set": r.set, "inner": null,
        });
        run.insert(
            1,
            json!({"t": "spawn", "q": 40, "name": "recorder", "process": rec}),
        );
        let end = run.pop().unwrap();
        let last = run
            .iter()
            .map(|v| v["q"].as_i64().unwrap_or(0))
            .max()
            .unwrap();
        run.push(
            json!({"t": "exit", "q": last + 300, "name": "recorder", "process": rec, "code": 0}),
        );
        link[0] = json!({
            "t": "open", "q": 10, "run": r.run, "process": me, "freq": 10_000_000,
            "location": "L", "com": "COM9", "path": "p",
        });
        link.push(json!({"t": "end", "q": last + 100, "problems": []}));
        recorder[0] = json!({
            "t": "ready", "q": 50, "run": r.run, "process": rec, "window": 9,
            "location": "L", "keyboard": 1, "pad": 2,
        });
        let lines =
            |records: &[Value]| -> String { records.iter().map(|v| format!("{v}\n")).collect() };
        let shot = |name: &str, stand_in: &[u8]| {
            shots.map_or_else(
                || stand_in.to_vec(),
                |from| std::fs::read(from.join(name)).unwrap(),
            )
        };
        let files: Vec<(&str, Vec<u8>)> = vec![
            (LINK, lines(&link).into_bytes()),
            (RECORDER, lines(&recorder).into_bytes()),
            (SHOT_A, shot(SHOT_A, b"r")),
            (SHOT_A_2, shot(SHOT_A_2, b"r")),
        ];
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
            run.push(super::super::owner::seal(dir, &dir.join(name), r.run, r.role).unwrap());
        }
        run.push(end);
        std::fs::write(dir.join(RUN), lines(&run)).unwrap();
    }

    /// Writes a `p1-compare` set in the new folder `dir` as its runs write
    /// it: the reservation, each role's run folder (A1 and A2 P1's, R1 and
    /// R2 `p1-ref`'s, R75 `p1-ref-75`'s; the shots from
    /// `shots/<role>/` when given), and the judge's set verdicts. The
    /// private verdicts are the caller's.
    fn set_folder(
        dir: &std::path::Path,
        shots: Option<&std::path::Path>,
    ) -> test_device::evidence::Set {
        super::super::reserve::reserve("p1-compare", dir, "0123456789ab").unwrap();
        let set = super::super::owner::reservation(dir).unwrap().unwrap();
        for (k, r) in set.roles.iter().enumerate() {
            let folder = dir.join(&r.dir);
            std::fs::create_dir(&folder).unwrap();
            let role = SetRole {
                role: &r.role,
                run: &r.run,
                set: &set.set,
                cut: 0x4000_0200 + u32::try_from(k).unwrap(),
            };
            let from = shots.map(|s| s.join(&r.role));
            if r.row == "p1" {
                panel_folder(
                    &folder,
                    "p1",
                    &Folder {
                        role: Some(role),
                        shot_files: from.as_deref(),
                        ..Folder::default()
                    },
                );
            } else {
                ref_folder(&folder, &r.row, role, from.as_deref());
            }
        }
        super::super::judge::judge("p1-compare", dir).unwrap();
        set
    }

    /// The private verdicts of a set as the private judges write them:
    /// the whole-frame judge's for A1's and A2's shots, and the
    /// comparison's per pair, region and knob, with `outcome` for each id.
    fn private_verdicts(
        dir: &std::path::Path,
        set: &test_device::evidence::Set,
        outcome: &dyn Fn(&str) -> &'static str,
    ) {
        use serde_json::json;
        let folder = |role: &str| {
            let r = set.roles.iter().find(|r| r.role == role).unwrap();
            dir.join(&r.dir)
        };
        let input = |role: &str, path: &str| {
            json!({"role": role, "path": path,
                   "sha256": super::super::owner::digest(&folder(role).join(path)).unwrap()})
        };
        let line = |id: &str, inputs: Vec<serde_json::Value>, producer: &str| {
            format!(
                "{}\n",
                json!({"row": "p1-compare", "run": set.set, "check": id, "outcome": outcome(id),
                       "detail": "", "inputs": inputs, "producer": {"name": producer, "version": "0"}})
            )
        };
        let mut exact = String::new();
        for role in ["A1", "A2"] {
            for (id, shot) in [
                ("panel.frame-a", "shot-a.png"),
                ("panel.frame-b", "shot-b.png"),
            ] {
                exact += &line(
                    &format!("{role}/{id}"),
                    vec![
                        input(role, "run.jsonl"),
                        input(role, "recorder.jsonl"),
                        input(role, shot),
                    ],
                    PANEL,
                );
            }
        }
        std::fs::write(dir.join(format!("verdicts-{PANEL}.jsonl")), exact).unwrap();
        let side = |role: &str| {
            let mut v = vec![input(role, "run.jsonl"), input(role, "shot-a.png")];
            if role.starts_with('R') {
                v.push(input(role, "shot-a-2.png"));
            }
            v
        };
        let mut compare = String::new();
        let regions: Vec<String> = [17, 18, 19]
            .iter()
            .flat_map(|n| ["band", "graph", "number"].map(|k| format!("{k}-{n}")))
            .chain([17, 18, 19].map(|n| format!("knob-{n}")))
            .collect();
        for (pair, a, r) in [
            ("a1-r1", "A1", "R1"),
            ("a2-r2", "A2", "R2"),
            ("r1-r2", "R1", "R2"),
            ("a1-r75", "A1", "R75"),
        ] {
            for m in &regions {
                compare += &line(
                    &format!("compare.{pair}.{m}"),
                    [side(a), side(r)].concat(),
                    COMPARE,
                );
            }
        }
        for n in [17, 18, 19] {
            compare += &line(
                &format!("compare.a1-r1-shift3.slider-{n}"),
                [side("A1"), side("R1")].concat(),
                COMPARE,
            );
        }
        std::fs::write(dir.join(format!("verdicts-{COMPARE}.jsonl")), compare).unwrap();
    }

    /// The comparison's outcomes as the set must have them: slider 17
    /// differs against the 75 % control, and every shifted slider differs.
    fn set_outcome(id: &str) -> &'static str {
        let control = id.starts_with("compare.a1-r75.") && id.ends_with("-17");
        if control || id.starts_with("compare.a1-r1-shift3.") {
            "fail"
        } else {
            "pass"
        }
    }

    fn check_set(dir: &std::path::Path) -> (bool, String) {
        let mut out = Vec::new();
        let r = run(ROWS, "p1-compare", dir, &mut out);
        (r.is_ok(), String::from_utf8(out).unwrap())
    }

    /// A `p1-compare` set written as its runs and judges write it matches
    /// the row: each role's run bound by the reservation and held to its
    /// own row's checks under the role's name, one provenance over every
    /// role's links, the shots of each panel role bound in its folder, and
    /// the comparison's controls failing as declared. A set whose
    /// comparison passes a control, whose role folders are swapped, or
    /// whose role runs the wrong row does not match.
    #[test]
    fn a_p1_compare_set_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("set");
        let set = set_folder(&dir, None);
        private_verdicts(&dir, &set, &set_outcome);
        let (ok, out) = check_set(&dir);
        assert!(ok, "{out}");
        assert!(out.ends_with("CONTRACT p1-compare ok\n"), "{out}");
        let judged = std::fs::read_to_string(dir.join("verdicts-judge.jsonl")).unwrap();
        for id in [
            "\"provenance\"",
            "\"A1/p1.volumes\"",
            "\"R75/ref.capture-bound\"",
            "\"R1/p1.clicks\"",
        ] {
            assert!(judged.contains(id), "{id}: {judged}");
        }
        // The control passing (a comparison that cannot tell 25 % from
        // 75 %) is a mismatch.
        private_verdicts(&dir, &set, &|_| "pass");
        let (ok, out) = check_set(&dir);
        assert!(!ok && out.contains("compare.a1-r75.band-17"), "{out}");

        // A1's folder in R1's place: the runs no longer bind to their roles.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("set");
        let set = set_folder(&dir, None);
        private_verdicts(&dir, &set, &set_outcome);
        let (a1, r1) = (dir.join("A1"), dir.join("R1"));
        std::fs::rename(&r1, dir.join("R1.kept")).unwrap();
        copy_dir(&a1, &r1);
        let (ok, out) = check_set(&dir);
        assert!(!ok && out.contains("role `R1`"), "{out}");
    }

    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }

    /// Writes a `p1-compare` set for the private judges' offline checks:
    /// the set in `$B4_SET_FIXTURE` (a new folder) with each role's shots
    /// from `$B4_SET_SHOTS/<role>/`, and the judge's set verdicts; the
    /// private judges then write theirs and `check` reads the set.
    #[test]
    #[ignore = "writes a fixture for the private judges: run with $B4_SET_FIXTURE and $B4_SET_SHOTS"]
    fn write_p1_compare_fixture() {
        let out = std::env::var_os("B4_SET_FIXTURE").expect("$B4_SET_FIXTURE");
        let shots = std::env::var_os("B4_SET_SHOTS").expect("$B4_SET_SHOTS");
        let set = set_folder(
            std::path::Path::new(&out),
            Some(std::path::Path::new(&shots)),
        );
        println!("set {} roles {}", set.set, set.roles.len());
    }

    /// The private panel judge's checks of `row` and the shot each reads,
    /// written out as that judge names them.
    fn panel_judge_checks(row: &str) -> Vec<(&'static str, &'static str)> {
        let a = "shot-a.png";
        let mut out = vec![("panel.frame-a", a)];
        if matches!(row, "p4" | "p5") {
            out.push(("panel.declared-a", a));
        }
        out.push(("panel.frame-b", "shot-b.png"));
        out
    }

    /// The frame checks' verdicts must come from the whole-frame judge: the
    /// same lines (row, run, outcomes, inputs and their digests) written by
    /// the earlier masked judge (`panel`) are a mismatch, in every panel
    /// row.
    #[test]
    fn the_frame_checks_are_the_whole_frame_judge_s() {
        for (row, failing) in [
            ("p1", &[][..]),
            ("p4", &[FRAME_A][..]),
            ("p5", &[FRAME_A][..]),
        ] {
            let left = row != "p1";
            let (ok, out) = verdict(
                row,
                &Folder {
                    left,
                    failing,
                    ..Folder::default()
                },
            );
            assert!(ok, "{row}: {out}");
            let (ok, out) = verdict(
                row,
                &Folder {
                    left,
                    failing,
                    producer: Some("panel"),
                    ..Folder::default()
                },
            );
            assert!(!ok, "{row}: {out}");
            assert!(
                out.contains(
                    "check `panel.frame-a` is produced by `panel`, the row declares `panel-exact`"
                ),
                "{row}: {out}"
            );
        }
    }

    /// The volume arm's witness binds each target's press: in a P1 folder, a
    /// press entry before its click call (stale), two presses swapped, or
    /// only drags is a mismatch on `p1.volume-arm`. In a P4 folder whose
    /// captures and settings are P4's, one press reaching the arm is a
    /// mismatch on `p4.no-volume-arm`. In P4 and P5 folders whose settings
    /// after the run keep 1.0, a volume off 1.0 before it is a mismatch on
    /// `p1.volumes-start`.
    #[test]
    fn a_panel_row_binds_its_presses_and_its_starting_volumes() {
        type Edit = fn(&mut Vec<serde_json::Value>);
        fn witness(r: &mut [serde_json::Value], vols: &serde_json::Value) {
            for v in r.iter_mut().filter(|v| v["t"] == "shot") {
                v["volumes"] = vols.clone();
            }
        }
        let cases: [(&str, Edit); 3] = [
            ("stale", |r| {
                witness(
                    r,
                    &serde_json::json!([
                        [17, 25, 230, "press"],
                        [18, 50, 315, "press"],
                        [19, 75, 335, "press"]
                    ]),
                );
            }),
            ("swapped", |r| {
                witness(
                    r,
                    &serde_json::json!([
                        [18, 50, 295, "press"],
                        [17, 25, 315, "press"],
                        [19, 75, 335, "press"]
                    ]),
                );
            }),
            ("drag-only", |r| {
                witness(
                    r,
                    &serde_json::json!([
                        [17, 25, 295, "drag"],
                        [18, 50, 315, "drag"],
                        [19, 75, 335, "drag"]
                    ]),
                );
            }),
        ];
        for (why, edit) in cases {
            let (ok, out) = verdict(
                "p1",
                &Folder {
                    shots: Some(edit),
                    ..Folder::default()
                },
            );
            assert!(!ok, "{why}: {out}");
            assert!(
                out.contains("check `p1.volume-arm` is Fail"),
                "{why}: {out}"
            );
        }
        let (ok, out) = verdict(
            "p4",
            &Folder {
                left: true,
                failing: &[FRAME_A],
                shots: Some(|r| witness(r, &serde_json::json!([[17, 100, 295, "press"]]))),
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("check `p4.no-volume-arm` is Fail"), "{out}");
        let before = super::super::judge::tests::DEVICE_TOML
            .replace("system_volume = 1.0", "system_volume = 0.25");
        for row in ["p4", "p5"] {
            let (ok, out) = verdict(
                row,
                &Folder {
                    left: true,
                    failing: &[FRAME_A],
                    before: Some(before.as_str()),
                    ..Folder::default()
                },
            );
            assert!(!ok, "{row}: {out}");
            assert!(
                out.contains("check `p1.volumes-start` is Fail"),
                "{row}: {out}"
            );
        }
    }

    /// A normal row binds its player and every answer's witness: P4 run on
    /// P5's mutated build (frames, settings and witnesses otherwise P4's) is
    /// a mismatch on `p1.player-bound`; P4 with a dispatch only in shot B's
    /// witness is one on `p4.no-volume-arm`; P1 with an extra entry only in
    /// shot B's is one on `p1.volume-arm`.
    #[test]
    fn a_panel_row_binds_its_player_and_every_witness() {
        let (ok, out) = verdict(
            "p4",
            &Folder {
                left: true,
                failing: &[FRAME_A],
                other_player: true,
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("check `p1.player-bound` is Fail"), "{out}");
        let (ok, out) = verdict(
            "p4",
            &Folder {
                left: true,
                failing: &[FRAME_A],
                shots: Some(|r| {
                    for v in r.iter_mut().filter(|v| v["what"] == "shot-b.png") {
                        v["volumes"] = serde_json::json!([[17, 100, 2600, "press"]]);
                    }
                }),
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("check `p4.no-volume-arm` is Fail"), "{out}");
        let (ok, out) = verdict(
            "p1",
            &Folder {
                shots: Some(|r| {
                    for v in r.iter_mut().filter(|v| v["what"] == "shot-b.png") {
                        if let Some(w) = v["volumes"].as_array_mut() {
                            w.push(serde_json::json!([18, 100, 2600, "press"]));
                        }
                    }
                }),
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("check `p1.volume-arm` is Fail"), "{out}");
    }

    fn recs_line(s: &super::super::judge::Stream) -> String {
        s.records.as_ref().unwrap()[0]["line"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn verdict(row: &str, how: &Folder<'_>) -> (bool, String) {
        let dir = tempfile::tempdir().unwrap();
        panel_folder(dir.path(), row, how);
        let mut out = Vec::new();
        let r = run(ROWS, row, dir.path(), &mut out);
        (r.is_ok(), String::from_utf8(out).unwrap())
    }

    /// Each panel verdict reads the run's and the recorder's streams and
    /// its shot, and each shot is the player's frame of its phase: a folder
    /// whose shot record names another process or pipe client, another
    /// size, or has no record of a shot or two of it, is a mismatch naming
    /// that shot; so is a present that did not advance the swap chain's
    /// count, a shot A requested before the last click's recorded
    /// button-up (the call logged before the request, its click recorded
    /// after it: a late pair), acquired before the buttons were held or presented after
    /// a release, a shot B requested or acquired before the neutral report
    /// or presented after the run's end began, and a frame acquired before
    /// its request or presented before its acquisition. A panel verdict
    /// that leaves the run's stream out of its inputs is a mismatch. The
    /// control matches.
    #[test]
    fn a_panel_verdict_binds_the_run_and_the_player_s_shot() {
        use super::super::run::{SHOT_A, SHOT_B};
        let (ok, out) = verdict("p1", &Folder::default());
        assert!(ok, "{out}");
        type Edit = fn(&mut Vec<serde_json::Value>);
        fn at(r: &mut [serde_json::Value], shot: &str) -> usize {
            r.iter().position(|v| v["what"] == shot).unwrap()
        }
        // The good folder: calls up to q 330, the last click's button-up
        // recorded at 246, held 1500, shot A requested
        // 2400 (frame 2410-2420), the neutral report 3000, shot B requested
        // 3400 (frame 3410-3420), the end's stop 5000.
        let edits: [(&str, &str, Edit); 15] = [
            ("another process", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["process"]["created"] = serde_json::json!(81);
            }),
            ("another pipe client", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["client"] = serde_json::json!(9);
            }),
            ("another size", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["size"] = serde_json::json!([2560, 1440]);
            }),
            ("no record", SHOT_B, |r| r.retain(|v| v["what"] != SHOT_B)),
            ("two records", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                let again = r[i].clone();
                r.insert(i, again);
            }),
            // Requested before the last click's recorded button-up (q 246),
            // its frame taken after the buttons were held: only the
            // last-click order fails.
            ("A before the last click's button-up", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["requested"] = serde_json::json!(245);
            }),
            ("A before the buttons held", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["requested"] = serde_json::json!(1400);
                r[i]["frame"]["acquired"] = serde_json::json!(1450);
            }),
            ("A presented after a release", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["frame"]["presented"] = serde_json::json!(3005);
            }),
            ("B before the neutral report", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["requested"] = serde_json::json!(2900);
                r[i]["frame"]["acquired"] = serde_json::json!(2950);
            }),
            ("B taken while held (stale)", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["requested"] = serde_json::json!(2430);
                r[i]["frame"] = serde_json::json!(
                    {"previous": 30, "present": 31, "acquired": 2440, "presented": 2450}
                );
            }),
            ("B presented after the end began", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["frame"]["presented"] = serde_json::json!(5001);
            }),
            ("a frame acquired before its request", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["frame"]["acquired"] = serde_json::json!(3400);
            }),
            ("a frame presented before its acquisition", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["frame"]["presented"] = serde_json::json!(2409);
            }),
            ("a present that did not advance", SHOT_B, |r| {
                let i = at(r, SHOT_B);
                r[i]["frame"]["previous"] = serde_json::json!(80);
            }),
            ("a present count that went back", SHOT_A, |r| {
                let i = at(r, SHOT_A);
                r[i]["frame"]["previous"] = serde_json::json!(31);
            }),
        ];
        for (what, shot, edit) in edits {
            let (ok, out) = verdict(
                "p1",
                &Folder {
                    shots: Some(edit),
                    ..Folder::default()
                },
            );
            assert!(!ok, "{what}: {out}");
            assert!(
                out.lines()
                    .any(|l| l.starts_with(&format!("{shot} "))
                        || l.starts_with(&format!("{shot}'s "))),
                "{what}: {out}"
            );
        }
        // A late pair: the last click's press and release recorded after
        // shot A's request (2400), its call logged long before it.
        let (ok, out) = verdict(
            "p1",
            &Folder {
                reports: Some(|r| {
                    let tag = r
                        .iter()
                        .filter(|v| v["t"] == "mouse")
                        .filter_map(|v| v["extra"].as_u64())
                        .max()
                        .unwrap();
                    // The press at 2401, its release at 2402.
                    for v in r.iter_mut().filter(|v| v["extra"].as_u64() == Some(tag)) {
                        v["q"] = serde_json::json!(2400 + v["buttons"].as_i64().unwrap());
                    }
                    r.sort_by_key(|v| v["q"].as_i64());
                }),
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("last click's one recorded button-up"), "{out}");
        // The runner's record of the neutral report it waited for.
        let (ok, out) = verdict(
            "p1",
            &Folder {
                shots: Some(|r| r.retain(|v| v["t"] != "released")),
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("`released`"), "{out}");
        let (ok, out) = verdict(
            "p1",
            &Folder {
                unbound: true,
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("not exactly its declared files"), "{out}");
    }

    /// A panel run folder matches its row's expectation through the
    /// checker: a good P1 run matches `p1`; P5's (the volumes left, shot A
    /// not P1's frame but its own declared one, every slider at 100 %)
    /// matches `p5` and not `p1`; and P1's matches no `p5`.
    #[test]
    fn a_panel_run_folder_matches_its_row() {
        let (ok, out) = verdict("p1", &Folder::default());
        assert!(ok, "{out}");
        let p5 = [FRAME_A];
        let left = Folder {
            left: true,
            failing: &p5,
            ..Folder::default()
        };
        let (ok, out) = verdict("p5", &left);
        assert!(ok, "{out}");
        let (ok, out) = verdict("p1", &left);
        assert!(!ok, "{out}");
        assert!(out.contains("p1.volumes"), "{out}");
        let (ok, out) = verdict("p5", &Folder::default());
        assert!(!ok, "{out}");
    }

    /// A negative row matches only when it failed for its declared reason:
    /// P4 and P5 with one volume moved, or P5's other two volumes kept but
    /// its file refused; P5 against a stale player (not the declared
    /// mutated build); P4 and P5 whose shot A is not their own declared
    /// frame (P1's frame not matched either way) each make the row a
    /// mismatch naming the check. The controls match.
    #[test]
    fn a_negative_row_fails_for_its_declared_reason_only() {
        let p4 = [FRAME_A];
        let p5 = [FRAME_A];
        let moved = super::super::judge::tests::DEVICE_TOML
            .replace("system_volume = 1.0", "system_volume = 0.25");
        let refused = super::super::judge::tests::DEVICE_TOML.replace("version = 3", "version = 2");
        for (row, failing) in [("p4", &p4[..]), ("p5", &p5[..])] {
            let (ok, out) = verdict(
                row,
                &Folder {
                    left: true,
                    failing,
                    ..Folder::default()
                },
            );
            assert!(ok, "{row}: {out}");
            for after in [&moved, &refused] {
                let (ok, out) = verdict(
                    row,
                    &Folder {
                        after: Some(after.as_str()),
                        failing,
                        ..Folder::default()
                    },
                );
                assert!(!ok, "{row}: {out}");
                assert!(out.contains(&format!("{row}.volumes-kept")), "{row}: {out}");
            }
        }
        let (ok, out) = verdict(
            "p5",
            &Folder {
                left: true,
                failing: &p5,
                stale: true,
                ..Folder::default()
            },
        );
        assert!(!ok, "{out}");
        assert!(out.contains("p5.mutation-build"), "{out}");
        // Shot A neither P1's frame nor the row's own (P4 with no panel
        // drawn; P5 with a slider not at 100 %): the row's declared frame
        // fails, so it did not fail for its declared reason.
        for row in ["p4", "p5"] {
            let (ok, out) = verdict(
                row,
                &Folder {
                    left: true,
                    failing: &[FRAME_A, DECLARED_A],
                    ..Folder::default()
                },
            );
            assert!(!ok, "{row}: {out}");
            assert!(out.contains(DECLARED_A), "{row}: {out}");
        }
    }
}
