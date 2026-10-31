//! A skin's font files, checked at load: their count, their paths inside
//! the skin folder, their sizes and their first bytes. They are not
//! parsed here.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::io::{self, Read as _};

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
    #[error("font {font}")]
    Io {
        font: usize,
        #[source]
        source: io::Error,
    },
}

impl FontError {
    fn path(font: usize, e: PathError) -> Self {
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

impl Sheet {
    /// This sheet with `fonts`, checked against its skin folder.
    ///
    /// Checks, in order: the count; then each font's name, its path
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
        let mut total = Total {
            sum: 0,
            cap: MAX_TOTAL_FONT_BYTES,
        };
        let mut paths = Vec::with_capacity(fonts.len());
        for (font, def) in fonts.iter().enumerate() {
            let (p, len) =
                path::resolve(&self.root, &def.path).map_err(|e| FontError::path(font, e))?;
            if !total.add(len) {
                return Err(FontError::TotalTooLarge { font });
            }
            paths.push(p);
        }
        for (font, p) in paths.iter().enumerate() {
            let (mut file, _) = path::open(p).map_err(|e| FontError::path(font, e))?;
            let mut magic = [0_u8; 4];
            match file.read_exact(&mut magic) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(FontError::Unsupported { font });
                }
                Err(source) => return Err(FontError::Io { font, source }),
            }
            if !MAGICS.contains(&magic) {
                return Err(FontError::Unsupported { font });
            }
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
