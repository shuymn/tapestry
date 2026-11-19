//! A run's provenance, every row's first check (docs/test-device.md
//! "Verdicts"): the run names a build, and every `hello` it recorded is of
//! this protocol version, reads `faults 0` and names the build the run
//! expected at that point, so a verdict is about the normal build its run
//! names.

use crate::evidence::Outcome;
use crate::protocol::{Answer, VERSION};

/// The check's id in every row's expectation.
pub const CHECK: &str = "provenance";

/// What the run recorded, at a host time (one counter for the whole run).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A `hello` answer and where it was recorded (a file).
    Hello {
        at: i64,
        line: String,
        source: String,
    },
    /// A flash the runner finished without an error, which put `build` on
    /// the device: the hellos after it must name it.
    Flashed { at: i64, build: String },
}

impl Event {
    const fn at(&self) -> i64 {
        match self {
            Self::Hello { at, .. } | Self::Flashed { at, .. } => *at,
        }
    }
}

/// The provenance check of a run whose build is `build`, from `events` in
/// any order: incomplete when the run names no build or recorded no
/// `hello`; failed by a `hello` that is not one of this protocol version,
/// reads `faults 1`, or names another build than the one expected at its
/// time; passed otherwise. The outcome and its detail.
#[must_use]
pub fn check(build: Option<&str>, events: &[Event]) -> (Outcome, String) {
    let Some(build) = build else {
        return (Outcome::Incomplete, "the run names no build".into());
    };
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| e.at());
    let mut want = build;
    let mut hellos = 0_usize;
    for e in sorted {
        match e {
            Event::Flashed { build, .. } => want = build,
            Event::Hello { at, line, source } => {
                hellos += 1;
                let wrong = match Answer::parse_canonical(line) {
                    Some(Answer::Hello(h)) if h.build != want => {
                        format!("names build {}, the run expects {want}", h.build)
                    }
                    Some(Answer::Hello(h)) if h.faults => "reads faults 1".into(),
                    Some(Answer::Hello(_)) => continue,
                    _ => format!("is not a hello of protocol version {VERSION}"),
                };
                return (Outcome::Fail, format!("{source} at {at}: `{line}` {wrong}"));
            }
        }
    }
    if hellos == 0 {
        return (
            Outcome::Incomplete,
            format!("no hello recorded: the run's build {build} is not shown"),
        );
    }
    (
        Outcome::Pass,
        format!(
            "{hellos} hellos of protocol version {VERSION} read faults 0 and name the expected \
             build (the run's: {build})"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Hello, Token};

    fn hello(at: i64, build: &str, faults: bool) -> Event {
        let h = Hello {
            version: VERSION,
            build,
            mode: crate::mode::Mode::from_scratch(0),
            lease: None,
            armed: false,
            reason: crate::lease::Reason::None,
            boot: crate::boot::Cause::Forced,
            count: 0,
            identified: true,
            faults,
            quiet: true,
            session: Some(Token {
                boot: 0x0123_4567_89ab_cdef,
                count: 2,
            }),
        };
        Event::Hello {
            at,
            line: Answer::Hello(h).to_string(),
            source: "link.jsonl".into(),
        }
    }

    #[test]
    fn every_hello_names_the_expected_build() {
        let good = [hello(1, "abc", false), hello(2, "abc", false)];
        assert_eq!(check(Some("abc"), &good).0, Outcome::Pass);
        let other = check(
            Some("abc"),
            &[hello(1, "abc", false), hello(2, "def", false)],
        );
        assert_eq!(other.0, Outcome::Fail);
        assert!(other.1.contains("names build def"), "{other:?}");
    }

    #[test]
    fn a_hello_of_another_version_fails() {
        let Event::Hello { line, .. } = hello(1, "abc", false) else {
            unreachable!()
        };
        let line = line.replacen(
            &format!("hello {VERSION} "),
            &format!("hello {} ", VERSION + 1),
            1,
        );
        let e = Event::Hello {
            at: 1,
            line,
            source: "link.jsonl".into(),
        };
        let other = check(Some("abc"), &[e]);
        assert_eq!(other.0, Outcome::Fail);
        assert!(other.1.contains("protocol version"), "{other:?}");
    }

    #[test]
    fn a_hello_with_faults_1_fails() {
        let faults = check(
            Some("abc"),
            &[hello(1, "abc", false), hello(2, "abc", true)],
        );
        assert_eq!(faults.0, Outcome::Fail);
        assert!(faults.1.contains("reads faults 1"), "{faults:?}");
    }

    #[test]
    fn a_flash_switches_the_expected_build() {
        let flashed = Event::Flashed {
            at: 5,
            build: "new".into(),
        };
        let events = [
            hello(9, "new", false),
            flashed.clone(),
            hello(1, "old", false),
        ];
        assert_eq!(check(Some("old"), &events).0, Outcome::Pass);
        assert_eq!(
            check(Some("old"), &[flashed, hello(9, "old", false)]).0,
            Outcome::Fail
        );
    }

    #[test]
    fn no_build_or_no_hello_is_incomplete() {
        assert_eq!(
            check(None, &[hello(1, "abc", false)]).0,
            Outcome::Incomplete
        );
        assert_eq!(check(Some("abc"), &[]).0, Outcome::Incomplete);
    }
}
