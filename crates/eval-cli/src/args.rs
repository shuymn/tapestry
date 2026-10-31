//! The command line, parsed before any file is opened. A [`UsageError`] is
//! printed with the usage line and exits 2.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use lexopt::{Arg, Parser};

pub const USAGE: &str = "usage: eval-cli --chart <chart> [--preset <built-in|file.toml>]... \
                         <record.tprl>...\n       eval-cli preset export <built-in> <file.toml>";

/// What to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Evaluate records.
    Evaluate(Args),
    /// Write a built-in preset to a new user preset file.
    Export {
        /// The built-in's name.
        built_in: String,
        /// The file to write.
        path: PathBuf,
    },
}

/// What to evaluate.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The chart every record is verified against.
    pub chart: PathBuf,
    /// The presets to evaluate under, in order, each a built-in's name or
    /// a user preset file; none means each record's own preset copy.
    pub presets: Vec<PathBuf>,
    /// The records, in order.
    pub records: Vec<PathBuf>,
}

/// Why the arguments are refused.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<lexopt::Error> for UsageError {
    fn from(e: lexopt::Error) -> Self {
        Self(e.to_string())
    }
}

/// Parses the arguments after the program name: `preset export` first
/// exports, anything else evaluates.
///
/// # Errors
///
/// An unknown option, an option without its value, a second `--chart`, a
/// missing chart or record, or an export without exactly its two values.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.first().is_some_and(|a| a == "preset") {
        return match &args[1..] {
            [export, built_in, path] if export == "export" => Ok(Command::Export {
                built_in: built_in.to_string_lossy().into_owned(),
                path: PathBuf::from(path),
            }),
            _ => Err(UsageError(
                "`preset` takes `export <built-in> <file.toml>`".to_owned(),
            )),
        };
    }
    parse_evaluate(args).map(Command::Evaluate)
}

/// Parses the arguments of an evaluation.
fn parse_evaluate(args: Vec<OsString>) -> Result<Args, UsageError> {
    let mut p = Parser::from_args(args);
    let mut chart = None;
    let mut presets = Vec::new();
    let mut records = Vec::new();
    while let Some(arg) = p.next()? {
        match arg {
            Arg::Long("chart") if chart.is_none() => chart = Some(PathBuf::from(p.value()?)),
            Arg::Long("chart") => return Err(UsageError("--chart is given twice".to_owned())),
            Arg::Long("preset") => presets.push(PathBuf::from(p.value()?)),
            Arg::Value(v) => records.push(PathBuf::from(v)),
            other => return Err(other.unexpected().into()),
        }
    }
    let Some(chart) = chart else {
        return Err(UsageError("--chart is missing".to_owned()));
    };
    if records.is_empty() {
        return Err(UsageError("no record is given".to_owned()));
    }
    Ok(Args {
        chart,
        presets,
        records,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Args, Command, parse};

    fn command(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from)).map_err(|e| e.to_string())
    }

    fn parsed(args: &[&str]) -> Result<Args, String> {
        match command(args)? {
            Command::Evaluate(args) => Ok(args),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn preset_export_takes_a_built_in_and_a_path() {
        assert_eq!(
            command(&["preset", "export", "beatoraja-style", "my-rules.toml"]),
            Ok(Command::Export {
                built_in: "beatoraja-style".to_owned(),
                path: PathBuf::from("my-rules.toml"),
            })
        );
        for args in [
            &["preset"][..],
            &["preset", "export", "tapestry"],
            &["preset", "export", "tapestry", "a.toml", "b.toml"],
            &["preset", "import", "tapestry", "a.toml"],
        ] {
            assert!(command(args).is_err(), "{args:?}");
        }
        // Anywhere but first, `preset` is a record.
        let args = parsed(&["--chart", "c.bms", "preset"]).unwrap();
        assert_eq!(args.records, [PathBuf::from("preset")]);
    }

    #[test]
    fn presets_and_records_keep_their_order() {
        let args = parsed(&["a.tprl", "--preset", "x.toml", "--chart", "c.bms", "b.tprl"]);
        assert_eq!(
            args,
            Ok(Args {
                chart: PathBuf::from("c.bms"),
                presets: vec![PathBuf::from("x.toml")],
                records: vec![PathBuf::from("a.tprl"), PathBuf::from("b.tprl")],
            })
        );
        let after_dashes = parsed(&["--chart", "c.bms", "--", "--preset"]).unwrap();
        assert_eq!(after_dashes.records, [PathBuf::from("--preset")]);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for args in [
            &[][..],
            &["a.tprl"],
            &["--chart", "c.bms"],
            &["--chart"],
            &["--chart", "c.bms", "--chart", "d.bms", "a.tprl"],
            &["--chart", "c.bms", "a.tprl", "--preset"],
            &["--chart", "c.bms", "--seed", "1", "a.tprl"],
            &["--chart", "c.bms", "-p", "a.tprl"],
        ] {
            assert!(parsed(args).is_err(), "{args:?}");
        }
    }
}
