//! What a song list shows, read without loading the chart: the header
//! commands ([`read_header`]), with the chart's mode ([`read_listing`]).

use crate::parse::{objects, split_channel, split_header};
use crate::{Limits, LoadError};

/// A chart's play mode (design.md "Loading and Song Selection", the mode), by
/// its recognized objects in every branch, with no draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ChartMode {
    /// An object on keys 6 or 7, visible, invisible, long or a mine, and
    /// none of the two-player side.
    SevenKey,
    /// Neither: an authored 5-key chart, a 7-key chart that uses only keys
    /// 1-5, and a chart with no such object at all, which no file tells
    /// apart.
    #[default]
    FiveKey,
    /// An object on the two-player side, visible, invisible, long or a mine.
    TwoPlayer,
}

/// What [`read_listing`] finds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The descriptive header.
    pub header: Header,
    /// The chart's mode.
    pub mode: ChartMode,
    /// Whether it declares `#RANDOM` or `#SETRANDOM`, in any branch.
    pub random: bool,
}

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
/// `#RANDOM` is drawn, no time is computed and no value is checked, so a chart
/// the loader refuses still has a header.
///
/// # Errors
///
/// [`LoadError::FileTooLarge`] for more than [`Limits::max_file_bytes`]
/// bytes, the loader's cap.
pub fn read_header(bytes: &[u8], limits: &Limits) -> Result<Header, LoadError> {
    read_listing(bytes, limits).map(|listing| listing.header)
}

/// Reads a chart's descriptive header as [`read_header`] does, with its mode
/// from the channel lines of every branch, and whether it draws.
///
/// # Errors
///
/// [`LoadError::FileTooLarge`] for more than [`Limits::max_file_bytes`]
/// bytes, the loader's cap.
pub fn read_listing(bytes: &[u8], limits: &Limits) -> Result<Listing, LoadError> {
    if bytes.len() > limits.max_file_bytes {
        return Err(LoadError::FileTooLarge { bytes: bytes.len() });
    }
    Ok(listing(&crate::decode(bytes, &mut Vec::new())))
}

/// Whether an object on `channel` makes a chart two-player, by the object
/// rule of the source's decoder (`4x` included, which the loader ignores).
const fn two_player(channel: [u8; 2]) -> bool {
    matches!(channel, [b'2' | b'4' | b'6' | b'E', b'1'..=b'9'])
}

/// Whether an object on `channel` lies on key 6 or 7.
const fn key_6_or_7(channel: [u8; 2]) -> bool {
    matches!(channel, [b'1' | b'3' | b'5' | b'D', b'8' | b'9'])
}

/// [`read_listing`]'s one pass over the decoded text.
pub(crate) fn listing(text: &str) -> Listing {
    let mut header = Header::default();
    let (mut two, mut seven, mut random) = (false, false, false);
    // Open `#IF` and `#SWITCH` blocks; the closers saturate, so a stray one
    // does not hide the rest of the file.
    let mut depth = 0_usize;
    for raw_line in text.lines() {
        let Some(body) = raw_line.trim().strip_prefix('#') else {
            continue;
        };
        if let Some((_, channel, data)) = split_channel(body) {
            // Only what could still change the mode is decoded: a two-player
            // object settles it, and a key 6 or 7 object leaves only those.
            if !two
                && (two_player(channel) || (!seven && key_6_or_7(channel)))
                && objects(channel, data.trim()).any(|(_, id)| id.is_some())
            {
                two |= two_player(channel);
                seven |= key_6_or_7(channel);
            }
            continue;
        }
        let (name, value) = split_header(body);
        let upper = name.to_ascii_uppercase();
        random |= matches!(upper.as_str(), "RANDOM" | "SETRANDOM");
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
    let mode = if two {
        ChartMode::TwoPlayer
    } else if seven {
        ChartMode::SevenKey
    } else {
        ChartMode::FiveKey
    };
    Listing {
        header,
        mode,
        random,
    }
}
