//! A run's provenance, every row's first check (docs/test-device.md
//! "Verdicts"): the run names a build, and every `hello` it recorded is of
//! this protocol version, reads `faults 0` and names the build the run
//! expected at that point, so a verdict is about the normal build its run
//! names.

use crate::evidence::Outcome;
use crate::protocol::{Answer, VERSION};

/// The check's id in every row's expectation.
pub const CHECK: &str = "provenance";

/// What the run recorded, at a host time (one counter for the whole run),
/// and where: the file and the record's line in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A `hello` answer.
    Hello {
        at: i64,
        line: String,
        source: String,
        index: usize,
    },
    /// A flash the runner finished without an error, which put `build` on
    /// the device: the hellos after it must name it.
    Flashed {
        at: i64,
        build: String,
        source: String,
        index: usize,
    },
}

impl Event {
    const fn at(&self) -> i64 {
        match self {
            Self::Hello { at, .. } | Self::Flashed { at, .. } => *at,
        }
    }

    fn place(&self) -> (&str, usize) {
        match self {
            Self::Hello { source, index, .. } | Self::Flashed { source, index, .. } => {
                (source, *index)
            }
        }
    }
}

/// Two events at one host time, at least one a flash, recorded in two
/// files: their order is not known.
fn unordered_tie<'a>(sorted: &[&'a Event]) -> Option<(&'a Event, &'a Event)> {
    sorted.windows(2).find_map(|w| {
        let [a, b] = w else { return None };
        let flash = matches!(a, Event::Flashed { .. }) || matches!(b, Event::Flashed { .. });
        (a.at() == b.at() && flash && a.place().0 != b.place().0).then_some((*a, *b))
    })
}

/// The provenance check of a run whose build is `build`, from `events` in
/// any order: incomplete when the run names no build or recorded no
/// `hello`; failed by a `hello` that is not one of this protocol version,
/// reads `faults 1`, or names another build than the one expected at its
/// time; passed otherwise. Events at one host time keep the order they were
/// recorded in one file; a flash at the same time as an event recorded in
/// another file leaves the order unknown, and the check incomplete. The
/// outcome and its detail.
#[must_use]
pub fn check(build: Option<&str>, events: &[Event]) -> (Outcome, String) {
    let Some(build) = build else {
        return (Outcome::Incomplete, "the run names no build".into());
    };
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by(|a, b| a.at().cmp(&b.at()).then_with(|| a.place().cmp(&b.place())));
    if let Some((a, b)) = unordered_tie(&sorted) {
        return (
            Outcome::Incomplete,
            format!(
                "{} line {} and {} line {} share host time {}: their order is not recorded",
                a.place().0,
                a.place().1,
                b.place().0,
                b.place().1,
                a.at()
            ),
        );
    }
    let mut want = build;
    let mut hellos = 0_usize;
    for e in sorted {
        match e {
            Event::Flashed { build, .. } => want = build,
            Event::Hello {
                at, line, source, ..
            } => {
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
            index: usize::try_from(at).unwrap(),
        }
    }

    fn flashed(at: i64, build: &str, source: &str, index: usize) -> Event {
        Event::Flashed {
            at,
            build: build.into(),
            source: source.into(),
            index,
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
            index: 1,
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
        let flashed = flashed(5, "new", "run.jsonl", 3);
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

    /// A flash and a hello at one host time: one file's order settles it,
    /// two files' order is not known. Every arrangement of a flash between
    /// two hellos of the old and the new build, at times that tie or not,
    /// in one file or two, against the order an oracle gives.
    #[test]
    fn a_tie_is_settled_by_recorded_order_only() {
        for flash_at in 0..=4 {
            for flash_index in [0, 2, 4] {
                for same_file in [true, false] {
                    let source = if same_file { "link.jsonl" } else { "run.jsonl" };
                    // Hellos at times 1 (old build) and 3 (new build).
                    let events = [
                        hello(1, "old", false),
                        flashed(flash_at, "new", source, flash_index),
                        hello(3, "new", false),
                    ];
                    let got = check(Some("old"), &events).0;
                    let tie = flash_at == 1 || flash_at == 3;
                    // Where the flash falls among the hellos, by time then
                    // by line in one file.
                    let after_old = flash_at > 1 || (flash_at == 1 && flash_index > 1);
                    let before_new = flash_at < 3 || (flash_at == 3 && flash_index < 3);
                    let want = if tie && !same_file {
                        Outcome::Incomplete
                    } else if after_old && before_new {
                        Outcome::Pass
                    } else {
                        Outcome::Fail
                    };
                    assert_eq!(
                        got, want,
                        "flash at {flash_at} line {flash_index} in {source}"
                    );
                }
            }
        }
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
