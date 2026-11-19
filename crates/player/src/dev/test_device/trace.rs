//! `tapestry test-device trace --out <jsonl> --build <id> [--port
//! <location>] [--ping-ms <ms>] [--no-trace] [--framing-probe]`
//! (docs/test-device.md "Host Commands"): the passive link.
//!
//! It binds the device's link ([`Connection::bind`]: the location, the COM
//! port resolved from it, the opened port resolved back to it), checks its
//! `hello` (this protocol version, naming `--build` with `faults 0`; else a
//! `refused` record, nothing sent after `hello`, exit 1), turns the timing
//! trace on, sends a ping every `--ping-ms`, and records every command,
//! answer, pong and trace record with the host counter, to the new file
//! `--out` and to standard output. It holds nothing and takes nothing from
//! its standard input: the input's end sends `flush`, waits for its `F`,
//! turns the trace off and ends. It fails at the end when its input failed
//! (a read error, or its reader ending without its result: only a clean
//! end of input is a normal end), a ping went unanswered, a command was
//! refused, or the trace shows a loss, a report still pending at the flush
//! (`P`) or an unacknowledged abort (`A`).
//!
//! `--framing-probe` sends each [`Malformed`] case instead, each followed
//! by a ping, and requires the case's answer (`err syntax` or
//! `err overlong`) and the ping's `pong`; then a `hello` that shows no
//! lease, `quiet 1` and the pad identified.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use test_device::protocol::{Command, Malformed};
use test_device::trace::Body;

use super::devices::System;
use super::link::{Connection, Endpoints, Event, What, hello_facts};
use super::win::qpc;

/// `trace`'s arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub out: PathBuf,
    /// The build `hello` must name.
    pub build: String,
    /// The device's location; the one device present when absent.
    pub port: Option<String>,
    /// Ping period; 0 for none.
    pub ping_ms: u64,
    /// Whether to turn the timing trace on.
    pub trace: bool,
    /// Probe the wire form instead of waiting for the input's end.
    pub framing_probe: bool,
}

/// How long the end may wait for `F` and for each answer.
const END_WAIT: Duration = Duration::from_secs(3);
/// How long a probe case may wait for its two answers.
const CASE_WAIT: Duration = Duration::from_secs(1);
/// The first probe ping's number: never 1, which a lenient reader would
/// read from several cases.
const PROBE_PING: u32 = 1000;

/// What the run saw that fails it at the end.
#[derive(Debug, Default)]
struct Watch {
    next_record: Option<u32>,
    problems: Vec<String>,
}

impl Watch {
    fn see(&mut self, event: &Event) {
        match event {
            Event::Record { record, line, .. } => {
                if let Some(n) = self.next_record
                    && record.n != n
                {
                    self.problems
                        .push(format!("trace record {} follows {}", record.n, n - 1));
                }
                self.next_record = Some(record.n.wrapping_add(1));
                if matches!(
                    record.body,
                    Body::Pending { .. } | Body::Unacknowledged { .. } | Body::Lost { .. }
                ) {
                    self.problems.push(format!("the trace shows `{line}`"));
                }
            }
            Event::Answer { line, to, .. } => {
                if let What::Command(c) = to.what
                    && line.starts_with("err ")
                {
                    self.problems.push(format!("`{c}` was answered `{line}`"));
                }
            }
        }
    }
}

fn emit(out: &mut dyn Write, v: &Value) -> anyhow::Result<()> {
    writeln!(out, "{v}").context("writing the trace's record")
}

fn drain(link: &mut Connection, out: &mut dyn Write) -> anyhow::Result<()> {
    for v in link.take_journal() {
        emit(out, &v)?;
    }
    out.flush().context("writing the trace's record")
}

/// Reads the link once, recording what arrived.
fn pump(
    link: &mut Connection,
    out: &mut dyn Write,
    watch: &mut Watch,
) -> anyhow::Result<Vec<Event>> {
    let events = link.poll();
    drain(link, out)?;
    let events = events?;
    for e in &events {
        watch.see(e);
    }
    Ok(events)
}

/// Reads the link until `done` says so, for at most `wait`.
fn until(
    link: &mut Connection,
    out: &mut dyn Write,
    watch: &mut Watch,
    wait: Duration,
    what: &str,
    mut done: impl FnMut(&Event) -> bool,
) -> anyhow::Result<()> {
    let end = Instant::now() + wait;
    loop {
        let mut met = false;
        for e in pump(link, out, watch)? {
            met |= done(&e);
        }
        if met {
            return Ok(());
        }
        if Instant::now() >= end {
            bail!("{what} within {wait:?}");
        }
    }
}

fn answers(e: &Event, cmd: Command) -> Option<&str> {
    match e {
        Event::Answer { to, line, .. } if to.what == What::Command(cmd) => Some(line),
        _ => None,
    }
}

/// Sends each malformed case and a ping after it, requiring the case's
/// answer and the ping's pong, then a quiet `hello`.
fn probe(
    link: &mut Connection,
    out: &mut dyn Write,
    watch: &mut Watch,
    build: &str,
) -> anyhow::Result<()> {
    for (i, m) in Malformed::ALL.into_iter().enumerate() {
        let n = PROBE_PING + u32::try_from(i).unwrap_or(u32::MAX - PROBE_PING);
        link.send_malformed(m)?;
        link.send(Command::Ping(n))?;
        drain(link, out)?;
        let mut answer = None;
        let mut pong = false;
        until(
            link,
            out,
            watch,
            CASE_WAIT,
            &format!("no answers to case {}", m.name()),
            |e| {
                if let Event::Answer { to, line, .. } = e {
                    if to.what == What::Malformed(m) {
                        answer = Some(line.clone());
                    }
                    pong |= to.what == What::Command(Command::Ping(n));
                }
                answer.is_some() && pong
            },
        )
        .with_context(|| format!("case {}", m.name()))?;
        let want = format!("err {}", m.answer().name());
        if answer.as_deref() != Some(want.as_str()) {
            bail!("case {}: answered {answer:?}, not `{want}`", m.name());
        }
    }
    link.send(Command::Hello)?;
    let mut hello = None;
    until(
        link,
        out,
        watch,
        END_WAIT,
        "no answer to `hello` after the probe",
        |e| {
            hello = hello
                .take()
                .or_else(|| answers(e, Command::Hello).map(str::to_owned));
            hello.is_some()
        },
    )?;
    let facts = hello_facts(hello.as_deref().unwrap_or_default(), Some(build))
        .map_err(anyhow::Error::msg)?;
    if facts.lease.is_some() || !facts.quiet || !facts.identified {
        bail!(
            "after the probe `hello` reads `{}`: not quiet with no lease and the pad identified",
            facts.line
        );
    }
    Ok(())
}

/// Waits for `input`'s end, pinging every `ping_ms`. The outer result is
/// the link's; the inner one the input's: a clean end, or why it failed (a
/// read error, or its reader ending without its result).
///
/// # Errors
///
/// The link failed.
fn passive(
    link: &mut Connection,
    out: &mut dyn Write,
    watch: &mut Watch,
    mut input: Box<dyn Read + Send>,
    ping_ms: u64,
) -> anyhow::Result<Result<(), String>> {
    let (tx, rx) = mpsc::channel::<std::io::Result<u64>>();
    std::thread::spawn(move || {
        // Nothing read is sent: the input's end is its only meaning.
        let _ = tx.send(std::io::copy(&mut input, &mut std::io::sink()));
    });
    let every = (ping_ms > 0).then(|| Duration::from_millis(ping_ms));
    let mut next_ping = Instant::now();
    let mut n: u32 = 0;
    loop {
        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Ok(Ok(_)) => return Ok(Ok(())),
            Ok(Err(e)) => return Ok(Err(format!("reading the standard input: {e}"))),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Ok(Err(
                    "the standard input's reader ended without its result".to_owned()
                ));
            }
        }
        if let Some(every) = every
            && Instant::now() >= next_ping
        {
            link.send(Command::Ping(n))?;
            n = n.wrapping_add(1);
            next_ping += every;
        }
        pump(link, out, watch)?;
    }
}

/// Flushes the trace, turns it off, and checks what the run saw.
fn end(
    link: &mut Connection,
    out: &mut dyn Write,
    watch: &mut Watch,
    trace_on: bool,
) -> anyhow::Result<()> {
    link.send(Command::Flush)?;
    until(
        link,
        out,
        watch,
        END_WAIT,
        "no `F` after `flush`",
        |e| matches!(e, Event::Record { record, .. } if record.body == Body::Flushed),
    )?;
    if trace_on {
        link.send(Command::Trace(false))?;
        until(
            link,
            out,
            watch,
            END_WAIT,
            "no answer to `trace off`",
            |e| answers(e, Command::Trace(false)).is_some(),
        )?;
    }
    // The last pings' pongs.
    let pings_left = |l: &Connection| {
        l.outstanding()
            .filter(|s| matches!(s.what, What::Command(Command::Ping(_))))
            .count()
    };
    let end_by = Instant::now() + END_WAIT;
    while pings_left(link) > 0 && Instant::now() < end_by {
        pump(link, out, watch)?;
    }
    let unanswered = link.unanswered_pings() + u32::try_from(pings_left(link)).unwrap_or(u32::MAX);
    if unanswered > 0 {
        watch
            .problems
            .push(format!("{unanswered} pings were not answered"));
    }
    emit(
        out,
        &json!({ "t": "end", "q": qpc(), "problems": watch.problems }),
    )?;
    drain(link, out)?;
    if let Some(first) = watch.problems.first() {
        bail!("{first} ({} problems)", watch.problems.len());
    }
    Ok(())
}

/// Runs the trace against `ep`, reading `input` for its end and writing
/// records to `out`.
///
/// # Errors
///
/// The link refused or failed, the probe failed, or the end's checks.
pub fn run<E: Endpoints + ?Sized>(
    ep: &E,
    args: &Args,
    input: Box<dyn Read + Send>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let refused = |out: &mut dyn Write, e: anyhow::Error| -> anyhow::Error {
        let _ = emit(
            out,
            &json!({ "t": "refused", "q": qpc(), "error": format!("{e:#}") }),
        );
        let _ = out.flush();
        e
    };
    let mut link = match Connection::bind(ep, args.port.as_deref()) {
        Ok(link) => link,
        Err(e) => return Err(refused(out, e)),
    };
    let hello = link.check_hello(Some(&args.build)).map(drop);
    drain(&mut link, out)?;
    if let Err(e) = hello {
        return Err(refused(out, e));
    }
    let mut watch = Watch::default();
    if args.trace {
        link.send(Command::Trace(true))?;
    }
    if args.framing_probe {
        probe(&mut link, out, &mut watch, &args.build)?;
    } else if let Err(why) = passive(&mut link, out, &mut watch, input, args.ping_ms)? {
        // A failed input still gets the orderly end, and fails the trace by
        // its own name.
        watch.problems.insert(0, why);
    }
    end(&mut link, out, &mut watch, args.trace)
}

/// Writes each record to the file and to standard output.
struct Tee {
    file: BufWriter<File>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write_all(buf)?;
        std::io::stdout().lock().write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        std::io::stdout().lock().flush()
    }
}

/// `trace`.
///
/// # Errors
///
/// As [`run`], or the record file cannot be created (it must be new).
pub fn trace(args: &Args) -> anyhow::Result<()> {
    let file = File::create_new(&args.out)
        .with_context(|| format!("{} (a new file)", args.out.display()))?;
    let mut out = Tee {
        file: BufWriter::new(file),
    };
    let result = run(&System, args, Box::new(std::io::stdin()), &mut out);
    out.flush().context("writing the trace's records")?;
    result
}

#[cfg(test)]
mod tests {
    use super::super::link::fake::{Dev, Host, hello};
    use super::*;

    const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
    const ELSEWHERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";
    const BUILD: &str = "0123456789ab";

    fn args(port: Option<&str>) -> Args {
        Args {
            out: PathBuf::new(),
            build: BUILD.to_owned(),
            port: port.map(str::to_owned),
            ping_ms: 20,
            trace: true,
            framing_probe: false,
        }
    }

    fn host(dev: &Dev) -> Host {
        Host {
            devs: vec![dev.clone()],
            ..Host::default()
        }
    }

    fn records(out: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(out)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn kinds(out: &[u8]) -> Vec<String> {
        records(out)
            .iter()
            .map(|r| r["t"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// An input whose lines would be commands, which ends after `after`.
    struct Input {
        lines: Option<&'static [u8]>,
        after: Duration,
    }

    impl Read for Input {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(lines) = self.lines.take() {
                let n = lines.len().min(buf.len());
                buf[..n].copy_from_slice(&lines[..n]);
                return Ok(n);
            }
            std::thread::sleep(self.after);
            Ok(0)
        }
    }

    fn ends_at_once() -> Box<dyn Read + Send> {
        Box::new(Input {
            lines: None,
            after: Duration::ZERO,
        })
    }

    /// A port whose location does not resolve back to the bound one is
    /// closed with nothing written; a location with no device is refused
    /// before any port opens.
    #[test]
    fn an_endpoint_off_the_bound_port_is_refused() {
        for back in [Some(ELSEWHERE.to_owned()), None] {
            let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
            let host = Host {
                back: Some(back),
                ..host(&dev)
            };
            let mut out = Vec::new();
            let e = run(&host, &args(Some(HERE)), ends_at_once(), &mut out).unwrap_err();
            assert!(format!("{e:#}").contains("resolves back"), "{e:#}");
            assert!(dev.device.with(|d| d.written.is_empty()));
            assert_eq!(kinds(&out), ["refused"]);
        }
        let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
        let host = host(&dev);
        let mut out = Vec::new();
        run(&host, &args(Some(ELSEWHERE)), ends_at_once(), &mut out).unwrap_err();
        assert!(host.opened.lock().unwrap().is_empty());
        assert_eq!(kinds(&out), ["refused"]);
    }

    /// A `hello` naming another build, or a fault build, is refused and
    /// nothing is sent after it.
    #[test]
    fn a_hello_of_another_build_is_refused() {
        for hello in [hello("fedcba987654", false), hello(BUILD, true)] {
            let dev = Dev::new(HERE, "COM5", hello);
            let mut out = Vec::new();
            let e = run(&host(&dev), &args(None), ends_at_once(), &mut out).unwrap_err();
            assert_eq!(dev.device.with(|d| d.lines()), ["hello"], "{e:#}");
            assert_eq!(kinds(&out).last().map(String::as_str), Some("refused"));
        }
        let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
        let mut out = Vec::new();
        let no_pings = Args {
            ping_ms: 0,
            ..args(None)
        };
        run(&host(&dev), &no_pings, ends_at_once(), &mut out).unwrap();
        assert_eq!(
            dev.device.with(|d| d.lines()),
            ["hello", "trace on", "flush", "trace off"]
        );
    }

    /// The passive trace writes only its own commands, whatever its input
    /// holds; the input's end ends it after the flush.
    #[test]
    fn a_passive_trace_sends_nothing_from_its_input() {
        let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
        let token = super::super::link::fake::TOKEN;
        let lines: &'static [u8] = Box::leak(
            format!("lease {token} 250 5000000\nbutton {token}:0 1 1\nrelease\nping 77\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let input = Box::new(Input {
            lines: Some(lines),
            after: Duration::from_millis(150),
        });
        let mut out = Vec::new();
        run(&host(&dev), &args(Some(HERE)), input, &mut out).unwrap();
        let written = dev.device.with(|d| d.lines());
        let (pings, others): (Vec<&String>, Vec<&String>) =
            written.iter().partition(|l| l.starts_with("ping "));
        assert_eq!(others, ["hello", "trace on", "flush", "trace off"]);
        assert!(!pings.is_empty());
        for (n, p) in pings.iter().enumerate() {
            assert_eq!(**p, format!("ping {n}"));
        }
        let records = records(&out);
        assert_eq!(records.first().map(|r| r["t"].clone()), Some(json!("open")));
        assert_eq!(
            records.first().map(|r| r["location"].clone()),
            Some(json!(HERE))
        );
        assert_eq!(
            records.first().map(|r| r["com"].clone()),
            Some(json!("COM5"))
        );
        assert_eq!(kinds(&out).last().map(String::as_str), Some("end"));
        assert_eq!(
            kinds(&out).iter().filter(|k| *k == "ping").count(),
            pings.len()
        );
    }

    /// An input that fails after a while: with an error, or by its reader
    /// panicking.
    struct Failing {
        panics: bool,
    }

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(50));
            assert!(!self.panics, "the input's reader fails");
            Err(std::io::Error::other("the console went away"))
        }
    }

    /// Only a clean end of the input ends the trace normally: a read error,
    /// or the reader ending without its result, fails it naming that, after
    /// the orderly end.
    #[test]
    fn a_failed_input_fails_the_trace() {
        for (panics, want) in [
            (false, "the console went away"),
            (true, "ended without its result"),
        ] {
            let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
            let mut out = Vec::new();
            let input = Box::new(Failing { panics });
            let e = run(&host(&dev), &args(Some(HERE)), input, &mut out).unwrap_err();
            assert!(format!("{e:#}").contains(want), "{e:#}");
            assert_eq!(kinds(&out).last().map(String::as_str), Some("end"));
            assert_eq!(
                dev.device.with(|d| d.lines()).last().map(String::as_str),
                Some("trace off")
            );
        }
    }

    /// The probe needs each case refused once and the ping after it
    /// answered; a device that reads a case leniently fails it.
    #[test]
    fn the_framing_probe_needs_each_case_refused_and_the_next_ping_answered() {
        let probe = Args {
            framing_probe: true,
            ..args(Some(HERE))
        };
        let dev = Dev::new(HERE, "COM5", hello(BUILD, false));
        let mut out = Vec::new();
        run(&host(&dev), &probe, ends_at_once(), &mut out).unwrap();
        let cases = records(&out)
            .iter()
            .filter(|r| r["t"] == "malformed")
            .count();
        assert_eq!(cases, Malformed::ALL.len());
        let lenient = Dev::new(HERE, "COM5", hello(BUILD, false));
        lenient.device.with(|d| d.lenient = true);
        let mut out = Vec::new();
        let e = run(&host(&lenient), &probe, ends_at_once(), &mut out).unwrap_err();
        assert!(format!("{e:#}").contains("tab"), "{e:#}");
    }
}
