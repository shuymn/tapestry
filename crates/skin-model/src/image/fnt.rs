//! Bitmap fonts: the BMFont text format, parsed as the source player's
//! libGDX 1.9.9 `BitmapFontData.load` reads it, under a strict grammar.
//!
//! Line 1 (`info`) gives the size and the padding; line 2 (`common`) the
//! line height, base, page size and page count, by position; then one
//! `page` line per page, the `char` lines, and an optional `kernings`
//! block. Values are read by position, as the source's tokenizer reads
//! them, and every key name read is checked. Anything the grammar does
//! not accept refuses the whole file with an [`FntError`] naming its
//! line. A glyph's offsets are kept raw (top-origin), as the file gives
//! them.
//!
//! The file is bytes: the quoted `face` value on line 1 may hold any
//! bytes (real fonts carry legacy-encoded names there) and is never
//! interpreted; every other byte must be UTF-8.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use super::MAX_PATH_BYTES;

/// The largest `.fnt` file, in bytes.
pub const MAX_FNT_BYTES: u64 = 4 << 20;
/// The most pages one `.fnt` file may name.
pub const MAX_FNT_PAGES: usize = 16;
/// The most glyphs of one file: its `char` lines (the missing-glyph
/// record included) and a synthesised space.
pub const MAX_FONT_GLYPHS: usize = 16_384;
/// The most kerning pairs one file keeps, repeated pairs included.
pub const MAX_FONT_KERNINGS: usize = 16_384;
/// The most glyphs across a sheet's `.fnt` files.
pub const MAX_SHEET_GLYPHS: usize = 65_536;
/// The most kerning pairs across a sheet's `.fnt` files.
pub const MAX_SHEET_KERNINGS: usize = 65_536;
/// The largest magnitude of a font value: a size, offset, advance,
/// padding, base or glyph rectangle edge.
pub const MAX_FNT_VALUE: i32 = 16_384;

/// The highest code point.
const MAX_CODE: i32 = 0x10_FFFF;
/// The highest code of the basic plane.
const MAX_BMP: u32 = 0xFFFF;
/// The glyphs whose height is the cap height, in order
/// (`BitmapFont$BitmapFontData` `capChars`, gdx 1.9.9 `load`:643-658).
const CAP_CHARS: &[u8] = b"MNBDCEFKAGHIJLOPQRSTUVWXYZ";
/// The glyphs the x-height is probed with, in order
/// (`BitmapFont$BitmapFontData` `xChars`, gdx 1.9.9).
const X_CHARS: &[u8] = b"xeaonsrcumvwz";
/// The glyphs whose raw y offset represents the font, in order
/// (`SkinTextBitmap.java:869` at `0.8.9`).
const ANCHORS: [u32; 13] = [
    'S' as u32, 'P' as u32, 'A' as u32, 'N' as u32, 'O' as u32, 'T' as u32, 'H' as u32, 'E' as u32,
    'R' as u32, '[' as u32, ']' as u32, 'M' as u32, '0' as u32,
];
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// One glyph of a font, as its `char` line gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FntGlyph {
    /// Its code point; 0 for the missing glyph.
    pub code: u32,
    /// Its left, top, width and height in its page, in pixels.
    pub src: [u16; 4],
    pub xoff: i16,
    /// The `yoffset` as written: the glyph's top below the line's top.
    pub raw_yoff: i16,
    pub adv: i16,
    pub page: u8,
}

/// A kerning pair: `amount` pixels between `first` and `second`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FntKerning {
    pub first: u16,
    pub second: u16,
    /// −128 to 127; a file with any other amount is refused.
    pub amount: i8,
}

/// How many glyphs and kerning pairs a parse may keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FntLimits {
    pub glyphs: usize,
    pub kernings: usize,
}

impl FntLimits {
    /// One file's caps.
    pub const FILE: Self = Self {
        glyphs: MAX_FONT_GLYPHS,
        kernings: MAX_FONT_KERNINGS,
    };
}

/// Why a `.fnt` file was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {kind}")]
pub struct FntError {
    /// The 1-based line, 0 for the whole file.
    pub line: u32,
    pub kind: FntErrorKind,
}

/// What was wrong with a `.fnt` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FntErrorKind {
    /// Larger than [`MAX_FNT_BYTES`].
    TooLarge,
    /// Bytes outside line 1's `face` value that are not UTF-8.
    Encoding,
    /// A carriage return not ending its line.
    LoneCr,
    /// Line 1 has no `padding=` of four integers: not a text font.
    Padding,
    /// Line 1 does not start with `info `.
    Info,
    /// Line 2 is missing, or its `lineHeight=` or `base=` does not parse.
    Common,
    /// A key that is not the one expected at its position.
    Key(&'static str),
    /// A value that does not parse or is out of range.
    Value(&'static str),
    /// Line 1's `size` or line 2's `scaleW` or `scaleH` is missing.
    Metadata(&'static str),
    /// `pages=` missing or outside 1 to [`MAX_FNT_PAGES`].
    Pages,
    /// A page line is missing or names no file.
    PageLine,
    /// A page's id is not its index.
    PageId,
    /// A page name, or its path, longer than [`MAX_PATH_BYTES`].
    PageName,
    /// A glyph id outside -1 to 0x10FFFF.
    Id,
    /// A glyph id given twice (-1 and 0 are the same glyph).
    DuplicateId,
    /// A glyph on a page the file does not have.
    GlyphPage,
    /// A line the grammar does not allow where it is.
    Unexpected,
    /// A declared `chars count` or `kernings count` not equal to the
    /// lines read.
    Count(&'static str),
    /// No glyph to give a synthesised space its advance.
    NoGlyphs,
    /// More glyphs than the limit.
    TooManyGlyphs,
    /// More kerning pairs than the limit.
    TooManyKernings,
    /// A glyph's rectangle outside its page; `code` is its code point.
    GlyphOutside { code: u32 },
    /// A page file that does not exist.
    PageMissing { page: u8 },
    /// A page file that cannot be read as an image.
    PageBroken { page: u8 },
    /// A page file refused by the image limits or formats.
    PageRefused { page: u8 },
}

impl FntErrorKind {
    /// Whether the source player also fails to load such a file: it then
    /// draws nothing, as a refused font does.
    #[must_use]
    pub fn upstream_fails(self) -> bool {
        matches!(
            self,
            Self::Padding
                | Self::Common
                | Self::PageLine
                | Self::PageId
                | Self::GlyphPage
                | Self::NoGlyphs
                | Self::PageMissing { .. }
                | Self::PageBroken { .. }
        )
    }
}

impl fmt::Display for FntErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "the file is larger than {MAX_FNT_BYTES} bytes"),
            Self::Encoding => f.write_str("bytes outside `face` that are not UTF-8"),
            Self::LoneCr => f.write_str("a carriage return inside a line"),
            Self::Padding => f.write_str("no `padding=` of four integers: not a text font"),
            Self::Info => f.write_str("line 1 does not start with `info`"),
            Self::Common => f.write_str("no `lineHeight=` and `base=` on line 2"),
            Self::Key(k) => write!(f, "`{k}` expected"),
            Self::Value(k) => write!(f, "`{k}` does not parse or is out of range"),
            Self::Metadata(k) => write!(f, "no `{k}=`"),
            Self::Pages => write!(f, "`pages=` missing or outside 1 to {MAX_FNT_PAGES}"),
            Self::PageLine => f.write_str("a page line is missing or names no file"),
            Self::PageId => f.write_str("a page id that is not its index"),
            Self::PageName => write!(f, "a page name longer than {MAX_PATH_BYTES} bytes"),
            Self::Id => f.write_str("a glyph id outside -1 to 0x10FFFF"),
            Self::DuplicateId => f.write_str("a glyph id given twice"),
            Self::GlyphPage => f.write_str("a glyph on a page the font does not have"),
            Self::Unexpected => f.write_str("a line not allowed here"),
            Self::Count(k) => write!(f, "`{k} count` is not the number of lines"),
            Self::NoGlyphs => f.write_str("no glyph to give a space its advance"),
            Self::TooManyGlyphs => f.write_str("too many glyphs"),
            Self::TooManyKernings => f.write_str("too many kerning pairs"),
            Self::GlyphOutside { code } => write!(f, "glyph {code} lies outside its page"),
            Self::PageMissing { page } => write!(f, "page {page} does not exist"),
            Self::PageBroken { page } => write!(f, "page {page} is not a readable image"),
            Self::PageRefused { page } => write!(f, "page {page} is refused by the image limits"),
        }
    }
}

/// An admitted `.fnt` file: its glyphs and, per page, the sheet image
/// that shows the whole page and the page's size.
#[derive(Clone, Debug)]
pub(crate) struct Fnt {
    pub(crate) file: FntFile,
    pub(crate) pages: Box<[FntPage]>,
}

/// A page of an admitted `.fnt` file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FntPage {
    pub(crate) image: u32,
    pub(crate) dims: [u16; 2],
}

/// A parsed `.fnt` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FntFile {
    size: u16,
    ascent: i32,
    rep: i32,
    /// Sorted by code, unique; the missing glyph aside.
    glyphs: Box<[FntGlyph]>,
    missing: Option<FntGlyph>,
    /// Sorted by pair, unique.
    kernings: Box<[FntKerning]>,
    /// The page names joined to the file's folder.
    pages: Box<[String]>,
}

fn err(line: usize, kind: FntErrorKind) -> FntError {
    FntError {
        line: u32::try_from(line).unwrap_or(u32::MAX),
        kind,
    }
}

/// `v` if it is within `lo..=hi`.
fn ranged(v: &str, lo: i32, hi: i32) -> Option<i32> {
    v.parse::<i32>().ok().filter(|v| (lo..=hi).contains(v))
}

/// The value of `key=value` when the token is that key.
fn keyed<'a>(token: &'a str, key: &str) -> Option<&'a str> {
    token.strip_prefix(key)?.strip_prefix('=')
}

fn narrow16(v: i32) -> i16 {
    i16::try_from(v).unwrap_or(0)
}

fn unsigned16(v: i32) -> u16 {
    u16::try_from(v).unwrap_or(0)
}

/// Line 1 without its quoted `face` value, which may hold any bytes.
fn without_face(line: &[u8]) -> Vec<u8> {
    let key = b" face=\"";
    let Some(start) = line
        .windows(key.len())
        .position(|w| w == key)
        .map(|at| at + 1)
    else {
        return line.to_vec();
    };
    let open = start + key.len() - 1;
    let Some(close) = line
        .get(open..)
        .and_then(|rest| rest.iter().position(|&b| b == b'"'))
    else {
        return line.to_vec();
    };
    let mut out = line.get(..open).unwrap_or_default().to_vec();
    out.extend_from_slice(line.get(open + close..).unwrap_or_default());
    out
}

/// The padding of line 1, as the source reads it: the text after
/// `padding=` up to the next space, four integers separated by commas
/// (gdx 1.9.9 `load`:492-498). Read on the bytes, before the encoding is
/// checked, so a binary file fails as the source fails.
fn padding(line: &[u8]) -> Option<[i32; 4]> {
    let key = b"padding=";
    let at = line.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = line.get(at..)?;
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let text = std::str::from_utf8(rest.get(..end)?).ok()?;
    let mut parts = text.split(',');
    let mut out = [0; 4];
    for v in &mut out {
        *v = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The values of a `char` line, by position.
const CHAR_KEYS: [&str; 9] = [
    "id", "x", "y", "width", "height", "xoffset", "yoffset", "xadvance", "page",
];
/// The values of a `kerning` line, by position.
const KERNING_KEYS: [&str; 3] = ["first", "second", "amount"];

/// The values of a `char` or `kerning` line split as the source's
/// `StringTokenizer(" =")` splits it, the keys checked by position;
/// further keys are ignored.
fn positional<'a, const N: usize>(
    line: &'a str,
    no: usize,
    head: &'static str,
    keys: [&'static str; N],
) -> Result<[&'a str; N], FntError> {
    let mut tokens = line.split([' ', '=']).filter(|t| !t.is_empty());
    if tokens.next() != Some(head) {
        return Err(err(no, FntErrorKind::Key(head)));
    }
    let mut out = [""; N];
    for (slot, key) in out.iter_mut().zip(keys) {
        if tokens.next() != Some(key) {
            return Err(err(no, FntErrorKind::Key(key)));
        }
        *slot = tokens.next().ok_or(err(no, FntErrorKind::Value(key)))?;
    }
    Ok(out)
}

/// The lines of a file: split at `\n`, one `\r` before it dropped.
struct Lines<'a> {
    rest: Option<&'a [u8]>,
    no: usize,
}

impl<'a> Lines<'a> {
    /// The next line and its number; `None` past the end, numbered as
    /// the line after the last.
    fn line(&mut self) -> Result<(usize, Option<&'a [u8]>), FntError> {
        let Some(rest) = self.rest else {
            return Ok((self.no + 1, None));
        };
        self.no += 1;
        let (line, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (rest.get(..i).unwrap_or_default(), rest.get(i + 1..)),
            None => (rest, None),
        };
        self.rest = next;
        // Only a carriage return before a line feed ends a line.
        let line = match next {
            Some(_) => line.strip_suffix(b"\r").unwrap_or(line),
            None => line,
        };
        if line.contains(&b'\r') {
            return Err(err(self.no, FntErrorKind::LoneCr));
        }
        Ok((self.no, Some(line)))
    }
}

fn text(no: usize, line: &[u8]) -> Result<&str, FntError> {
    std::str::from_utf8(line).map_err(|_| err(no, FntErrorKind::Encoding))
}

/// What the `char` lines give, as they are read.
struct Chars {
    glyphs: Vec<(FntGlyph, usize)>,
    missing: Option<FntGlyph>,
    /// Records read, the missing glyph included.
    count: usize,
    first_visible: Option<i32>,
    anchors: [Option<i32>; 13],
}

impl Chars {
    fn push(
        &mut self,
        no: usize,
        line: &str,
        pages: usize,
        limits: FntLimits,
    ) -> Result<(), FntError> {
        let v = positional(line, no, "char", CHAR_KEYS)?;
        let value = |i: usize, lo: i32, hi: i32| {
            let key = CHAR_KEYS.get(i).copied().unwrap_or("char");
            v.get(i)
                .and_then(|s| ranged(s, lo, hi))
                .ok_or(err(no, FntErrorKind::Value(key)))
        };
        let id = value(0, i32::MIN, i32::MAX)?;
        let [x, y, w, h] = [1, 2, 3, 4].map(|i| value(i, 0, MAX_FNT_VALUE));
        let [xoff, yoff, adv] = [5, 6, 7].map(|i| value(i, -MAX_FNT_VALUE, MAX_FNT_VALUE));
        let page = value(8, i32::MIN, i32::MAX)?;
        let (x, y, w, h) = (x?, y?, w?, h?);
        let (xoff, yoff, adv) = (xoff?, yoff?, adv?);
        if !(-1..=MAX_CODE).contains(&id) {
            return Err(err(no, FntErrorKind::Id));
        }
        let page = usize::try_from(page)
            .ok()
            .filter(|&p| p < pages)
            .and_then(|p| u8::try_from(p).ok())
            .ok_or(err(no, FntErrorKind::GlyphPage))?;
        if self.count >= limits.glyphs {
            return Err(err(no, FntErrorKind::TooManyGlyphs));
        }
        self.count += 1;
        let glyph = FntGlyph {
            code: u32::try_from(id.max(0)).unwrap_or(0),
            src: [x, y, w, h].map(unsigned16),
            xoff: narrow16(xoff),
            raw_yoff: narrow16(yoff),
            adv: narrow16(adv),
            page,
        };
        // The representative offset scans every char line
        // (`SkinTextBitmap.java:847-877`).
        if h > 0 {
            self.first_visible.get_or_insert(yoff);
            if let Some(i) = ANCHORS.iter().position(|&a| a == glyph.code)
                && let Some(slot) = self.anchors.get_mut(i)
            {
                *slot = Some(yoff);
            }
        }
        if id <= 0 {
            if self.missing.is_some() {
                return Err(err(no, FntErrorKind::DuplicateId));
            }
            self.missing = Some(glyph);
        } else {
            self.glyphs.push((glyph, no));
        }
        Ok(())
    }

    /// The glyphs sorted by code; a repeated code refuses.
    fn sorted(&mut self) -> Result<(), FntError> {
        // Line numbers are unique, so the order is total.
        self.glyphs.sort_unstable_by_key(|(g, no)| (g.code, *no));
        if let Some(pair) = self.glyphs.windows(2).find(|p| match p {
            [(a, _), (b, _)] => a.code == b.code,
            _ => false,
        }) && let Some(&(_, no)) = pair.get(1)
        {
            return Err(err(no, FntErrorKind::DuplicateId));
        }
        Ok(())
    }

    fn get(&self, code: u32) -> Option<&FntGlyph> {
        self.glyphs
            .binary_search_by_key(&code, |(g, _)| g.code)
            .ok()
            .and_then(|i| self.glyphs.get(i))
            .map(|(g, _)| g)
    }
}

impl FntFile {
    /// Parses a `.fnt` file whose page names are relative to the folder
    /// `dir` (a skin-relative name, `""` for the skin folder).
    ///
    /// Refuses more than [`MAX_FNT_BYTES`] first; each glyph and kerning
    /// pair is counted against `limits` before it is kept; a page name's
    /// length, and its length joined to `dir`, are checked against
    /// [`MAX_PATH_BYTES`] before it is copied.
    ///
    /// # Errors
    ///
    /// The first part of the file the grammar refuses.
    pub fn parse(bytes: &[u8], dir: &str, limits: FntLimits) -> Result<Self, FntError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FNT_BYTES {
            return Err(err(0, FntErrorKind::TooLarge));
        }
        let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
        let mut lines = Lines {
            rest: Some(bytes),
            no: 0,
        };

        // Line 1: the padding, then `info`, then the size. The copy
        // without `face` ends with the block.
        let ([pad_top, pad_right, pad_bottom, pad_left], size) = {
            let (no, info) = lines.line()?;
            let info = info.unwrap_or_default();
            let pad = padding(info).ok_or(err(no, FntErrorKind::Padding))?;
            let info = without_face(info);
            let info = text(no, &info)?;
            if pad.iter().any(|p| !(0..=MAX_FNT_VALUE).contains(p)) {
                return Err(err(no, FntErrorKind::Value("padding")));
            }
            if !info.starts_with("info ") {
                return Err(err(no, FntErrorKind::Info));
            }
            let size = info
                .split(' ')
                .find_map(|t| keyed(t, "size"))
                .ok_or(err(no, FntErrorKind::Metadata("size")))?;
            let size =
                ranged(size, 1, MAX_FNT_VALUE).ok_or(err(no, FntErrorKind::Value("size")))?;
            (pad, size)
        };

        // Line 2, by position (gdx 1.9.9 `load`:503-517).
        let (no, common) = lines.line()?;
        let common = text(no, common.ok_or(err(no, FntErrorKind::Common))?)?;
        let f: Vec<&str> = common.splitn(7, ' ').collect();
        let field = |i: usize, key: &str| f.get(i).and_then(|t| keyed(t, key));
        let parsed = |i: usize, key: &str| field(i, key).and_then(|v| v.parse::<i32>().ok());
        let (Some(line_height), Some(base)) = (parsed(1, "lineHeight"), parsed(2, "base")) else {
            return Err(err(no, FntErrorKind::Common));
        };
        if f.first() != Some(&"common") {
            return Err(err(no, FntErrorKind::Key("common")));
        }
        if !(1..=MAX_FNT_VALUE).contains(&line_height) {
            return Err(err(no, FntErrorKind::Value("lineHeight")));
        }
        if !(0..=MAX_FNT_VALUE).contains(&base) {
            return Err(err(no, FntErrorKind::Value("base")));
        }
        for (i, key) in [(3, "scaleW"), (4, "scaleH")] {
            let v = field(i, key).ok_or(err(no, FntErrorKind::Metadata(key)))?;
            ranged(v, 1, MAX_FNT_VALUE).ok_or(err(no, FntErrorKind::Value(key)))?;
        }
        let max_pages = i32::try_from(MAX_FNT_PAGES).unwrap_or(1);
        let pages = field(5, "pages")
            .and_then(|v| ranged(v, 1, max_pages))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(err(no, FntErrorKind::Pages))?;

        // Page lines (gdx 1.9.9 `load`:525-546).
        let mut names = Vec::with_capacity(pages);
        for p in 0..pages {
            let (no, line) = lines.line()?;
            let line = text(no, line.ok_or(err(no, FntErrorKind::PageLine))?)?;
            names.push(page_name(line, no, p, dir)?);
        }

        // Char lines, then the kerning block (gdx 1.9.9 `load`:551-618).
        let mut chars = Chars {
            glyphs: Vec::new(),
            missing: None,
            count: 0,
            first_visible: None,
            anchors: [None; 13],
        };
        let mut declared_chars = None;
        let mut kerning_header = None;
        loop {
            let (no, line) = lines.line()?;
            let Some(line) = line else { break };
            let line = text(no, line)?;
            if line.starts_with("char ") {
                chars.push(no, line, pages, limits)?;
            } else if line.starts_with("chars ") {
                // One declaration; a second is not allowed.
                if declared_chars.is_some() {
                    return Err(err(no, FntErrorKind::Unexpected));
                }
                declared_chars = Some(count(line, no, "chars")?);
            } else if line.starts_with("kernings ") {
                kerning_header = Some(count(line, no, "kernings")?);
                break;
            } else if !line.trim_matches(' ').is_empty() {
                return Err(err(no, FntErrorKind::Unexpected));
            }
        }
        if declared_chars.is_some_and(|n| n != chars.count) {
            return Err(err(0, FntErrorKind::Count("chars")));
        }
        chars.sorted()?;

        let mut kernings: Vec<FntKerning> = Vec::new();
        let mut kerning_lines = 0_usize;
        if let Some(declared) = kerning_header {
            let mut tail = false;
            loop {
                let (no, line) = lines.line()?;
                let Some(line) = line else { break };
                let line = text(no, line)?;
                if !tail && line.starts_with("kerning ") {
                    kerning_lines += 1;
                    let [first, second, amount] = positional(line, no, "kerning", KERNING_KEYS)?;
                    let code = |v: &str, key| {
                        ranged(v, i32::MIN, i32::MAX).ok_or(err(no, FntErrorKind::Value(key)))
                    };
                    let (first, second) = (code(first, "first")?, code(second, "second")?);
                    let amount = amount
                        .parse::<i8>()
                        .map_err(|_| err(no, FntErrorKind::Value("amount")))?;
                    // Kept only within the basic plane and after an existing
                    // glyph (gdx 1.9.9 `load`:600-618).
                    let (Ok(first), Ok(second)) = (u16::try_from(first), u16::try_from(second))
                    else {
                        continue;
                    };
                    if first == 0 || chars.get(u32::from(first)).is_none() {
                        continue;
                    }
                    if kernings.len() >= limits.kernings {
                        return Err(err(no, FntErrorKind::TooManyKernings));
                    }
                    kernings.push(FntKerning {
                        first,
                        second,
                        amount,
                    });
                } else if line.trim_matches(' ').is_empty() {
                    tail = true;
                } else {
                    return Err(err(no, FntErrorKind::Unexpected));
                }
            }
            if declared != kerning_lines {
                return Err(err(0, FntErrorKind::Count("kernings")));
            }
        }
        // A repeated pair: the last one wins. The stable sort keeps file
        // order within a pair, so `dedup` on the reversed list keeps the
        // last of each.
        kernings.sort_by_key(|k| (k.first, k.second));
        kernings.reverse();
        kernings.dedup_by_key(|k| (k.first, k.second));
        kernings.reverse();

        // The space (gdx 1.9.9 `load`:620-631).
        let space = u32::from(b' ');
        if chars.get(space).is_none() {
            let adv = chars
                .get(u32::from(b'l'))
                .or_else(|| {
                    chars
                        .glyphs
                        .iter()
                        .map(|(g, _)| g)
                        .find(|g| g.code <= MAX_BMP && g.src[2] != 0 && g.src[3] != 0)
                })
                .map(|g| g.adv)
                .ok_or(err(0, FntErrorKind::NoGlyphs))?;
            if chars.count >= limits.glyphs {
                return Err(err(0, FntErrorKind::TooManyGlyphs));
            }
            chars.count += 1;
            let glyph = FntGlyph {
                code: space,
                src: [0; 4],
                xoff: 0,
                raw_yoff: 0,
                adv,
                page: 0,
            };
            let at = chars.glyphs.partition_point(|(g, _)| g.code < space);
            chars.glyphs.insert(at, (glyph, 0));
        }
        if let Ok(i) = chars.glyphs.binary_search_by_key(&space, |(g, _)| g.code)
            && let Some((g, _)) = chars.glyphs.get_mut(i)
            && g.src[2] == 0
        {
            let w = pad_left + i32::from(g.adv) + pad_right;
            if !(0..=MAX_FNT_VALUE).contains(&w) {
                return Err(err(0, FntErrorKind::Value("width")));
            }
            g.src[2] = unsigned16(w);
            g.xoff = narrow16(-pad_left);
        }
        // Without any of `X_CHARS`, the x-height probe takes the first
        // basic-plane glyph drawn with a size, which must exist (gdx
        // 1.9.9 `load`:635-641, `getFirstGlyph`:748-755); the missing
        // glyph is not in that table, and a space with a height counts.
        if !X_CHARS.iter().any(|&c| chars.get(u32::from(c)).is_some())
            && !chars
                .glyphs
                .iter()
                .any(|(g, _)| g.code <= MAX_BMP && g.src[2] != 0 && g.src[3] != 0)
        {
            return Err(err(0, FntErrorKind::NoGlyphs));
        }

        // Cap height and ascent (gdx 1.9.9 `load`:643-660).
        let cap = CAP_CHARS.iter().find_map(|&c| chars.get(u32::from(c)));
        let cap_height = match cap {
            Some(g) => i32::from(g.src[3]),
            None => chars
                .glyphs
                .iter()
                .map(|(g, _)| g)
                .filter(|g| g.code <= MAX_BMP && g.src[2] != 0 && g.src[3] != 0)
                .map(|g| i32::from(g.src[3]))
                .fold(1, i32::max),
        } - (pad_top + pad_bottom);
        let rep = ANCHORS
            .iter()
            .zip(chars.anchors)
            .find_map(|(_, v)| v)
            .or(chars.first_visible)
            .unwrap_or(0);
        Ok(Self {
            size: unsigned16(size),
            ascent: base - cap_height,
            rep,
            glyphs: chars.glyphs.into_iter().map(|(g, _)| g).collect(),
            missing: chars.missing,
            kernings: kernings.into_boxed_slice(),
            pages: names.into_boxed_slice(),
        })
    }

    /// The size the glyphs were drawn at: the font's em, in pixels.
    #[must_use]
    pub fn size(&self) -> u16 {
        self.size
    }

    /// The base minus the cap height, in pixels.
    #[must_use]
    pub fn ascent(&self) -> i32 {
        self.ascent
    }

    /// The representative raw y offset that aligns a fallback font's
    /// glyphs to a primary's.
    #[must_use]
    pub fn rep(&self) -> i32 {
        self.rep
    }

    /// The glyphs, sorted by code, the missing glyph aside.
    #[must_use]
    pub fn glyphs(&self) -> &[FntGlyph] {
        &self.glyphs
    }

    /// The glyph drawn for a basic-plane character the font lacks.
    #[must_use]
    pub fn missing(&self) -> Option<FntGlyph> {
        self.missing
    }

    /// The kerning pairs, sorted, one per pair.
    #[must_use]
    pub fn kernings(&self) -> &[FntKerning] {
        &self.kernings
    }

    /// The page files, joined to the file's folder.
    #[must_use]
    pub fn pages(&self) -> &[String] {
        &self.pages
    }

    /// The glyph for code point `code`.
    #[must_use]
    pub fn glyph(&self, code: u32) -> Option<&FntGlyph> {
        self.glyphs
            .binary_search_by_key(&code, |g| g.code)
            .ok()
            .and_then(|i| self.glyphs.get(i))
    }

    /// The kerning from `first` to `second`, 0 without a pair.
    #[must_use]
    pub fn kerning(&self, first: u32, second: u32) -> i8 {
        let (Ok(a), Ok(b)) = (u16::try_from(first), u16::try_from(second)) else {
            return 0;
        };
        self.kernings
            .binary_search_by_key(&(a, b), |k| (k.first, k.second))
            .ok()
            .and_then(|i| self.kernings.get(i))
            .map_or(0, |k| k.amount)
    }
}

/// The count of a `chars count=N` or `kernings count=N` line.
fn count(line: &str, no: usize, head: &'static str) -> Result<usize, FntError> {
    let mut tokens = line.split(' ').filter(|t| !t.is_empty());
    let (Some(_), Some(v), None) = (tokens.next(), tokens.next(), tokens.next()) else {
        return Err(err(no, FntErrorKind::Key("count")));
    };
    keyed(v, "count")
        .ok_or(err(no, FntErrorKind::Key("count")))?
        .parse()
        .map_err(|_| err(no, FntErrorKind::Value(head)))
}

/// Page `p`'s file name from its line `page id=<p> file="<name>"`,
/// joined to `dir` with `\` as `/`.
fn page_name(line: &str, no: usize, p: usize, dir: &str) -> Result<String, FntError> {
    // Without `file=` the source fails too (gdx 1.9.9 `load`:540).
    if !line.contains("file=") {
        return Err(err(no, FntErrorKind::PageLine));
    }
    let rest = line
        .strip_prefix("page ")
        .ok_or(err(no, FntErrorKind::Key("page")))?;
    let (id, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let id = keyed(id, "id").ok_or(err(no, FntErrorKind::Key("id")))?;
    if id.parse::<usize>().ok() != Some(p) {
        return Err(err(no, FntErrorKind::PageId));
    }
    let rest = rest.trim_start_matches(' ');
    let Some(value) = keyed(rest, "file") else {
        return Err(err(no, FntErrorKind::PageLine));
    };
    let (name, tail) = value
        .strip_prefix('"')
        .and_then(|v| v.split_once('"'))
        .ok_or(err(no, FntErrorKind::Key("file")))?;
    if !tail.trim_matches(' ').is_empty() {
        return Err(err(no, FntErrorKind::Unexpected));
    }
    if name.is_empty() {
        return Err(err(no, FntErrorKind::PageLine));
    }
    let joined = if dir.is_empty() {
        name.len()
    } else {
        dir.len() + 1 + name.len()
    };
    if name.len() > MAX_PATH_BYTES || joined > MAX_PATH_BYTES {
        return Err(err(no, FntErrorKind::PageName));
    }
    let mut out = String::with_capacity(joined);
    if !dir.is_empty() {
        out.push_str(dir);
        out.push('/');
    }
    out.push_str(name);
    Ok(out.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "info face=\"x\" size=10 bold=0 padding=0,0,0,0 spacing=1,1";
    const COMMON: &str = "common lineHeight=12 base=10 scaleW=64 scaleH=64 pages=1 packed=0";
    const PAGE: &str = "page id=0 file=\"p.png\"";

    /// A `char` line.
    fn ch(id: i64, [w, h]: [i32; 2], [xoff, yoff, adv]: [i32; 3]) -> String {
        format!(
            "char id={id}   x=0 y=0 width={w} height={h} xoffset={xoff} yoffset={yoff} \
             xadvance={adv} page=0 chnl=15"
        )
    }

    /// A one-page font of `chars`, then `tail`.
    fn font(chars: &[String], tail: &str) -> String {
        format!(
            "{INFO}\n{COMMON}\n{PAGE}\nchars count={}\n{}\n{tail}",
            chars.len(),
            chars.join("\n")
        )
    }

    fn parse(s: &str) -> Result<FntFile, FntError> {
        FntFile::parse(s.as_bytes(), "", FntLimits::FILE)
    }

    fn kind(s: &str) -> FntErrorKind {
        parse(s).unwrap_err().kind
    }

    fn a() -> String {
        ch(65, [4, 9], [0, 1, 6])
    }

    #[test]
    fn a_bom_and_crlf_are_accepted_and_a_lone_cr_is_not() {
        let s = format!("\u{feff}{}", font(&[a()], "").replace('\n', "\r\n"));
        let f = parse(&s).unwrap();
        assert!(f.glyph(65).is_some());
        let lone = font(&[a()], "").replacen("page=0 ", "page=0\r", 1);
        assert_eq!(parse(&lone).unwrap_err(), err(5, FntErrorKind::LoneCr));
    }

    #[test]
    fn the_face_may_hold_any_bytes_and_nothing_else_may() {
        let mut bytes =
            b"info face=\"\x83\x81\x83\x43\x83\x8a\x83\x49\" size=10 padding=0,0,0,0\n".to_vec();
        bytes.extend_from_slice(font(&[a()], "").split_once('\n').unwrap().1.as_bytes());
        let f = FntFile::parse(&bytes, "", FntLimits::FILE).unwrap();
        assert_eq!(f.size(), 10);
        let bad = font(&[a()], "")
            .replace("p.png", "p\u{1}.png")
            .replace('\u{1}', "\u{80}");
        let mut bad = bad.into_bytes();
        let at = bad.iter().position(|&b| b == 0xC2).unwrap();
        bad.remove(at);
        assert_eq!(
            FntFile::parse(&bad, "", FntLimits::FILE).unwrap_err(),
            err(3, FntErrorKind::Encoding)
        );
    }

    #[test]
    fn binary_and_xml_files_fail_as_the_source_fails() {
        for s in ["BMF\u{3}\u{1}", "<?xml version=\"1.0\"?>\n<font>"] {
            let e = kind(s);
            assert_eq!(e, FntErrorKind::Padding);
            assert!(e.upstream_fails());
        }
        assert_eq!(kind(""), FntErrorKind::Padding);
    }

    #[test]
    fn the_header_is_read_by_position_and_strictly() {
        let base = font(&[a()], "");
        let cases: [(&str, &str, FntErrorKind); 9] = [
            ("pages=1", "pages=0", FntErrorKind::Pages),
            ("pages=1", "pages=17", FntErrorKind::Pages),
            (" pages=1", "", FntErrorKind::Pages),
            ("scaleW=64 ", "", FntErrorKind::Metadata("scaleW")),
            (" size=10", "", FntErrorKind::Metadata("size")),
            ("size=10", "size=0", FntErrorKind::Value("size")),
            ("base=10", "base=x", FntErrorKind::Common),
            ("info ", "inf ", FntErrorKind::Info),
            ("page id=0", "page id=1", FntErrorKind::PageId),
        ];
        for (from, to, want) in cases {
            assert_eq!(kind(&base.replacen(from, to, 1)), want, "{from} → {to}");
        }
        assert!(!FntErrorKind::Metadata("size").upstream_fails());
        assert!(FntErrorKind::Common.upstream_fails() && FntErrorKind::PageId.upstream_fails());
        // A missing page line.
        let two = base.replacen("pages=1", "pages=2", 1);
        assert_eq!(kind(&two), FntErrorKind::PageLine);
    }

    #[test]
    fn key_names_are_checked_at_their_positions() {
        let base = font(&[a()], "");
        assert_eq!(
            kind(&base.replacen(" x=0", " X=0", 1)),
            FntErrorKind::Key("x")
        );
        assert_eq!(
            kind(&base.replacen("xadvance", "advance", 1)),
            FntErrorKind::Key("xadvance")
        );
        assert!(parse(&base.replacen("chnl=15", "", 1)).is_ok());
        assert_eq!(
            kind(&base.replacen("page=0", "page=x", 1)),
            FntErrorKind::Value("page")
        );
        assert_eq!(
            kind(&base.replacen("page=0", "page=1", 1)),
            FntErrorKind::GlyphPage
        );
    }

    #[test]
    fn ids_follow_the_missing_glyph_convention() {
        for id in [0, -1] {
            let f = parse(&font(&[a(), ch(id, [3, 3], [0, 0, 4])], "")).unwrap();
            assert_eq!(f.missing().map(|g| g.adv), Some(4));
            assert_eq!(f.glyphs().iter().filter(|g| g.code == 0).count(), 0);
        }
        let both = font(&[ch(0, [1, 1], [0, 0, 1]), ch(-1, [1, 1], [0, 0, 1])], "");
        assert_eq!(kind(&both), FntErrorKind::DuplicateId);
        for id in [-2, 0x11_0000] {
            assert_eq!(
                kind(&font(&[ch(id, [1, 1], [0, 0, 1])], "")),
                FntErrorKind::Id
            );
        }
        let f = parse(&font(&[a(), ch(0x1_F600, [5, 5], [0, 0, 7])], "")).unwrap();
        assert_eq!(f.glyph(0x1_F600).map(|g| g.adv), Some(7));
        assert_eq!(kind(&font(&[a(), a()], "")), FntErrorKind::DuplicateId);
        assert_eq!(parse(&font(&[a(), a()], "")).unwrap_err().line, 6);
    }

    #[test]
    fn kernings_are_bytes_and_the_last_pair_wins() {
        let b = ch(66, [4, 9], [0, 1, 6]);
        let tail = "kernings count=4\nkerning first=65 second=66 amount=-128\n\
                    kerning first=66 second=65 amount=1\nkerning first=66 second=65 amount=127\n\
                    kerning first=67 second=65 amount=2\n\n";
        let f = parse(&font(&[a(), b.clone()], tail)).unwrap();
        assert_eq!(f.kerning(65, 66), -128);
        assert_eq!(f.kerning(66, 65), 127);
        // An amount past a byte refuses the file, a dropped pair's too.
        for (from, to) in [
            ("-128", "-129"),
            ("=127", "=128"),
            ("amount=2\n", "amount=200\n"),
        ] {
            let over = font(&[a(), b.clone()], &tail.replacen(from, to, 1));
            assert_eq!(kind(&over), FntErrorKind::Value("amount"), "{to}");
        }
        assert!(!FntErrorKind::Value("amount").upstream_fails());
        // No glyph 67: its pair is not kept.
        assert_eq!(f.kerning(67, 65), 0);
        assert_eq!(f.kernings().len(), 2);
        // The kerning block ends the file but for blank lines.
        let after = format!("{tail}{}", ch(67, [1, 1], [0, 0, 1]));
        assert_eq!(
            kind(&font(&[a(), b.clone()], &after)),
            FntErrorKind::Unexpected
        );
        let mismatch = tail.replacen("count=4", "count=3", 1);
        assert_eq!(
            kind(&font(&[a(), b.clone()], &mismatch)),
            FntErrorKind::Count("kernings")
        );
        let chars = font(&[a(), b], "").replacen("chars count=2", "chars count=3", 1);
        assert_eq!(kind(&chars), FntErrorKind::Count("chars"));
    }

    #[test]
    fn a_missing_space_takes_the_advance_of_l_or_the_first_visible_glyph() {
        let bang = ch(33, [2, 9], [0, 0, 9]);
        let l = ch(108, [2, 9], [0, 0, 5]);
        let f = parse(&font(&[bang.clone(), l], "")).unwrap();
        assert_eq!(f.glyph(32).map(|g| g.adv), Some(5));
        // Without `l`: the lowest code drawn with a size.
        let empty = ch(34, [0, 9], [0, 0, 3]);
        let f = parse(&font(&[ch(90, [2, 2], [0, 0, 7]), bang, empty], "")).unwrap();
        assert_eq!(f.glyph(32).map(|g| g.adv), Some(9));
        assert_eq!(
            kind(&font(&[ch(34, [0, 9], [0, 0, 3])], "")),
            FntErrorKind::NoGlyphs
        );
        // A space of width 0 spans the padding and its advance.
        let padded = font(&[a()], "").replacen("padding=0,0,0,0", "padding=1,2,3,4", 1);
        let space = *parse(&padded).unwrap().glyph(32).unwrap();
        assert_eq!((space.src[2], space.xoff), (4 + 6 + 2, -4));
    }

    #[test]
    fn the_cap_height_sets_the_ascent() {
        // `M` before `N`, less the vertical padding: base 10 − (9 − 3).
        let m = ch(77, [4, 9], [0, 0, 5]);
        let n = ch(78, [4, 7], [0, 0, 5]);
        let padded =
            |chars: &[String]| font(chars, "").replacen("padding=0,0,0,0", "padding=2,0,1,0", 1);
        assert_eq!(parse(&padded(&[n.clone(), m])).unwrap().ascent(), 4);
        // No cap glyph: the tallest basic-plane glyph drawn with a size.
        let low = ch(97, [4, 5], [0, 0, 5]);
        let flat = ch(98, [0, 12], [0, 0, 5]);
        let wide = ch(0x1_0000, [4, 14], [0, 0, 5]);
        let f = parse(&padded(&[low, flat, wide])).unwrap();
        assert_eq!(f.ascent(), 10 - (5 - 3));
        assert_eq!(parse(&padded(&[n])).unwrap().ascent(), 10 - (7 - 3));
    }

    #[test]
    fn the_representative_offset_scans_every_char_line() {
        let o = ch(79, [4, 9], [0, 3, 5]);
        let s = ch(83, [4, 9], [0, 7, 5]);
        let s0 = ch(83, [4, 0], [0, 8, 5]);
        let first = ch(97, [4, 9], [0, 2, 5]);
        // `S` comes before `O` in the anchor order.
        assert_eq!(
            parse(&font(&[first.clone(), o.clone(), s], ""))
                .unwrap()
                .rep(),
            7
        );
        // A glyph of height 0 is skipped.
        assert_eq!(parse(&font(&[first.clone(), o, s0], "")).unwrap().rep(), 3);
        // No anchor: the first char line drawn with a height.
        let flat = ch(98, [4, 0], [0, 9, 5]);
        assert_eq!(parse(&font(&[flat, first], "")).unwrap().rep(), 2);
    }

    #[test]
    fn values_hold_at_their_extremes() {
        let at = ch(65, [16_384, 16_384], [-16_384, 16_384, -16_384]);
        let f = parse(&font(&[at, ch(108, [1, 1], [0, 0, 1])], "")).unwrap();
        assert_eq!(
            f.glyph(65).map(|g| (g.xoff, g.raw_yoff, g.adv)),
            Some((-16_384, 16_384, -16_384))
        );
        for over in [
            ch(65, [16_385, 1], [0, 0, 1]),
            ch(65, [1, 1], [-16_385, 0, 1]),
            ch(65, [1, 1], [0, 16_385, 1]),
        ] {
            assert!(matches!(kind(&font(&[over], "")), FntErrorKind::Value(_)));
        }
        let base = font(&[a()], "");
        for (from, to) in [
            ("size=10", "size=16385"),
            ("base=10", "base=16385"),
            ("scaleW=64", "scaleW=0"),
            ("padding=0,0,0,0", "padding=0,16385,0,0"),
            ("padding=0,0,0,0", "padding=-1,0,0,0"),
        ] {
            assert!(
                matches!(kind(&base.replacen(from, to, 1)), FntErrorKind::Value(_)),
                "{to}"
            );
        }
        assert!(parse(&base.replacen("size=10", "size=16384", 1)).is_ok());
    }

    #[test]
    fn glyph_and_kerning_caps_hold_at_the_cap() {
        let glyphs = |n: i64, space: bool| {
            let mut v: Vec<String> = (0..n).map(|i| ch(100 + i, [1, 1], [0, 0, 1])).collect();
            if space {
                v[0] = ch(32, [0, 0], [0, 0, 1]);
            }
            v
        };
        // 16 383 lines and a synthesised space fill the cap; 16 384 lines
        // without a space pass it.
        assert!(parse(&font(&glyphs(16_383, false), "")).is_ok());
        assert!(parse(&font(&glyphs(16_384, true), "")).is_ok());
        assert_eq!(
            kind(&font(&glyphs(16_384, false), "")),
            FntErrorKind::TooManyGlyphs
        );
        assert_eq!(
            kind(&font(&glyphs(16_385, true), "")),
            FntErrorKind::TooManyGlyphs
        );
        let kernings = |n: usize| {
            let lines: Vec<String> = (0..n)
                .map(|i| format!("kerning first=65 second={} amount=1", i % 60_000))
                .collect();
            format!("kernings count={n}\n{}", lines.join("\n"))
        };
        assert!(parse(&font(&[a()], &kernings(16_384))).is_ok());
        assert_eq!(
            kind(&font(&[a()], &kernings(16_385))),
            FntErrorKind::TooManyKernings
        );
        // A smaller limit is met at its count.
        let small = FntLimits {
            glyphs: 2,
            kernings: 1,
        };
        let e = FntFile::parse(font(&glyphs(2, false), "").as_bytes(), "", small).unwrap_err();
        assert_eq!(e.kind, FntErrorKind::TooManyGlyphs);
    }

    #[test]
    fn size_and_page_names_are_bounded() {
        let mut big = font(&[a()], "").into_bytes();
        big.resize(usize::try_from(MAX_FNT_BYTES).unwrap(), b'\n');
        assert!(FntFile::parse(&big, "", FntLimits::FILE).is_ok());
        big.push(b'\n');
        assert_eq!(
            FntFile::parse(&big, "", FntLimits::FILE).unwrap_err(),
            err(0, FntErrorKind::TooLarge)
        );
        let f = FntFile::parse(
            font(&[a()], "").replace("p.png", "a\\p.png").as_bytes(),
            "fonts",
            FntLimits::FILE,
        )
        .unwrap();
        assert_eq!(f.pages(), ["fonts/a/p.png"]);
        let long = "n".repeat(MAX_PATH_BYTES - 6);
        let named = font(&[a()], "").replace("p.png", &long);
        assert!(FntFile::parse(named.as_bytes(), "fonts", FntLimits::FILE).is_ok());
        assert_eq!(
            FntFile::parse(named.as_bytes(), "fonts1", FntLimits::FILE)
                .unwrap_err()
                .kind,
            FntErrorKind::PageName
        );
    }

    #[test]
    fn a_font_needs_a_drawn_basic_plane_glyph_without_an_x_char() {
        let space = ch(32, [0, 0], [0, 0, 4]);
        let missing = ch(0, [3, 3], [0, 0, 4]);
        let emoji = ch(0x1_F600, [5, 5], [0, 0, 7]);
        for chars in [[space.clone(), missing.clone()], [space.clone(), emoji]] {
            assert_eq!(kind(&font(&chars, "")), FntErrorKind::NoGlyphs);
        }
        // An `x` char, even an empty one, is what the probe takes.
        let x = ch(120, [0, 0], [0, 0, 4]);
        assert!(parse(&font(&[space.clone(), missing, x], "")).is_ok());
        // A space with a height is drawn with a size once it has a width.
        assert!(parse(&font(&[ch(32, [0, 3], [0, 0, 4])], "")).is_ok());
    }

    #[test]
    fn a_negative_synthesised_space_is_refused() {
        let l = ch(108, [2, 9], [0, 0, -5]);
        assert_eq!(kind(&font(&[l], "")), FntErrorKind::Value("width"));
    }

    #[test]
    fn the_face_exception_needs_its_own_attribute() {
        let mut bytes = b"info preface=\"\xFF\" size=10 padding=0,0,0,0\n".to_vec();
        bytes.extend_from_slice(font(&[a()], "").split_once('\n').unwrap().1.as_bytes());
        assert_eq!(
            FntFile::parse(&bytes, "", FntLimits::FILE).unwrap_err(),
            err(1, FntErrorKind::Encoding)
        );
    }

    #[test]
    fn a_carriage_return_ends_a_line_only_before_a_line_feed() {
        let s = format!("{}\r", font(&[a()], "").trim_end_matches('\n'));
        assert_eq!(parse(&s).unwrap_err(), err(5, FntErrorKind::LoneCr));
    }

    #[test]
    fn the_char_count_is_declared_once() {
        let s = font(&[a()], "").replacen("chars count=1", "chars count=1\nchars count=1", 1);
        assert_eq!(parse(&s).unwrap_err(), err(5, FntErrorKind::Unexpected));
    }
}
