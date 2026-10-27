//! The command line, parsed into a [`Command`] before anything runs.
//!
//! Parsing opens no file, device, window or log: a [`UsageError`] is found
//! before any of them, and `main` prints it and exits 2. Development
//! options exist only with the `dev-tools` feature, so a production build
//! reports them as it reports any other unknown argument.

use std::ffi::OsString;
use std::path::PathBuf;

use lexopt::{Arg, Parser};

pub const USAGE: &str =
    "usage: tapestry play [--auto] <chart.bms> | tapestry keys | tapestry --version";

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    /// `--version`.
    Version,
    /// `keys`: the key config.
    Keys(Keys),
    /// `play <chart>`.
    Play(Play),
    /// `--dev-tools-probe`.
    #[cfg(feature = "dev-tools")]
    DevToolsProbe,
    /// `measure play [--perfect] <chart>`.
    #[cfg(feature = "dev-tools")]
    Measure { chart: PathBuf, perfect: bool },
}

/// `play`'s arguments.
#[derive(Debug)]
pub struct Play {
    pub chart: PathBuf,
    /// How the session's input is received: the devices, autoplay
    /// (`--auto`), or an input log (`--dev-scripted-input`).
    pub input: crate::PlayInput,
    #[cfg(feature = "dev-tools")]
    pub dev: crate::dev::Options,
}

/// `keys`' arguments.
#[derive(Debug)]
pub struct Keys {
    /// `--dev-scripted-keys <steps>`.
    #[cfg(feature = "dev-tools")]
    pub script: Option<crate::dev::KeyScript>,
}

/// A usage error: printed as is, exit 2.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    /// An argument no command takes there.
    #[error("unknown argument: {0}")]
    Unknown(String),
    /// A usage line, a missing chart, a missing or bad option value.
    #[error("{0}")]
    Usage(String),
}

impl From<lexopt::Error> for UsageError {
    fn from(e: lexopt::Error) -> Self {
        use lexopt::Error as E;
        // Exhaustive on purpose: lexopt's `Error` is not `non_exhaustive`,
        // so a new variant fails the build instead of being mapped blindly.
        match e {
            E::UnexpectedOption(option) => Self::Unknown(option),
            E::UnexpectedArgument(value) => Self::Unknown(value.to_string_lossy().into_owned()),
            E::UnexpectedValue { option, value } => {
                Self::Unknown(format!("{option}={}", value.to_string_lossy()))
            }
            E::MissingValue {
                option: Some(option),
            } => Self::Usage(format!("{option} needs a value")),
            e @ (E::MissingValue { option: None }
            | E::ParsingFailed { .. }
            | E::NonUnicodeValue(_)
            | E::Custom(_)) => Self::Usage(e.to_string()),
        }
    }
}

/// Parses the arguments after the program name.
///
/// # Errors
///
/// The usage error, before anything has run.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut p = Parser::from_args(args);
    let command = match p.next()? {
        None => return Err(UsageError::Usage(USAGE.to_owned())),
        Some(Arg::Long("version")) => {
            end(&mut p)?;
            Command::Version
        }
        #[cfg(feature = "dev-tools")]
        Some(Arg::Long("dev-tools-probe")) => {
            end(&mut p)?;
            Command::DevToolsProbe
        }
        Some(Arg::Value(v)) if v == "play" => Command::Play(play(&mut p)?),
        Some(Arg::Value(v)) if v == "keys" => Command::Keys(keys(&mut p)?),
        #[cfg(feature = "dev-tools")]
        Some(Arg::Value(v)) if v == "measure" => crate::dev::measure_args(&mut p)?,
        Some(other) => return Err(other.unexpected().into()),
    };
    Ok(command)
}

/// Refuses any further argument.
pub(crate) fn end(p: &mut Parser) -> Result<(), UsageError> {
    match p.next()? {
        None => Ok(()),
        Some(extra) => Err(extra.unexpected().into()),
    }
}

fn play(p: &mut Parser) -> Result<Play, UsageError> {
    let mut chart = None;
    let mut auto = false;
    #[cfg(feature = "dev-tools")]
    let mut options = crate::dev::Options::default();
    while let Some(arg) = p.next()? {
        match arg {
            Arg::Value(v) if chart.is_none() => chart = Some(PathBuf::from(v)),
            // A second `--auto` is unknown, as a second chart is.
            Arg::Long("auto") if !auto => auto = true,
            #[cfg(feature = "dev-tools")]
            Arg::Long(name) => {
                // The name borrows the parser, which the value needs.
                let name = name.to_owned();
                options.take(&name, p)?;
            }
            other => return Err(other.unexpected().into()),
        }
    }
    let Some(chart) = chart else {
        return Err(UsageError::Usage(format!(
            "play needs a chart file\n{USAGE}"
        )));
    };
    let input = if auto {
        crate::PlayInput::Autoplay
    } else {
        crate::PlayInput::Devices
    };
    // An input log replaces the devices, and refuses autoplay.
    #[cfg(feature = "dev-tools")]
    let input = options.play_input(input)?;
    Ok(Play {
        chart,
        input,
        #[cfg(feature = "dev-tools")]
        dev: options,
    })
}

fn keys(p: &mut Parser) -> Result<Keys, UsageError> {
    #[cfg(not(feature = "dev-tools"))]
    end(p)?;
    #[cfg(feature = "dev-tools")]
    let mut script = None;
    #[cfg(feature = "dev-tools")]
    while let Some(arg) = p.next()? {
        match arg {
            Arg::Long("dev-scripted-keys") => {
                let steps = p.value()?;
                script = Some(
                    crate::dev::KeyScript::parse(&steps.to_string_lossy())
                        .map_err(UsageError::Usage)?,
                );
            }
            other => return Err(other.unexpected().into()),
        }
    }
    Ok(Keys {
        #[cfg(feature = "dev-tools")]
        script,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{Command, USAGE, UsageError, parse};
    use crate::PlayInput;

    fn parsed(args: &[&str]) -> Result<Command, UsageError> {
        parse(args.iter().map(OsString::from))
    }

    fn unknown(arg: &str) -> UsageError {
        UsageError::Unknown(arg.to_owned())
    }

    /// The chart `play` parsed to.
    #[track_caller]
    fn chart(args: &[&str]) -> super::Play {
        match parsed(args) {
            Ok(Command::Play(play)) => play,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn a_flag_with_an_attached_value_is_named_whole() {
        assert_eq!(
            parsed(&["--version=1"]).unwrap_err(),
            unknown("--version=1")
        );
    }

    #[test]
    fn a_single_dash_starts_a_short_option() {
        assert_eq!(parsed(&["play", "-x"]).unwrap_err(), unknown("-x"));
        assert_eq!(
            parsed(&["play", "a.bms", "-xy"]).unwrap_err(),
            unknown("-x")
        );
        // A lone dash is still a value.
        assert_eq!(chart(&["play", "-"]).chart, Path::new("-"));
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        assert_eq!(chart(&["play", "--", "-x.bms"]).chart, Path::new("-x.bms"));
        // A lone `--` is no argument at all.
        assert_eq!(
            parsed(&["--"]).unwrap_err(),
            UsageError::Usage(USAGE.to_owned())
        );
        assert_eq!(
            parsed(&["play", "--"]).unwrap_err(),
            UsageError::Usage(format!("play needs a chart file\n{USAGE}"))
        );
    }

    #[test]
    fn an_unknown_option_with_a_value_is_named_without_it() {
        assert_eq!(parsed(&["play", "--x=v"]).unwrap_err(), unknown("--x"));
    }

    #[test]
    fn a_malformed_argument_is_named_lossily() {
        use std::os::windows::ffi::OsStringExt as _;
        let bad = OsString::from_wide(&[u16::from(b'a'), 0xD800]);
        let args = ["play".into(), "a.bms".into(), bad];
        assert_eq!(parse(args).unwrap_err(), unknown("a\u{FFFD}"));
    }

    /// `--auto` is a production option of `play`, before or after the
    /// chart; a second one is unknown, as a second chart is.
    #[test]
    fn auto_chooses_autoplay_once() {
        for args in [["play", "--auto", "a.bms"], ["play", "a.bms", "--auto"]] {
            let play = chart(&args);
            assert_eq!(play.chart, Path::new("a.bms"));
            assert!(matches!(play.input, PlayInput::Autoplay), "{args:?}");
        }
        assert!(matches!(
            chart(&["play", "a.bms"]).input,
            PlayInput::Devices
        ));
        assert_eq!(
            parsed(&["play", "--auto", "--auto", "a.bms"]).unwrap_err(),
            unknown("--auto")
        );
        assert_eq!(
            parsed(&["play", "--auto", "a.bms", "--auto"]).unwrap_err(),
            unknown("--auto")
        );
        assert_eq!(parsed(&["--auto", "play"]).unwrap_err(), unknown("--auto"));
        assert_eq!(
            parsed(&["play", "--auto=1", "a.bms"]).unwrap_err(),
            unknown("--auto=1")
        );
        assert_eq!(
            parsed(&["play", "--auto"]).unwrap_err(),
            UsageError::Usage(format!("play needs a chart file\n{USAGE}"))
        );
    }

    /// Autoplay, an input log and the perfect measurement are three ways to
    /// receive input: any two are refused together, in either order;
    /// `--dev-headless` is for generated input alone.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn autoplay_refuses_an_input_log_and_takes_headless() {
        let both =
            UsageError::Usage("--auto and --dev-scripted-input cannot be used together".to_owned());
        let perfect = UsageError::Usage(
            "--dev-perfect cannot be used with --auto or --dev-scripted-input".to_owned(),
        );
        let headless = UsageError::Usage("--dev-headless needs --auto or --dev-perfect".to_owned());
        for (args, refusal) in [
            (&["play", "--auto", "a.bms", "--dev-perfect"][..], &perfect),
            (&["play", "a.bms", "--dev-perfect", "--auto"], &perfect),
            (
                &[
                    "play",
                    "a.bms",
                    "--dev-perfect",
                    "--dev-scripted-input",
                    "l",
                ],
                &perfect,
            ),
            (
                &["play", "--auto", "a.bms", "--dev-scripted-input", "l"][..],
                &both,
            ),
            (
                &["play", "a.bms", "--dev-scripted-input", "l", "--auto"],
                &both,
            ),
            (&["play", "a.bms", "--dev-headless"], &headless),
            (
                &[
                    "play",
                    "a.bms",
                    "--dev-scripted-input",
                    "l",
                    "--dev-headless",
                ],
                &headless,
            ),
        ] {
            assert_eq!(&parsed(args).unwrap_err(), refusal, "{args:?}");
        }
        let play = chart(&["play", "--auto", "a.bms", "--dev-headless"]);
        assert!(matches!(play.input, PlayInput::Autoplay));
        assert!(play.dev.headless);
        let play = chart(&["play", "a.bms", "--dev-scripted-input", "l"]);
        assert!(matches!(play.input, PlayInput::Script(ref l) if l == Path::new("l")));
        let play = chart(&["play", "--dev-perfect", "a.bms", "--dev-headless"]);
        assert!(matches!(play.input, PlayInput::Perfect));
        assert!(play.dev.headless);
        assert!(matches!(
            chart(&["play", "a.bms", "--dev-perfect"]).input,
            PlayInput::Perfect
        ));
    }

    /// `measure play` takes one chart and `--perfect`, in either order,
    /// once each.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn measure_play_takes_perfect() {
        for (args, want) in [
            (&["measure", "play", "a.bms"][..], false),
            (&["measure", "play", "--perfect", "a.bms"], true),
            (&["measure", "play", "a.bms", "--perfect"], true),
        ] {
            match parsed(args).unwrap() {
                Command::Measure { chart, perfect } => {
                    assert_eq!((chart.as_path(), perfect), (Path::new("a.bms"), want));
                }
                other => panic!("{other:?}"),
            }
        }
        for (args, refusal) in [
            (
                &["measure", "play", "--perfect", "a.bms", "--perfect"][..],
                unknown("--perfect"),
            ),
            (&["measure", "play", "a.bms", "b.bms"], unknown("b.bms")),
            (&["measure", "play", "a.bms", "--auto"], unknown("--auto")),
            (
                &["measure", "play", "--perfect"],
                UsageError::Usage(
                    "measure play needs a chart file\n\
                     usage: tapestry measure play [--perfect] <chart.bms>"
                        .to_owned(),
                ),
            ),
        ] {
            assert_eq!(parsed(args).unwrap_err(), refusal, "{args:?}");
        }
    }

    /// A production build does not know `--dev-headless`, with `--auto`
    /// or without.
    #[cfg(not(feature = "dev-tools"))]
    #[test]
    fn headless_is_unknown_in_production() {
        assert_eq!(
            parsed(&["play", "a.bms", "--auto", "--dev-headless"]).unwrap_err(),
            unknown("--dev-headless")
        );
    }

    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_development_value_may_be_attached_or_negative() {
        let play = chart(&["play", "a.bms", "--dev-adjust-at=5"]);
        assert_eq!(play.dev.adjust_at_ms, 5);
        let play = chart(&["play", "a.bms", "--dev-adjust-at", "-5"]);
        assert_eq!(play.dev.adjust_at_ms, -5);
    }

    /// Each development flag and path lands in its own option.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn development_flags_and_paths_are_taken() {
        let play = chart(&[
            "play",
            "a.bms",
            "--dev-overlay",
            "--dev-auto-retry",
            "--dev-frames",
            "d",
            "--dev-scripted-input",
            "l",
        ]);
        assert_eq!(play.chart, Path::new("a.bms"));
        assert!(play.dev.overlay);
        assert!(play.dev.auto_retry);
        assert_eq!(play.dev.frames.as_deref(), Some(Path::new("d")));
        assert_eq!(play.dev.scripted_input.as_deref(), Some(Path::new("l")));
    }
}
