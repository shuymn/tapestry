//! The command line, parsed into a [`Command`] before anything runs.
//!
//! Parsing opens no file, device, window or log: a [`UsageError`] is found
//! before any of them, and `main` prints it and exits 2. Development
//! options exist only with the `dev-tools` feature, so a production build
//! reports them as it reports any other unknown argument.

use std::ffi::OsString;
use std::path::PathBuf;

use lexopt::{Arg, Parser};

pub const USAGE: &str = "usage: tapestry play <chart.bms> | tapestry keys | tapestry --version";

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
    /// `measure play <chart>`.
    #[cfg(feature = "dev-tools")]
    Measure { chart: PathBuf },
}

/// `play`'s arguments.
#[derive(Debug)]
pub struct Play {
    pub chart: PathBuf,
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
    #[cfg(feature = "dev-tools")]
    let mut options = crate::dev::Options::default();
    while let Some(arg) = p.next()? {
        match arg {
            Arg::Value(v) if chart.is_none() => chart = Some(PathBuf::from(v)),
            #[cfg(feature = "dev-tools")]
            Arg::Long(name) => {
                // The name borrows the parser, which the value needs.
                let name = name.to_owned();
                if !options.take(&name, p)? {
                    return Err(UsageError::Unknown(format!("--{name}")));
                }
            }
            other => return Err(other.unexpected().into()),
        }
    }
    let Some(chart) = chart else {
        return Err(UsageError::Usage(format!(
            "play needs a chart file\n{USAGE}"
        )));
    };
    Ok(Play {
        chart,
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

    use super::{Command, UsageError, parse};

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

    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_development_value_may_be_attached_or_negative() {
        let play = chart(&["play", "a.bms", "--dev-adjust-at=5"]);
        assert_eq!(play.dev.adjust_at_ms, 5);
        let play = chart(&["play", "a.bms", "--dev-adjust-at", "-5"]);
        assert_eq!(play.dev.adjust_at_ms, -5);
    }
}
