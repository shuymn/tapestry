//! `tapestry test-device judge <row> <dir>` (docs/test-device.md
//! "Verdicts"): the repository's verdicts for a run of `sink`,
//! `sink-early-close`, `sink-kill`, the panel rows `p1`, `p4` and `p5`,
//! `p1-kill`, the reference rows `p1-ref` and `p1-ref-75`, or the set of
//! `p1-compare`, from its kept files, written to `verdicts-judge.jsonl`
//! (a set's in its own folder: each role's row checks on that role's run,
//! named by the role, and one provenance over every role's links). Each check reads only records of what a runner,
//! the recorder or the device observed, and their order within one
//! stream; a check whose stretch of the recorder's stream an `error`
//! record covers, or whose file is missing or unreadable, is
//! `incomplete`.

use std::path::Path;

use serde_json::{Value, json};
use test_device::clock::{self, Ping, ticks_to_us};
use test_device::contract::{self, INNER, LINK, RECORDER, RUN, SINGLE};
use test_device::evidence::{self, Kinds, Outcome};
use test_device::lease::Reason;
use test_device::protocol::{Answer, Command, Token};
use test_device::provenance;

use super::guard::SLICE_MS;
use super::launch::{DEVICE_AFTER, DEVICE_BEFORE, PLAYER_AFTER, PLAYER_BEFORE, START_VOLUME};
use super::listen::{BUTTON_EDGES, a_runner_s};
use super::run::{F24_SCAN, FOCUS_POINT, MODE_LINK, PANEL_MODE, SELECT, START, pad_buttons};

/// The producer's name.
pub const PRODUCER: &str = "judge";

/// One stream as read: its records, or why it could not be read, and the
/// sha256 of its bytes.
#[derive(Clone, Debug)]
pub struct Stream {
    pub records: Result<Vec<Value>, String>,
    pub sha: String,
}

impl Stream {
    #[cfg(test)]
    pub fn of(records: Vec<Value>) -> Self {
        Self {
            records: Ok(records),
            sha: String::new(),
        }
    }

    /// Reads the stream at `path` with the contract's strict reader for
    /// its type (`R`), so the judge reads exactly what `check` reads: a
    /// cut writer's unterminated last line is dropped, any other fault
    /// leaves no records.
    fn read<R: Kinds>(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self {
                records: Err(format!("{} is missing", path.display())),
                sha: String::new(),
            };
        };
        let sha = test_device::contract::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        let records = evidence::read::<R>(&bytes)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|_| {
                let whole = bytes
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(&bytes[..0], |i| &bytes[..=i]);
                std::str::from_utf8(whole)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .lines()
                    .map(|l| {
                        serde_json::from_str(l).map_err(|e| format!("{}: {e}", path.display()))
                    })
                    .collect()
            });
        Self { records, sha }
    }
}

/// A run's three streams.
#[derive(Clone, Debug)]
pub struct Files {
    pub run: Stream,
    pub link: Stream,
    pub recorder: Stream,
}

impl Files {
    pub fn read(dir: &Path) -> Self {
        Self {
            run: Stream::read::<evidence::Run>(&dir.join(RUN)),
            link: Stream::read::<evidence::Link>(&dir.join(LINK)),
            recorder: Stream::read::<evidence::Recorder>(&dir.join(RECORDER)),
        }
    }

    /// Keeps only the streams the checker takes as evidence for role
    /// `role` (each bound to its run and writer, finished as its writer's
    /// ending allows, its seal matching): any other stream's checks are
    /// `incomplete`, never judged on a prefix the checker refuses.
    fn standing(mut self, role: &str, standing: &contract::Standing) -> Self {
        for (name, s) in [
            (RUN, &mut self.run),
            (LINK, &mut self.link),
            (RECORDER, &mut self.recorder),
        ] {
            if s.records.is_ok()
                && let Err(why) = standing.of(role, name)
            {
                s.records = Err(format!("not evidence: {why}"));
            }
        }
        self
    }
}

/// A verdict before its inputs' digests: the check, the outcome, the
/// detail, and the files it read (role, path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub check: &'static str,
    pub outcome: Outcome,
    pub detail: String,
    pub inputs: Vec<(&'static str, &'static str)>,
}

fn verdict(
    check: &'static str,
    inputs: &[(&'static str, &'static str)],
    r: Result<Result<(), String>, String>,
) -> Judged {
    let (outcome, detail) = match r {
        Ok(Ok(())) => (Outcome::Pass, String::new()),
        Ok(Err(why)) => (Outcome::Fail, why),
        Err(why) => (Outcome::Incomplete, why),
    };
    Judged {
        check,
        outcome,
        detail,
        inputs: inputs.to_vec(),
    }
}

fn recs(s: &Stream) -> Result<&[Value], String> {
    s.records.as_deref().map_err(Clone::clone)
}

fn q(v: &Value) -> i64 {
    v["q"].as_i64().unwrap_or(i64::MIN)
}

/// The recorder's errors covering any of `[from, to]`: the check reading
/// that stretch is incomplete.
fn covered(rec: &[Value], from: i64, to: i64) -> Result<(), String> {
    let last = rec.iter().map(q).max().unwrap_or(i64::MIN);
    if last < to {
        return Err(format!(
            "the recorder's stream ends at {last}, before the stretch the check reads ends ({to})"
        ));
    }
    match rec.iter().find(|v| {
        v["t"] == "error"
            && v["from"].as_i64().unwrap_or(i64::MIN) <= to
            && v["to"].as_i64().unwrap_or(i64::MAX) >= from
    }) {
        Some(e) => Err(format!(
            "a recorder error covers the stretch the check reads: {e}"
        )),
        None => Ok(()),
    }
}

/// The provenance check over `links`' `hello` answers.
fn provenance(
    build: Option<&str>,
    links: &[(&str, &Stream)],
) -> Result<Result<(), String>, String> {
    let mut events = Vec::new();
    for (source, s) in links {
        for (index, v) in recs(s)?.iter().enumerate() {
            let line = v["line"].as_str().unwrap_or_default();
            if v["t"] == "ans" && line.starts_with("hello ") {
                events.push(provenance::Event::Hello {
                    at: q(v),
                    line: line.to_owned(),
                    source: (*source).to_owned(),
                    index,
                });
            }
        }
    }
    match provenance::check(build, &events) {
        (Outcome::Pass, _) => Ok(Ok(())),
        (Outcome::Fail, d) => Ok(Err(d)),
        (Outcome::Incomplete, d) => Err(d),
    }
}

/// The connection's token: its first `hello` answer's.
fn token(link: &[Value]) -> Option<Token> {
    link.iter()
        .find_map(|v| match Answer::parse_canonical(v["line"].as_str()?) {
            Some(Answer::Hello(h)) if v["t"] == "ans" => h.session,
            _ => None,
        })
}

/// Every grant and renewal written follows the guard's good check of the
/// same line, names the connection's token, a grant's length is the
/// slice, and its deadline is no later than the guard's own bound from
/// that check ([`clock::deadline`]: the least device time the check may
/// be, plus the slice shortened by the clocks' rate bound) over the round
/// trips read before it was written; none without a round trip.
pub fn grants(link: &[Value]) -> Result<(), String> {
    let tok = token(link).ok_or("the link read no token")?;
    let freq = link
        .iter()
        .find(|v| v["t"] == "open")
        .and_then(|v| v["freq"].as_i64())
        .ok_or("no `open` record")?;
    let mut pings: Vec<(i64, Ping)> = Vec::new();
    let mut last_check: Option<&Value> = None;
    let mut n = 0;
    for v in link {
        match v["t"].as_str() {
            Some("ping") => {
                let (Some(sent), Some(device), Some(recv)) =
                    (v["sent"].as_i64(), v["device"].as_u64(), v["recv"].as_i64())
                else {
                    continue;
                };
                pings.push((recv, Ping::from_ticks(sent, device, recv, freq)));
            }
            Some("check") => last_check = Some(v),
            Some("cmd") => {
                let line = v["line"].as_str().unwrap_or_default();
                let (named, deadline) = match Command::parse_canonical(line) {
                    Some(Command::Lease {
                        token,
                        ms,
                        deadline,
                    }) => {
                        if ms != SLICE_MS {
                            return Err(format!("`{line}` grants {ms} ms, not a slice"));
                        }
                        (token, deadline)
                    }
                    Some(Command::Renew { id, deadline }) => (id.token, deadline),
                    _ => continue,
                };
                n += 1;
                let check = last_check
                    .take()
                    .filter(|c| c["line"] == line && q(c) <= q(v))
                    .ok_or_else(|| format!("`{line}` was written without a check before it"))?;
                if named != tok {
                    return Err(format!(
                        "`{line}` names another token than the connection's {tok}"
                    ));
                }
                let seen: Vec<Ping> = pings
                    .iter()
                    .filter(|(recv, _)| *recv <= q(v))
                    .map(|(_, p)| *p)
                    .collect();
                let bound = clock::deadline(&seen, ticks_to_us(q(check), freq), SLICE_MS.into())
                    .ok_or_else(|| {
                        format!("`{line}` was written with no round trip that bounds the clock")
                    })?;
                if deadline > bound {
                    return Err(format!(
                        "`{line}`'s deadline {deadline} is past a slice after its check ({bound})"
                    ));
                }
            }
            _ => {}
        }
    }
    if n == 0 {
        return Err("no grant was written".into());
    }
    Ok(())
}

/// The first ours press of button 1 and of F24 in the recorder's stream,
/// with the sink in front when `front` names it: their counters.
/// A press: when the recorder read it, and the handle it came from.
#[derive(Clone, Copy, Debug)]
struct Press {
    q: i64,
    h: u64,
}

fn presses(rec: &[Value], front: Option<u64>) -> Result<(Press, Press), String> {
    let (pad, key) = pressed(rec, front, 1, true)?;
    Ok((pad, key.ok_or("no make of F24")?))
}

/// The first ours pad report with every one of `buttons` pressed, and with
/// `key` the first ours make of F24, each with `front` in front when it
/// names a window: when the recorder read them and their handles.
fn pressed(
    rec: &[Value],
    front: Option<u64>,
    buttons: u16,
    key: bool,
) -> Result<(Press, Option<Press>), String> {
    let in_front = |v: &Value| front.is_none_or(|w| v["fg"]["window"] == w);
    let pad = rec
        .iter()
        .find(|v| {
            v["t"] == "pad"
                && v["ours"] == true
                && v["bytes"]
                    .as_str()
                    .and_then(pad_buttons)
                    .is_some_and(|b| b & buttons == buttons)
        })
        .ok_or_else(|| {
            format!("the recorder saw no press of buttons {buttons:#06x} from the device's pad")
        })?;
    let key = if key {
        Some(
            rec.iter()
                .find(|v| {
                    v["t"] == "key"
                        && v["ours"] == true
                        && v["code"] == F24_SCAN
                        && v["down"] == true
                })
                .ok_or("the recorder saw no make of F24 from the device's keyboard")?,
        )
    } else {
        None
    };
    for v in std::iter::once(pad).chain(key) {
        if !in_front(v) {
            return Err(format!("the run's window was not in front: {v}"));
        }
    }
    let press = |v: &Value| Press {
        q: q(v),
        h: v["h"].as_u64().unwrap_or(u64::MAX),
    };
    Ok((press(pad), key.map(press)))
}

/// When `press` was first released: after it, the first record of its own
/// handle that `is_release` takes, or the handle's removal; none while it
/// stays held.
fn first_release(rec: &[Value], press: Press, is_release: impl Fn(&Value) -> bool) -> Option<i64> {
    rec.iter()
        .filter(|v| q(v) > press.q && v["h"].as_u64() == Some(press.h))
        .find(|v| (v["t"] == "dev" && v["change"] == "removal") || is_release(v))
        .map(q)
}

/// Whether `press` was released: after it, the first record of its own
/// handle is a release (`is_release`) or the handle's removal. A device
/// change of that handle before then (it arrived again: another binding)
/// or no such record leaves it unreleased; another interface's removal
/// says nothing of it.
fn released(rec: &[Value], press: Press, is_release: impl Fn(&Value) -> bool) -> bool {
    rec.iter()
        .filter(|v| q(v) > press.q && v["h"].as_u64() == Some(press.h))
        .find_map(|v| match v["t"].as_str() {
            Some("dev") => Some(v["change"] == "removal"),
            _ => is_release(v).then_some(true),
        })
        .unwrap_or(false)
}

/// After the presses, the recorder holds the neutral pad report and F24's
/// break, each from the handle of its press, or that handle's removal.
fn neutral(rec: &[Value], pad: Press, key: Press) -> Result<(), String> {
    neutral_of(rec, pad, Some(key))
}

/// [`neutral`] with the key's press optional.
fn neutral_of(rec: &[Value], pad: Press, key: Option<Press>) -> Result<(), String> {
    let pad = released(rec, pad, |v| {
        v["t"] == "pad" && v["ours"] == true && v["bytes"].as_str().and_then(pad_buttons) == Some(0)
    });
    let key = key.is_none_or(|key| {
        released(rec, key, |v| {
            v["t"] == "key" && v["ours"] == true && v["code"] == F24_SCAN && v["down"] == false
        })
    });
    match (pad, key) {
        (true, true) => Ok(()),
        (false, _) => Err("no neutral pad report from the pressed pad after the press".into()),
        (_, false) => Err("no break of F24 from the pressed keyboard after the make".into()),
    }
}

/// No foreign input in `[from, to]`: no key edge or controller report from
/// another handle, and no mouse button edge but the runner's calls'.
fn no_foreign(rec: &[Value], calls: &[u32], from: i64, to: i64) -> Result<(), String> {
    for v in rec.iter().filter(|v| (from..=to).contains(&q(v))) {
        let foreign = match v["t"].as_str() {
            Some("key" | "pad") => v["ours"] == false,
            Some("mouse") => {
                let extra = v["extra"].as_u64().and_then(|e| u32::try_from(e).ok());
                v["buttons"].as_u64().unwrap_or(0) & u64::from(BUTTON_EDGES) != 0
                    && !extra.is_some_and(|e| a_runner_s(e, calls))
            }
            _ => false,
        };
        if foreign {
            return Err(format!("foreign input during the run: {v}"));
        }
    }
    Ok(())
}

/// The release evidence before the close: the run's `quiet` record, of a
/// `hello` the link read after its fence that names the connection's
/// token and `quiet 1`, and then the `close`. With `require_released` (a
/// row that leases) the `hello` must also read reason `released`; a row
/// that grants no lease cannot give that reason, and the device keeps the
/// one it had (`none` after a boot, `closed` after a connection ended
/// holding).
fn quiet_before_close(run: &[Value], link: &[Value], require_released: bool) -> Result<(), String> {
    let close = run
        .iter()
        .position(|v| v["t"] == "close")
        .ok_or("the run recorded no close of its sink")?;
    let Some(quiet) = run.iter().position(|v| v["t"] == "quiet") else {
        return Err("no release evidence was taken".into());
    };
    if quiet > close {
        return Err("the sink closed before the release evidence".into());
    }
    let line = run[quiet]["line"].as_str().unwrap_or_default();
    let tok = token(link);
    match Answer::parse_canonical(line) {
        Some(Answer::Hello(h))
            if h.quiet
                && (!require_released || h.reason == Reason::Released)
                && h.session == tok
                && tok.is_some() => {}
        _ => {
            return Err(format!(
                "the evidence `{line}` is not a quiet `hello` of the run's token{}",
                if require_released {
                    " after a release"
                } else {
                    ""
                }
            ));
        }
    }
    evidence_order(link, line)
}

/// The order the release evidence needs in the link's stream, which its
/// one writer keeps in the order of the events, each later record also
/// later on the host's counter, and each answer matched to its command by
/// the counter of its write (`sent`): the fence, then `release` written,
/// then its `ok` read, then the `hello` written, then `line` read as that
/// `hello`'s answer.
fn evidence_order(link: &[Value], line: &str) -> Result<(), String> {
    let after = |from: usize, pick: &dyn Fn(&Value) -> bool| {
        link.iter()
            .enumerate()
            .skip(from + 1)
            .filter(|(_, v)| q(v) > q(&link[from]))
            .find(|(_, v)| pick(v))
            .map(|(i, _)| i)
    };
    let answer = |cmd: usize, want: &dyn Fn(&Value) -> bool| {
        after(cmd, &|a| {
            a["t"] == "ans" && a["sent"] == link[cmd]["q"] && a["cmd"] == link[cmd]["line"]
        })
        .filter(|&a| want(&link[a]))
    };
    let fence = link
        .iter()
        .position(|v| v["t"] == "fence")
        .ok_or("the link recorded no fence")?;
    let release = after(fence, &|v| v["t"] == "cmd" && v["line"] == "release")
        .ok_or("no `release` written after the fence")?;
    let ok = answer(release, &|a| a["line"] == "ok")
        .ok_or("the `release` after the fence was not answered `ok`")?;
    let mut from = ok;
    while let Some(hello) = after(from, &|v| v["t"] == "cmd" && v["line"] == "hello") {
        if answer(hello, &|a| a["line"] == line).is_some() {
            return Ok(());
        }
        from = hello;
    }
    Err(
        "the evidence is not the answer to a `hello` written after the `release`'s `ok` was read"
            .into(),
    )
}

/// The end's `release`: the end stopped holds, and the link wrote
/// `release` and read its `ok`.
fn release_requested(run: &[Value], link: &[Value]) -> Result<(), String> {
    if !run.iter().any(|v| v["t"] == "stop" && v["what"] == "end") {
        return Err("the end did not stop holds".into());
    }
    if !link
        .iter()
        .any(|v| v["t"] == "cmd" && v["line"] == "release")
    {
        return Err("no `release` was written".into());
    }
    if !link
        .iter()
        .any(|v| v["t"] == "ans" && v["cmd"] == "release" && v["line"] == "ok")
    {
        return Err("the `release` was not answered `ok`".into());
    }
    Ok(())
}

const RUN_RUN: (&str, &str) = (SINGLE, RUN);
const RUN_LINK: (&str, &str) = (SINGLE, LINK);
const RUN_REC: (&str, &str) = (SINGLE, RECORDER);

/// The host counter of `run`'s first record of `kind`.
fn first_q(run: &Stream, kind: &str) -> Result<i64, String> {
    recs(run)?
        .iter()
        .find(|v| v["t"] == kind)
        .map(q)
        .ok_or_else(|| format!("the run recorded no `{kind}`"))
}

/// The provenance check of a panel row: [`provenance`], and every `hello`
/// the driving connection read reports in [`PANEL_MODE`], the mode the
/// run's verdicts read reports in.
fn panel_provenance(
    build: Option<&str>,
    f: &Files,
    mode: &Stream,
) -> Result<Result<(), String>, String> {
    if let Err(why) = provenance(build, &[(LINK, &f.link), (MODE_LINK, mode)])? {
        return Ok(Err(why));
    }
    in_panel_mode(&f.link)
}

/// Every `hello` the driving connection `link` read reports in
/// [`PANEL_MODE`].
fn in_panel_mode(link: &Stream) -> Result<Result<(), String>, String> {
    for v in recs(link)?.iter().filter(|v| v["t"] == "ans") {
        if let Some(Answer::Hello(h)) = v["line"].as_str().and_then(Answer::parse_canonical)
            && h.mode != PANEL_MODE
        {
            return Ok(Err(format!(
                "the driving connection's `hello` reads mode {:?}, not {PANEL_MODE:?}: {}",
                h.mode, v["line"]
            )));
        }
    }
    Ok(Ok(()))
}

/// The tags of the run's click calls.
fn click_tags(run: &[Value]) -> Vec<u32> {
    run.iter()
        .filter(|v| v["t"] == "call" && v["what"] == "click")
        .filter_map(|v| v["tag"].as_u64().and_then(|t| u32::try_from(t).ok()))
        .collect()
}

/// The verdicts of a run of `sink` or `sink-early-close`.
pub fn n1(build: Option<&str>, f: &Files) -> Vec<Judged> {
    let front = recs(&f.run)
        .ok()
        .and_then(|r| r.iter().find(|v| v["t"] == "sink"))
        .and_then(|v| v["window"].as_u64());
    let span = |kind: &str| first_q(&f.run, kind);
    let pressed = || -> Result<Result<(Press, Press), String>, String> {
        let rec = recs(&f.recorder)?;
        covered(rec, span("call")?, span("held")?)?;
        Ok(presses(rec, front))
    };
    let calls = recs(&f.run).map(click_tags).unwrap_or_default();
    vec![
        verdict(
            provenance::CHECK,
            &[RUN_LINK],
            provenance(build, &[(LINK, &f.link)]),
        ),
        verdict("n1.grants", &[RUN_LINK], recs(&f.link).map(grants)),
        verdict(
            "n1.press",
            &[RUN_RUN, RUN_REC],
            pressed().map(|r| r.map(drop)),
        ),
        verdict(
            "n1.release-requested",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(release_requested(r, recs(&f.link)?))),
        ),
        verdict(
            "n1.neutral",
            &[RUN_RUN, RUN_REC],
            pressed().and_then(|p| {
                let rec = recs(&f.recorder)?;
                let Ok((pad, key)) = p else {
                    return Err("no press to be released".into());
                };
                let end = rec.iter().map(q).max().unwrap_or(i64::MAX);
                covered(rec, pad.q.min(key.q), end)?;
                Ok(neutral(rec, pad, key))
            }),
        ),
        verdict(
            "n1.quiet-before-close",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(quiet_before_close(r, recs(&f.link)?, true))),
        ),
        verdict(
            "n1.no-foreign",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                // Until the release evidence: no input is held after it,
                // and the recorder ends before the sink closes.
                let (from, to) = (span("call")?, span("quiet")?);
                covered(rec, from, to)?;
                Ok(no_foreign(rec, &calls, from, to))
            })(),
        ),
    ]
}

const INNER_RUN: (&str, &str) = (INNER, RUN);
const INNER_LINK: (&str, &str) = (INNER, LINK);
const INNER_MODE: (&str, &str) = (INNER, MODE_LINK);

/// An outer row's checks: their ids, the fixture record the inner reaches
/// before the kill, what the inner holds, and whether it started a player.
pub struct Kill {
    pub abnormal: &'static str,
    pub neutral: &'static str,
    pub quiet: &'static str,
    pub no_foreign: &'static str,
    pub children_gone: &'static str,
    /// N2p's: the inner's player ended with the inner's job.
    pub player_gone: Option<&'static str>,
    /// The inner's record before which the outer must not kill.
    pub reached: &'static str,
    pub buttons: u16,
    pub key: bool,
}

/// N2 (`sink-kill`).
pub const N2: Kill = Kill {
    abnormal: "n2.abnormal",
    neutral: "n2.neutral",
    quiet: "n2.quiet",
    no_foreign: "n2.no-foreign",
    children_gone: "n2.children-gone",
    player_gone: None,
    reached: "held",
    buttons: 1,
    key: true,
};

/// N2p (`p1-kill`): START and SELECT held at the inner's barrier.
pub const N2P: Kill = Kill {
    abnormal: "n2p.abnormal",
    neutral: "n2p.neutral",
    quiet: "n2p.quiet",
    no_foreign: "n2p.no-foreign",
    children_gone: "n2p.children-gone",
    player_gone: Some("n2p.player-gone"),
    reached: "barrier",
    buttons: 1 << (START - 1) | 1 << (SELECT - 1),
    key: false,
};

/// The outer's kill of the inner: the counter read just before the
/// termination call, and the record's own, written once it returned.
fn kill_of(run: &[Value]) -> Result<(i64, i64), String> {
    let k = run
        .iter()
        .find(|v| v["t"] == "kill" && v["name"] == "inner")
        .ok_or("the outer recorded no kill of the inner")?;
    let requested = k["requested"]
        .as_i64()
        .ok_or("the outer's kill of the inner has no request counter")?;
    Ok((requested, q(k)))
}

/// The verdicts of a run of outer row `k` (`sink-kill`, `p1-kill`):
/// `outer` the outer's files, `inner` the inner's, and for N2p the inner's
/// mode link (`inner_mode`): the inner drives the device as a panel run, so
/// its provenance takes its mode connection and its driving connection
/// must report in the panel mode.
pub fn kill(
    k: &Kill,
    build: Option<&str>,
    outer: &Files,
    inner: &Files,
    inner_mode: Option<&Stream>,
) -> Vec<Judged> {
    let abnormal = || -> Result<Result<(), String>, String> {
        let run = recs(&inner.run)?;
        let link = recs(&inner.link)?;
        if !run.iter().any(|v| v["t"] == k.reached) {
            return Err(format!(
                "the inner recorded no `{}`: the kill's fixture was not reached",
                k.reached
            ));
        }
        if link
            .iter()
            .any(|v| v["t"] == "cmd" && v["line"] == "release")
        {
            return Ok(Err("the inner wrote `release`".into()));
        }
        if run.iter().any(|v| v["t"] == "stop") {
            return Ok(Err("the inner stopped holds itself".into()));
        }
        Ok(Ok(()))
    };
    // The device quiet after the kill, and the kill's fixture reached: a
    // lease live when the inner's connection ended, and that connection's
    // end the kill's. The device numbers its connections and sets reason
    // `closed` only when a held lease ends with one, so the outer's first
    // `hello` naming the connection right after the inner's, with reason
    // `closed`, shows the inner's connection ended holding a lease and no
    // other connection ended between (a device-side end before the kill
    // would be a second one).
    let quiet = || -> Result<Result<(), String>, String> {
        fn hello_of<'a>(
            link: &'a [Value],
            what: &str,
        ) -> Result<test_device::protocol::Hello<'a>, String> {
            link.iter()
                .filter(|v| v["t"] == "ans")
                .find_map(|v| match Answer::parse_canonical(v["line"].as_str()?) {
                    Some(Answer::Hello(h)) => Some(h),
                    _ => None,
                })
                .ok_or_else(|| format!("{what} read no `hello`"))
        }
        let first = hello_of(recs(&outer.link)?, "the outer")?;
        let inner_link = recs(&inner.link)?;
        let theirs = hello_of(inner_link, "the inner")?;
        let (kill, _) = kill_of(recs(&outer.run)?)?;
        if first.lease.is_some() || !first.quiet {
            return Ok(Err(format!(
                "the outer's first `hello` is not quiet with no lease: {first:?}"
            )));
        }
        // The inner's last answer to a grant or renewal before the kill
        // granted.
        let granted = inner_link
            .iter()
            .rev()
            .filter(|v| v["t"] == "ans" && q(v) <= kill)
            .find(|v| {
                matches!(
                    Command::parse_canonical(v["cmd"].as_str().unwrap_or_default()),
                    Some(Command::Lease { .. } | Command::Renew { .. })
                )
            })
            .ok_or("the inner read no answer to a grant before the kill")?;
        if !granted["line"]
            .as_str()
            .is_some_and(|l| l.starts_with("lease "))
        {
            return Err(format!(
                "the inner's last grant before the kill was refused: {granted}"
            ));
        }
        let (Some(mine), Some(before)) = (first.session, theirs.session) else {
            return Err("a `hello` names no session".into());
        };
        if mine.boot != before.boot || Some(mine.count) != before.count.checked_add(1) {
            return Err(format!(
                "the outer's connection {mine} is not the one after the inner's {before}: \
                 another connection ended between, so the kill did not end a live lease"
            ));
        }
        if first.reason != Reason::Closed {
            return Err(format!(
                "the outer's first `hello` reads reason {:?}: the inner's lease had ended \
                 before its connection did",
                first.reason
            ));
        }
        Ok(Ok(()))
    };
    // No foreign input while the inner held: from its first call to the
    // inner's end as the outer observed it, on the outer's recorder, live
    // throughout.
    let no_foreign_inner = || -> Result<Result<(), String>, String> {
        let rec = recs(&outer.recorder)?;
        let inner_run = recs(&inner.run)?;
        let from = inner_run
            .iter()
            .find(|v| v["t"] == "call")
            .map(q)
            .ok_or("the inner recorded no call")?;
        let to = recs(&outer.run)?
            .iter()
            .find(|v| v["t"] == "exit" && v["name"] == "inner")
            .map(q)
            .ok_or("the outer observed no end of the inner")?;
        let calls = click_tags(inner_run);
        covered(rec, from, to)?;
        Ok(no_foreign(rec, &calls, from, to))
    };
    let gone = || -> Result<Result<(), String>, String> {
        let run = recs(&outer.run)?;
        let cut = run.first().map(|v| v["cut"].clone()).unwrap_or_default();
        let kill = run
            .iter()
            .position(|v| v["t"] == "kill" && v["name"] == "inner")
            .ok_or("the outer recorded no kill of the inner")?;
        // The inner's job, the recorder with it, ends with the outer's cut
        // code.
        Ok(
            match run[kill..]
                .iter()
                .find(|v| v["t"] == "exit" && v["name"] == "inner-recorder")
            {
                Some(v) if v["code"] == cut => Ok(()),
                Some(v) => Err(format!(
                    "the inner's recorder did not end with the inner's job: {v}"
                )),
                None => Err("the outer never observed the inner's recorder end".into()),
            },
        )
    };
    // The presses held at the inner's fixture record and released only
    // after the outer's kill, then neutral: a release before the kill
    // would leave the kill nothing to release.
    let neutral_after = || -> Result<Result<(), String>, String> {
        let rec = recs(&outer.recorder)?;
        let Ok((pad, key)) = pressed(rec, None, k.buttons, k.key) else {
            return Err(
                "the outer's recorder saw no press: the kill's fixture was not reached".into(),
            );
        };
        let reached = recs(&inner.run)?
            .iter()
            .find(|v| v["t"] == k.reached)
            .map(q)
            .ok_or_else(|| format!("the inner recorded no `{}`", k.reached))?;
        let (requested, kill) = kill_of(recs(&outer.run)?)?;
        let end = rec.iter().map(q).max().unwrap_or(i64::MAX);
        covered(rec, key.map_or(pad.q, |key| pad.q.min(key.q)), end)?;
        let pad_up = first_release(rec, pad, |v| {
            v["t"] == "pad"
                && v["bytes"]
                    .as_str()
                    .and_then(pad_buttons)
                    .is_some_and(|b| b & k.buttons != k.buttons)
        });
        let key_up = key.map(|key| {
            (
                key,
                first_release(rec, key, |v| {
                    v["t"] == "key" && v["code"] == F24_SCAN && v["down"] == false
                }),
            )
        });
        for (what, press, up) in std::iter::once(("the pad's buttons", pad, pad_up))
            .chain(key_up.map(|(key, up)| ("F24", key, up)))
        {
            if press.q > reached {
                return Ok(Err(format!(
                    "{what} were not pressed at the inner's `{}`",
                    k.reached
                )));
            }
            match up {
                Some(at) if at <= requested => {
                    return Ok(Err(format!(
                        "{what} were released at {at}, before the outer requested the kill at                          {requested}"
                    )));
                }
                // Between the request and its return, the release's order
                // with the termination is unknown.
                Some(at) if at <= kill => {
                    return Err(format!(
                        "{what} were released at {at}, while the outer's kill was being                          requested ({requested} to {kill})"
                    ));
                }
                _ => {}
            }
        }
        Ok(neutral_of(rec, pad, key))
    };
    // The inner's player ended with the inner's job: the outer observed its
    // exit with the outer's cut code after the kill of the inner, before
    // it ended the player itself.
    let player_gone = || -> Result<Result<(), String>, String> {
        let run = recs(&outer.run)?;
        let cut = run.first().map(|v| v["cut"].clone()).unwrap_or_default();
        let kill = run
            .iter()
            .position(|v| v["t"] == "kill" && v["name"] == "inner")
            .ok_or("the outer recorded no kill of the inner")?;
        let after = &run[kill..];
        let exit = after
            .iter()
            .position(|v| v["t"] == "exit" && v["name"] == "inner-player");
        let own = after
            .iter()
            .position(|v| v["t"] == "kill" && v["name"] == "inner-player");
        Ok(match (exit, own) {
            (None, _) => Err("the outer never observed the inner's player end".into()),
            (Some(e), Some(o)) if o < e => Err(format!(
                "the inner's player outlived the inner's job and the outer ended it: {}",
                after[e]
            )),
            (Some(e), _) if after[e]["code"] == cut => Ok(()),
            (Some(e), _) => Err(format!(
                "the inner's player did not end with the inner's job: {}",
                after[e]
            )),
        })
    };
    let origin = || -> Result<Result<(), String>, String> {
        let mut links = vec![("outer", &outer.link), ("inner", &inner.link)];
        if k.player_gone.is_some() {
            let mode = inner_mode.ok_or("the inner's mode link was not read")?;
            links.push(("inner mode", mode));
        }
        if let Err(why) = provenance(build, &links)? {
            return Ok(Err(why));
        }
        if k.player_gone.is_some() {
            return in_panel_mode(&inner.link);
        }
        Ok(Ok(()))
    };
    let origin_inputs: &[(&str, &str)] = if k.player_gone.is_some() {
        &[RUN_LINK, INNER_LINK, INNER_MODE]
    } else {
        &[RUN_LINK, INNER_LINK]
    };
    let mut out = vec![
        verdict(provenance::CHECK, origin_inputs, origin()),
        verdict(k.abnormal, &[INNER_RUN, INNER_LINK], abnormal()),
        verdict(k.neutral, &[RUN_RUN, RUN_REC, INNER_RUN], neutral_after()),
        verdict(k.quiet, &[RUN_RUN, RUN_LINK, INNER_LINK], quiet()),
        verdict(
            k.no_foreign,
            &[RUN_RUN, RUN_REC, INNER_RUN],
            no_foreign_inner(),
        ),
        verdict(k.children_gone, &[RUN_RUN], gone()),
    ];
    if let Some(id) = k.player_gone {
        out.push(verdict(id, &[RUN_RUN], player_gone()));
    }
    out
}

/// A file a panel check reads besides the streams: its bytes, or why it
/// could not be read.
pub type Bytes = Result<Vec<u8>, String>;

/// The panel run's files besides its three streams, each read once: every
/// check and every digest of its verdicts comes from these bytes.
#[derive(Clone, Debug)]
pub struct PanelFiles {
    pub mode: Stream,
    pub device_before: Bytes,
    pub device_after: Bytes,
    pub player_before: Bytes,
    pub player_after: Bytes,
    /// Each settings file's sha256, of the bytes above.
    pub digests: Vec<(&'static str, String)>,
}

impl PanelFiles {
    fn read(dir: &Path) -> Self {
        let mut digests = Vec::new();
        let mut bytes = |name: &'static str| {
            let b = std::fs::read(dir.join(name))
                .map_err(|e| format!("{}: {e}", dir.join(name).display()))?;
            digests.push((
                name,
                contract::hex(&<sha2::Sha256 as sha2::Digest>::digest(&b)),
            ));
            Ok(b)
        };
        let device_before = bytes(DEVICE_BEFORE);
        let device_after = bytes(DEVICE_AFTER);
        let player_before = bytes(PLAYER_BEFORE);
        let player_after = bytes(PLAYER_AFTER);
        Self {
            mode: Stream::read::<evidence::Link>(&dir.join(MODE_LINK)),
            device_before,
            device_after,
            player_before,
            player_after,
            digests,
        }
    }

    /// The sha256 of file `name` as read, if it is one of these.
    fn digest(&self, name: &str) -> Option<String> {
        if name == MODE_LINK {
            return Some(self.mode.sha.clone());
        }
        self.digests
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| d.clone())
    }
}

/// The `device.toml` key of each volume slider's value.
const VOLUME_KEYS: [(u8, &str); 3] = [
    (17, "system_volume"),
    (18, "key_volume"),
    (19, "background_volume"),
];

/// The three volumes `device.toml` holds: before the run each at
/// [`START_VOLUME`], as the runner writes it (a target the clicks must
/// move, never one already set), and after it each target's percent.
pub fn volumes(before: &[u8], after: &[u8]) -> Result<(), String> {
    let read = |b: &[u8], when: &str| -> Result<toml::Table, String> {
        std::str::from_utf8(b)
            .map_err(|e| e.to_string())
            .and_then(|t| toml::from_str(t).map_err(|e| e.to_string()))
            .map_err(|e| format!("device.toml {when} the run does not read: {e}"))
    };
    let (before, table) = (read(before, "before")?, read(after, "after")?);
    let mut wrong = Vec::new();
    for (_, key) in VOLUME_KEYS {
        match before.get(key).and_then(toml::Value::as_float) {
            Some(got) if got == START_VOLUME => {}
            got => wrong.push(format!(
                "{key} = {got:?} before the run, not {START_VOLUME}"
            )),
        }
    }
    for t in test_device::panel::TARGETS {
        let Some((_, key)) = VOLUME_KEYS.iter().find(|(s, _)| *s == t.slider) else {
            continue;
        };
        let want = f64::from(t.percent) / 100.0;
        match table.get(*key).and_then(toml::Value::as_float) {
            Some(got) if got == want => {}
            got => wrong.push(format!("{key} = {got:?} after the run, not {want}")),
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

/// A negative row's declared absence of any volume change: `device.toml`
/// after the run reads through the player's own refusals and types, and
/// each of the three volumes is exactly 1.0, as written before the run.
pub fn volumes_kept(after: &[u8]) -> Result<(), String> {
    use game::SettingsFile as _;
    let text = std::str::from_utf8(after).map_err(|e| format!("device.toml after the run: {e}"))?;
    let table: toml::Table = toml::from_str(text)
        .map_err(|e| format!("device.toml after the run does not read: {e}"))?;
    game::DeviceSettings::refuse(&table)
        .map_err(|e| format!("the player refuses device.toml after the run: {e}"))?;
    toml::from_str::<game::DeviceSettings>(text)
        .map_err(|e| format!("the player refuses device.toml after the run: {e}"))?;
    at_start_volume(&table, "")
}

/// Each of the three volumes in `table` at exactly [`START_VOLUME`]; else
/// each volume that is not, named with `when`.
fn at_start_volume(table: &toml::Table, when: &str) -> Result<(), String> {
    let off: Vec<String> = VOLUME_KEYS
        .iter()
        .filter_map(
            |(_, key)| match table.get(*key).and_then(toml::Value::as_float) {
                Some(got) if got == START_VOLUME => None,
                got => Some(format!("{key} = {got:?}{when}, not {START_VOLUME}")),
            },
        )
        .collect();
    if off.is_empty() {
        Ok(())
    } else {
        Err(off.join("; "))
    }
}

/// P5's player is the mutated build it declared: the `player` record's
/// executable has the sha256 the row declared, which is not the runner's
/// own (the build under test). The detail names both either way.
fn mutation_build(run: &[Value]) -> Result<Result<String, String>, String> {
    let p = run
        .iter()
        .find(|v| v["t"] == "player")
        .ok_or("the run recorded no player")?;
    let (sha, runner) = (
        p["sha256"].as_str().unwrap_or_default(),
        p["runner_sha256"].as_str().unwrap_or_default(),
    );
    let detail = format!(
        "the player run has sha256 {sha}; declared {}; the runner's own {runner}",
        p["declared"]
    );
    Ok(match p["declared"].as_str() {
        Some(d) if d == sha && sha != runner && !sha.is_empty() => Ok(detail),
        _ => Err(detail),
    })
}

/// One entry of the volume arm's witness: the slider, the percent, the
/// counter and the pointer input that brought it.
type ArmEntry = (u64, u64, i64, String);

/// What the run's answers witnessed of the volume arm (docs/test-device.md
/// "Run Guard", shots): shot A's frame counter and witness, and the witness
/// of every later answer, by its shot's name. The witness is cumulative, so
/// a later answer holds A's entries and any the arm took after A.
struct Witnesses {
    acquired: i64,
    a: Vec<ArmEntry>,
    later: Vec<(String, Vec<ArmEntry>)>,
}

/// Each answer's witness in the run's `shot` records: each entry's slider,
/// percent, counter and the pointer input that brought it. Error: not
/// exactly one shot A record, or a record whose witness does not read.
fn arm_witness(run: &[Value]) -> Result<Witnesses, String> {
    let shots: Vec<&Value> = run.iter().filter(|v| v["t"] == "shot").collect();
    let at_a: Vec<usize> = shots
        .iter()
        .enumerate()
        .filter(|(_, v)| v["what"] == "shot-a.png")
        .map(|(i, _)| i)
        .collect();
    let [at_a] = at_a.as_slice() else {
        return Err(format!("{} shot A records, not one", at_a.len()));
    };
    let read = |shot: &Value| -> Result<Vec<ArmEntry>, String> {
        let what = shot["what"].as_str().unwrap_or("a shot");
        shot["volumes"]
            .as_array()
            .ok_or_else(|| format!("{what}'s record has no volume witness"))?
            .iter()
            .map(|v| {
                (v.as_array().map(Vec::len) == Some(4)).then_some(())?;
                Some((
                    v[0].as_u64()?,
                    v[1].as_u64()?,
                    v[2].as_i64()?,
                    v[3].as_str()?.to_owned(),
                ))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("a volume in {what}'s witness does not read"))
    };
    let shot = shots.get(*at_a).ok_or("shot A's record is gone")?;
    let acquired = shot["frame"]["acquired"]
        .as_i64()
        .ok_or("shot A's record has no frame counter")?;
    let a = read(shot)?;
    let later = shots
        .iter()
        .skip(at_a + 1)
        .map(|v| Ok((v["what"].as_str().unwrap_or("a shot").to_owned(), read(v)?)))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Witnesses { acquired, a, later })
}

/// The entries each later answer's witness holds beyond shot A's (all of
/// them when it does not start with A's): what the arm took after shot A.
fn after_a(w: &Witnesses) -> Vec<String> {
    w.later
        .iter()
        .filter(|(_, later)| *later != w.a)
        .map(|(what, later)| {
            let extra = later.strip_prefix(w.a.as_slice()).unwrap_or(later);
            format!("{what}'s witness holds entries beyond shot A's: {extra:?}")
        })
        .collect()
}

/// The witness's press entries bound to the target clicks (`p1.volume-arm`):
/// the entries a press brought to the arm (a drag's are other evidence and
/// not counted) are exactly one per target, in the target table's order,
/// each the target's slider and percent, each taken after its target's
/// click call and before shot A's frame; so each target's own press reached
/// the arm with its value, whatever the player did with it after. Outer
/// error (incomplete): the witness does not read, or the run's click calls
/// at the targets are not one per target in order. Every later answer's
/// witness must equal shot A's: an entry the arm took after shot A (a
/// press dispatched late, or anything else) fails.
fn volume_arm(run: &[Value]) -> Result<Result<(), String>, String> {
    let w = arm_witness(run)?;
    let (acquired, reached) = (w.acquired, &w.a);
    let targets = test_device::panel::TARGETS;
    let point = |t: &test_device::panel::Target| (i64::from(t.x), i64::from(t.y));
    // The run's click calls at a target's point, in order: each point and
    // its counter.
    let at_targets: Vec<((i64, i64), Option<i64>)> = run
        .iter()
        .filter(|v| v["t"] == "call" && v["what"] == "click")
        .filter_map(|v| {
            Some((
                (v["point"][0].as_i64()?, v["point"][1].as_i64()?),
                v["q"].as_i64(),
            ))
        })
        .filter(|(p, _)| targets.iter().any(|t| point(t) == *p))
        .collect();
    let calls: Vec<i64> = at_targets.iter().filter_map(|(_, q)| *q).collect();
    if !at_targets
        .iter()
        .map(|(p, _)| *p)
        .eq(targets.iter().map(point))
    {
        return Err("the run's click calls at the targets are not one per target, in order".into());
    }
    let presses: Vec<&ArmEntry> = reached.iter().filter(|(_, _, _, o)| o == "press").collect();
    if presses.len() != targets.len() {
        return Ok(Err(format!(
            "{} presses reached the volume arm, not one per target ({}): {reached:?}",
            presses.len(),
            targets.len()
        )));
    }
    let mut wrong = Vec::new();
    for ((t, call), &&(slider, percent, at, _)) in targets.iter().zip(&calls).zip(&presses) {
        if (slider, percent) != (u64::from(t.slider), u64::from(t.percent)) {
            wrong.push(format!(
                "the press for slider {} at {} % reached the arm as slider {slider} at {percent} %",
                t.slider, t.percent
            ));
        }
        if at <= *call {
            wrong.push(format!(
                "slider {slider}'s press reached the arm at {at}, not after its click call at {call}"
            ));
        }
        if at >= acquired {
            wrong.push(format!(
                "slider {slider}'s press reached the arm at {at}, not before shot A's frame at {acquired}"
            ));
        }
    }
    wrong.extend(after_a(&w));
    Ok(if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    })
}

/// P4's affirmative absence (`p4.no-volume-arm`): no press, drag or other
/// input brought any volume to the arm in any answer's witness, shot A's or
/// a later one's, so its clicks set nothing, not even a wrong slider and not
/// late. Outer error (incomplete): a witness does not read.
fn no_volume_arm(run: &[Value]) -> Result<Result<(), String>, String> {
    let w = arm_witness(run)?;
    let reached: Vec<String> = std::iter::once(("shot-a.png", &w.a))
        .chain(w.later.iter().map(|(what, l)| (what.as_str(), l)))
        .filter(|(_, l)| !l.is_empty())
        .map(|(what, l)| format!("{what}: {l:?}"))
        .collect();
    Ok(if reached.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the volume arm was reached: {}",
            reached.join("; ")
        ))
    })
}

/// Every panel row's player is the one the row expected
/// (`p1.player-bound`): the `player` record's digest of the held copy
/// equals the digest the runner recorded as expected before the holds, the
/// runner's own executable's for every row but P5, whose mutated build is
/// named by `--player-sha256`, and the launched executable's (the first
/// copied file's). A run whose record names another digest, or none, fails.
fn player_bound(run: &[Value], row: &str) -> Result<Result<(), String>, String> {
    let p = run
        .iter()
        .find(|v| v["t"] == "player")
        .ok_or("the run recorded no player")?;
    let (sha, expected, runner, launched) = (
        p["sha256"].as_str().unwrap_or_default(),
        p["expected"].as_str().unwrap_or_default(),
        p["runner_sha256"].as_str().unwrap_or_default(),
        p["files"][0]["sha256"].as_str().unwrap_or_default(),
    );
    let own = row != "p5";
    Ok(
        if !sha.is_empty() && sha == expected && sha == launched && (!own || expected == runner) {
            Ok(())
        } else {
            Err(format!(
                "the player run has sha256 {sha}; expected {}; the runner's own {runner}",
                p["expected"]
            ))
        },
    )
}

/// Every panel row's starting volumes (`p1.volumes-start`): the
/// `device.toml` written before the run holds each of the three volumes at
/// exactly [`START_VOLUME`].
pub fn volumes_start(before: &[u8]) -> Result<(), String> {
    let table: toml::Table = std::str::from_utf8(before)
        .map_err(|e| e.to_string())
        .and_then(|t| toml::from_str(t).map_err(|e| e.to_string()))
        .map_err(|e| format!("device.toml before the run does not read: {e}"))?;
    at_start_volume(&table, " before the run")
}

/// Every byte of `device.toml` but the three volumes' lines is unchanged:
/// the files have the same lines, in order, and a line that differs is a
/// volume's in both.
pub fn device_rest(before: &[u8], after: &[u8]) -> Result<(), String> {
    let lines =
        |b: &[u8]| -> Vec<Vec<u8>> { b.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect() };
    let (b, a) = (lines(before), lines(after));
    if b.len() != a.len() {
        return Err(format!(
            "device.toml has {} lines after the run, {} before",
            a.len(),
            b.len()
        ));
    }
    let volume = |l: &[u8]| {
        VOLUME_KEYS
            .iter()
            .any(|(_, k)| l.starts_with(format!("{k} = ").as_bytes()))
    };
    for (i, (x, y)) in b.iter().zip(&a).enumerate() {
        if x != y
            && !(volume(x)
                && volume(y)
                && x.split(|&c| c == b' ').next() == y.split(|&c| c == b' ').next())
        {
            return Err(format!(
                "device.toml line {} changed: `{}` -> `{}`",
                i + 1,
                String::from_utf8_lossy(x),
                String::from_utf8_lossy(y)
            ));
        }
    }
    Ok(())
}

/// The run's own clicks: its calls are exactly the row's (`want`: what
/// each sends and the client point it aims at, in order; a point of
/// `None`, the sink's focus click, is any point, the same for its move and
/// its click), each with a tag of its own; and in the recorder's stream
/// each click call's tag gives exactly one left press and then its
/// release, with no other button edge between them, and no left button is
/// down at the end.
pub fn clicks(
    run: &[Value],
    rec: &[Value],
    want: &[(&str, Option<(i32, i32)>)],
) -> Result<(), String> {
    // What each call sends, its tag and the client point it aims at.
    type Call<'a> = (&'a str, u32, Option<(i64, i64)>);
    let calls: Vec<Call<'_>> = run
        .iter()
        .filter(|v| v["t"] == "call")
        .map(|v| {
            let point = v["point"][0].as_i64().zip(v["point"][1].as_i64());
            Some((
                v["what"].as_str()?,
                u32::try_from(v["tag"].as_u64()?).ok()?,
                point,
            ))
        })
        .collect::<Option<_>>()
        .ok_or("a call record without its tag or kind")?;
    let got: Vec<String> = calls.iter().map(|(w, _, p)| format!("{w} {p:?}")).collect();
    let shape = || format!("the run's calls {got:?}, not the row's {want:?}");
    if calls.len() != want.len() {
        return Err(shape());
    }
    let mut any = None;
    for ((what, _, point), (want_what, want_point)) in calls.iter().zip(want) {
        let point = point.ok_or_else(shape)?;
        let fits = match want_point {
            Some((x, y)) => point == (i64::from(*x), i64::from(*y)),
            None => *any.get_or_insert(point) == point,
        };
        if what != want_what || !fits {
            return Err(shape());
        }
    }
    let mut tags: Vec<u32> = calls.iter().map(|&(_, t, _)| t).collect();
    tags.sort_unstable();
    tags.dedup();
    if tags.len() != calls.len() {
        return Err(format!("two calls share a tag: {got:?}"));
    }
    let clicks: Vec<u32> = calls
        .iter()
        .filter(|(w, _, _)| *w == "click")
        .map(|&(_, t, _)| t)
        .collect();
    let edges: Vec<&Value> = rec
        .iter()
        .filter(|v| {
            v["t"] == "mouse" && v["buttons"].as_u64().unwrap_or(0) & u64::from(BUTTON_EDGES) != 0
        })
        .collect();
    for tag in &clicks {
        let mine: Vec<usize> = edges
            .iter()
            .enumerate()
            .filter(|(_, v)| v["extra"].as_u64() == Some(u64::from(*tag)))
            .map(|(i, _)| i)
            .collect();
        let [down, up] = mine[..] else {
            return Err(format!(
                "click {tag:#010x}: {} button edges, not its press and release",
                mine.len()
            ));
        };
        if edges[down]["buttons"] != 1 || edges[up]["buttons"] != 2 || up != down + 1 {
            return Err(format!(
                "click {tag:#010x}: not a left press followed at once by its release: {} {}",
                edges[down], edges[up]
            ));
        }
    }
    // No left button down at the end: presses and releases pair up.
    let mut down = 0_i64;
    for v in &edges {
        let b = v["buttons"].as_u64().unwrap_or(0);
        down += i64::from(b & 1 != 0) - i64::from(b & 2 != 0);
    }
    if down != 0 {
        return Err(format!("{down} left presses without their release"));
    }
    Ok(())
}

/// START's and SELECT's edges: each of `held` pressed and then released in
/// the device's own pad reports (from one handle), any other never pressed
/// there, and neither pressed in another device's report.
pub fn panel_keys(rec: &[Value], held: &[u8]) -> Result<(), String> {
    for b in [START, SELECT] {
        let bit = 1_u16 << (b - 1);
        let set = |v: &&Value| {
            v["bytes"]
                .as_str()
                .and_then(pad_buttons)
                .is_some_and(|x| x & bit != 0)
        };
        let pads = || rec.iter().filter(|v| v["t"] == "pad");
        if let Some(v) = pads().filter(|v| v["ours"] != true).find(set) {
            return Err(format!("button {b} pressed by another device: {v}"));
        }
        let ours: Vec<&Value> = pads().filter(|v| v["ours"] == true).collect();
        let press = ours.iter().position(&set);
        match (held.contains(&b), press) {
            (false, None) => {}
            (false, Some(i)) => {
                return Err(format!(
                    "button {b} was pressed, and the row holds no {b}: {}",
                    ours[i]
                ));
            }
            (true, None) => return Err(format!("no press of button {b} from the device's pad")),
            (true, Some(i)) => {
                let h = &ours[i]["h"];
                if !ours[i..].iter().any(|v| v["h"] == *h && !set(v)) {
                    return Err(format!(
                        "no release of button {b} from the pad that pressed it"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The verdicts of a panel row's run (`p1`, `p4`, `p5`): P1's checks, and
/// a negative's affirmative ones, which show that it fails for its
/// declared reason only (P4's and P5's volumes all kept at 1.0, P5's
/// player the declared mutated build).
pub fn panel(build: Option<&str>, f: &Files, x: &PanelFiles, row: &str) -> Vec<Judged> {
    let Some(held) = held_by(row) else {
        return Vec::new();
    };
    let want = panel_calls(super::run::targets_of(row));
    let span = |kind: &str| first_q(&f.run, kind);
    let bytes = |b: &Bytes| b.clone();
    let buttons = held.iter().fold(0_u16, |a, b| a | 1 << (b - 1));
    let calls = recs(&f.run).map(click_tags).unwrap_or_default();
    let mode = (SINGLE, MODE_LINK);
    let file = |name: &'static str| (SINGLE, name);
    let mut out = vec![
        verdict(
            provenance::CHECK,
            &[RUN_LINK, mode],
            panel_provenance(build, f, &x.mode),
        ),
        verdict("p1.grants", &[RUN_LINK], recs(&f.link).map(grants)),
        verdict(
            "p1.player-bound",
            &[RUN_RUN],
            recs(&f.run).and_then(|r| player_bound(r, row)),
        ),
        verdict(
            "p1.volumes",
            &[file(DEVICE_BEFORE), file(DEVICE_AFTER)],
            bytes(&x.device_before).and_then(|b| Ok(volumes(&b, &bytes(&x.device_after)?))),
        ),
        verdict(
            "p1.volumes-start",
            &[file(DEVICE_BEFORE)],
            bytes(&x.device_before).map(|b| volumes_start(&b)),
        ),
        verdict(
            "p1.volume-arm",
            &[RUN_RUN],
            recs(&f.run).and_then(volume_arm),
        ),
        verdict(
            "p1.device-toml",
            &[file(DEVICE_BEFORE), file(DEVICE_AFTER)],
            bytes(&x.device_before).and_then(|b| Ok(device_rest(&b, &bytes(&x.device_after)?))),
        ),
        verdict(
            "p1.player-toml",
            &[file(PLAYER_BEFORE), file(PLAYER_AFTER)],
            bytes(&x.player_before).and_then(|b| {
                let a = bytes(&x.player_after)?;
                Ok(if a == b {
                    Ok(())
                } else {
                    Err("player.toml changed".into())
                })
            }),
        ),
        verdict(
            "p1.panel-keys",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                covered(rec, span("call")?, span("quiet")?)?;
                Ok(panel_keys(rec, held))
            })(),
        ),
        verdict(
            "p1.clicks",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                covered(rec, span("call")?, span("quiet")?)?;
                Ok(clicks(recs(&f.run)?, rec, &want))
            })(),
        ),
        verdict(
            "p1.no-foreign",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                let (from, to) = (span("call")?, span("quiet")?);
                covered(rec, from, to)?;
                Ok(no_foreign(rec, &calls, from, to))
            })(),
        ),
        verdict(
            "p1.buttons-released",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                let Ok((pad, _)) = pressed(rec, None, buttons, false) else {
                    return Err("no press of the row's buttons to be released".into());
                };
                let end = rec.iter().map(q).max().unwrap_or(i64::MAX);
                covered(rec, pad.q, end)?;
                Ok(neutral_of(rec, pad, None))
            })(),
        ),
        verdict(
            "p1.release-requested",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(release_requested(r, recs(&f.link)?))),
        ),
        verdict(
            "p1.quiet-before-close",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(quiet_before_close(r, recs(&f.link)?, true))),
        ),
    ];
    let kept = |id| {
        verdict(
            id,
            &[file(DEVICE_AFTER)],
            bytes(&x.device_after).map(|a| volumes_kept(&a)),
        )
    };
    match row {
        "p4" => {
            out.push(kept("p4.volumes-kept"));
            out.push(verdict(
                "p4.no-volume-arm",
                &[RUN_RUN],
                recs(&f.run).and_then(no_volume_arm),
            ));
        }
        "p5" => {
            out.push(kept("p5.volumes-kept"));
            let (outcome, detail) = match recs(&f.run).and_then(mutation_build) {
                Ok(Ok(d)) => (Outcome::Pass, d),
                Ok(Err(d)) => (Outcome::Fail, d),
                Err(d) => (Outcome::Incomplete, d),
            };
            out.push(Judged {
                check: "p5.mutation-build",
                outcome,
                detail,
                inputs: vec![RUN_RUN],
            });
        }
        _ => {}
    }
    out
}

/// A reference row's attachment (`ref.attached`): one `attach` record,
/// made after the sink's focus click and before the focus click on the
/// attached window, of a 1920×1080 client area.
fn attached(run: &[Value]) -> Result<(), String> {
    let at: Vec<usize> = run
        .iter()
        .enumerate()
        .filter(|(_, v)| v["t"] == "attach")
        .map(|(i, _)| i)
        .collect();
    let [at] = at[..] else {
        return Err(format!("{} `attach` records, not one", at.len()));
    };
    let a = &run[at];
    let size = [a["client"][2].as_i64(), a["client"][3].as_i64()];
    let want = test_device::panel::CLIENT.map(|c| Some(i64::from(c)));
    if size != want {
        return Err(format!(
            "the attached window's client area is {:?}, not {:?}",
            a["client"],
            test_device::panel::CLIENT
        ));
    }
    let calls: Vec<usize> = run
        .iter()
        .enumerate()
        .filter(|(_, v)| v["t"] == "call")
        .map(|(i, _)| i)
        .collect();
    // The sink's move and click come before it, every other call after.
    if calls.len() < 3 || !(calls[1] < at && at < calls[2]) {
        return Err(
            "the attachment is not between the sink's focus click and the next call".into(),
        );
    }
    Ok(())
}

/// A reference row holds nothing on the device (`ref.device-idle`): its
/// link writes no `lease` and no held change, and the recorder holds no
/// report of the device's pad with a button down and no key edge of its
/// keyboard.
fn device_idle(link: &[Value], rec: &[Value]) -> Result<(), String> {
    if let Some(v) = link.iter().find(|v| {
        v["t"] == "cmd"
            && v["line"]
                .as_str()
                .and_then(Command::parse_canonical)
                .is_some_and(|c| c.holds_input())
    }) {
        return Err(format!(
            "the link wrote a hold-bearing command: {}",
            v["line"]
        ));
    }
    if let Some(v) = rec.iter().find(|v| {
        v["ours"] == true
            && (v["t"] == "key"
                || (v["t"] == "pad" && v["bytes"].as_str().and_then(pad_buttons) != Some(0)))
    }) {
        return Err(format!("the device gave input during the run: {v}"));
    }
    Ok(())
}

/// A reference row's captures bound to the attachment and the clicks
/// (`ref.capture-bound`): one `capture` record, of shot A, of the attached
/// process and window and its client area as attached; two captures,
/// [`super::run::SHOT_A`] and [`super::run::SHOT_A_2`], each begun after the
/// recorder's button-up of the run's last click call, the second begun at
/// least [`super::run::CAPTURE_GAP`] after the first ended (the link's
/// counter frequency).
fn capture_bound(run: &[Value], rec: &[Value], freq: i64) -> Result<Result<(), String>, String> {
    use super::run::{CAPTURE_GAP, SHOT_A, SHOT_A_2};
    let attach = run
        .iter()
        .find(|v| v["t"] == "attach")
        .ok_or("the run recorded no attachment")?;
    let caps: Vec<&Value> = run.iter().filter(|v| v["t"] == "capture").collect();
    let [cap] = caps[..] else {
        return Ok(Err(format!("{} `capture` records, not one", caps.len())));
    };
    let mut wrong = Vec::new();
    if cap["what"] != SHOT_A {
        wrong.push(format!("the capture is of {}, not {SHOT_A}", cap["what"]));
    }
    for key in ["process", "window", "client"] {
        if cap[key] != attach[key] {
            wrong.push(format!(
                "the capture's {key} {} is not the attachment's {}",
                cap[key], attach[key]
            ));
        }
    }
    let last = run
        .iter()
        .rev()
        .find(|v| v["t"] == "call" && v["what"] == "click")
        .and_then(|v| v["tag"].as_u64())
        .ok_or("the run made no click")?;
    let up = rec
        .iter()
        .find(|v| v["t"] == "mouse" && v["extra"].as_u64() == Some(last) && v["buttons"] == 2)
        .map(q)
        .ok_or("the recorder saw no button-up of the last click")?;
    let grabs = cap["grabs"]
        .as_array()
        .ok_or("the capture lists no grabs")?;
    let files: Vec<&str> = grabs.iter().filter_map(|g| g["file"].as_str()).collect();
    if files != [SHOT_A, SHOT_A_2] {
        wrong.push(format!(
            "the captures are {files:?}, not {SHOT_A} and {SHOT_A_2}"
        ));
    }
    for g in grabs {
        if g["before"].as_i64().is_none_or(|b| b <= up) {
            wrong.push(format!(
                "{} was begun at {}, not after the last click's button-up at {up}",
                g["file"], g["before"]
            ));
        }
    }
    if let [a, b] = &grabs[..] {
        let gap =
            b["before"].as_i64().unwrap_or(i64::MIN) - a["after"].as_i64().unwrap_or(i64::MAX);
        let want = i64::try_from(CAPTURE_GAP.as_micros()).unwrap_or(i64::MAX) * freq / 1_000_000;
        if gap < want {
            wrong.push(format!(
                "the second capture was begun {gap} ticks after the first ended, not at least {want}"
            ));
        }
    }
    Ok(if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    })
}

/// The verdicts of a reference row's run (`p1-ref`, `p1-ref-75`): the
/// device's provenance, the attachment, the row's clicks and no foreign
/// input, the device idle, the captures bound, and the end's release
/// request and quiet evidence before the close (no lease, so any reason).
pub fn reference(build: Option<&str>, f: &Files, row: &str) -> Vec<Judged> {
    let want = panel_calls(super::run::targets_of(row));
    let span = |kind: &str| first_q(&f.run, kind);
    let calls = recs(&f.run).map(click_tags).unwrap_or_default();
    let freq = || {
        recs(&f.link)?
            .iter()
            .find(|v| v["t"] == "open")
            .and_then(|v| v["freq"].as_i64())
            .ok_or_else(|| "the link has no `open` record".to_owned())
    };
    vec![
        verdict(
            provenance::CHECK,
            &[RUN_LINK],
            provenance(build, &[(LINK, &f.link)]),
        ),
        verdict("ref.attached", &[RUN_RUN], recs(&f.run).map(attached)),
        verdict(
            "p1.clicks",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                covered(rec, span("call")?, span("quiet")?)?;
                Ok(clicks(recs(&f.run)?, rec, &want))
            })(),
        ),
        verdict(
            "p1.no-foreign",
            &[RUN_RUN, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                let (from, to) = (span("call")?, span("quiet")?);
                covered(rec, from, to)?;
                Ok(no_foreign(rec, &calls, from, to))
            })(),
        ),
        verdict(
            "ref.device-idle",
            &[RUN_LINK, RUN_REC],
            (|| {
                let rec = recs(&f.recorder)?;
                covered(rec, span("call")?, span("quiet")?)?;
                Ok(device_idle(recs(&f.link)?, rec))
            })(),
        ),
        verdict(
            "ref.capture-bound",
            &[RUN_RUN, RUN_REC],
            (|| capture_bound(recs(&f.run)?, recs(&f.recorder)?, freq()?))(),
        ),
        verdict(
            "p1.release-requested",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(release_requested(r, recs(&f.link)?))),
        ),
        verdict(
            "ref.quiet-before-close",
            &[RUN_RUN, RUN_LINK],
            recs(&f.run).and_then(|r| Ok(quiet_before_close(r, recs(&f.link)?, false))),
        ),
    ]
}

/// The calls every panel or reference row must make, from its target
/// table alone (`targets`), not from the runner's steps (which a broken
/// runner would change along with its calls): the sink's focus click (its
/// move and its click, at a point of its own), the player's focus click at
/// [`FOCUS_POINT`], a calibration call at the focus point and at each
/// target, then each target's move and click, in the table's order.
pub fn panel_calls(
    targets: &[test_device::panel::Target],
) -> Vec<(&'static str, Option<(i32, i32)>)> {
    let targets: Vec<(i32, i32)> = targets.iter().map(|t| (t.x, t.y)).collect();
    let mut out = vec![
        ("move", None),
        ("click", None),
        ("move", Some(FOCUS_POINT)),
        ("click", Some(FOCUS_POINT)),
    ];
    out.extend(
        std::iter::once(FOCUS_POINT)
            .chain(targets.iter().copied())
            .map(|p| ("calibrate", Some(p))),
    );
    for p in targets {
        out.extend([("move", Some(p)), ("click", Some(p))]);
    }
    out
}

/// The buttons a panel row holds.
pub fn held_by(row: &str) -> Option<&'static [u8]> {
    match row {
        "p1" | "p5" => Some(&[START, SELECT]),
        "p4" => Some(&[START]),
        _ => None,
    }
}

/// What `judge` reads of a run's folder, once: every check and every
/// digest of its verdicts comes from these bytes, so a verdict never
/// carries the digest of a file other than the one it judged.
pub struct Reading {
    start: Value,
    files: Files,
    inner: Option<Files>,
    /// N2p's inner's mode link.
    inner_mode: Option<Stream>,
    panel: Option<PanelFiles>,
}

/// Reads the run in `dir` for row `row`.
///
/// # Errors
///
/// An unknown row, or the run's `start` unreadable.
pub fn read(row: &str, dir: &Path) -> anyhow::Result<Reading> {
    let e = super::check::expectation_of(row)
        .ok_or_else(|| anyhow::anyhow!("`judge` has no row `{row}`"))?;
    let standing = contract::standing(&e, dir, super::COMMIT);
    read_role(row, dir, SINGLE, &standing)
}

/// Reads the run in `dir` as role `role` of row `row`, its streams kept
/// only as `standing` takes them for that role.
fn read_role(
    row: &str,
    dir: &Path,
    role: &str,
    standing: &contract::Standing,
) -> anyhow::Result<Reading> {
    let files = Files::read(dir);
    let start = recs(&files.run)
        .ok()
        .and_then(|r| r.first().cloned())
        .filter(|v| v["t"] == "start")
        .ok_or_else(|| anyhow::anyhow!("{}: no `start` record", dir.display()))?;
    let files = files.standing(role, standing);
    let (inner, panel) = match row {
        "sink" | "sink-early-close" | "p1-ref" | "p1-ref-75" => (None, None),
        "sink-kill" | "p1-kill" => (
            Some(Files::read(&dir.join(INNER)).standing(INNER, standing)),
            None,
        ),
        _ if held_by(row).is_some() => (None, Some(PanelFiles::read(dir))),
        _ => anyhow::bail!("`judge` has no row `{row}`"),
    };
    let inner_mode = (row == "p1-kill")
        .then(|| Stream::read::<evidence::Link>(&dir.join(INNER).join(MODE_LINK)));
    Ok(Reading {
        start,
        files,
        inner,
        inner_mode,
        panel,
    })
}

/// The verdict lines of `r`, read as row `row`, each input's digest that
/// of the bytes read.
pub fn lines(row: &str, r: &Reading) -> Vec<Value> {
    let run = r.start["run"].as_str().unwrap_or_default();
    let build = r.start["build"].as_str();
    let f = &r.files;
    let judged = match (row, &r.inner, &r.panel) {
        ("sink-kill", Some(inner), _) => kill(&N2, build, f, inner, None),
        ("p1-kill", Some(inner), _) => kill(&N2P, build, f, inner, r.inner_mode.as_ref()),
        ("p1-ref" | "p1-ref-75", _, _) => reference(build, f, row),
        (_, _, Some(x)) => panel(build, f, x, row),
        _ => n1(build, f),
    };
    let sha = |role: &str, path: &str| {
        let files = if role == INNER {
            r.inner.as_ref()
        } else {
            Some(f)
        };
        match (files, path) {
            (None, _) => String::new(),
            (Some(_), MODE_LINK) if role == INNER => r
                .inner_mode
                .as_ref()
                .map(|m| m.sha.clone())
                .unwrap_or_default(),
            (Some(fs), RUN) => fs.run.sha.clone(),
            (Some(fs), LINK) => fs.link.sha.clone(),
            (Some(fs), RECORDER) => fs.recorder.sha.clone(),
            (Some(_), other) => r
                .panel
                .as_ref()
                .filter(|_| role != INNER)
                .and_then(|x| x.digest(other))
                .unwrap_or_default(),
        }
    };
    judged
        .into_iter()
        .map(|j| {
            json!({
                "row": row, "run": run, "check": j.check,
                "outcome": match j.outcome {
                    Outcome::Pass => "pass",
                    Outcome::Fail => "fail",
                    Outcome::Incomplete => "incomplete",
                },
                "detail": j.detail,
                "inputs": j.inputs.iter().map(|(role, path)| json!({
                    "role": role, "path": path, "sha256": sha(role, path),
                })).collect::<Vec<_>>(),
                "producer": { "name": PRODUCER, "version": super::COMMIT },
            })
        })
        .collect()
}

/// The verdict lines of the set in `dir` for its row `e`: the set's
/// provenance over every role's link (and each panel role's mode link), the
/// reservation's build named in every `hello`, each panel role's driving
/// connection in the panel mode; then each role's row checks on its run,
/// its own provenance left out, each id and input named by the role. The
/// standing of every role's streams is read once, for the whole set; every
/// line names the set's row and id.
///
/// # Errors
///
/// The reservation does not read, or a role's `start` does not.
pub fn set_lines(e: &contract::Expectation, dir: &Path) -> anyhow::Result<Vec<Value>> {
    let set = super::owner::reservation(dir)?
        .ok_or_else(|| anyhow::anyhow!("{} holds no reservation", dir.display()))?;
    let standing = contract::standing(e, dir, super::COMMIT);
    let mut out = Vec::new();
    let mut links: Vec<(&'static str, &'static str, Stream)> = Vec::new();
    let mut modes_ok: Result<Result<(), String>, String> = Ok(Ok(()));
    for role in &e.roles {
        let reserved = set
            .roles
            .iter()
            .find(|r| r.role == role.name)
            .ok_or_else(|| anyhow::anyhow!("the set reserves no role `{}`", role.name))?;
        let r = read_role(role.row, &dir.join(&reserved.dir), role.name, &standing)?;
        links.push((role.name, LINK, r.files.link.clone()));
        if let Some(x) = &r.panel {
            links.push((role.name, MODE_LINK, x.mode.clone()));
            if let Ok(Ok(())) = modes_ok {
                modes_ok = in_panel_mode(&r.files.link);
            }
        }
        for mut line in lines(role.row, &r) {
            if line["check"] == provenance::CHECK {
                continue;
            }
            line["check"] = json!(format!(
                "{}/{}",
                role.name,
                line["check"].as_str().unwrap_or("")
            ));
            for input in line["inputs"].as_array_mut().into_iter().flatten() {
                if input["role"] == SINGLE {
                    input["role"] = json!(role.name);
                }
            }
            line["row"] = json!(e.row);
            line["run"] = json!(set.set);
            out.push(line);
        }
    }
    let sources: Vec<(String, &Stream)> = links
        .iter()
        .map(|(role, path, s)| (format!("{role}/{path}"), s))
        .collect();
    let named: Vec<(&str, &Stream)> = sources.iter().map(|(n, s)| (n.as_str(), *s)).collect();
    let judged = match provenance(Some(&set.build), &named) {
        Ok(Ok(())) => modes_ok,
        other => other,
    };
    let (outcome, detail) = match judged {
        Ok(Ok(())) => ("pass", String::new()),
        Ok(Err(why)) => ("fail", why),
        Err(why) => ("incomplete", why),
    };
    let first = json!({
        "row": e.row, "run": set.set, "check": provenance::CHECK, "outcome": outcome,
        "detail": detail,
        "inputs": links.iter().map(|(role, path, s)| json!({
            "role": role, "path": path, "sha256": s.sha,
        })).collect::<Vec<_>>(),
        "producer": { "name": PRODUCER, "version": super::COMMIT },
    });
    out.insert(0, first);
    Ok(out)
}

/// Judges the run in `dir` as row `row` and writes `verdicts-judge.jsonl`.
///
/// # Errors
///
/// An unknown row, the run's `start` unreadable, or the file cannot be
/// written.
pub fn judge(row: &str, dir: &Path) -> anyhow::Result<()> {
    let lines = match super::check::expectation_of(row) {
        Some(e) if e.binding == contract::Binding::Set => set_lines(&e, dir)?,
        _ => lines(row, &read(row, dir)?),
    };
    let out = super::owner::Stream::create(&dir.join(format!("verdicts-{PRODUCER}.jsonl")))?;
    for line in lines {
        out.write(&line);
    }
    if let Some(e) = out.close() {
        anyhow::bail!("{e}");
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use test_device::protocol::VERSION;
    use test_device::report::PadReport;

    use super::super::link::fake::TOKEN;
    use super::*;

    const SINK: u64 = 0x5151;
    const FREQ: i64 = 10_000_000;

    fn hello(quiet: bool, reason: &str, lease: &str) -> String {
        hello_of(TOKEN, quiet, reason, lease)
    }

    fn hello_of(token: Token, quiet: bool, reason: &str, lease: &str) -> String {
        format!(
            "hello {VERSION} build 0123456789ab mode 8 lease {lease} armed 0 reason {reason} \
             boot power-on count 0 identified 1 faults 0 quiet {} session {token}",
            u8::from(quiet)
        )
    }

    /// The connection `n` after [`TOKEN`]'s.
    fn after(n: u32) -> Token {
        Token {
            count: TOKEN.count + n,
            ..TOKEN
        }
    }

    fn pad(q: i64, buttons: u16, ours: bool) -> Value {
        let r = PadReport {
            buttons,
            ..PadReport::default()
        }
        .encode();
        let hex: String = std::iter::once(0)
            .chain(r)
            .map(|b| format!("{b:02x}"))
            .collect();
        json!({"t": "pad", "q": q, "h": 2, "ours": ours, "bytes": hex, "fg": {"window": SINK, "pid": 1}})
    }

    fn key(q: i64, down: bool, ours: bool) -> Value {
        json!({"t": "key", "q": q, "h": 1, "ours": ours, "code": F24_SCAN, "down": down, "fg": {"window": SINK, "pid": 1}})
    }

    /// The device's µs at host tick `t`: 9 s ahead of the host.
    fn device(t: i64) -> u64 {
        u64::try_from(t / 10 + 9_000_000).unwrap()
    }

    fn deadline(check: i64) -> u64 {
        device(check) + u64::from(SLICE_MS) * 1000 - 300
    }

    /// A good N1 run: run, link and recorder records.
    pub fn good() -> Files {
        let tag = 0xABCD_0001_u32;
        let run = vec![
            json!({"t": "start"}),
            json!({"t": "sink", "q": 100, "window": SINK}),
            json!({"t": "call", "q": 200, "tag": tag - 1, "what": "move", "point": [100, 50]}),
            json!({"t": "call", "q": 210, "tag": tag, "what": "click", "point": [100, 50]}),
            json!({"t": "held", "q": 2000}),
            json!({"t": "stop", "q": 5000, "what": "end"}),
            json!({"t": "quiet", "q": 5600, "line": hello(true, "released", "-")}),
            json!({"t": "close", "q": 5800, "window": SINK}),
            json!({"t": "end", "exit": 0, "errors": []}),
        ];
        let mut link = vec![
            json!({"t": "open", "q": 10, "freq": FREQ}),
            json!({"t": "cmd", "q": 20, "line": "hello"}),
            json!({"t": "ans", "q": 30, "sent": 20, "cmd": "hello", "line": hello(true, "none", "-")}),
        ];
        for k in 0..4 {
            let (sent, recv) = (40 + k * 100, 60 + k * 100);
            link.push(json!({"t": "ping", "n": k, "sent": sent, "device": device(50 + k * 100), "recv": recv}));
        }
        let lease = format!("lease {TOKEN} 250 {}", deadline(1000));
        let renew = format!("renew {TOKEN}:1 {}", deadline(1800));
        link.extend([
            json!({"t": "check", "q": 1000, "line": lease}),
            json!({"t": "cmd", "q": 1010, "line": lease}),
            json!({"t": "ans", "q": 1020, "sent": 1010, "cmd": lease, "line": format!("lease {TOKEN}:1 {}", deadline(1000))}),
            json!({"t": "check", "q": 1800, "line": renew}),
            json!({"t": "cmd", "q": 1810, "line": renew}),
            json!({"t": "fence", "q": 5100}),
            json!({"t": "cmd", "q": 5110, "line": "release"}),
            json!({"t": "ans", "q": 5120, "sent": 5110, "cmd": "release", "line": "ok"}),
            json!({"t": "cmd", "q": 5500, "line": "hello"}),
            json!({"t": "ans", "q": 5510, "sent": 5500, "cmd": "hello", "line": hello(true, "released", "-")}),
        ]);
        let recorder = vec![
            json!({"t": "ready", "q": 50}),
            json!({"t": "mouse", "q": 220, "h": 0, "buttons": 1, "extra": tag, "fg": {"window": SINK, "pid": 1}}),
            json!({"t": "mouse", "q": 221, "h": 0, "buttons": 2, "extra": tag, "fg": {"window": SINK, "pid": 1}}),
            pad(1500, 1, true),
            key(1600, true, true),
            pad(5200, 0, true),
            key(5210, false, true),
            json!({"t": "end", "q": 6000}),
        ];
        Files {
            run: Stream::of(run),
            link: Stream::of(link),
            recorder: Stream::of(recorder),
        }
    }

    fn outcomes(f: &Files) -> Vec<(&'static str, Outcome)> {
        n1(Some("0123456789ab"), f)
            .into_iter()
            .map(|j| (j.check, j.outcome))
            .collect()
    }

    fn outcome(f: &Files, check: &str) -> (Outcome, String) {
        let j = n1(Some("0123456789ab"), f)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap();
        (j.outcome, j.detail)
    }

    fn edit(s: &mut Stream, f: impl FnOnce(&mut Vec<Value>)) {
        f(s.records.as_mut().unwrap());
    }

    #[test]
    fn a_good_run_passes_every_check() {
        let o = outcomes(&good());
        assert!(o.iter().all(|(_, x)| *x == Outcome::Pass), "{o:?}");
        assert_eq!(o.len(), 7);
    }

    #[test]
    fn n1_needs_the_quiet_hello_before_the_close() {
        let mut f = good();
        // The early-close negative: the close before the evidence.
        edit(&mut f.run, |r| {
            let close = r.remove(7);
            r.insert(5, close);
        });
        assert_eq!(outcome(&f, "n1.quiet-before-close").0, Outcome::Fail);
        // A quiet `hello` read before the fence does not count.
        let mut f = good();
        edit(&mut f.link, |l| {
            let fence = l.iter().position(|v| v["t"] == "fence").unwrap();
            let f = l.remove(fence);
            l.push(f);
        });
        assert_eq!(outcome(&f, "n1.quiet-before-close").0, Outcome::Fail);
        // Quiet with another reason.
        let mut f = good();
        edit(&mut f.run, |r| {
            r[6]["line"] = json!(hello(true, "expired", "-"))
        });
        assert_eq!(outcome(&f, "n1.quiet-before-close").0, Outcome::Fail);
    }

    /// Every order of the five records the release evidence rests on (the
    /// fence, the `release` written, its `ok` read, the `hello` written,
    /// and the evidence read as its answer, each answer naming its
    /// command's write by `sent`), once on the host's counter with the
    /// stream in that order, and once in the stream alone with the counter
    /// in the release order. The evidence stands exactly in the order
    /// fence, `release`, `ok`, `hello`, answer, which is the positive
    /// control of each.
    #[test]
    fn the_evidence_stands_only_in_the_release_order() {
        fn permutations(n: usize) -> Vec<Vec<usize>> {
            if n == 0 {
                return vec![Vec::new()];
            }
            let mut out = Vec::new();
            for p in permutations(n - 1) {
                for at in 0..=p.len() {
                    let mut q = p.clone();
                    q.insert(at, n - 1);
                    out.push(q);
                }
            }
            out
        }
        let quiet = hello(true, "released", "-");
        let mut passed = 0;
        for (order, in_stream) in permutations(5)
            .into_iter()
            .flat_map(|o| [(o.clone(), false), (o, true)])
        {
            // order[k]: the rank of record k (fence, release, ok, hello,
            // answer) among the five, on the counter or in the stream.
            let rank = |k: usize| i64::try_from(order[k]).unwrap();
            let at = |k: usize| {
                5100 + 10
                    * if in_stream {
                        i64::try_from(k).unwrap()
                    } else {
                        rank(k)
                    }
            };
            let mut five: Vec<(usize, Value)> = [
                json!({"t": "fence", "q": at(0)}),
                json!({"t": "cmd", "q": at(1), "line": "release"}),
                json!({"t": "ans", "q": at(2), "sent": at(1), "cmd": "release", "line": "ok"}),
                json!({"t": "cmd", "q": at(3), "line": "hello"}),
                json!({"t": "ans", "q": at(4), "sent": at(3), "cmd": "hello", "line": quiet}),
            ]
            .into_iter()
            .enumerate()
            .collect();
            // The stream follows the ranks in both cases.
            five.sort_by_key(|(k, _)| order[*k]);
            let mut f = good();
            edit(&mut f.link, |l| {
                let from = l.iter().position(|v| v["t"] == "fence").unwrap();
                l.truncate(from);
                l.extend(five.into_iter().map(|(_, v)| v));
            });
            let got = outcome(&f, "n1.quiet-before-close").0;
            let ordered = order == [0, 1, 2, 3, 4];
            assert_eq!(
                got == Outcome::Pass,
                ordered,
                "{order:?} {in_stream}: {f:?}"
            );
            passed += usize::from(got == Outcome::Pass);
        }
        assert_eq!(passed, 2, "the positive controls");
        // An answer bound to another write does not count: the `hello`
        // answered names an earlier `hello`'s write.
        let mut f = good();
        edit(&mut f.link, |l| {
            let at = l.iter().position(|v| v["q"] == 5510).unwrap();
            l[at]["sent"] = json!(20);
        });
        assert_eq!(outcome(&f, "n1.quiet-before-close").0, Outcome::Fail);
    }

    /// The judge reads a stream as `check` does: a cut writer's
    /// unterminated last line is dropped, a record the strict reader
    /// refuses (an unknown field) leaves the stream unread.
    #[test]
    fn the_judge_reads_streams_strictly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RECORDER);
        let ready = r#"{"t":"ready","q":1,"run":"r","process":{"pid":1,"created":2},"window":3,"location":"L","keyboard":null,"pad":null}"#;
        std::fs::write(&path, format!("{ready}\n{{\"t\":\"pad\",\"q\":")).unwrap();
        let s = Stream::read::<evidence::Recorder>(&path);
        assert_eq!(s.records.unwrap().len(), 1);
        let extra = ready.replace(r#""q":1,"#, r#""q":1,"x":0,"#);
        std::fs::write(&path, format!("{extra}\n")).unwrap();
        assert!(Stream::read::<evidence::Recorder>(&path).records.is_err());
    }

    #[test]
    fn a_grant_or_renewal_before_its_check_fails() {
        // The renewal's check after its write.
        let mut f = good();
        edit(&mut f.link, |l| {
            let c = l
                .iter()
                .position(|v| v["t"] == "check" && v["q"] == 1800)
                .unwrap();
            l[c]["q"] = json!(1820);
        });
        assert_eq!(outcome(&f, "n1.grants").0, Outcome::Fail);
        // No check at all.
        let mut f = good();
        edit(&mut f.link, |l| {
            l.retain(|v| !(v["t"] == "check" && v["q"] == 1000))
        });
        assert_eq!(outcome(&f, "n1.grants").0, Outcome::Fail);
        // A grant of more than a slice.
        let mut f = good();
        edit(&mut f.link, |l| {
            let long = format!("lease {TOKEN} 300 {}", deadline(1000));
            for v in l.iter_mut().filter(|v| v["q"] == 1000 || v["q"] == 1010) {
                v["line"] = json!(long);
            }
        });
        assert_eq!(outcome(&f, "n1.grants").0, Outcome::Fail);
        // A deadline from a later instant than its check.
        let mut f = good();
        edit(&mut f.link, |l| {
            let late = format!("lease {TOKEN} 250 {}", deadline(1000) + 50_000);
            for v in l.iter_mut().filter(|v| v["q"] == 1000 || v["q"] == 1010) {
                v["line"] = json!(late);
            }
        });
        assert_eq!(outcome(&f, "n1.grants").0, Outcome::Fail);
        // No round trip before the grant.
        let mut f = good();
        edit(&mut f.link, |l| l.retain(|v| v["t"] != "ping"));
        assert_eq!(outcome(&f, "n1.grants").0, Outcome::Fail);
    }

    #[test]
    fn a_press_without_its_release_fails() {
        let mut f = good();
        edit(&mut f.recorder, |r| r.retain(|v| q(v) != 5200));
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Fail);
        let mut f = good();
        edit(&mut f.recorder, |r| r.retain(|v| q(v) != 5210));
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Fail);
    }

    /// A release counts only from the handle of its press: the pad's
    /// removal does not stand for the keyboard's break, and a handle that
    /// arrives again after the press is another binding, whose records do
    /// not release it.
    #[test]
    fn a_release_is_matched_to_its_press_s_handle() {
        let removal =
            |q: i64, h: u64| json!({"t": "dev", "q": q, "h": h, "change": "removal", "ours": true});
        let arrival =
            |q: i64, h: u64| json!({"t": "dev", "q": q, "h": h, "change": "arrival", "ours": true});
        // Neither release read; the removal of both interfaces stands for
        // both (the control).
        let mut f = good();
        edit(&mut f.recorder, |r| {
            r.retain(|v| q(v) != 5200 && q(v) != 5210);
            r.insert(5, removal(5300, 2));
            r.insert(6, removal(5310, 1));
        });
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Pass);
        // The pad's removal alone leaves F24 held.
        let mut f = good();
        edit(&mut f.recorder, |r| {
            r.retain(|v| q(v) != 5200 && q(v) != 5210);
            r.insert(5, removal(5300, 2));
        });
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Fail);
        // The keyboard's handle arrives again before its break.
        let mut f = good();
        edit(&mut f.recorder, |r| r.insert(5, arrival(5100, 1)));
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Fail);
        // The break from another handle of the device.
        let mut f = good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 5210).unwrap();
            r[at]["h"] = json!(9);
        });
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Fail);
    }

    #[test]
    fn a_press_the_recorder_never_saw_fails() {
        // From another handle only.
        let mut f = good();
        edit(&mut f.recorder, |r| r[3]["ours"] = json!(false));
        assert_eq!(outcome(&f, "n1.press").0, Outcome::Fail);
        let mut f = good();
        edit(&mut f.recorder, |r| r.retain(|v| q(v) != 1600));
        assert_eq!(outcome(&f, "n1.press").0, Outcome::Fail);
        // With another window in front.
        let mut f = good();
        edit(&mut f.recorder, |r| r[4]["fg"]["window"] = json!(7));
        assert_eq!(outcome(&f, "n1.press").0, Outcome::Fail);
    }

    #[test]
    fn a_foreign_edge_fails() {
        for foreign in [
            json!({"t": "mouse", "q": 3000, "h": 0, "buttons": 1, "extra": 0, "fg": {}}),
            // The tag of a move call: a button edge is only a click's.
            json!({"t": "mouse", "q": 3000, "h": 0, "buttons": 1, "extra": 0xABCD_0000_u32, "fg": {}}),
            json!({"t": "key", "q": 3000, "h": 5, "ours": false, "code": 30, "down": true, "fg": {}}),
            pad(3000, 0, false),
        ] {
            let mut f = good();
            edit(&mut f.recorder, |r| r.insert(5, foreign.clone()));
            assert_eq!(outcome(&f, "n1.no-foreign").0, Outcome::Fail, "{foreign}");
        }
        // After the close, it is not the run's.
        let mut f = good();
        edit(&mut f.recorder, |r| {
            r.insert(
                7,
                json!({"t": "mouse", "q": 5900, "h": 0, "buttons": 1, "extra": 0, "fg": {}}),
            );
        });
        assert_eq!(outcome(&f, "n1.no-foreign").0, Outcome::Pass);
    }

    /// A recorder stream that ends before the stretch a check reads
    /// cannot vouch for its end.
    #[test]
    fn a_recorder_that_ended_early_leaves_its_stretch_incomplete() {
        let mut f = good();
        edit(&mut f.recorder, |r| r.retain(|v| q(v) < 5300));
        assert_eq!(outcome(&f, "n1.no-foreign").0, Outcome::Incomplete);
        // The control: the whole stream.
        assert_eq!(outcome(&good(), "n1.no-foreign").0, Outcome::Pass);
    }

    #[test]
    fn a_recorder_error_makes_its_stretch_incomplete() {
        let mut f = good();
        edit(&mut f.recorder, |r| {
            r.insert(
                6,
                json!({"t": "error", "from": 5205, "to": 5208, "what": "x"}),
            );
        });
        assert_eq!(outcome(&f, "n1.neutral").0, Outcome::Incomplete);
        assert_eq!(outcome(&f, "n1.no-foreign").0, Outcome::Incomplete);
        // The press's stretch ended before it.
        assert_eq!(outcome(&f, "n1.press").0, Outcome::Pass);
    }

    /// The outer's cut code.
    const CUT: i32 = 0x4000_0123;

    fn n2_good() -> (Files, Files) {
        let n1 = good();
        let inner = Files {
            run: Stream::of(vec![
                json!({"t": "start"}),
                json!({"t": "call", "q": 200, "tag": 0xABCD_0000_u32, "what": "move", "point": [100, 50]}),
                json!({"t": "call", "q": 210, "tag": 0xABCD_0001_u32, "what": "click", "point": [100, 50]}),
                json!({"t": "held", "q": 2000}),
            ]),
            // Through the grant's answer.
            link: Stream::of(recs(&n1.link).unwrap()[..10].to_vec()),
            recorder: Stream::of(Vec::new()),
        };
        let outer = Files {
            run: Stream::of(vec![
                json!({"t": "start", "cut": CUT}),
                json!({"t": "kill", "q": 2050, "name": "inner", "requested": 2045}),
                json!({"t": "exit", "q": 2100, "name": "inner", "code": CUT}),
                json!({"t": "exit", "q": 2200, "name": "inner-recorder", "code": CUT}),
                json!({"t": "end", "exit": 0, "errors": []}),
            ]),
            link: Stream::of(vec![
                json!({"t": "open", "q": 2300, "freq": FREQ}),
                json!({"t": "ans", "q": 2310, "cmd": "hello", "line": hello_of(after(1), true, "closed", "-")}),
            ]),
            recorder: Stream::of(vec![
                pad(1500, 1, true),
                key(1600, true, true),
                pad(2150, 0, true),
                key(2160, false, true),
            ]),
        };
        (outer, inner)
    }

    fn n2_outcome(o: &Files, i: &Files, check: &str) -> Outcome {
        kill(&N2, Some("0123456789ab"), o, i, None)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap()
            .outcome
    }

    #[test]
    fn n2_needs_the_children_gone() {
        let (o, i) = n2_good();
        let all: Vec<Outcome> = kill(&N2, Some("0123456789ab"), &o, &i, None)
            .into_iter()
            .map(|j| j.outcome)
            .collect();
        assert!(all.iter().all(|x| *x == Outcome::Pass), "{all:?}");
        // The kill's fixture not reached, so nothing shown either way: the
        // lease had expired before the inner's connection ended; another
        // connection ended between the inner's and the outer's (a
        // device-side end before the kill); the inner's last grant was
        // refused.
        let (mut o, i) = n2_good();
        edit(&mut o.link, |l| {
            l[1]["line"] = json!(hello_of(after(1), true, "expired", "-"))
        });
        assert_eq!(n2_outcome(&o, &i, "n2.quiet"), Outcome::Incomplete);
        let (mut o, i) = n2_good();
        edit(&mut o.link, |l| {
            l[1]["line"] = json!(hello_of(after(2), true, "closed", "-"))
        });
        assert_eq!(n2_outcome(&o, &i, "n2.quiet"), Outcome::Incomplete);
        let (o, mut i) = n2_good();
        edit(&mut i.link, |l| l[9]["line"] = json!("err stale"));
        assert_eq!(n2_outcome(&o, &i, "n2.quiet"), Outcome::Incomplete);
        // A foreign edge after the kill and before the inner's end was
        // observed; and no observed end at all.
        let (mut o, i) = n2_good();
        edit(&mut o.recorder, |r| {
            r.insert(
                2,
                json!({"t": "key", "q": 2080, "h": 5, "ours": false, "code": 30, "down": true, "fg": {}}),
            );
        });
        assert_eq!(n2_outcome(&o, &i, "n2.no-foreign"), Outcome::Fail);
        let (mut o, i) = n2_good();
        edit(&mut o.run, |r| {
            r.remove(2);
        });
        assert_eq!(n2_outcome(&o, &i, "n2.no-foreign"), Outcome::Incomplete);
        // A foreign edge while the inner held, and one with tag 0 though a
        // move call had it.
        for (extra, h) in [(0_u32, 0), (0xABCD_0000, 0)] {
            let (mut o, i) = n2_good();
            edit(&mut o.recorder, |r| {
                r.insert(
                    0,
                    json!({"t": "mouse", "q": 1000, "h": h, "buttons": 1, "extra": extra, "fg": {}}),
                );
            });
            assert_eq!(
                n2_outcome(&o, &i, "n2.no-foreign"),
                Outcome::Fail,
                "{extra}"
            );
        }
        // The recorder ended by itself, not with the inner's job.
        let (mut o, i) = n2_good();
        edit(&mut o.run, |r| r[3]["code"] = json!(0));
        assert_eq!(n2_outcome(&o, &i, "n2.children-gone"), Outcome::Fail);
        // Its end observed before the kill only.
        let (mut o, i) = n2_good();
        edit(&mut o.run, |r| {
            let x = r.remove(3);
            r.insert(1, x);
        });
        assert_eq!(n2_outcome(&o, &i, "n2.children-gone"), Outcome::Fail);
        // Never observed.
        let (mut o, i) = n2_good();
        edit(&mut o.run, |r| {
            r.remove(3);
        });
        assert_eq!(n2_outcome(&o, &i, "n2.children-gone"), Outcome::Fail);
        // The inner wrote a release: not an abnormal exit.
        let (o, mut i) = n2_good();
        edit(&mut i.link, |l| {
            l.push(json!({"t": "cmd", "q": 2050, "line": "release"}))
        });
        assert_eq!(n2_outcome(&o, &i, "n2.abnormal"), Outcome::Fail);
        // A lease still held on the outer's first hello.
        let (mut o, i) = n2_good();
        edit(&mut o.link, |l| {
            l[1]["line"] = json!(hello_of(after(1), false, "none", "1"))
        });
        assert_eq!(n2_outcome(&o, &i, "n2.quiet"), Outcome::Fail);
    }

    /// The device's `device.toml` before a panel run: volumes at 1.0.
    pub const DEVICE_TOML: &str = "version = 3\nmonitor = \"M\"\nsystem_volume = 1.0\nkey_volume = 1.0\nbackground_volume = 1.0\n\n[library]\nroots = [\"C:/lib\"]\n";

    pub fn panel_pad(q: i64, buttons: u16, h: u64, ours: bool) -> Value {
        let mut v = pad(q, buttons, ours);
        v["h"] = json!(h);
        v
    }

    fn mouse(q: i64, buttons: u64, extra: u32) -> Value {
        json!({"t": "mouse", "q": q, "h": 0, "buttons": buttons, "extra": extra, "fg": {"window": SINK, "pid": 1}})
    }

    /// START and SELECT, as pad bits.
    const PANEL3: u16 = 1 << 8 | 1 << 9;

    /// A good P1 run: N1's link, a run with the panel's calls and records,
    /// the recorder's clicks and buttons, and the settings.
    pub fn panel_good() -> (Files, PanelFiles) {
        let n1 = good();
        // The volume arm's witness: each target's slider and percent,
        // reached after its click and before shot A's frame (2410).
        let arm = json!([
            [17, 25, 295, "press"],
            [18, 50, 315, "press"],
            [19, 75, 335, "press"]
        ]);
        // Calls: the sink's focus (move, click), the player's focus (move,
        // click), four calibrations, three targets (move, click), tagged
        // 0xABCD0000 on, at 10-tick steps from 200.
        // The runner's own calls, from its steps: a runner whose steps
        // drift from the target table fails the click check.
        let calls = super::super::run::calls_of(&super::super::run::row("p1", false).unwrap());
        let mut run = vec![
            json!({"t": "start"}),
            json!({"t": "sink", "q": 100, "window": SINK}),
            // The runner's own build, expected and held.
            json!({"t": "player", "q": 150, "path": "d/player-bin/t.exe", "files": [{"name": "tapestry.exe", "sha256": "4e"}], "source": "t.exe", "sha256": "4e", "expected": "4e", "declared": null,
                   "runner_sha256": "4e", "commit": "nogit", "modified": 1}),
        ];
        for (k, (what, point)) in calls.iter().enumerate() {
            let (x, y) = point.unwrap_or((100, 50));
            run.push(json!({
                "t": "call", "q": 200 + 10 * i64::try_from(k).unwrap(),
                "tag": 0xABCD_0000 + u32::try_from(k).unwrap(), "what": what, "point": [x, y],
            }));
        }
        run.extend([
            json!({"t": "held", "q": 1700}),
            json!({"t": "shot", "q": 2500, "what": "shot-a.png",
                   "process": {"pid": 8, "created": 80}, "client": 8, "size": [1920, 1080],
                   "requested": 2400, "frame": {"previous": 29, "present": 30, "acquired": 2410, "presented": 2420},
                   "volumes": arm.clone()}),
            json!({"t": "released", "q": 3010, "report": 3000}),
            json!({"t": "shot", "q": 3500, "what": "shot-b.png",
                   "process": {"pid": 8, "created": 80}, "client": 8, "size": [1920, 1080],
                   "requested": 3400, "frame": {"previous": 79, "present": 80, "acquired": 3410, "presented": 3420},
                   "volumes": arm.clone()}),
            json!({"t": "stop", "q": 5000, "what": "end"}),
            json!({"t": "quiet", "q": 5600, "line": hello(true, "released", "-")}),
            json!({"t": "close", "q": 5800, "window": SINK}),
            json!({"t": "end", "exit": 0, "errors": []}),
        ]);
        let clicks = [
            0xABCD_0001_u32,
            0xABCD_0003,
            0xABCD_0009,
            0xABCD_000B,
            0xABCD_000D,
        ];
        let mut recorder = vec![json!({"t": "ready", "q": 50})];
        for (k, tag) in clicks.iter().enumerate() {
            let q = 205 + 10 * i64::try_from(k).unwrap();
            recorder.extend([mouse(q, 1, *tag), mouse(q + 1, 2, *tag)]);
        }
        recorder.extend([
            panel_pad(1500, PANEL3, 2, true),
            panel_pad(3000, 0, 2, true),
            json!({"t": "end", "q": 6000}),
        ]);
        recorder.sort_by_key(q);
        let after = DEVICE_TOML
            .replace("system_volume = 1.0", "system_volume = 0.25")
            .replace("key_volume = 1.0", "key_volume = 0.5")
            .replace("background_volume = 1.0", "background_volume = 0.75");
        let files = Files {
            run: Stream::of(run),
            link: n1.link,
            recorder: Stream::of(recorder),
        };
        let x = PanelFiles {
            mode: Stream::of(vec![
                json!({"t": "ans", "q": 1, "cmd": "hello", "line": hello(true, "none", "-")}),
            ]),
            device_before: Ok(DEVICE_TOML.as_bytes().to_vec()),
            device_after: Ok(after.into_bytes()),
            player_before: Ok(b"[skins]\n".to_vec()),
            player_after: Ok(b"[skins]\n".to_vec()),
            digests: Vec::new(),
        };
        (files, x)
    }

    /// The reference player's process and window in a reference run.
    pub const REF_PROCESS: (u32, u64) = (30, 300);
    pub const REF_WINDOW: u64 = 0x6060;

    /// A good reference run of `row` (`p1-ref`, `p1-ref-75`): the sink's
    /// focus click, the attachment, the row's calls from its steps, the
    /// recorder's clicks, the two captures 50 ms apart after the last
    /// click's button-up, and a link that grants no lease, its evidence
    /// `hello` quiet with reason `none`.
    pub fn ref_good(row: &str) -> Files {
        let calls = super::super::run::calls_of(&super::super::run::row(row, false).unwrap());
        let (pid, created) = REF_PROCESS;
        let process = json!({"pid": pid, "created": created});
        let client = json!([8, 31, 1920, 1080]);
        let mut run = vec![
            json!({"t": "start"}),
            json!({"t": "sink", "q": 100, "window": SINK}),
        ];
        let mut recorder = vec![json!({"t": "ready", "q": 50})];
        for (k, (what, point)) in calls.iter().enumerate() {
            let (x, y) = point.unwrap_or((100, 50));
            let q = 200 + 10 * i64::try_from(k).unwrap();
            let tag = 0xABCD_0000 + u32::try_from(k).unwrap();
            run.push(json!({"t": "call", "q": q, "tag": tag, "what": what, "point": [x, y]}));
            if *what == "click" {
                recorder.extend([mouse(q + 5, 1, tag), mouse(q + 6, 2, tag)]);
            }
            if k == 1 {
                run.push(json!({"t": "attach", "q": 215, "process": process, "window": REF_WINDOW, "client": client}));
            }
        }
        // 50 ms at the link's frequency.
        let gap = FREQ / 20;
        run.extend([
            json!({"t": "capture", "q": 2500 + gap, "what": "shot-a.png", "process": process,
            "window": REF_WINDOW, "client": client, "grabs": [
                {"file": "shot-a.png", "before": 2400, "after": 2410},
                {"file": "shot-a-2.png", "before": 2410 + gap, "after": 2420 + gap},
            ]}),
            json!({"t": "stop", "q": 5000 + gap, "what": "end"}),
            json!({"t": "quiet", "q": 5600 + gap, "line": hello(true, "none", "-")}),
            json!({"t": "close", "q": 5800 + gap, "window": SINK}),
            json!({"t": "end", "exit": 0, "errors": []}),
        ]);
        recorder.push(json!({"t": "end", "q": 6000 + gap}));
        let mut link = vec![
            json!({"t": "open", "q": 10, "freq": FREQ}),
            json!({"t": "cmd", "q": 20, "line": "hello"}),
            json!({"t": "ans", "q": 30, "sent": 20, "cmd": "hello", "line": hello(true, "none", "-")}),
        ];
        for k in 0..4 {
            let (sent, recv) = (40 + k * 100, 60 + k * 100);
            link.push(json!({"t": "ping", "n": k, "sent": sent, "device": device(50 + k * 100), "recv": recv}));
        }
        link.extend([
            json!({"t": "fence", "q": 5100 + gap}),
            json!({"t": "cmd", "q": 5110 + gap, "line": "release"}),
            json!({"t": "ans", "q": 5120 + gap, "sent": 5110 + gap, "cmd": "release", "line": "ok"}),
            json!({"t": "cmd", "q": 5500 + gap, "line": "hello"}),
            json!({"t": "ans", "q": 5510 + gap, "sent": 5500 + gap, "cmd": "hello", "line": hello(true, "none", "-")}),
        ]);
        Files {
            run: Stream::of(run),
            link: Stream::of(link),
            recorder: Stream::of(recorder),
        }
    }

    fn ref_outcome(f: &Files, row: &str, check: &str) -> (Outcome, String) {
        let j = reference(Some("0123456789ab"), f, row)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap_or_else(|| panic!("no {check} for {row}"));
        (j.outcome, j.detail)
    }

    #[test]
    fn a_good_reference_run_passes_every_check() {
        for row in ["p1-ref", "p1-ref-75"] {
            let all = reference(Some("0123456789ab"), &ref_good(row), row);
            assert_eq!(all.len(), 8);
            for j in &all {
                assert_eq!(j.outcome, Outcome::Pass, "{row}: {j:?}");
            }
        }
        // Each row is held to its own targets: `p1-ref-75`'s calls are not
        // `p1-ref`'s.
        assert_eq!(
            ref_outcome(&ref_good("p1-ref-75"), "p1-ref", "p1.clicks").0,
            Outcome::Fail
        );
    }

    /// The device holds nothing in a reference run: a pad report with a
    /// button down, a key edge, or a hold-bearing command on the link
    /// fails `ref.device-idle`; a neutral pad report does not.
    #[test]
    fn ref_device_idle_fails_on_a_press() {
        let mut f = ref_good("p1-ref");
        edit(&mut f.recorder, |r| r.insert(1, panel_pad(300, 0, 2, true)));
        assert_eq!(
            ref_outcome(&f, "p1-ref", "ref.device-idle").0,
            Outcome::Pass
        );
        let mut f = ref_good("p1-ref");
        edit(&mut f.recorder, |r| {
            r.insert(1, panel_pad(300, PANEL3, 2, true))
        });
        let (o, why) = ref_outcome(&f, "p1-ref", "ref.device-idle");
        assert_eq!(o, Outcome::Fail, "{why}");
        assert!(why.contains("gave input"), "{why}");
        let mut f = ref_good("p1-ref");
        edit(&mut f.recorder, |r| r.insert(1, key(300, true, true)));
        assert_eq!(
            ref_outcome(&f, "p1-ref", "ref.device-idle").0,
            Outcome::Fail
        );
        // Another device's key is not the test device's.
        let mut f = ref_good("p1-ref");
        edit(&mut f.recorder, |r| r.insert(1, key(300, true, false)));
        assert_eq!(
            ref_outcome(&f, "p1-ref", "ref.device-idle").0,
            Outcome::Pass
        );
        let mut f = ref_good("p1-ref");
        let lease = format!("lease {TOKEN} 250 {}", deadline(1000));
        edit(&mut f.link, |l| {
            l.push(json!({"t": "cmd", "q": 1010, "line": lease}))
        });
        let (o, why) = ref_outcome(&f, "p1-ref", "ref.device-idle");
        assert_eq!(o, Outcome::Fail, "{why}");
        assert!(why.contains("hold-bearing"), "{why}");
    }

    /// The captures follow the last click's button-up, 50 ms apart, of the
    /// attached window: a capture begun before the button-up, a second
    /// capture too soon, one of another process, or a missing button-up
    /// each fails or holds `ref.capture-bound`.
    #[test]
    fn ref_capture_bound_needs_the_last_click_up() {
        fn grab(r: &mut [Value], k: usize, field: &str, v: i64) {
            let at = r.iter().position(|v| v["t"] == "capture").unwrap();
            r[at]["grabs"][k][field] = json!(v);
        }
        let ok = |f: &Files| ref_outcome(f, "p1-ref", "ref.capture-bound");
        assert_eq!(ok(&ref_good("p1-ref")).0, Outcome::Pass);
        let last_up = {
            let f = ref_good("p1-ref");
            let r = f.recorder.records.unwrap();
            r.iter()
                .filter(|v| v["t"] == "mouse" && v["buttons"] == 2)
                .map(q)
                .max()
                .unwrap()
        };
        // Begun at the button-up itself: not after it.
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| grab(r, 0, "before", last_up));
        let (o, why) = ok(&f);
        assert_eq!(o, Outcome::Fail, "{why}");
        assert!(
            why.contains("not after the last click's button-up"),
            "{why}"
        );
        // The second capture 1 tick short of 50 ms after the first.
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| grab(r, 1, "before", 2410 + FREQ / 20 - 1));
        let (o, why) = ok(&f);
        assert_eq!(o, Outcome::Fail, "{why}");
        assert!(why.contains("not at least"), "{why}");
        // Another process than the one attached.
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| {
            let at = r.iter().position(|v| v["t"] == "capture").unwrap();
            r[at]["process"]["created"] = json!(301);
        });
        assert!(ok(&f).1.contains("process"), "{:?}", ok(&f));
        // The last click's button-up not recorded: not judged.
        let mut f = ref_good("p1-ref");
        edit(&mut f.recorder, |r| {
            let at = r
                .iter()
                .rposition(|v| v["t"] == "mouse" && v["buttons"] == 2)
                .unwrap();
            r.remove(at);
        });
        assert_eq!(ok(&f).0, Outcome::Incomplete);
    }

    /// A run without a lease takes any reason in its evidence `hello`, and
    /// still needs it quiet, of its token, read after the fence and before
    /// the close; a leasing row still needs `released`.
    #[test]
    fn quiet_before_close_without_a_lease() {
        let f = ref_good("p1-ref");
        let (run, link) = (f.run.records.unwrap(), f.link.records.unwrap());
        assert_eq!(quiet_before_close(&run, &link, false), Ok(()));
        assert!(quiet_before_close(&run, &link, true).is_err());
        for reason in ["closed", "expired"] {
            let mut r = run.clone();
            let at = r.iter().position(|v| v["t"] == "quiet").unwrap();
            r[at]["line"] = json!(hello(true, reason, "-"));
            let mut l = link.clone();
            let at = l.iter().rposition(|v| v["t"] == "ans").unwrap();
            l[at]["line"] = json!(hello(true, reason, "-"));
            assert_eq!(quiet_before_close(&r, &l, false), Ok(()), "{reason}");
        }
        // Not quiet.
        let mut r = run.clone();
        let mut l = link.clone();
        let at = r.iter().position(|v| v["t"] == "quiet").unwrap();
        r[at]["line"] = json!(hello(false, "none", "-"));
        let at2 = l.iter().rposition(|v| v["t"] == "ans").unwrap();
        l[at2]["line"] = json!(hello(false, "none", "-"));
        assert!(quiet_before_close(&r, &l, false).is_err());
        // The close before the evidence.
        let mut r = run;
        let close = r.iter().position(|v| v["t"] == "close").unwrap();
        let c = r.remove(close);
        let stop = r.iter().position(|v| v["t"] == "stop").unwrap();
        r.insert(stop, c);
        assert!(quiet_before_close(&r, &link, false).is_err());
        assert_eq!(
            ref_outcome(&ref_good("p1-ref"), "p1-ref", "ref.quiet-before-close").0,
            Outcome::Pass
        );
    }

    /// The attachment is one record of a 1920×1080 client area, between
    /// the sink's focus click and the next call.
    #[test]
    fn ref_attached_is_one_record_in_its_place() {
        let at = |f: &Files| ref_outcome(f, "p1-ref", "ref.attached").0;
        assert_eq!(at(&ref_good("p1-ref")), Outcome::Pass);
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| {
            let a = r.iter().position(|v| v["t"] == "attach").unwrap();
            r[a]["client"] = json!([8, 31, 1280, 720]);
        });
        assert_eq!(at(&f), Outcome::Fail);
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| {
            let a = r.iter().position(|v| v["t"] == "attach").unwrap();
            let v = r.remove(a);
            r.insert(2, v);
        });
        assert_eq!(at(&f), Outcome::Fail);
        let mut f = ref_good("p1-ref");
        edit(&mut f.run, |r| {
            let a = r.iter().position(|v| v["t"] == "attach").unwrap();
            let v = r[a].clone();
            r.insert(a, v);
        });
        assert_eq!(at(&f), Outcome::Fail);
    }

    fn panel_outcome(f: &Files, x: &PanelFiles, row: &str, check: &str) -> (Outcome, String) {
        let j = panel(Some("0123456789ab"), f, x, row)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap_or_else(|| panic!("no {check} for {row}"));
        (j.outcome, j.detail)
    }

    #[test]
    fn a_good_panel_run_passes_every_check() {
        let (f, x) = panel_good();
        let all = panel(Some("0123456789ab"), &f, &x, "p1");
        assert_eq!(all.len(), 14);
        for j in &all {
            assert_eq!(j.outcome, Outcome::Pass, "{j:?}");
        }
    }

    /// The volumes as P4's and P5's players leave them: all three at 1.0.
    fn kept() -> PanelFiles {
        let (_, x) = panel_good();
        kept_of(x)
    }

    /// `x` with the file after the run as the file before it.
    fn kept_of(mut x: PanelFiles) -> PanelFiles {
        x.device_after = x.device_before.clone();
        x
    }

    /// A P5 run: P1's records with the declared mutated player, the
    /// volumes kept.
    fn p5_good() -> (Files, PanelFiles) {
        let (mut f, _) = panel_good();
        edit(&mut f.run, |r| {
            r[2] = json!({"t": "player", "q": 150, "path": "d/player-bin/t.exe", "files": [{"name": "tapestry.exe", "sha256": "5a"}], "source": "p5/t.exe", "sha256": "5a", "expected": "5a", "declared": "5a",
                          "runner_sha256": "4e", "commit": "nogit", "modified": 1});
        });
        (f, kept())
    }

    /// P4 and P5 fail `p1.volumes` for their declared reason only: each
    /// row's own check passes only when `device.toml` reads through the
    /// player's refusals and types with all three volumes exactly 1.0. One
    /// volume moved, a malformed or refused file, or a volume line gone
    /// fails it; an unreadable copy is incomplete. P1's checks have none.
    #[test]
    fn p4_and_p5_keep_every_volume_at_1() {
        let (f, _) = panel_good();
        for (row, id) in [("p4", "p4.volumes-kept"), ("p5", "p5.volumes-kept")] {
            let x = kept();
            assert_eq!(panel_outcome(&f, &x, row, "p1.volumes").0, Outcome::Fail);
            assert_eq!(panel_outcome(&f, &x, row, id).0, Outcome::Pass, "{row}");
            let before = String::from_utf8(x.device_before.clone().unwrap()).unwrap();
            for (why, after) in [
                (
                    "one moved",
                    before.replace("system_volume = 1.0", "system_volume = 0.25"),
                ),
                (
                    "another moved",
                    before.replace("background_volume = 1.0", "background_volume = 0.75"),
                ),
                (
                    "malformed",
                    before.replace("background_volume = 1.0", "background_volume = \"x\""),
                ),
                (
                    "an integer",
                    before.replace("key_volume = 1.0", "key_volume = 1"),
                ),
                ("gone", before.replace("key_volume = 1.0\n", "")),
                ("refused", before.replace("version = 3", "version = 2")),
                ("not toml", "system_volume = = 1.0\n".to_owned()),
            ] {
                let mut x = kept();
                x.device_after = Ok(after.into_bytes());
                let (o, d) = panel_outcome(&f, &x, row, id);
                assert_eq!(o, Outcome::Fail, "{row} {why}: {d}");
            }
            let mut x = kept();
            x.device_after = Err("missing".into());
            assert_eq!(panel_outcome(&f, &x, row, id).0, Outcome::Incomplete);
        }
        let (f, x) = panel_good();
        let ids: Vec<&str> = panel(Some("0123456789ab"), &f, &x, "p1")
            .iter()
            .map(|j| j.check)
            .collect();
        assert!(!ids.iter().any(|c| c.ends_with("-kept")), "{ids:?}");
    }

    /// P5's player must be the mutated build it declared: the `player`
    /// record's digest equal to the declared one and not the runner's own;
    /// the verdict names the digests. A stale player (another digest), the
    /// build under test itself, or no declaration fails; no record is
    /// incomplete.
    #[test]
    fn p5_runs_the_declared_mutation_build() {
        let (f, x) = p5_good();
        let (o, d) = panel_outcome(&f, &x, "p5", "p5.mutation-build");
        assert_eq!(o, Outcome::Pass, "{d}");
        assert!(d.contains("sha256 5a") && d.contains("own 4e"), "{d}");
        for (field, value) in [
            ("sha256", json!("3c")),
            ("sha256", json!("4e")),
            ("declared", Value::Null),
        ] {
            let (mut f, x) = p5_good();
            edit(&mut f.run, |r| {
                r[2][field] = value.clone();
                if field == "sha256" && value == "4e" {
                    r[2]["declared"] = json!("4e");
                }
            });
            assert_eq!(
                panel_outcome(&f, &x, "p5", "p5.mutation-build").0,
                Outcome::Fail,
                "{field} {value}"
            );
        }
        let (mut f, x) = p5_good();
        edit(&mut f.run, |r| r.retain(|v| v["t"] != "player"));
        assert_eq!(
            panel_outcome(&f, &x, "p5", "p5.mutation-build").0,
            Outcome::Incomplete
        );
    }

    /// The click check takes the run's calls exactly as the row makes them:
    /// a target's click left out (its move, its call and its press and
    /// release with it), a click at another point, an extra call, or two
    /// calls with one tag fail. The control: the good run.
    #[test]
    fn the_clicks_are_exactly_the_row_s() {
        let (f, x) = panel_good();
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Pass);
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| {
            r.retain(|v| !(v["t"] == "call" && v["tag"].as_u64().unwrap() >= 0xABCD_000C));
        });
        edit(&mut f.recorder, |r| {
            r.retain(|v| v["extra"] != 0xABCD_000D_u32);
        });
        let (o, d) = panel_outcome(&f, &x, "p1", "p1.clicks");
        assert_eq!(o, Outcome::Fail, "{d}");
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| {
            let at = r.iter().position(|v| v["tag"] == 0xABCD_000B_u32).unwrap();
            r[at]["point"] = json!([301, 657]);
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| {
            let at = r.iter().position(|v| v["tag"] == 0xABCD_000D_u32).unwrap();
            let mut extra = r[at].clone();
            extra["tag"] = json!(0xABCD_000E_u32);
            r.insert(at + 1, extra);
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| {
            let at = r.iter().position(|v| v["tag"] == 0xABCD_000C_u32).unwrap();
            r[at]["tag"] = json!(0xABCD_000A_u32);
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
    }

    /// The expected calls come from the target table, independently of the
    /// runner's steps: they are what P1's and P4's steps make (every target
    /// clicked once, in order, after the calibration of each point), and a
    /// runner whose steps leave out a target makes the click check fail.
    #[test]
    fn the_expected_calls_are_the_target_table_s() {
        let want = panel_calls(&test_device::panel::TARGETS);
        assert_eq!(want.len(), 4 + 4 + 2 * test_device::panel::TARGETS.len());
        for row in ["p1", "p4", "p5"] {
            let steps = super::super::run::row(row, false).unwrap();
            assert_eq!(super::super::run::calls_of(&steps), want, "{row}");
        }
        // The reference rows make their own table's calls: `p1-ref` P1's,
        // `p1-ref-75` the control's, which differ at slider 17 alone.
        let steps = super::super::run::row("p1-ref", false).unwrap();
        assert_eq!(super::super::run::calls_of(&steps), want);
        let control = panel_calls(&test_device::panel::REF_75);
        let steps = super::super::run::row("p1-ref-75", false).unwrap();
        assert_eq!(super::super::run::calls_of(&steps), control);
        let differ: Vec<_> = want
            .iter()
            .zip(&control)
            .filter(|(a, b)| a != b)
            .map(|(a, _)| a.1)
            .collect();
        assert!(
            !differ.is_empty() && differ.iter().all(|p| *p == Some((256, 606))),
            "{differ:?}"
        );
        let steps: Vec<_> = super::super::run::row("p1", false)
            .unwrap()
            .into_iter()
            .filter(|s| !matches!(s, super::super::run::Step::Click { x: 344, .. }))
            .collect();
        assert_ne!(super::super::run::calls_of(&steps), want);
    }

    /// A panel run's driving connection must report in the panel mode: a
    /// `hello` of another mode on it fails provenance.
    #[test]
    fn a_panel_run_in_another_mode_fails_provenance() {
        let (f, x) = panel_good();
        assert_eq!(panel_outcome(&f, &x, "p1", "provenance").0, Outcome::Pass);
        let (mut f, x) = panel_good();
        edit(&mut f.link, |l| {
            let line = l[2]["line"]
                .as_str()
                .unwrap()
                .replace(" mode 8 ", " mode 1 ");
            l[2]["line"] = json!(line);
        });
        let (o, d) = panel_outcome(&f, &x, "p1", "provenance");
        assert_eq!(o, Outcome::Fail, "{d}");
        assert!(d.contains("driving connection"), "{d}");
    }

    /// Each verdict carries the digest of the bytes it judged: a settings
    /// copy replaced on disk after the judge read it leaves the verdicts'
    /// digests those of the copy read.
    #[test]
    fn a_verdict_s_digest_is_of_the_bytes_it_judged() {
        use sha2::Digest as _;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let start = json!({
            "t": "start", "run": "0123456789abcdef0123456789abcdef", "row": "p1", "role": SINGLE,
            "build": "0123456789ab", "process": {"pid": 1, "created": 2}, "commit": super::super::COMMIT,
            "tag": 0xABCD, "cut": 0x4000_0123, "args": [], "set": null, "inner": null,
        });
        let end = json!({"t": "end", "exit": 0, "errors": []});
        std::fs::write(d.join(RUN), format!("{start}\n{end}\n")).unwrap();
        let (_, x) = panel_good();
        let after = x.device_after.unwrap();
        for (name, bytes) in [
            (DEVICE_BEFORE, x.device_before.unwrap()),
            (DEVICE_AFTER, after.clone()),
            (PLAYER_BEFORE, x.player_before.unwrap()),
            (PLAYER_AFTER, x.player_after.unwrap()),
        ] {
            std::fs::write(d.join(name), bytes).unwrap();
        }
        let r = read("p1", d).unwrap();
        std::fs::write(d.join(DEVICE_AFTER), b"system_volume = 0.0\n").unwrap();
        let lines = lines("p1", &r);
        let v = lines.iter().find(|v| v["check"] == "p1.volumes").unwrap();
        assert_eq!(v["outcome"], "pass", "{v}");
        let read_sha = contract::hex(&sha2::Sha256::digest(&after));
        assert_eq!(v["inputs"][1]["path"], DEVICE_AFTER, "{v}");
        assert_eq!(v["inputs"][1]["sha256"], read_sha.as_str(), "{v}");
    }

    /// The volumes are read from the copied `device.toml` after the run:
    /// left at 1.0, or one off by a step, fails; an unreadable copy is
    /// incomplete.
    #[test]
    fn p_volumes_are_read_from_the_copied_settings() {
        let (f, mut x) = panel_good();
        x.device_after = Ok(DEVICE_TOML.as_bytes().to_vec());
        let (o, d) = panel_outcome(&f, &x, "p1", "p1.volumes");
        assert_eq!(o, Outcome::Fail);
        assert!(
            d.contains("system_volume = Some(1.0) after the run, not 0.25"),
            "{d}"
        );
        let (f, mut x) = panel_good();
        let a = String::from_utf8(x.device_after.unwrap()).unwrap();
        x.device_after = Ok(a
            .replace("key_volume = 0.5", "key_volume = 0.51")
            .into_bytes());
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.volumes").0, Outcome::Fail);
        let (f, mut x) = panel_good();
        x.device_after = Err("missing".into());
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.volumes").0,
            Outcome::Incomplete
        );
    }

    /// The clicks must move each volume from the 1.0 the runner wrote: a
    /// `device.toml` already holding the targets before the run (with
    /// clicks that set nothing, the file after it the same) fails, and so
    /// does any one volume off 1.0 before it; an unreadable copy from before
    /// is incomplete.
    #[test]
    fn prefilled_volumes_before_the_run_fail() {
        let (f, mut x) = panel_good();
        x.device_before.clone_from(&x.device_after);
        let (o, d) = panel_outcome(&f, &x, "p1", "p1.volumes");
        assert_eq!(o, Outcome::Fail);
        for want in [
            "system_volume = Some(0.25) before the run, not 1",
            "key_volume = Some(0.5) before the run, not 1",
            "background_volume = Some(0.75) before the run, not 1",
        ] {
            assert!(d.contains(want), "{d}");
        }
        let (f, mut x) = panel_good();
        let b = String::from_utf8(x.device_before.unwrap()).unwrap();
        x.device_before = Ok(b
            .replace("key_volume = 1.0", "key_volume = 0.99")
            .into_bytes());
        let (o, d) = panel_outcome(&f, &x, "p1", "p1.volumes");
        assert_eq!(o, Outcome::Fail);
        assert!(d.contains("key_volume = Some(0.99) before the run"), "{d}");
        let (f, mut x) = panel_good();
        x.device_before = Err("missing".into());
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.volumes").0,
            Outcome::Incomplete
        );
    }

    /// Shot A's witness of the player's volume arm (`p1.volume-arm`): the
    /// press entries, one per target in the target table's order, each the
    /// target's slider and percent, after its click call (290, 310, 330)
    /// and before shot A's frame (2410). A drag entry besides them is not
    /// counted. A press entry before its call (stale), two presses swapped,
    /// a drag in place of a press (drag only), a target's press missing, a
    /// percent no target sets, or a press reached only after the frame
    /// fails; P5's player, whose arm drops the values it reached, passes;
    /// a witness that does not read, a run with no shot A record, or click
    /// calls at the targets out of order is incomplete.
    #[test]
    fn each_target_s_press_must_reach_the_volume_arm_in_order() {
        let arm = |vols: Value| {
            let (mut f, x) = panel_good();
            edit(&mut f.run, |r| {
                for v in r.iter_mut().filter(|v| v["t"] == "shot") {
                    v["volumes"] = vols.clone();
                }
            });
            panel_outcome(&f, &x, "p1", "p1.volume-arm")
        };
        let good = json!([
            [17, 25, 295, "press"],
            [18, 50, 315, "press"],
            [19, 75, 335, "press"]
        ]);
        assert_eq!(arm(good).0, Outcome::Pass);
        // Dispatched after all three calls: each still after its own.
        assert_eq!(
            arm(json!([
                [17, 25, 340, "press"],
                [18, 50, 341, "press"],
                [19, 75, 342, "press"]
            ]))
            .0,
            Outcome::Pass
        );
        // A drag besides the presses is not counted.
        assert_eq!(
            arm(json!([
                [17, 25, 295, "press"],
                [17, 30, 296, "drag"],
                [18, 50, 315, "press"],
                [19, 75, 335, "press"]
            ]))
            .0,
            Outcome::Pass
        );
        let fails = |vols: Value, want: &str| {
            let (o, d) = arm(vols);
            assert_eq!(o, Outcome::Fail, "{d}");
            assert!(d.contains(want), "{want}: {d}");
        };
        // Stale: slider 17's entry before its call at 290.
        fails(
            json!([
                [17, 25, 230, "press"],
                [18, 50, 315, "press"],
                [19, 75, 335, "press"]
            ]),
            "slider 17's press reached the arm at 230, not after its click call at 290",
        );
        // Swapped: the first two presses' results in each other's place.
        fails(
            json!([
                [18, 50, 295, "press"],
                [17, 25, 315, "press"],
                [19, 75, 335, "press"]
            ]),
            "the press for slider 17 at 25 % reached the arm as slider 18 at 50 %",
        );
        // Drag only: the values arrive, none by a press.
        fails(
            json!([
                [17, 25, 295, "drag"],
                [18, 50, 315, "drag"],
                [19, 75, 335, "drag"]
            ]),
            "0 presses reached the volume arm, not one per target (3)",
        );
        fails(
            json!([[17, 25, 295, "press"], [19, 75, 335, "press"]]),
            "2 presses reached the volume arm",
        );
        fails(
            json!([
                [17, 25, 295, "press"],
                [18, 51, 315, "press"],
                [19, 75, 335, "press"]
            ]),
            "the press for slider 18 at 50 % reached the arm as slider 18 at 51 %",
        );
        fails(
            json!([
                [17, 25, 295, "press"],
                [18, 50, 315, "press"],
                [19, 75, 2410, "press"]
            ]),
            "not before shot A's frame at 2410",
        );
        assert_eq!(arm(json!([[17, 25, 295]])).0, Outcome::Incomplete);
        assert_eq!(arm(Value::Null).0, Outcome::Incomplete);
        let (f, x) = p5_good();
        assert_eq!(
            panel_outcome(&f, &x, "p5", "p1.volume-arm").0,
            Outcome::Pass
        );
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| r.retain(|v| v["what"] != "shot-a.png"));
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.volume-arm").0,
            Outcome::Incomplete
        );
        // The second target's click call moved last: out of order.
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| {
            let at = r
                .iter()
                .position(|v| v["what"] == "click" && v["point"] == json!([300, 657]))
                .unwrap();
            let call = r.remove(at);
            let last = r.iter().rposition(|v| v["t"] == "call").unwrap();
            r.insert(last + 1, call);
        });
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.volume-arm").0,
            Outcome::Incomplete
        );
    }

    /// Every panel row's player is the one it expected (`p1.player-bound`):
    /// the runner's own build in P1 and P4, P5's declared mutated build in
    /// P5. A P4 run whose held copy is P5's mutated build, though the record
    /// expected the runner's (an otherwise valid run on the wrong player),
    /// fails; so do a P1 run whose expected digest is not the runner's, a
    /// record with no expected digest, a P5 run whose copy is not the one it
    /// declared, and a record whose digest is not the launched file's; a
    /// run with no `player` record is incomplete.
    #[test]
    fn every_panel_row_runs_the_player_it_expected() {
        let (f, x) = panel_good();
        for row in ["p1", "p4"] {
            assert_eq!(
                panel_outcome(&f, &x, row, "p1.player-bound").0,
                Outcome::Pass,
                "{row}"
            );
        }
        let (f5, x5) = p5_good();
        assert_eq!(
            panel_outcome(&f5, &x5, "p5", "p1.player-bound").0,
            Outcome::Pass
        );
        let bound = |row: &str, set: &[(&str, Value)]| {
            let (mut f, x) = if row == "p5" { p5_good() } else { panel_good() };
            edit(&mut f.run, |r| {
                for (k, v) in set {
                    r[2][*k] = v.clone();
                }
            });
            panel_outcome(&f, &x, row, "p1.player-bound")
        };
        // Each case a consistent copy (its digest the launched file's), so
        // it fails on the binding alone.
        let copy = |sha: &str| {
            vec![
                ("sha256", json!(sha)),
                ("files", json!([{"name": "tapestry.exe", "sha256": sha}])),
            ]
        };
        let (o, d) = bound("p4", &copy("5a"));
        assert_eq!(o, Outcome::Fail);
        assert!(d.contains("sha256 5a; expected \"4e\""), "{d}");
        let mut own = copy("5a");
        own.push(("expected", json!("5a")));
        assert_eq!(
            bound("p1", &own).0,
            Outcome::Fail,
            "a normal row expects the runner's own build"
        );
        assert_eq!(bound("p1", &[("expected", Value::Null)]).0, Outcome::Fail);
        assert_eq!(bound("p5", &copy("3c")).0, Outcome::Fail);
        // The record's digest not the launched file's.
        assert_eq!(
            bound(
                "p1",
                &[("files", json!([{"name": "tapestry.exe", "sha256": "3c"}]))]
            )
            .0,
            Outcome::Fail
        );
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| r.retain(|v| v["t"] != "player"));
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.player-bound").0,
            Outcome::Incomplete
        );
    }

    /// Every answer's witness counts, not shot A's alone: an entry the arm
    /// took after shot A, seen only in shot B's witness (a target's press
    /// dispatched late, both frames and settings as they were), fails
    /// P4's `p4.no-volume-arm` and P1's `p1.volume-arm`; shot B's witness
    /// equal to shot A's passes, and one that does not read is incomplete.
    #[test]
    fn a_later_answer_s_witness_counts() {
        let only_b = |row: &str, check: &str, vols: Value| {
            let (mut f, x) = panel_good();
            edit(&mut f.run, |r| {
                for v in r.iter_mut().filter(|v| v["what"] == "shot-a.png") {
                    if row == "p4" {
                        v["volumes"] = json!([]);
                    }
                }
                for v in r.iter_mut().filter(|v| v["what"] == "shot-b.png") {
                    v["volumes"] = vols.clone();
                }
            });
            let x = if row == "p4" { kept_of(x) } else { x };
            panel_outcome(&f, &x, row, check)
        };
        assert_eq!(only_b("p4", "p4.no-volume-arm", json!([])).0, Outcome::Pass);
        let (o, d) = only_b("p4", "p4.no-volume-arm", json!([[17, 100, 2600, "press"]]));
        assert_eq!(o, Outcome::Fail);
        assert!(d.contains("shot-b.png: [(17, 100, 2600"), "{d}");
        let a = json!([
            [17, 25, 295, "press"],
            [18, 50, 315, "press"],
            [19, 75, 335, "press"]
        ]);
        assert_eq!(only_b("p1", "p1.volume-arm", a.clone()).0, Outcome::Pass);
        let mut more = a.as_array().unwrap().clone();
        more.push(json!([18, 100, 2600, "press"]));
        let (o, d) = only_b("p1", "p1.volume-arm", Value::Array(more));
        assert_eq!(o, Outcome::Fail);
        assert!(
            d.contains("shot-b.png's witness holds entries beyond shot A's: [(18, 100, 2600"),
            "{d}"
        );
        assert_eq!(
            only_b("p4", "p4.no-volume-arm", Value::Null).0,
            Outcome::Incomplete
        );
    }

    /// P4's clicks bring nothing to the volume arm (`p4.no-volume-arm`): an
    /// empty witness passes; one press, even to a slider at 100 % that
    /// leaves the settings and the frame as P4's, fails; so does a drag; a
    /// witness that does not read is incomplete.
    #[test]
    fn p4_s_clicks_reach_no_volume_arm() {
        let p4 = |vols: Value| {
            let (mut f, x) = panel_good();
            edit(&mut f.run, |r| {
                for v in r.iter_mut().filter(|v| v["t"] == "shot") {
                    v["volumes"] = vols.clone();
                }
            });
            panel_outcome(&f, &kept_of(x), "p4", "p4.no-volume-arm")
        };
        assert_eq!(p4(json!([])).0, Outcome::Pass);
        let (o, d) = p4(json!([[17, 100, 295, "press"]]));
        assert_eq!(o, Outcome::Fail);
        assert!(d.contains("the volume arm was reached"), "{d}");
        assert_eq!(p4(json!([[17, 100, 295, "drag"]])).0, Outcome::Fail);
        assert_eq!(p4(Value::Null).0, Outcome::Incomplete);
        let (f, x) = panel_good();
        assert!(
            panel(Some("0123456789ab"), &f, &x, "p1")
                .iter()
                .all(|j| j.check != "p4.no-volume-arm"),
            "P4's alone"
        );
    }

    /// Every panel row's starting volumes (`p1.volumes-start`): 1.0 each in
    /// `device.toml` before the run. In P1, P4 and P5 alike, one volume off
    /// 1.0 before the run fails it, though the file after the run (all
    /// 1.0, as a negative leaves it) passes the row's kept-volumes check;
    /// an unreadable file is incomplete.
    #[test]
    fn every_panel_row_starts_at_full_volume() {
        for row in ["p1", "p4", "p5"] {
            let (f, x) = panel_good();
            assert_eq!(
                panel_outcome(&f, &x, row, "p1.volumes-start").0,
                Outcome::Pass,
                "{row}"
            );
            let mut x = kept_of(x);
            let b = String::from_utf8(x.device_before.clone().unwrap()).unwrap();
            x.device_before = Ok(b
                .replace("system_volume = 1.0", "system_volume = 0.25")
                .into_bytes());
            let (o, d) = panel_outcome(&f, &x, row, "p1.volumes-start");
            assert_eq!(o, Outcome::Fail, "{row}");
            assert!(
                d.contains("system_volume = Some(0.25) before the run, not 1"),
                "{d}"
            );
            if row != "p1" {
                assert_eq!(
                    panel_outcome(&f, &x, row, &format!("{row}.volumes-kept")).0,
                    Outcome::Pass,
                    "{row}: the after file alone keeps 1.0"
                );
            }
            x.device_before = Err("missing".into());
            assert_eq!(
                panel_outcome(&f, &x, row, "p1.volumes-start").0,
                Outcome::Incomplete
            );
        }
    }

    #[test]
    fn a_changed_byte_elsewhere_in_device_toml_fails() {
        let (f, mut x) = panel_good();
        let a = String::from_utf8(x.device_after.unwrap()).unwrap();
        x.device_after = Ok(a.replace("C:/lib", "C:/lic").into_bytes());
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.device-toml").0,
            Outcome::Fail
        );
        // A line added.
        let (f, mut x) = panel_good();
        let mut a = x.device_after.unwrap();
        a.extend_from_slice(b"x = 1\n");
        x.device_after = Ok(a);
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.device-toml").0,
            Outcome::Fail
        );
        // A volume's line swapped for another key's.
        let (f, mut x) = panel_good();
        let a = String::from_utf8(x.device_after.unwrap()).unwrap();
        x.device_after = Ok(a
            .replace("key_volume = 0.5", "system_volume = 0.5")
            .into_bytes());
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.device-toml").0,
            Outcome::Fail
        );
    }

    #[test]
    fn a_changed_player_toml_fails() {
        let (f, mut x) = panel_good();
        x.player_after = Ok(b"[skins]\n\n".to_vec());
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.player-toml").0,
            Outcome::Fail
        );
    }

    /// START or SELECT from another device's report fails, as does a
    /// release from another handle than the press's.
    #[test]
    fn a_panel_key_from_another_handle_fails() {
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            r.insert(3, panel_pad(1400, 1 << 9, 9, false));
        });
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.panel-keys").0,
            Outcome::Fail
        );
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 3000).unwrap();
            r[at]["h"] = json!(7);
        });
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.panel-keys").0,
            Outcome::Fail
        );
    }

    /// P4 holds START alone: its run passes with no SELECT edge, and a
    /// SELECT press fails it; P1's expectation of both fails a run without
    /// SELECT.
    #[test]
    fn p4_expects_no_select_edge() {
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 1500).unwrap();
            r[at] = panel_pad(1500, 1 << 8, 2, true);
        });
        assert_eq!(
            panel_outcome(&f, &x, "p4", "p1.panel-keys").0,
            Outcome::Pass
        );
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.panel-keys").0,
            Outcome::Fail
        );
        let (f, x) = panel_good();
        assert_eq!(
            panel_outcome(&f, &x, "p4", "p1.panel-keys").0,
            Outcome::Fail
        );
    }

    #[test]
    fn every_injected_down_has_its_up_by_tag() {
        // A click's release missing.
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 236).unwrap();
            r.remove(at);
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
        // Another edge between a press and its release.
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 236).unwrap();
            r.insert(at, mouse(235, 4, 0xABCD_000B));
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
        // A target clicked before the calibration.
        let (mut f, x) = panel_good();
        edit(&mut f.run, |r| r.retain(|v| v["what"] != "calibrate"));
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
        // A press of the run's tag never released.
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            r.retain(|v| {
                !(v["t"] == "mouse" && v["buttons"] == 2 && v["extra"] == 0xABCD_000D_u32)
            });
        });
        assert_eq!(panel_outcome(&f, &x, "p1", "p1.clicks").0, Outcome::Fail);
    }

    #[test]
    fn p1_buttons_released_after_their_presses() {
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| r.retain(|v| q(v) != 3000));
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.buttons-released").0,
            Outcome::Fail
        );
        // The release from another pad does not count.
        let (mut f, x) = panel_good();
        edit(&mut f.recorder, |r| {
            let at = r.iter().position(|v| q(v) == 3000).unwrap();
            r[at]["h"] = json!(7);
        });
        assert_eq!(
            panel_outcome(&f, &x, "p1", "p1.buttons-released").0,
            Outcome::Fail
        );
    }

    /// N2p's inner's mode link: its `hello` in the panel mode.
    fn inner_mode() -> Stream {
        Stream::of(vec![
            json!({"t": "ans", "q": 1, "cmd": "hello", "line": hello(true, "none", "-")}),
        ])
    }

    /// N2p's inner drives the device as a panel run: a `hello` of another
    /// mode on its driving connection fails provenance, and its mode link
    /// unread makes it incomplete. N2's inner (no player) takes no mode.
    #[test]
    fn n2p_s_inner_must_drive_in_the_panel_mode() {
        let (o, i) = n2p_good();
        assert_eq!(n2p_outcome(&o, &i, "provenance"), Outcome::Pass);
        let (o, mut i) = n2p_good();
        edit(&mut i.link, |l| {
            for v in l.iter_mut().filter(|v| v["t"] == "ans") {
                if let Some(line) = v["line"].as_str().filter(|x| x.starts_with("hello ")) {
                    v["line"] = json!(line.replace(" mode 8 ", " mode 1 "));
                }
            }
        });
        let j = kill(&N2P, Some("0123456789ab"), &o, &i, Some(&inner_mode()));
        let p = j.iter().find(|j| j.check == "provenance").unwrap();
        assert_eq!(p.outcome, Outcome::Fail, "{}", p.detail);
        assert!(p.detail.contains("driving connection"), "{}", p.detail);
        let (o, i) = n2p_good();
        let j = kill(&N2P, Some("0123456789ab"), &o, &i, None);
        let p = j.iter().find(|j| j.check == "provenance").unwrap();
        assert_eq!(p.outcome, Outcome::Incomplete, "{}", p.detail);
        // The same inner link in another mode passes N2, which holds no
        // panel.
        let (o, mut i) = n2_good();
        edit(&mut i.link, |l| {
            for v in l.iter_mut().filter(|v| v["t"] == "ans") {
                if let Some(line) = v["line"].as_str().filter(|x| x.starts_with("hello ")) {
                    v["line"] = json!(line.replace(" mode 8 ", " mode 1 "));
                }
            }
        });
        assert_eq!(n2_outcome(&o, &i, "provenance"), Outcome::Pass);
    }

    fn n2p_good() -> (Files, Files) {
        let (mut o, mut i) = n2_good();
        edit(&mut i.run, |r| {
            r.insert(3, json!({"t": "barrier", "q": 2010}));
        });
        edit(&mut o.run, |r| {
            r.insert(
                4,
                json!({"t": "exit", "q": 2250, "name": "inner-player", "code": CUT}),
            );
        });
        o.recorder = Stream::of(vec![
            panel_pad(1500, PANEL3, 2, true),
            panel_pad(2150, 0, 2, true),
        ]);
        (o, i)
    }

    fn n2p_outcome(o: &Files, i: &Files, check: &str) -> Outcome {
        kill(&N2P, Some("0123456789ab"), o, i, Some(&inner_mode()))
            .into_iter()
            .find(|j| j.check == check)
            .unwrap()
            .outcome
    }

    #[test]
    fn n2p_needs_the_buttons_held_until_the_kill() {
        let (o, i) = n2p_good();
        assert_eq!(n2p_outcome(&o, &i, "n2p.neutral"), Outcome::Pass);
        // A neutral report after the barrier and before the kill (2050),
        // one before the barrier, and one releasing SELECT only: each
        // leaves the kill nothing to release.
        for (at, buttons) in [(2040, 0), (1800, 0), (2040, 1 << 8)] {
            let (mut o, i) = n2p_good();
            edit(&mut o.recorder, |r| {
                r.insert(1, panel_pad(at, buttons, 2, true));
            });
            assert_eq!(
                n2p_outcome(&o, &i, "n2p.neutral"),
                Outcome::Fail,
                "{at} {buttons:#x}"
            );
        }
        // The kill is recorded once its call returned (2050), its request
        // counter read before the call (2045): a neutral report at the
        // request itself fails, one between the request and the record is
        // of unknown order with the termination (incomplete), one a tick
        // after the record passes.
        for (at, want) in [
            (2045, Outcome::Fail),
            (2046, Outcome::Incomplete),
            (2050, Outcome::Incomplete),
            (2051, Outcome::Pass),
        ] {
            let (mut o, i) = n2p_good();
            edit(&mut o.recorder, |r| r[1]["q"] = json!(at));
            assert_eq!(n2p_outcome(&o, &i, "n2p.neutral"), want, "{at}");
        }
        // A kill record without its request counter shows no order.
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| {
            r[1].as_object_mut().unwrap().remove("requested");
        });
        assert_eq!(n2p_outcome(&o, &i, "n2p.neutral"), Outcome::Incomplete);
        // Pressed only after the barrier.
        let (mut o, i) = n2p_good();
        edit(&mut o.recorder, |r| r[0]["q"] = json!(2020));
        assert_eq!(n2p_outcome(&o, &i, "n2p.neutral"), Outcome::Fail);
        // N2's F24 broken before the kill.
        let (mut o, i) = n2_good();
        edit(&mut o.recorder, |r| r.insert(2, key(2030, false, true)));
        assert_eq!(n2_outcome(&o, &i, "n2.neutral"), Outcome::Fail);
        let (o, i) = n2_good();
        assert_eq!(n2_outcome(&o, &i, "n2.neutral"), Outcome::Pass);
    }

    #[test]
    fn n2p_needs_the_player_gone() {
        let (o, i) = n2p_good();
        let all = kill(&N2P, Some("0123456789ab"), &o, &i, Some(&inner_mode()));
        assert_eq!(all.len(), 7);
        assert!(all.iter().all(|j| j.outcome == Outcome::Pass), "{all:?}");
        // The negative: the player outlived the inner's job, and the outer
        // ended it.
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| {
            r.insert(
                4,
                json!({"t": "kill", "q": 2240, "name": "inner-player", "requested": 2235}),
            );
        });
        assert_eq!(n2p_outcome(&o, &i, "n2p.player-gone"), Outcome::Fail);
        // Never observed; ended by itself.
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| r.retain(|v| v["name"] != "inner-player"));
        assert_eq!(n2p_outcome(&o, &i, "n2p.player-gone"), Outcome::Fail);
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| r[4]["code"] = json!(0));
        assert_eq!(n2p_outcome(&o, &i, "n2p.player-gone"), Outcome::Fail);
        // The barrier not reached: nothing shown.
        let (o, mut i) = n2p_good();
        edit(&mut i.run, |r| r.retain(|v| v["t"] != "barrier"));
        assert_eq!(n2p_outcome(&o, &i, "n2p.abnormal"), Outcome::Incomplete);
    }

    /// N2p's negative, on the records survivors would leave (no run can make
    /// one: the inner starts its player in its own job, killed with it,
    /// inside the job the outer ends, neither allowing breakaway). A player
    /// still running after the wait, which the outer then ends itself, and a
    /// recorder whose end the outer never sees fail `n2p.player-gone` and
    /// `n2p.children-gone`; so do both ending with code 0, as the inner's
    /// own job ends them when the outer kills the inner's process alone.
    #[test]
    fn n2p_fails_when_the_player_and_the_recorder_outlive_the_inner() {
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| {
            r.retain(|v| !(v["t"] == "exit" && v["name"] == "inner-recorder"));
            let at = r.iter().position(|v| v["name"] == "inner-player").unwrap();
            r.insert(
                at,
                json!({"t": "kill", "q": 12_240, "name": "inner-player", "requested": 12_235}),
            );
            r[at + 1]["q"] = json!(12_250);
        });
        assert_eq!(n2p_outcome(&o, &i, "n2p.player-gone"), Outcome::Fail);
        assert_eq!(n2p_outcome(&o, &i, "n2p.children-gone"), Outcome::Fail);
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| {
            for v in r
                .iter_mut()
                .filter(|v| v["name"] == "inner-recorder" || v["name"] == "inner-player")
            {
                v["code"] = json!(0);
            }
        });
        assert_eq!(n2p_outcome(&o, &i, "n2p.player-gone"), Outcome::Fail);
        assert_eq!(n2p_outcome(&o, &i, "n2p.children-gone"), Outcome::Fail);
    }
}
