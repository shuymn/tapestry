//! The header commands a song list shows, read without loading the chart
//! ([`read_header`]).

use crate::parse::{split_channel, split_header};
use crate::{Limits, LoadError};

/// The descriptive header values of a chart, as written (trimmed), each
/// `None` when the file does not declare it. The last declaration wins, as
/// in [`load_bms`](crate::load_bms).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// `#TITLE`.
    pub title: Option<String>,
    /// `#SUBTITLE`.
    pub subtitle: Option<String>,
    /// `#ARTIST`.
    pub artist: Option<String>,
    /// `#SUBARTIST`.
    pub subartist: Option<String>,
    /// `#GENRE`.
    pub genre: Option<String>,
    /// `#PLAYLEVEL`.
    pub playlevel: Option<String>,
    /// `#DIFFICULTY`.
    pub difficulty: Option<String>,
    /// `#BPM`, the start tempo as written.
    pub bpm: Option<String>,
}

/// Reads the descriptive header commands of a chart from its file bytes,
/// decoded as [`load_bms`](crate::load_bms) decodes them.
///
/// Only top-level commands count: a command inside an `#IF` or `#SWITCH`
/// block is skipped, since which branch is read depends on a draw. No
/// `#RANDOM` is drawn, no channel line or time is read, and no value is
/// checked, so a chart the loader refuses still has a header.
///
/// # Errors
///
/// [`LoadError::FileTooLarge`] for more than [`Limits::max_file_bytes`]
/// bytes, the loader's cap.
pub fn read_header(bytes: &[u8], limits: &Limits) -> Result<Header, LoadError> {
    if bytes.len() > limits.max_file_bytes {
        return Err(LoadError::FileTooLarge { bytes: bytes.len() });
    }
    let text = crate::decode(bytes, &mut Vec::new());
    let mut header = Header::default();
    // Open `#IF` and `#SWITCH` blocks; the closers saturate, so a stray one
    // does not hide the rest of the file.
    let mut depth = 0_usize;
    for raw_line in text.lines() {
        let Some(body) = raw_line.trim().strip_prefix('#') else {
            continue;
        };
        if split_channel(body).is_some() {
            continue;
        }
        let (name, value) = split_header(body);
        let upper = name.to_ascii_uppercase();
        let field = match upper.as_str() {
            "IF" | "SWITCH" | "SETSWITCH" => {
                depth = depth.saturating_add(1);
                continue;
            }
            "ENDIF" | "ENDSW" => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ if depth > 0 => continue,
            "TITLE" => &mut header.title,
            "SUBTITLE" => &mut header.subtitle,
            "ARTIST" => &mut header.artist,
            "SUBARTIST" => &mut header.subartist,
            "GENRE" => &mut header.genre,
            "PLAYLEVEL" => &mut header.playlevel,
            "DIFFICULTY" => &mut header.difficulty,
            "BPM" => &mut header.bpm,
            _ => continue,
        };
        *field = Some(value.to_owned());
    }
    Ok(header)
}
