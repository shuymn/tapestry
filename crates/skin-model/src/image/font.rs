//! A skin's font files, checked at load: their count, their paths inside
//! the skin folder, their sizes and their first bytes; then read again for
//! installation by a [`FontReader`], against one budget. They are not
//! parsed here.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use super::path::{self, PathError};
use super::{MAX_FILE_BYTES, Sheet, Total};

/// The most font files one skin may name.
pub const MAX_FONTS: usize = 16;
/// The largest font file, in bytes.
pub const MAX_FONT_BYTES: u64 = MAX_FILE_BYTES;
/// The most bytes across one skin's font files.
pub const MAX_TOTAL_FONT_BYTES: u64 = 64 << 20;

/// The first four bytes of a TrueType, OpenType or collection file.
const MAGICS: [[u8; 4]; 4] = [[0, 1, 0, 0], *b"OTTO", *b"true", *b"ttcf"];

/// A font file of the skin: a name relative to the skin folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontDef {
    pub path: String,
}

/// Why [`Sheet::with_fonts`] refused the fonts; `font` is an index.
#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("the skin names {n} fonts, more than {MAX_FONTS}")]
    TooManyFonts { n: usize },
    #[error("the sheet has no skin folder")]
    NoFolder,
    /// Fonts given to a sheet holding bitmap fonts, which share the font
    /// slots and bytes: such a sheet takes its fonts through
    /// [`SheetBuilder::admit_font`](super::builder::SheetBuilder::admit_font).
    #[error("the sheet holds bitmap fonts; its fonts are admitted with them")]
    BitmapFonts,
    #[error("font {font}'s name may leave the skin folder")]
    PathRejected { font: usize },
    #[error("font {font} does not exist")]
    Missing { font: usize },
    #[error("font {font} resolves outside the skin folder")]
    PathEscapes { font: usize },
    #[error("font {font} is not a regular file")]
    NotAFile { font: usize },
    #[error("font {font} is larger than {MAX_FONT_BYTES} bytes")]
    TooLarge { font: usize },
    #[error("the fonts up to {font} pass {MAX_TOTAL_FONT_BYTES} bytes")]
    TotalTooLarge { font: usize },
    #[error("font {font} is not a TrueType, OpenType or collection file")]
    Unsupported { font: usize },
    #[error("the sheet has no font {font}")]
    Unknown { font: usize },
    #[error("font {font} changed while it was read")]
    Changed { font: usize },
    #[error("font {font}")]
    Io {
        font: usize,
        #[source]
        source: io::Error,
    },
}

impl FontError {
    pub(crate) fn path(font: usize, e: PathError) -> Self {
        match e {
            PathError::Rejected => Self::PathRejected { font },
            PathError::Missing => Self::Missing { font },
            PathError::Escapes => Self::PathEscapes { font },
            PathError::NotAFile => Self::NotAFile { font },
            PathError::TooLarge => Self::TooLarge { font },
            PathError::Io(source) => Self::Io { font, source },
        }
    }
}

/// Font `font`'s name resolved inside the canonical skin folder `root`,
/// with its length.
pub(crate) fn resolve(root: &Path, font: usize, name: &str) -> Result<(PathBuf, u64), FontError> {
    path::resolve(root, name).map_err(|e| FontError::path(font, e))
}

/// Whether `bytes` start with a TrueType, OpenType or collection magic.
fn has_magic(bytes: &[u8]) -> bool {
    bytes
        .first_chunk::<4>()
        .is_some_and(|magic| MAGICS.contains(magic))
}

/// Whether the resolved font file starts with a TrueType, OpenType or
/// collection magic.
pub(crate) fn check_magic(font: usize, p: &Path) -> Result<(), FontError> {
    let (mut file, _) = path::open(p).map_err(|e| FontError::path(font, e))?;
    let mut magic = [0_u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(FontError::Unsupported { font });
        }
        Err(source) => return Err(FontError::Io { font, source }),
    }
    if has_magic(&magic) {
        Ok(())
    } else {
        Err(FontError::Unsupported { font })
    }
}

impl Sheet {
    /// This sheet with `fonts`, checked against its skin folder.
    ///
    /// Checks, in order: the count; a sheet holding bitmap fonts takes
    /// none ([`FontError::BitmapFonts`]); then each font's name, its path
    /// inside the folder, a regular file of at most [`MAX_FONT_BYTES`],
    /// and the running total of at most [`MAX_TOTAL_FONT_BYTES`]; then
    /// each file's first four bytes: `00 01 00 00`, `OTTO`, `true` or
    /// `ttcf`. A sheet without a folder ([`Sheet::empty`]) takes no
    /// fonts.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn with_fonts(mut self, fonts: Vec<FontDef>) -> Result<Self, FontError> {
        if fonts.len() > MAX_FONTS {
            return Err(FontError::TooManyFonts { n: fonts.len() });
        }
        if !fonts.is_empty() && self.root.as_os_str().is_empty() {
            return Err(FontError::NoFolder);
        }
        if !fonts.is_empty() && !self.fnts.is_empty() {
            return Err(FontError::BitmapFonts);
        }
        let mut total = Total {
            sum: 0,
            cap: MAX_TOTAL_FONT_BYTES,
        };
        let mut paths = Vec::with_capacity(fonts.len());
        for (font, def) in fonts.iter().enumerate() {
            let (p, len) = resolve(&self.root, font, &def.path)?;
            if !total.add(len) {
                return Err(FontError::TotalTooLarge { font });
            }
            paths.push(p);
        }
        for (font, p) in paths.iter().enumerate() {
            check_magic(font, p)?;
        }
        self.fonts = fonts.into_iter().map(|f| f.path).collect();
        Ok(self)
    }

    /// The font files [`with_fonts`](Self::with_fonts) took.
    #[must_use]
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }
}

/// Reads a sheet's font files for installation, against one budget of
/// [`MAX_TOTAL_FONT_BYTES`] bytes actually read. Each name is resolved
/// again inside the skin folder; the opened handle must be a regular file
/// of at most [`MAX_FONT_BYTES`] and of the budget left; at most that
/// handle's length plus one byte is read; and the magic is checked on the
/// bytes read. What a read takes from the budget stays taken, a failed
/// read's included.
#[derive(Debug)]
pub struct FontReader<'a> {
    sheet: &'a Sheet,
    remaining: u64,
}

impl<'a> FontReader<'a> {
    /// A reader of `sheet`'s fonts with the whole budget.
    #[must_use]
    pub fn new(sheet: &'a Sheet) -> Self {
        Self {
            sheet,
            remaining: MAX_TOTAL_FONT_BYTES,
        }
    }

    /// The budget's bytes not yet read.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Font `font`'s bytes, read whole.
    ///
    /// # Errors
    ///
    /// [`FontError::Unknown`] for an index past the sheet's fonts; the
    /// path checks' errors; [`FontError::TooLarge`] past
    /// [`MAX_FONT_BYTES`] and [`FontError::TotalTooLarge`] past the budget
    /// left, both by the opened handle's length; [`FontError::Changed`]
    /// when another number of bytes than that length was read; and
    /// [`FontError::Unsupported`] for bytes without a font magic.
    pub fn read(&mut self, font: u32) -> Result<Vec<u8>, FontError> {
        let index = usize::try_from(font).unwrap_or(usize::MAX);
        let name = self
            .sheet
            .fonts
            .get(index)
            .ok_or(FontError::Unknown { font: index })?;
        let (canonical, _) = resolve(&self.sheet.root, index, name)?;
        let (file, len) = path::open(&canonical).map_err(|e| FontError::path(index, e))?;
        if len > MAX_FONT_BYTES {
            return Err(FontError::TooLarge { font: index });
        }
        if len > self.remaining {
            return Err(FontError::TotalTooLarge { font: index });
        }
        let (taken, read) = read_limited(file, len, index);
        self.remaining = self.remaining.saturating_sub(taken);
        let bytes = read?;
        if !has_magic(&bytes) {
            return Err(FontError::Unsupported { font: index });
        }
        Ok(bytes)
    }
}

/// At most `len + 1` bytes of `reader`, into a buffer reserved for `len`,
/// with the bytes read: `len` bytes exactly, or [`FontError::Changed`]
/// when the source holds another number.
fn read_limited(reader: impl io::Read, len: u64, font: usize) -> (u64, Result<Vec<u8>, FontError>) {
    let mut bytes = Vec::new();
    let reserved = usize::try_from(len)
        .ok()
        .is_some_and(|n| bytes.try_reserve_exact(n).is_ok());
    if !reserved {
        return (0, Err(FontError::TooLarge { font }));
    }
    let read = reader.take(len.saturating_add(1)).read_to_end(&mut bytes);
    let n = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let out = match read {
        Err(source) => Err(FontError::Io { font, source }),
        Ok(_) if n != len => Err(FontError::Changed { font }),
        Ok(_) => Ok(bytes),
    };
    (n, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_length_than_the_handle_s_is_a_change() {
        let grown = [0_u8, 1, 0, 0, 9, 9];
        let (n, r) = read_limited(&grown[..], 4, 3);
        assert_eq!(n, 5, "one byte past the length");
        assert!(matches!(r, Err(FontError::Changed { font: 3 })), "{r:?}");
        let (n, r) = read_limited(&grown[..3], 4, 3);
        assert_eq!(n, 3);
        assert!(matches!(r, Err(FontError::Changed { font: 3 })), "{r:?}");
        let (n, r) = read_limited(&grown[..4], 4, 3);
        assert_eq!((n, r.unwrap()), (4, vec![0, 1, 0, 0]));
    }
}
