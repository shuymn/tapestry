//! Text to headers and raw objects, with the text limits of D14.

use std::collections::{BTreeMap, BTreeSet};

use bms_model::{JudgeRank, Lane, LnMode};
use num_traits::Signed;

use crate::decimal::{Rational, parse_base36, parse_decimal, parse_hex};
use crate::{Limits, LoadError, Warning, WarningKind};

/// What an object does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// Channel 01: a BGM sound.
    Bgm(u16),
    /// Channel 03: an integer BPM.
    BpmHex(u16),
    /// Channel 08: a `#BPMxx` reference.
    BpmRef(u16),
    /// Channel 09: a `#STOPxx` reference.
    StopRef(u16),
    /// Channels 11-19: a note, or an `#LNOBJ` end.
    Note(Lane, u16),
    /// Channels 51-59: a long-note start or end (`#LNTYPE 1`).
    Long(Lane, u16),
    /// Channels D1-D9: a mine; the id is its damage.
    Mine(Lane, u16),
}

/// An object at `index / count` of measure `measure`.
#[derive(Clone, Copy, Debug)]
pub struct RawObject {
    pub measure: u16,
    pub index: u32,
    pub count: u32,
    pub kind: ObjectKind,
    pub line: usize,
}

/// A value and the line that defined it.
#[derive(Clone, Debug)]
pub struct Defined<T> {
    pub value: T,
    pub line: usize,
}

/// Everything read from the text.
#[derive(Debug, Default)]
pub struct Source {
    pub title: String,
    pub bpm: Option<Defined<Rational>>,
    pub bpm_defs: BTreeMap<u16, Rational>,
    pub stop_defs: BTreeMap<u16, Rational>,
    pub wavs: BTreeMap<u16, Defined<String>>,
    pub rank: Option<JudgeRank>,
    pub total_milli: Option<i64>,
    pub ln_mode: Option<LnMode>,
    /// Bounded by the 1295 ids, so lookups stay cheap.
    pub lnobj: BTreeSet<u16>,
    /// `#xxx02` values, by measure.
    pub measure_lengths: BTreeMap<u16, Defined<Rational>>,
    /// In file order.
    pub objects: Vec<RawObject>,
}

/// Descriptive headers read and ignored without a warning.
const IGNORED_HEADERS: &[&str] = &[
    "ARTIST",
    "SUBARTIST",
    "SUBTITLE",
    "GENRE",
    "PLAYLEVEL",
    "DIFFICULTY",
    "STAGEFILE",
    "BANNER",
    "BACKBMP",
    "VOLWAV",
    "COMMENT",
    "PREVIEW",
];

/// Branching directives: `#RANDOM` and friends come with PR3; `#SWITCH` and
/// friends are not supported.
const UNSUPPORTED_HEADERS: &[&str] = &[
    "RANDOM",
    "SETRANDOM",
    "IF",
    "ELSEIF",
    "ELSE",
    "ENDIF",
    "ENDRANDOM",
    "SWITCH",
    "SETSWITCH",
    "CASE",
    "SKIP",
    "DEF",
    "ENDSW",
];

/// Channels whose objects are ignored without a warning (BGA, stage 3).
const SILENT_CHANNELS: &[&str] = &["04", "06", "07"];

pub fn parse(
    text: &str,
    limits: &Limits,
    warnings: &mut Vec<Warning>,
) -> Result<Source, LoadError> {
    let mut source = Source::default();
    let mut objects = 0_usize;
    for (i, raw_line) in text.lines().enumerate() {
        let line = i + 1;
        if line > limits.max_lines {
            return Err(LoadError::TooManyLines { line });
        }
        let Some(body) = raw_line.trim().strip_prefix('#') else {
            continue;
        };
        if let Some((measure, channel, data)) = split_channel(body) {
            let measure = u16::try_from(measure)
                .ok()
                .filter(|&m| m <= limits.max_measure)
                .ok_or(LoadError::MeasureOutOfRange { line, measure })?;
            parse_channel(
                &mut source,
                limits,
                &mut objects,
                (line, measure, channel),
                data.trim(),
                warnings,
            )?;
        } else {
            parse_header(&mut source, line, body, warnings)?;
        }
    }
    Ok(source)
}

/// Splits `mmmcc:data`. The measure takes three or more digits, so an
/// oversized measure is refused rather than read as a header; it saturates
/// at `u32::MAX`.
fn split_channel(body: &str) -> Option<(u32, [u8; 2], &str)> {
    let (head, data) = body.split_once(':')?;
    let head = head.trim_end().as_bytes();
    let (digits, channel) = head.split_last_chunk::<2>()?;
    if digits.len() < 3
        || !digits.iter().all(u8::is_ascii_digit)
        || !channel.iter().all(u8::is_ascii_alphanumeric)
    {
        return None;
    }
    let measure = digits.iter().fold(0_u32, |m, d| {
        m.saturating_mul(10).saturating_add(u32::from(d - b'0'))
    });
    Some((measure, channel.map(|c| c.to_ascii_uppercase()), data))
}

/// The lane of a 1P channel digit (`1`-`9`), or `None` for the free zone
/// and unused digits.
fn lane_of(digit: u8) -> Option<Lane> {
    let index = match digit {
        b'6' => 0,
        b'1'..=b'5' => digit - b'0',
        b'8' | b'9' => digit - b'2',
        _ => return None,
    };
    Lane::new(index)
}

fn parse_channel(
    source: &mut Source,
    limits: &Limits,
    objects: &mut usize,
    (line, measure, channel): (usize, u16, [u8; 2]),
    data: &str,
    warnings: &mut Vec<Warning>,
) -> Result<(), LoadError> {
    let name = String::from_utf8_lossy(&channel).into_owned();
    if channel == *b"02" {
        let length = parse_decimal(data).map_err(|e| LoadError::InvalidValue {
            line,
            what: "measure length",
            reason: e.reason(),
        })?;
        if !length.is_positive() {
            return Err(LoadError::InvalidValue {
                line,
                what: "measure length",
                reason: "not positive",
            });
        }
        source.measure_lengths.insert(
            measure,
            Defined {
                value: length,
                line,
            },
        );
        return Ok(());
    }
    let bytes = data.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        warnings.push(Warning {
            line: Some(line),
            kind: WarningKind::OddObjectData,
        });
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let count = u32::try_from(pairs.len()).map_err(|_| LoadError::TooManyObjects { line })?;
    let mut warned_channel = false;
    for (index, pair) in (0..).zip(pairs) {
        if pair == b"00" {
            continue;
        }
        *objects += 1;
        if *objects > limits.max_objects {
            return Err(LoadError::TooManyObjects { line });
        }
        let id = if channel == *b"03" {
            parse_hex(pair.as_slice())
        } else {
            parse_base36(pair.as_slice())
        };
        let Some(id) = id else {
            warnings.push(Warning {
                line: Some(line),
                kind: WarningKind::InvalidObject,
            });
            continue;
        };
        let kind = match channel {
            [b'0', b'1'] => Some(ObjectKind::Bgm(id)),
            [b'0', b'3'] => Some(ObjectKind::BpmHex(id)),
            [b'0', b'8'] => Some(ObjectKind::BpmRef(id)),
            [b'0', b'9'] => Some(ObjectKind::StopRef(id)),
            [b'1', d] => lane_of(d).map(|lane| ObjectKind::Note(lane, id)),
            [b'5', d] => lane_of(d).map(|lane| ObjectKind::Long(lane, id)),
            [b'D', d] => lane_of(d).map(|lane| ObjectKind::Mine(lane, id)),
            [b'2' | b'6' | b'E', b'1'..=b'9'] => {
                return Err(LoadError::Unsupported {
                    line,
                    what: format!("2P lane channel {name}"),
                });
            }
            _ => None,
        };
        match kind {
            Some(kind) => source.objects.push(RawObject {
                measure,
                index,
                count,
                kind,
                line,
            }),
            None => {
                if !warned_channel && !SILENT_CHANNELS.contains(&name.as_str()) {
                    warnings.push(Warning {
                        line: Some(line),
                        kind: WarningKind::IgnoredChannel(name.clone()),
                    });
                }
                warned_channel = true;
            }
        }
    }
    Ok(())
}

fn parse_header(
    source: &mut Source,
    line: usize,
    body: &str,
    warnings: &mut Vec<Warning>,
) -> Result<(), LoadError> {
    let (name, value) = body
        .split_once(|c: char| c.is_ascii_whitespace())
        .map_or((body, ""), |(n, v)| (n, v.trim()));
    let upper = name.to_ascii_uppercase();
    let defined_id = |prefix: &str| {
        upper
            .strip_prefix(prefix)
            .filter(|rest| rest.len() == 2)
            .and_then(|rest| parse_base36(rest.as_bytes()))
    };
    let invalid =
        |what: &'static str, reason: &'static str| LoadError::InvalidValue { line, what, reason };
    let positive = |what: &'static str| -> Result<Rational, LoadError> {
        let v = parse_decimal(value).map_err(|e| invalid(what, e.reason()))?;
        if v.is_positive() {
            Ok(v)
        } else {
            Err(invalid(what, "not positive"))
        }
    };
    let ignore = |warnings: &mut Vec<Warning>, what: &'static str| {
        warnings.push(Warning {
            line: Some(line),
            kind: WarningKind::IgnoredValue(what),
        });
    };
    match upper.as_str() {
        "TITLE" => source.title = value.to_owned(),
        "BPM" => {
            source.bpm = Some(Defined {
                value: positive("#BPM")?,
                line,
            });
        }
        "RANK" => match value.parse::<i32>() {
            Ok(rank) => source.rank = Some(JudgeRank::Rank(rank)),
            Err(_) => ignore(warnings, "#RANK"),
        },
        "DEFEXRANK" => match thousandths(value) {
            Some(v) => source.rank = Some(JudgeRank::DefExRankMilli(v)),
            None => ignore(warnings, "#DEFEXRANK"),
        },
        "TOTAL" => match thousandths(value) {
            Some(v) => source.total_milli = Some(v),
            None => ignore(warnings, "#TOTAL"),
        },
        "LNOBJ" => match parse_base36(value.as_bytes()).filter(|&id| id != 0) {
            Some(id) => {
                source.lnobj.insert(id);
            }
            None => return Err(invalid("#LNOBJ", "not an object id")),
        },
        "LNTYPE" => {
            if value != "1" {
                return Err(LoadError::Unsupported {
                    line,
                    what: format!("#LNTYPE {value}"),
                });
            }
        }
        "LNMODE" => {
            if value != "1" {
                return Err(LoadError::Unsupported {
                    line,
                    what: format!("#LNMODE {value}"),
                });
            }
            source.ln_mode = Some(LnMode::Ln);
        }
        "PLAYER" => {
            if value != "1" {
                warnings.push(Warning {
                    line: Some(line),
                    kind: WarningKind::PlayerNotOne,
                });
            }
        }
        _ if UNSUPPORTED_HEADERS.contains(&upper.as_str()) => {
            return Err(LoadError::Unsupported {
                line,
                what: format!("#{upper}"),
            });
        }
        _ if IGNORED_HEADERS.contains(&upper.as_str()) || defined_id("BMP").is_some() => {}
        _ => {
            if let Some(id) = defined_id("BPM") {
                source.bpm_defs.insert(id, positive("#BPMxx")?);
            } else if let Some(id) = defined_id("STOP") {
                let v = parse_decimal(value).map_err(|e| invalid("#STOPxx", e.reason()))?;
                if v.is_negative() {
                    return Err(LoadError::Unsupported {
                        line,
                        what: format!("negative #{upper}"),
                    });
                }
                source.stop_defs.insert(id, v);
            } else if let Some(id) = defined_id("WAV") {
                source.wavs.insert(
                    id,
                    Defined {
                        value: value.to_owned(),
                        line,
                    },
                );
            } else {
                warnings.push(Warning {
                    line: Some(line),
                    kind: WarningKind::UnknownHeader(upper),
                });
            }
        }
    }
    Ok(())
}

/// A non-negative decimal in thousandths, rounded down.
fn thousandths(value: &str) -> Option<i64> {
    let v = parse_decimal(value).ok().filter(|v| !v.is_negative())?;
    let milli = (v * Rational::from_integer(1_000)).floor().to_integer();
    i64::try_from(milli).ok()
}
