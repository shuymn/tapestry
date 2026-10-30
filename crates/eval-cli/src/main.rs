//! `eval-cli`: verifies play records against a chart and re-evaluates
//! their input under presets, without the game (design.md "Verifying
//! Without Launching the UI").
//!
//! `eval-cli --chart <chart> [--preset <file.toml>]... <record.tprl>...`
//!
//! The chart and the preset files are checked first: a file that cannot
//! be read or is too large, an invalid preset, or a chart that loads under
//! none of the records' seeds ends the run before any line.
//! Each record is then decoded, refused if its input is incomplete, and
//! verified against the chart with `replay::verify` (design.md
//! "Determinism"). It is evaluated under each `--preset` in order, or under
//! its own preset copy when none is given (design.md "Extension
//! Boundaries"), and one line per record and preset goes to stdout:
//!
//! `<record> | <preset> | <label> | perfect <n> great <n> good <n> bad <n>
//! poor <n> | ex <n> | bp <n> | max combo <n> | <gauge> <value>% <cleared>...`
//!
//! `<preset>` is `preset <file>` or `recorded preset`. The label is
//! `as played` or `what-if`, followed by the coverage when the input does
//! not cover all of it. A refused record's lines end in
//! `refused: <reason>` in place of the label and totals, one per preset.
//! The output names only the paths given and nothing that varies between
//! runs.
//!
//! Exit codes: 0 when every record was evaluated, 1 when any was refused
//! or the chart or a preset file cannot be used, and 2 for bad arguments,
//! with the usage line.

mod args;

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::ExitCode;

use bms_model::Chart;
use bms_parser::LoadOptions;
use replay::verify::{Coverage, ReEvaluation, evaluate_record, load_recorded_chart};
use replay::{DecodeLimits, Incomplete, PlayRecord};
use rules::{GaugeValue, RulePreset};
use rules_toml::MAX_PRESET_FILE_BYTES;

fn main() -> ExitCode {
    let args = match args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("eval-cli: {e}\n{}", args::USAGE);
            return ExitCode::from(2);
        }
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let result = run(&args, &mut out).and_then(|all| {
        out.flush().map_err(|e| format!("stdout: {e}"))?;
        Ok(all)
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("eval-cli: {e}");
            ExitCode::from(1)
        }
    }
}

/// What a record is evaluated under, and its label.
struct Choice {
    /// `preset <file>` or `recorded preset`.
    label: String,
    /// The preset file's preset, or `None` for the record's own copy.
    preset: Option<RulePreset>,
}

/// Writes every record's lines to `out`. Returns whether every record was
/// evaluated.
///
/// # Errors
///
/// A chart or preset file that cannot be used, before any line is
/// written, or a failed write.
fn run(args: &args::Args, out: &mut impl io::Write) -> Result<bool, String> {
    let choices = if args.presets.is_empty() {
        vec![Choice {
            label: "recorded preset".to_owned(),
            preset: None,
        }]
    } else {
        args.presets
            .iter()
            .map(|path| {
                let preset = read_preset(path).map_err(|e| format!("{}: {e}", shown(path)))?;
                Ok(Choice {
                    label: format!("preset {}", shown(path)),
                    preset: Some(preset),
                })
            })
            .collect::<Result<_, String>>()?
    };
    let chart_bytes =
        read_chart(&args.chart).map_err(|e| format!("{}: {e}", shown(&args.chart)))?;
    let chart_dir = chart_dir(&args.chart);
    check_chart_loads(args, &chart_bytes, chart_dir)
        .map_err(|e| format!("{}: {e}", shown(&args.chart)))?;
    let write = |e: io::Error| format!("stdout: {e}");
    let mut all = true;
    for path in &args.records {
        let name = shown(path);
        let verified = verified(path, &chart_bytes, chart_dir);
        for choice in &choices {
            let line = verified
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|(record, chart)| evaluated(record, chart, choice.preset.as_ref()));
            match line {
                Ok(line) => writeln!(out, "{name} | {} | {line}", choice.label),
                Err(reason) => {
                    all = false;
                    writeln!(out, "{name} | {} | refused: {reason}", choice.label)
                }
            }
            .map_err(write)?;
        }
    }
    Ok(all)
}

/// The folder the chart's sounds resolve in. A bare file name's parent is
/// the empty path, which names no folder.
fn chart_dir(chart: &Path) -> &Path {
    chart
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// The chart file's bytes, read one byte past the loader's limit at most,
/// as the game reads it.
///
/// # Errors
///
/// The file cannot be read, or is larger than the limit.
fn read_chart(path: &Path) -> Result<Vec<u8>, String> {
    let limit = LoadOptions::default().limits.max_file_bytes;
    let bytes = read_bounded(path, limit)?;
    if bytes.len() > limit {
        return Err(format!(
            "the file is larger than {limit} bytes, the largest chart accepted"
        ));
    }
    Ok(bytes)
}

/// Checks that the chart loads under the recorded seed of some record that
/// decodes and is complete, or under the default seed when there is none.
/// Whether a chart loads can depend on the branches its `#RANDOM` draws, so
/// a chart refused under every seed the records draw cannot verify any of
/// them, while a record whose own seed alone fails is refused on its own.
///
/// The records are decoded here one at a time, keeping only their seeds,
/// and again when their lines are written, so one decoded record is held
/// at a time.
///
/// # Errors
///
/// The load error under the first seed, when every seed fails.
fn check_chart_loads(args: &args::Args, bytes: &[u8], dir: &Path) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut seeds: Vec<u64> = args
        .records
        .iter()
        .filter_map(|path| decoded(path).ok())
        .map(|record| record.chart.random_seed)
        .filter(|&seed| seen.insert(seed))
        .collect();
    if seeds.is_empty() {
        seeds.push(LoadOptions::default().seed);
    }
    let mut first = None;
    for seed in seeds {
        let opts = LoadOptions {
            seed,
            ..LoadOptions::default()
        };
        match bms_parser::load_bms(bytes, dir, &opts) {
            Ok(_) => return Ok(()),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.map_or_else(String::new, |e| chain(&e)))
}

/// Reads, decodes and verifies the record at `path` against the chart,
/// returning it with its chart.
///
/// # Errors
///
/// Why the record is refused: see [`decoded`], or verification refuses it.
fn verified(
    path: &Path,
    chart_bytes: &[u8],
    chart_dir: &Path,
) -> Result<(PlayRecord, Chart), String> {
    let record = decoded(path)?;
    let chart = load_recorded_chart(&record, chart_bytes, chart_dir).map_err(|e| chain(&e))?;
    Ok((record, chart))
}

/// Reads and decodes the record at `path`.
///
/// # Errors
///
/// Why the record is refused: it cannot be read or decoded, or its input is
/// incomplete. Incompleteness is checked before verification, so an
/// incomplete record is refused as incomplete whatever verification would
/// say, without loading the chart.
fn decoded(path: &Path) -> Result<PlayRecord, String> {
    let limits = DecodeLimits::STANDARD;
    // The limits the game saves records under. The file is untrusted, so
    // it is read one byte past the largest file they accept at most, as the
    // player's input log reader reads it.
    let bytes = read_bounded(path, limits.max_compressed)?;
    if bytes.len() > limits.max_compressed {
        return Err(format!(
            "the file is larger than {} bytes, the largest record accepted",
            limits.max_compressed
        ));
    }
    let record = replay::decode(&bytes, &limits).map_err(|e| chain(&e))?;
    if let Some(why) = incomplete(record.status.incomplete) {
        return Err(format!("the record's input is incomplete ({why})"));
    }
    Ok(record)
}

/// The record evaluated under `preset`, or else under its own preset copy,
/// as a line's label and totals.
///
/// # Errors
///
/// Why the evaluation is refused.
fn evaluated(
    record: &PlayRecord,
    chart: &Chart,
    preset: Option<&RulePreset>,
) -> Result<String, String> {
    let r = evaluate_record(record, chart, preset).map_err(|e| chain(&e))?;
    // The gauges are named by the preset the evaluation ran under; the
    // record's copy decoded in `evaluate_record`, so this does not fail.
    let preset = match preset {
        Some(p) => Cow::Borrowed(p),
        None => Cow::Owned(
            RulePreset::from_canonical_bytes(&record.preset.canonical_bytes)
                .map_err(|e| chain(&e))?,
        ),
    };
    Ok(result_line(&r, &preset))
}

/// The reasons the input is incomplete, if any.
fn incomplete(i: Incomplete) -> Option<String> {
    let why: Vec<&str> = [
        (i.overflow, "overflow"),
        (i.source_limit, "source limit"),
        (i.suspend, "suspend"),
        (i.input_failed, "input failed"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    (!why.is_empty()).then(|| why.join(", "))
}

/// A re-evaluation's label and totals, the gauges named by `preset`, the
/// preset it ran under.
fn result_line(r: &ReEvaluation, preset: &RulePreset) -> String {
    let mut line = if r.as_played { "as played" } else { "what-if" }.to_owned();
    match r.coverage {
        Coverage::Recorded => {}
        Coverage::NoInputAfter(t) => {
            let _ = write!(line, ", no input after {}", seconds(t.0));
        }
        Coverage::EndedMidway(t) => {
            let _ = write!(line, ", ended midway at {}", seconds(t.0));
        }
    }
    let t = &r.evaluation.tally;
    let c = &t.counts;
    let _ = write!(
        line,
        " | perfect {} great {} good {} bad {} poor {} | ex {} | bp {} | max combo {}",
        c.perfect, c.great, c.good, c.bad, c.poor, t.ex_score, t.bp, t.max_combo
    );
    for (g, rule) in r.evaluation.gauges.iter().zip(&preset.gauges) {
        let _ = write!(
            line,
            " | {} {} {}",
            escaped(&rule.id),
            percent(g.value),
            if g.cleared { "cleared" } else { "not cleared" }
        );
    }
    line
}

/// `us` as seconds to the millisecond, truncated: `4.180 s`.
fn seconds(us: i64) -> String {
    let sign = if us < 0 { "-" } else { "" };
    let a = us.unsigned_abs();
    format!("{sign}{}.{:03} s", a / 1_000_000, a % 1_000_000 / 1_000)
}

/// A gauge value to a thousandth of a percent, truncated: `80.000%`.
fn percent(v: GaugeValue) -> String {
    let sign = if v.0 < 0 { "-" } else { "" };
    let a = v.0.unsigned_abs();
    let unit = GaugeValue::PERCENT.unsigned_abs();
    format!("{sign}{}.{:03}%", a / unit, a % unit / (unit / 1_000))
}

/// The preset file at `path`, read and validated as the game reads it,
/// within [`MAX_PRESET_FILE_BYTES`].
fn read_preset(path: &Path) -> Result<RulePreset, String> {
    let bytes = read_bounded(path, MAX_PRESET_FILE_BYTES)?;
    if bytes.len() > MAX_PRESET_FILE_BYTES {
        return Err(format!(
            "the file is larger than {MAX_PRESET_FILE_BYTES} bytes, the largest preset file \
             accepted"
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".to_owned())?;
    rules_toml::load_preset_toml(&text).map_err(|e| chain(&e))
}

/// Everything in `path` up to one byte past `limit`; a larger file comes
/// back one byte longer than `limit`, for the caller to refuse.
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limit.saturating_add(1)).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// The error and its sources, joined with `: ` (design.md "Errors").
fn chain(e: &(dyn Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let _ = write!(s, ": {c}");
        cause = c.source();
    }
    escaped(&s)
}

/// A path as given, on one line.
fn shown(path: &Path) -> String {
    escaped(&path.display().to_string())
}

/// `s` with its control characters escaped, so that each output line stays
/// one line.
fn escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use rules::GaugeValue;

    use super::{escaped, percent, seconds};

    #[test]
    fn times_and_gauges_truncate_to_their_last_digit() {
        assert_eq!(seconds(4_180_999), "4.180 s");
        assert_eq!(seconds(-1_500), "-0.001 s");
        assert_eq!(percent(GaugeValue::FULL), "100.000%");
        assert_eq!(
            percent(GaugeValue(80 * GaugeValue::PERCENT + 1_299)),
            "80.012%"
        );
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(escaped("a\nb\tc\\d"), "a\\nb\\tc\\d");
    }
}
