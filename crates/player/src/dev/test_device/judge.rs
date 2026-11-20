//! `tapestry test-device judge <row> <dir>` (docs/test-device.md
//! "Verdicts"): the repository's verdicts for a run of `sink`,
//! `sink-early-close` or `sink-kill`, from its kept files, written to
//! `verdicts-judge.jsonl`. Each check reads only records of what a runner,
//! the recorder or the device observed, and their order within one
//! stream; a check whose stretch of the recorder's stream an `error`
//! record covers, or whose file is missing or unreadable, is
//! `incomplete`.

use std::path::Path;

use serde_json::{Value, json};
use test_device::clock::{Bounds, Ping, ticks_to_us};
use test_device::contract::{INNER, LINK, RECORDER, RUN, SINGLE};
use test_device::evidence::Outcome;
use test_device::lease::Reason;
use test_device::protocol::{Answer, Command, Token};
use test_device::provenance;

use super::guard::SLICE_MS;
use super::listen::{BUTTON_EDGES, a_runner_s};
use super::run::{F24_SCAN, pad_buttons};

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

    fn read(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self {
                records: Err(format!("{} is missing", path.display())),
                sha: String::new(),
            };
        };
        let sha = test_device::contract::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        let records = std::str::from_utf8(&bytes)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|t| {
                t.lines()
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
            run: Stream::read(&dir.join(RUN)),
            link: Stream::read(&dir.join(LINK)),
            recorder: Stream::read(&dir.join(RECORDER)),
        }
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
/// same line, names the connection's token, and has a deadline the device
/// reaches no later than a slice after that check, by the round trips
/// read before it was written; none without a round trip.
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
                        token, deadline, ..
                    }) => (token, deadline),
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
                let bounds = Bounds::fit(&seen).map_err(|e| {
                    format!("`{line}` was written with no round trip that bounds the clock: {e:?}")
                })?;
                let (_, most) = bounds.unmap(ticks_to_us(q(check), freq));
                let bound = most + u64::from(SLICE_MS) * 1000;
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
    let in_front = |v: &Value| front.is_none_or(|w| v["fg"]["window"] == w);
    let pad = rec
        .iter()
        .find(|v| {
            v["t"] == "pad"
                && v["ours"] == true
                && v["bytes"]
                    .as_str()
                    .and_then(pad_buttons)
                    .is_some_and(|b| b & 1 != 0)
        })
        .ok_or("the recorder saw no press of button 1 from the device's pad")?;
    let key = rec
        .iter()
        .find(|v| {
            v["t"] == "key" && v["ours"] == true && v["code"] == F24_SCAN && v["down"] == true
        })
        .ok_or("the recorder saw no make of F24 from the device's keyboard")?;
    for v in [pad, key] {
        if !in_front(v) {
            return Err(format!("the sink was not in front: {v}"));
        }
    }
    let press = |v: &Value| Press {
        q: q(v),
        h: v["h"].as_u64().unwrap_or(u64::MAX),
    };
    Ok((press(pad), press(key)))
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
    let pad = released(rec, pad, |v| {
        v["t"] == "pad" && v["ours"] == true && v["bytes"].as_str().and_then(pad_buttons) == Some(0)
    });
    let key = released(rec, key, |v| {
        v["t"] == "key" && v["ours"] == true && v["code"] == F24_SCAN && v["down"] == false
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
    let fence = link
        .iter()
        .position(|v| v["t"] == "fence")
        .ok_or("the link recorded no fence")?;
    if !link[fence..]
        .iter()
        .any(|v| v["t"] == "ans" && v["cmd"] == "hello" && v["line"] == line)
    {
        return Err("the evidence is not a `hello` the link read after its fence".into());
    }
    Ok(())
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

/// The verdicts of a run of `sink` or `sink-early-close`.
pub fn n1(build: Option<&str>, f: &Files) -> Vec<Judged> {
    let front = recs(&f.run)
        .ok()
        .and_then(|r| r.iter().find(|v| v["t"] == "sink"))
        .and_then(|v| v["window"].as_u64());
    let span = |kind: &str| -> Result<i64, String> {
        recs(&f.run)?
            .iter()
            .find(|v| v["t"] == kind)
            .map(q)
            .ok_or_else(|| format!("the run recorded no `{kind}`"))
    };
    let pressed = || -> Result<Result<(Press, Press), String>, String> {
        let rec = recs(&f.recorder)?;
        covered(rec, span("call")?, span("held")?)?;
        Ok(presses(rec, front))
    };
    let calls: Vec<u32> = recs(&f.run)
        .map(|r| {
            r.iter()
                .filter(|v| v["t"] == "call")
                .filter_map(|v| v["tag"].as_u64().and_then(|t| u32::try_from(t).ok()))
                .collect()
        })
        .unwrap_or_default();
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
                let (from, to) = (span("call")?, span("close")?);
                covered(rec, from, to)?;
                Ok(no_foreign(rec, &calls, from, to))
            })(),
        ),
    ]
}

const INNER_RUN: (&str, &str) = (INNER, RUN);
const INNER_LINK: (&str, &str) = (INNER, LINK);

/// The verdicts of a run of `sink-kill`: `outer` the outer's files, `inner`
/// the inner's.
pub fn n2(build: Option<&str>, outer: &Files, inner: &Files) -> Vec<Judged> {
    let abnormal = || -> Result<Result<(), String>, String> {
        let run = recs(&inner.run)?;
        let link = recs(&inner.link)?;
        if !run.iter().any(|v| v["t"] == "held") {
            return Err("the inner recorded no `held`: the kill's fixture was not reached".into());
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
    let quiet = || -> Result<Result<(), String>, String> {
        let link = recs(&outer.link)?;
        let first = link
            .iter()
            .find(|v| {
                v["t"] == "ans" && v["line"].as_str().is_some_and(|l| l.starts_with("hello "))
            })
            .ok_or("the outer read no `hello`")?;
        Ok(
            match Answer::parse_canonical(first["line"].as_str().unwrap_or_default()) {
                Some(Answer::Hello(h)) if h.lease.is_none() && h.quiet => Ok(()),
                _ => Err(format!(
                    "the outer's first `hello` is not quiet with no lease: {first}"
                )),
            },
        )
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
    let neutral_after = || -> Result<Result<(), String>, String> {
        let rec = recs(&outer.recorder)?;
        let Ok((pad, key)) = presses(rec, None) else {
            return Err(
                "the outer's recorder saw no press: the kill's fixture was not reached".into(),
            );
        };
        let end = rec.iter().map(q).max().unwrap_or(i64::MAX);
        covered(rec, pad.q.min(key.q), end)?;
        Ok(neutral(rec, pad, key))
    };
    vec![
        verdict(
            provenance::CHECK,
            &[RUN_LINK, INNER_LINK],
            provenance(build, &[("outer", &outer.link), ("inner", &inner.link)]),
        ),
        verdict("n2.abnormal", &[INNER_RUN, INNER_LINK], abnormal()),
        verdict("n2.neutral", &[RUN_REC], neutral_after()),
        verdict("n2.quiet", &[RUN_LINK], quiet()),
        verdict("n2.children-gone", &[RUN_RUN], gone()),
    ]
}

/// Judges the run in `dir` as row `row` and writes `verdicts-judge.jsonl`.
///
/// # Errors
///
/// An unknown row, the run's `start` unreadable, or the file cannot be
/// written.
pub fn judge(row: &str, dir: &Path) -> anyhow::Result<()> {
    let f = Files::read(dir);
    let start = recs(&f.run)
        .ok()
        .and_then(|r| r.first().cloned())
        .filter(|v| v["t"] == "start")
        .ok_or_else(|| anyhow::anyhow!("{}: no `start` record", dir.display()))?;
    let run = start["run"].as_str().unwrap_or_default().to_owned();
    let build = start["build"].as_str().map(str::to_owned);
    let (judged, inner) = match row {
        "sink" | "sink-early-close" => (n1(build.as_deref(), &f), None),
        "sink-kill" => {
            let inner = Files::read(&dir.join(INNER));
            (n2(build.as_deref(), &f, &inner), Some(inner))
        }
        _ => anyhow::bail!("`judge` has no row `{row}`"),
    };
    let sha = |role: &str, path: &str| {
        let files = if role == INNER {
            inner.as_ref()
        } else {
            Some(&f)
        };
        files.map_or_else(String::new, |fs| match path {
            RUN => fs.run.sha.clone(),
            LINK => fs.link.sha.clone(),
            _ => fs.recorder.sha.clone(),
        })
    };
    let out = super::owner::Stream::create(&dir.join(format!("verdicts-{PRODUCER}.jsonl")))?;
    for j in judged {
        out.write(&json!({
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
        }));
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
        format!(
            "hello {VERSION} build 0123456789ab mode 8 lease {lease} armed 0 reason {reason} \
             boot power-on count 0 identified 1 faults 0 quiet {} session {TOKEN}",
            u8::from(quiet)
        )
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
            json!({"t": "call", "q": 200, "tag": tag - 1, "what": "move"}),
            json!({"t": "call", "q": 210, "tag": tag, "what": "click"}),
            json!({"t": "held", "q": 2000}),
            json!({"t": "stop", "q": 5000, "what": "end"}),
            json!({"t": "quiet", "q": 5600, "line": hello(true, "released", "-")}),
            json!({"t": "close", "q": 5800, "window": SINK}),
            json!({"t": "end", "exit": 0, "errors": []}),
        ];
        let mut link = vec![
            json!({"t": "open", "q": 10, "freq": FREQ}),
            json!({"t": "cmd", "q": 20, "line": "hello"}),
            json!({"t": "ans", "q": 30, "cmd": "hello", "line": hello(true, "none", "-")}),
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
            json!({"t": "ans", "q": 1020, "cmd": lease, "line": format!("lease {TOKEN}:1 {}", deadline(1000))}),
            json!({"t": "check", "q": 1800, "line": renew}),
            json!({"t": "cmd", "q": 1810, "line": renew}),
            json!({"t": "fence", "q": 5100}),
            json!({"t": "cmd", "q": 5110, "line": "release"}),
            json!({"t": "ans", "q": 5120, "cmd": "release", "line": "ok"}),
            json!({"t": "cmd", "q": 5500, "line": "hello"}),
            json!({"t": "ans", "q": 5510, "cmd": "hello", "line": hello(true, "released", "-")}),
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
            run: Stream::of(vec![json!({"t": "start"}), json!({"t": "held", "q": 2000})]),
            link: Stream::of(recs(&n1.link).unwrap()[..9].to_vec()),
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
                json!({"t": "ans", "q": 2310, "cmd": "hello", "line": hello(true, "closed", "-")}),
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
        n2(Some("0123456789ab"), o, i)
            .into_iter()
            .find(|j| j.check == check)
            .unwrap()
            .outcome
    }

    #[test]
    fn n2_needs_the_children_gone() {
        let (o, i) = n2_good();
        let all: Vec<Outcome> = n2(Some("0123456789ab"), &o, &i)
            .into_iter()
            .map(|j| j.outcome)
            .collect();
        assert!(all.iter().all(|x| *x == Outcome::Pass), "{all:?}");
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
            l[1]["line"] = json!(hello(false, "none", "1"))
        });
        assert_eq!(n2_outcome(&o, &i, "n2.quiet"), Outcome::Fail);
    }
}
