//! The run's end (docs/test-device.md "Run Guard"): on every way out the
//! runner stops holds, then closes its sink only on positive release
//! evidence: the fence and the `release` answered, then a `hello` of the
//! run's own token that reads `quiet 1` (with reason `released` when a
//! lease was granted), taken every 10 ms for up to 1 s; then the link's
//! end within its bound, the recorder's end, and the close. Without that
//! evidence, or on a `hello` of another token, the run fails and recovers
//! with its sink open: it asks for the sink in front and checks it is
//! (going on if Windows refuses), then `release` and `hello` again on a
//! reopened connection of the bound location, then the 1200-baud touch,
//! whose reboot the recorder must see as the bound handles' removal; the
//! sink closes only on that removal, however long it takes. Every step's
//! error is kept.

use std::time::Duration;

use serde_json::{Value, json};
use test_device::lease::Reason;
use test_device::protocol::{Answer, Token};

/// How long the end looks for the release evidence.
pub const QUIET_WAIT: Duration = Duration::from_secs(1);
/// How often it asks `hello`.
pub const HELLO_EVERY: Duration = Duration::from_millis(10);
/// How long the link's end may take: the flush's own bound plus 2 s.
pub const LINK_END: Duration = Duration::from_secs(3);
/// How long recovery waits for the removal before it says so.
pub const RECOVERY_WAIT: Duration = Duration::from_secs(30);

/// The end's view of the link.
pub trait Link {
    /// Stops holds (once) and requests the one `release`.
    fn stop_holds(&mut self, cause: &str);
    /// Waits until the fence is recorded and the `release` answered `ok`.
    ///
    /// # Errors
    ///
    /// Not within `wait`, or the link failed.
    fn released(&mut self, wait: Duration) -> Result<(), String>;
    /// Writes `hello` and waits for its answer: the line, if it came.
    fn hello(&mut self, wait: Duration) -> Option<String>;
    /// Ends the link: `flush` and its `F`, `trace off`, the port closed.
    ///
    /// # Errors
    ///
    /// What failed, or `deadline` passed first.
    fn finish(&mut self, deadline: Duration) -> Result<(), String>;
    /// Reopens a connection to the bound location, writes `release` and
    /// `hello`: the `hello`'s answer.
    ///
    /// # Errors
    ///
    /// The device is not there, or the connection failed.
    fn reopen_release(&mut self) -> Result<String, String>;
    /// The 1200-baud touch on the bound location.
    ///
    /// # Errors
    ///
    /// The port cannot be opened.
    fn touch_bootsel(&mut self) -> Result<(), String>;
}

/// The end's view of the recorder.
pub trait Recorder {
    /// Whether none of the device's input interfaces is at the bound
    /// location now, by this process's own enumeration: fresh at every
    /// call, every interface counted, the recorder's stream not needed.
    ///
    /// # Errors
    ///
    /// The enumeration failed: the removal is not observed.
    fn removed(&mut self) -> Result<bool, String>;
    /// Ends it (its input closed), after it drains, and waits for its exit.
    ///
    /// # Errors
    ///
    /// It did not end, or ended with an error.
    fn stop(&mut self) -> Result<(), String>;
}

/// The run's windows: the sink, and the player a panel row started.
pub trait Window {
    /// Asks for the sink in front and checks that it is.
    ///
    /// # Errors
    ///
    /// Windows kept another window in front.
    fn front(&mut self) -> Result<(), String>;
    /// Closes it.
    ///
    /// # Errors
    ///
    /// It could not be closed.
    fn close(&mut self) -> Result<(), String>;
}

/// The player a panel row started, as the end closes it.
pub trait Player {
    /// Asks it to close (`WM_CLOSE` to its main window).
    ///
    /// # Errors
    ///
    /// It has no window, or the message cannot be posted.
    fn ask_close(&mut self) -> Result<(), String>;
    /// Waits up to `wait` for it to end: its exit code once its end is
    /// observed, `None` while it runs.
    ///
    /// # Errors
    ///
    /// The wait failed: its end is not observed.
    fn wait(&mut self, wait: Duration) -> Result<Option<i32>, String>;
    /// Terminates it with the runner's cut code and waits for that end.
    ///
    /// # Errors
    ///
    /// The termination failed, or its end was not observed.
    fn kill(&mut self) -> Result<(), String>;
}

/// How long the player may take to close: it writes its settings as it
/// ends.
pub const PLAYER_CLOSE: Duration = Duration::from_secs(15);

/// Closes the player: asks it to close, then waits up to [`PLAYER_CLOSE`]
/// for it to end by itself with code 0. A player that does not end in time
/// is terminated, and the run fails.
///
/// # Errors
///
/// It could not be asked, it exited with another code, it missed the
/// deadline, or its end was not observed.
pub fn close_player(p: &mut dyn Player) -> Result<(), String> {
    let asked = p.ask_close();
    match p.wait(PLAYER_CLOSE)? {
        Some(0) => asked,
        Some(code) => Err(format!("the player exited {code}")),
        None => Err(match p.kill() {
            Ok(()) => format!("the player did not close within {PLAYER_CLOSE:?}: terminated"),
            Err(e) => format!(
                "the player did not close within {PLAYER_CLOSE:?}, and terminating it failed: {e}"
            ),
        }),
    }
}

pub trait Clock {
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clock.
pub struct Wall(pub std::time::Instant);

impl Clock for Wall {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// What the end is given.
pub struct Run<'a> {
    pub link: &'a mut dyn Link,
    pub recorder: &'a mut dyn Recorder,
    pub sink: Option<&'a mut dyn Window>,
    pub clock: &'a dyn Clock,
    /// The run's own token.
    pub token: Option<Token>,
    /// Whether the device granted a lease in the run.
    pub granted: bool,
    /// Writes a record of the run's stream; `q` is filled in.
    pub record: &'a mut dyn FnMut(Value),
    /// The cause the end stops holds with.
    pub cause: &'a str,
}

/// Whether `line` is release evidence for the run: a `hello` of `token`
/// that reads `quiet 1`, with reason `released` when a lease was granted.
/// `Err` when it is a `hello` of another token (the connection ended and
/// another began, or the device rebooted).
fn evidence(line: &str, token: Option<Token>, granted: bool) -> Result<bool, String> {
    let Some(Answer::Hello(h)) = Answer::parse_canonical(line) else {
        return Ok(false);
    };
    if token.is_none() || h.session != token {
        return Err(format!("a `hello` of another token: `{line}`"));
    }
    Ok(h.quiet && (!granted || h.reason == Reason::Released))
}

/// Ends the run: the errors it met, every one kept.
pub fn end(r: Run<'_>) -> Vec<String> {
    let mut errors = Vec::new();
    r.link.stop_holds(r.cause);
    let mut sink = r.sink;
    if let Err(e) = r.link.released(QUIET_WAIT) {
        errors.push(format!("the release: {e}"));
    }
    let begun = r.clock.now();
    let mut found = None;
    let mut other = None;
    while found.is_none() && other.is_none() && r.clock.now() - begun < QUIET_WAIT {
        if let Some(line) = r.link.hello(HELLO_EVERY) {
            match evidence(&line, r.token, r.granted) {
                Ok(true) => found = Some(line),
                Ok(false) => {}
                Err(e) => other = Some(e),
            }
        }
        if found.is_none() {
            r.clock.sleep(HELLO_EVERY);
        }
    }
    if let Some(line) = &found {
        (r.record)(json!({ "t": "quiet", "line": line }));
    }
    if let Err(e) = r.link.finish(LINK_END) {
        errors.push(format!("the link's end: {e}"));
    }
    if found.is_none() {
        errors.push(other.unwrap_or_else(|| {
            format!("no `hello` of the run's token read `quiet 1` within {QUIET_WAIT:?}")
        }));
        recover(
            r.link,
            r.recorder,
            &mut sink,
            r.clock,
            r.record,
            &mut errors,
        );
    }
    if let Err(e) = r.recorder.stop() {
        errors.push(format!("the recorder's end: {e}"));
    }
    if let Some(s) = sink {
        close(s, r.record, &mut errors);
    }
    errors
}

fn close(s: &mut dyn Window, record: &mut dyn FnMut(Value), errors: &mut Vec<String>) {
    match s.close() {
        Ok(()) => record(json!({ "t": "close" })),
        Err(e) => errors.push(format!("closing the run's windows: {e}")),
    }
}

/// Recovery, with the sink open and holds stopped: the sink asked in front
/// and checked there (a refusal is recorded and kept, and recovery goes
/// on, since the device's disarm does not need it), `release` and `hello`
/// on a reopened connection, the 1200-baud touch, then the device's input
/// interfaces
/// gone from the bound location, waited for however long it takes. Only
/// an enumeration that succeeded and found neither interface is the
/// removal; a failed one is recorded (each new failure once) and looked
/// at again.
fn recover(
    link: &mut dyn Link,
    rec: &mut dyn Recorder,
    sink: &mut Option<&mut dyn Window>,
    clock: &dyn Clock,
    record: &mut dyn FnMut(Value),
    errors: &mut Vec<String>,
) {
    let mut step = |what: String| record(json!({ "t": "recovery", "what": what }));
    if let Some(s) = sink.as_mut() {
        match s.front() {
            Ok(()) => step("the sink in front".into()),
            Err(e) => {
                let msg = format!("the sink could not be brought in front: {e}");
                step(msg.clone());
                errors.push(msg);
            }
        }
    }
    match link.reopen_release() {
        Ok(line) => step(format!("reopened: `release`, then `hello` read `{line}`")),
        Err(e) => step(format!("reopening failed: {e}")),
    }
    match link.touch_bootsel() {
        Ok(()) => step("the 1200-baud touch".into()),
        Err(e) => step(format!("the 1200-baud touch failed: {e}")),
    }
    let begun = clock.now();
    let mut told = false;
    let mut failed: Option<String> = None;
    loop {
        match rec.removed() {
            Ok(true) => break,
            Ok(false) => failed = None,
            Err(e) => {
                if failed.as_ref() != Some(&e) {
                    step(format!("the enumeration failed, no removal observed: {e}"));
                }
                failed = Some(e);
            }
        }
        if !told && clock.now() - begun >= RECOVERY_WAIT {
            told = true;
            let msg = "the device's input interfaces are still at the bound location 30 s \
                       after the touch: the sink stays open until the board is unplugged or the \
                       runner is ended";
            eprintln!("{msg}");
            step(msg.into());
            errors.push(msg.into());
        }
        clock.sleep(HELLO_EVERY);
    }
    step("the device's input interfaces are gone from the bound location".into());
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use test_device::protocol::VERSION;

    use super::*;

    const TOKEN: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 3,
    };
    const OTHER: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 4,
    };

    fn hello(token: Token, quiet: bool, reason: &str) -> String {
        format!(
            "hello {VERSION} build abc mode 8 lease - armed 0 reason {reason} boot power-on \
             count 0 identified 1 faults 0 quiet {} session {token}",
            u8::from(quiet)
        )
    }

    /// A clock that moves only when slept.
    #[derive(Default)]
    struct Fake(Cell<Duration>);

    impl Clock for Fake {
        fn now(&self) -> Duration {
            self.0.get()
        }
        fn sleep(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    type Log = Rc<RefCell<Vec<String>>>;

    /// A stand-in link: `hellos` answers in turn (the last repeats).
    struct L {
        log: Log,
        hellos: Vec<String>,
        n: usize,
        finish: Result<(), String>,
        released: Result<(), String>,
    }

    impl Link for L {
        fn stop_holds(&mut self, cause: &str) {
            self.log.borrow_mut().push(format!("stop {cause}"));
        }
        fn released(&mut self, _: Duration) -> Result<(), String> {
            self.log.borrow_mut().push("released".into());
            self.released.clone()
        }
        fn hello(&mut self, _: Duration) -> Option<String> {
            let line = self.hellos.get(self.n.min(self.hellos.len() - 1)).cloned();
            self.n += 1;
            self.log.borrow_mut().push("hello".into());
            line
        }
        fn finish(&mut self, deadline: Duration) -> Result<(), String> {
            self.log.borrow_mut().push("finish".into());
            self.finish
                .clone()
                .map_err(|e| format!("{e} (deadline {deadline:?})"))
        }
        fn reopen_release(&mut self) -> Result<String, String> {
            self.log.borrow_mut().push("reopen".into());
            Ok(hello(OTHER, false, "none"))
        }
        fn touch_bootsel(&mut self) -> Result<(), String> {
            self.log.borrow_mut().push("touch".into());
            Ok(())
        }
    }

    /// A stand-in recorder: each look at the removal takes the next of
    /// `looks` (the last repeats; none present when empty).
    struct R {
        log: Log,
        n: usize,
        looks: Vec<Result<bool, String>>,
        stop: Result<(), String>,
    }

    impl Recorder for R {
        fn removed(&mut self) -> Result<bool, String> {
            let r = self
                .looks
                .get(self.n.min(self.looks.len().saturating_sub(1)))
                .cloned()
                .unwrap_or(Ok(false));
            self.n += 1;
            if r == Ok(true) {
                self.log.borrow_mut().push("removed".into());
            }
            r
        }
        fn stop(&mut self) -> Result<(), String> {
            self.log.borrow_mut().push("recorder stop".into());
            self.stop.clone()
        }
    }

    /// A stand-in sink: `front` answers as given.
    struct W(Log, Result<(), String>);

    impl Window for W {
        fn front(&mut self) -> Result<(), String> {
            self.0.borrow_mut().push("front".into());
            self.1.clone()
        }
        fn close(&mut self) -> Result<(), String> {
            self.0.borrow_mut().push("close".into());
            Ok(())
        }
    }

    struct Case {
        hellos: Vec<String>,
        finish: Result<(), String>,
        /// What each look at the removal finds.
        looks: Vec<Result<bool, String>>,
        stop: Result<(), String>,
        granted: bool,
        /// What asking for the sink in front gives.
        front: Result<(), String>,
    }

    /// Looks that find the interfaces `n` times, then their removal.
    fn removed_after(n: usize) -> Vec<Result<bool, String>> {
        let mut v = vec![Ok(false); n];
        v.push(Ok(true));
        v
    }

    impl Default for Case {
        fn default() -> Self {
            Self {
                hellos: vec![hello(TOKEN, true, "released")],
                finish: Ok(()),
                looks: removed_after(0),
                stop: Ok(()),
                granted: true,
                front: Ok(()),
            }
        }
    }

    /// Runs the end: its errors, the steps in order, the records.
    fn run(c: Case) -> (Vec<String>, Vec<String>, Vec<Value>) {
        let log: Log = Rc::default();
        let mut link = L {
            log: Rc::clone(&log),
            hellos: c.hellos,
            n: 0,
            finish: c.finish,
            released: Ok(()),
        };
        let mut rec = R {
            log: Rc::clone(&log),
            n: 0,
            looks: c.looks,
            stop: c.stop,
        };
        let mut sink = W(Rc::clone(&log), c.front);
        let clock = Fake::default();
        let records = RefCell::new(Vec::new());
        let mut record = |v: Value| {
            log.borrow_mut()
                .push(format!("record {}", v["t"].as_str().unwrap()));
            records.borrow_mut().push(v);
        };
        let errors = end(Run {
            link: &mut link,
            recorder: &mut rec,
            sink: Some(&mut sink),
            clock: &clock,
            token: Some(TOKEN),
            granted: c.granted,
            record: &mut record,
            cause: "end",
        });
        let steps = log.borrow().clone();
        (errors, steps, records.into_inner())
    }

    fn at(steps: &[String], s: &str) -> usize {
        steps
            .iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("no `{s}` in {steps:?}"))
    }

    #[test]
    fn a_failed_teardown_step_fails_the_run() {
        let (errors, _, _) = run(Case {
            stop: Err("exit 1".into()),
            ..Case::default()
        });
        assert_eq!(errors, ["the recorder's end: exit 1"]);
        // Positive control: a clean end has none.
        assert!(run(Case::default()).0.is_empty());
    }

    #[test]
    fn a_stalled_link_end_fails_the_run_within_its_deadline() {
        let (errors, steps, _) = run(Case {
            finish: Err("no `F`".into()),
            ..Case::default()
        });
        assert_eq!(
            errors,
            [format!("the link's end: no `F` (deadline {LINK_END:?})")]
        );
        // The end went on to the recorder and the close.
        assert!(at(&steps, "finish") < at(&steps, "recorder stop"));
        assert!(at(&steps, "recorder stop") < at(&steps, "close"));
    }

    #[test]
    fn the_window_closes_only_after_a_quiet_hello_of_the_run_s_token() {
        // Not quiet twice, then quiet.
        let (errors, steps, records) = run(Case {
            hellos: vec![
                hello(TOKEN, false, "released"),
                hello(TOKEN, false, "released"),
                hello(TOKEN, true, "released"),
            ],
            ..Case::default()
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert!(at(&steps, "record quiet") < at(&steps, "close"));
        assert_eq!(steps.iter().filter(|s| *s == "hello").count(), 3);
        let quiet = records.iter().find(|v| v["t"] == "quiet").unwrap();
        assert_eq!(quiet["line"], hello(TOKEN, true, "released"));
        // Quiet with another reason after a grant is not the evidence.
        let (errors, steps, _) = run(Case {
            hellos: vec![hello(TOKEN, true, "expired")],
            ..Case::default()
        });
        assert!(!errors.is_empty());
        assert!(!steps.contains(&"record quiet".to_owned()));
        // Without a grant any reason does.
        let (errors, _, _) = run(Case {
            hellos: vec![hello(TOKEN, true, "none")],
            granted: false,
            ..Case::default()
        });
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A recorder whose stream is all neutral does not stand in for the
    /// device's `quiet`: with `quiet 0` throughout, the sink closes only
    /// after the recovery's removal.
    #[test]
    fn a_lagging_recorder_cannot_close_the_window() {
        let (errors, steps, _) = run(Case {
            hellos: vec![hello(TOKEN, false, "released")],
            looks: removed_after(5),
            ..Case::default()
        });
        assert!(errors[0].contains("no `hello`"), "{errors:?}");
        assert!(at(&steps, "removed") < at(&steps, "close"));
        assert!(at(&steps, "touch") < at(&steps, "removed"));
    }

    /// Every sequence of up to four looks at the removal, each a failed
    /// enumeration, the interfaces still there, or their absence, then
    /// their absence: the sink closes exactly after the first look that
    /// enumerated and found neither interface, never on a failed one, and
    /// each failure is recorded. The control: absence at the first look.
    #[test]
    fn the_sink_closes_only_after_an_observed_removal() {
        let kinds = [Err("no list".to_owned()), Ok(false), Ok(true)];
        let mut runs = 0;
        for len in 0..=4_u32 {
            for n in 0..3_usize.pow(len) {
                let mut looks: Vec<Result<bool, String>> = (0..len)
                    .map(|i| kinds[n / 3_usize.pow(i) % 3].clone())
                    .collect();
                looks.push(Ok(true));
                let first = looks.iter().position(|l| *l == Ok(true)).unwrap();
                let (_, steps, records) = run(Case {
                    hellos: vec!["ok".into()],
                    looks: looks.clone(),
                    ..Case::default()
                });
                // The removal is the look it is, and the close follows it.
                let removed = at(&steps, "removed");
                assert!(removed < at(&steps, "close"), "{looks:?}: {steps:?}");
                assert_eq!(
                    steps.iter().filter(|s| *s == "removed").count(),
                    1,
                    "{looks:?}"
                );
                let failures = records
                    .iter()
                    .filter(|v| {
                        v["t"] == "recovery"
                            && v["what"]
                                .as_str()
                                .is_some_and(|w| w.contains("no removal observed"))
                    })
                    .count();
                // Each run of failures is recorded once.
                let runs_of_failures = looks[..first]
                    .iter()
                    .enumerate()
                    .filter(|&(i, l)| l.is_err() && (i == 0 || looks[i - 1].is_ok()))
                    .count();
                assert_eq!(failures, runs_of_failures, "{looks:?}: {records:?}");
                runs += 1;
            }
        }
        assert_eq!(runs, 1 + 3 + 9 + 27 + 81);
    }

    #[test]
    fn another_token_starts_recovery_with_the_sink_open() {
        let (errors, steps, records) = run(Case {
            hellos: vec![hello(OTHER, true, "released")],
            ..Case::default()
        });
        assert!(errors[0].contains("another token"), "{errors:?}");
        // One `hello` was enough to know.
        assert_eq!(steps.iter().filter(|s| *s == "hello").count(), 1);
        assert!(at(&steps, "reopen") < at(&steps, "close"));
        assert!(!records.iter().any(|v| v["t"] == "quiet"));
    }

    #[test]
    fn the_sink_closes_only_on_release_evidence() {
        // No answer at all: recovery; the removal shows after 40 s of
        // looks (past the 30 s report), and only then the close.
        let looks = 4000;
        let (errors, steps, records) = run(Case {
            hellos: vec!["ok".into()],
            looks: removed_after(looks),
            ..Case::default()
        });
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[1].contains("stays open"));
        let close = at(&steps, "close");
        assert!(at(&steps, "removed") < close);
        // The close and its record are the last steps.
        assert_eq!(close, steps.len() - 2);
        assert_eq!(at(&steps, "record close"), steps.len() - 1);
        let recovery: Vec<&str> = records
            .iter()
            .filter(|v| v["t"] == "recovery")
            .map(|v| v["what"].as_str().unwrap())
            .collect();
        assert_eq!(recovery.len(), 5, "{recovery:?}");
        assert_eq!(recovery[0], "the sink in front");
        assert!(recovery[1].starts_with("reopened"));
        assert_eq!(recovery[2], "the 1200-baud touch");
        assert!(recovery[3].contains("stays open"));
        assert!(recovery[4].contains("gone from the bound location"));
    }

    /// Recovery first asks for the sink in front, after holds stopped and
    /// before the device is reopened; when Windows refuses, the refusal is
    /// recorded and kept, and the disarm, the touch and the wait for the
    /// removal still follow. A run with its evidence asks for nothing.
    #[test]
    fn recovery_brings_the_sink_in_front_first() {
        for front in [Ok(()), Err("another window kept the foreground".to_owned())] {
            let (errors, steps, records) = run(Case {
                hellos: vec![hello(OTHER, true, "released")],
                looks: removed_after(2),
                front: front.clone(),
                ..Case::default()
            });
            assert!(at(&steps, "stop end") < at(&steps, "front"), "{steps:?}");
            assert!(at(&steps, "front") < at(&steps, "reopen"), "{steps:?}");
            assert!(at(&steps, "reopen") < at(&steps, "touch"), "{steps:?}");
            assert!(at(&steps, "touch") < at(&steps, "removed"), "{steps:?}");
            assert!(at(&steps, "removed") < at(&steps, "close"), "{steps:?}");
            let first = records.iter().find(|v| v["t"] == "recovery").unwrap();
            let refused = errors.iter().any(|e| e.contains("brought in front"));
            match front {
                Ok(()) => {
                    assert_eq!(first["what"], "the sink in front");
                    assert!(!refused, "{errors:?}");
                }
                Err(_) => {
                    assert!(
                        first["what"]
                            .as_str()
                            .unwrap()
                            .contains("another window kept the foreground"),
                        "{first}"
                    );
                    assert!(refused, "{errors:?}");
                }
            }
        }
        let (_, steps, _) = run(Case::default());
        assert!(!steps.contains(&"front".to_owned()), "{steps:?}");
    }

    /// A stand-in player: it ends by itself with `code` after `after`, or
    /// never.
    struct P {
        did: Vec<&'static str>,
        ends: Option<(Duration, i32)>,
        killed: bool,
    }

    impl Player for P {
        fn ask_close(&mut self) -> Result<(), String> {
            self.did.push("ask");
            Ok(())
        }
        fn wait(&mut self, wait: Duration) -> Result<Option<i32>, String> {
            self.did.push("wait");
            if self.killed {
                return Ok(Some(0x4000_0123));
            }
            Ok(self
                .ends
                .filter(|(after, _)| *after <= wait)
                .map(|(_, c)| c))
        }
        fn kill(&mut self) -> Result<(), String> {
            self.did.push("kill");
            self.killed = true;
            Ok(())
        }
    }

    /// A player that does not end within its close's deadline is
    /// terminated and fails the run; one that ends in time with 0 closes it
    /// cleanly (the control), and one that ends with another code fails.
    #[test]
    fn a_player_that_misses_its_close_fails_the_run() {
        let mut never = P {
            did: Vec::new(),
            ends: None,
            killed: false,
        };
        let e = close_player(&mut never).unwrap_err();
        assert!(e.contains("did not close within"), "{e}");
        assert_eq!(never.did, ["ask", "wait", "kill"]);
        let mut late = P {
            did: Vec::new(),
            ends: Some((PLAYER_CLOSE + Duration::from_millis(1), 0)),
            killed: false,
        };
        assert!(close_player(&mut late).is_err());
        assert!(late.did.contains(&"kill"));
        let mut clean = P {
            did: Vec::new(),
            ends: Some((Duration::from_secs(2), 0)),
            killed: false,
        };
        assert_eq!(close_player(&mut clean), Ok(()));
        assert_eq!(clean.did, ["ask", "wait"]);
        let mut failed = P {
            did: Vec::new(),
            ends: Some((Duration::from_secs(2), 3)),
            killed: false,
        };
        assert_eq!(close_player(&mut failed), Err("the player exited 3".into()));
    }
}
