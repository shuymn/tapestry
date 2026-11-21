//! `tapestry test-device judge <row> <dir>` (docs/test-device.md
//! "Verdicts"): the repository's verdicts for a run of `sink`,
//! `sink-early-close`, `sink-kill`, the panel rows `p1`, `p4` and `p5`,
//! or `p1-kill`, from its kept files, written to
//! `verdicts-judge.jsonl`. Each check reads only records of what a runner,
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
use super::launch::{DEVICE_AFTER, DEVICE_BEFORE, PLAYER_AFTER, PLAYER_BEFORE};
use super::listen::{BUTTON_EDGES, a_runner_s};
use super::run::{F24_SCAN, MODE_LINK, PANEL_MODE, SELECT, START, pad_buttons};

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
    fn read(dir: &Path) -> Self {
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
/// token, `quiet 1` and reason `released`, and then the `close`.
fn quiet_before_close(run: &[Value], link: &[Value]) -> Result<(), String> {
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
            if h.quiet && h.reason == Reason::Released && h.session == tok && tok.is_some() => {}
        _ => {
            return Err(format!(
                "the evidence `{line}` is not a quiet `hello` of the run's token after a release"
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
    for v in recs(&f.link)?.iter().filter(|v| v["t"] == "ans") {
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
            recs(&f.run).and_then(|r| Ok(quiet_before_close(r, recs(&f.link)?))),
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

/// The verdicts of a run of outer row `k` (`sink-kill`, `p1-kill`):
/// `outer` the outer's files, `inner` the inner's.
pub fn kill(k: &Kill, build: Option<&str>, outer: &Files, inner: &Files) -> Vec<Judged> {
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
        let run = recs(&outer.run)?;
        let kill = run
            .iter()
            .find(|v| v["t"] == "kill" && v["name"] == "inner")
            .map(q)
            .ok_or("the outer recorded no kill of the inner")?;
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
        let kill = recs(&outer.run)?
            .iter()
            .find(|v| v["t"] == "kill" && v["name"] == "inner")
            .map(q)
            .ok_or("the outer recorded no kill of the inner")?;
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
            if let Some(at) = up
                && at <= kill
            {
                return Ok(Err(format!(
                    "{what} were released at {at}, before the outer's kill at {kill}"
                )));
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
    let mut out = vec![
        verdict(
            provenance::CHECK,
            &[RUN_LINK, INNER_LINK],
            provenance(build, &[("outer", &outer.link), ("inner", &inner.link)]),
        ),
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

/// The three volumes `device.toml` holds after the run: each target's
/// percent.
pub fn volumes(after: &[u8]) -> Result<(), String> {
    let table: toml::Table = std::str::from_utf8(after)
        .map_err(|e| e.to_string())
        .and_then(|t| toml::from_str(t).map_err(|e| e.to_string()))
        .map_err(|e| format!("device.toml after the run does not read: {e}"))?;
    let mut wrong = Vec::new();
    for t in test_device::panel::TARGETS {
        let Some((_, key)) = VOLUME_KEYS.iter().find(|(s, _)| *s == t.slider) else {
            continue;
        };
        let want = f64::from(t.percent) / 100.0;
        match table.get(*key).and_then(toml::Value::as_float) {
            Some(got) if got == want => {}
            got => wrong.push(format!("{key} = {got:?}, not {want}")),
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
    let moved: Vec<String> = VOLUME_KEYS
        .iter()
        .filter_map(
            |(_, key)| match table.get(*key).and_then(toml::Value::as_float) {
                Some(1.0) => None,
                got => Some(format!("{key} = {got:?}, not 1.0")),
            },
        )
        .collect();
    if moved.is_empty() {
        Ok(())
    } else {
        Err(moved.join("; "))
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
    let want: Vec<(&str, Option<(i32, i32)>)> = super::run::row(row, false)
        .map(|steps| super::run::calls_of(&steps))
        .unwrap_or_default();
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
            "p1.volumes",
            &[file(DEVICE_AFTER)],
            bytes(&x.device_after).map(|a| volumes(&a)),
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
            recs(&f.run).and_then(|r| Ok(quiet_before_close(r, recs(&f.link)?))),
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
        "p4" => out.push(kept("p4.volumes-kept")),
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
    panel: Option<PanelFiles>,
}

/// Reads the run in `dir` for row `row`.
///
/// # Errors
///
/// An unknown row, or the run's `start` unreadable.
pub fn read(row: &str, dir: &Path) -> anyhow::Result<Reading> {
    let files = Files::read(dir);
    let start = recs(&files.run)
        .ok()
        .and_then(|r| r.first().cloned())
        .filter(|v| v["t"] == "start")
        .ok_or_else(|| anyhow::anyhow!("{}: no `start` record", dir.display()))?;
    let e = super::check::expectation_of(row)
        .ok_or_else(|| anyhow::anyhow!("`judge` has no row `{row}`"))?;
    let standing = contract::standing(&e, dir, super::COMMIT);
    let files = files.standing(SINGLE, &standing);
    let (inner, panel) = match row {
        "sink" | "sink-early-close" => (None, None),
        "sink-kill" | "p1-kill" => (
            Some(Files::read(&dir.join(INNER)).standing(INNER, &standing)),
            None,
        ),
        _ if held_by(row).is_some() => (None, Some(PanelFiles::read(dir))),
        _ => anyhow::bail!("`judge` has no row `{row}`"),
    };
    Ok(Reading {
        start,
        files,
        inner,
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
        ("sink-kill", Some(inner), _) => kill(&N2, build, f, inner),
        ("p1-kill", Some(inner), _) => kill(&N2P, build, f, inner),
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

/// Judges the run in `dir` as row `row` and writes `verdicts-judge.jsonl`.
///
/// # Errors
///
/// An unknown row, the run's `start` unreadable, or the file cannot be
/// written.
pub fn judge(row: &str, dir: &Path) -> anyhow::Result<()> {
    let r = read(row, dir)?;
    let out = super::owner::Stream::create(&dir.join(format!("verdicts-{PRODUCER}.jsonl")))?;
    for line in lines(row, &r) {
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
                json!({"t": "kill", "q": 2050, "name": "inner"}),
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
        kill(&N2, Some("0123456789ab"), o, i)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap()
            .outcome
    }

    #[test]
    fn n2_needs_the_children_gone() {
        let (o, i) = n2_good();
        let all: Vec<Outcome> = kill(&N2, Some("0123456789ab"), &o, &i)
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
        // Calls: the sink's focus (move, click), the player's focus (move,
        // click), four calibrations, three targets (move, click), tagged
        // 0xABCD0000 on, at 10-tick steps from 200.
        let calls = super::super::run::calls_of(&super::super::run::row("p1", false).unwrap());
        assert_eq!(calls.len(), 14);
        let mut run = vec![
            json!({"t": "start"}),
            json!({"t": "sink", "q": 100, "window": SINK}),
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
            json!({"t": "shot", "q": 2500, "what": "shot-a.png", "owned": true, "doubts": []}),
            json!({"t": "shot", "q": 3500, "what": "shot-b.png", "owned": true, "doubts": []}),
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
        assert_eq!(all.len(), 11);
        for j in &all {
            assert_eq!(j.outcome, Outcome::Pass, "{j:?}");
        }
    }

    /// The volumes as P4's and P5's players leave them: all three at 1.0.
    fn kept() -> PanelFiles {
        let (_, mut x) = panel_good();
        x.device_after = x.device_before.clone();
        x
    }

    /// A P5 run: P1's records with the declared mutated player, the
    /// volumes kept.
    fn p5_good() -> (Files, PanelFiles) {
        let (mut f, _) = panel_good();
        edit(&mut f.run, |r| {
            r.insert(
                2,
                json!({"t": "player", "q": 150, "path": "p5/t.exe", "sha256": "5a", "declared": "5a",
                       "runner_sha256": "4e", "commit": "nogit", "modified": 1}),
            );
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
        assert_eq!(v["inputs"][0]["sha256"], read_sha.as_str(), "{v}");
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
        assert!(d.contains("system_volume = Some(1.0), not 0.25"), "{d}");
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
        kill(&N2P, Some("0123456789ab"), o, i)
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
        let all = kill(&N2P, Some("0123456789ab"), &o, &i);
        assert_eq!(all.len(), 7);
        assert!(all.iter().all(|j| j.outcome == Outcome::Pass), "{all:?}");
        // The negative: the player outlived the inner's job, and the outer
        // ended it.
        let (mut o, i) = n2p_good();
        edit(&mut o.run, |r| {
            r.insert(4, json!({"t": "kill", "q": 2240, "name": "inner-player"}));
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
                json!({"t": "kill", "q": 12_240, "name": "inner-player"}),
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
