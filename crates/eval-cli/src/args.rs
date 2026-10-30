//! The command line, parsed before any file is opened. A [`UsageError`] is
//! printed with the usage line and exits 2.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use lexopt::{Arg, Parser};

pub const USAGE: &str =
    "usage: eval-cli --chart <chart> [--preset <file.toml>]... <record.tprl>...";

/// What to evaluate.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The chart every record is verified against.
    pub chart: PathBuf,
    /// The presets to evaluate under, in order; none means each record's
    /// own preset copy.
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

/// Parses the arguments after the program name.
///
/// # Errors
///
/// An unknown option, an option without its value, a second `--chart`, or
/// a missing chart or record.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, UsageError> {
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

    use super::{Args, parse};

    fn parsed(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(OsString::from)).map_err(|e| e.to_string())
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
