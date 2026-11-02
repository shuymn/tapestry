//! The skin runtime model.
//!
//! A [`Schema`] names the state a skin may read: typed fields with declared
//! ranges, timers and enumerations, each at a fixed slot. Expressions are
//! compiled against it ([`compile`]) into a [`Program`]: a postfix list of
//! pure, total integer operations whose stack depth and length are known at
//! load. Every integer subexpression is proven to stay inside `i32` at
//! compile time, and every read is clamped into its declared range, so
//! evaluating a program cannot overflow, allocates nothing and never panics.
//!
//! A frame's drawing is a [`QuadBatch`]: one ordered list of [`Quad`]s split
//! into runs wherever the blend or the clip changes.
//!
//! A [`Sheet`] holds a skin's files and images: rectangles of the files cut
//! into grids of frames, each image with its own frame cycle and filter,
//! and its font files. Loading it checks the counts, confines the paths to
//! the skin folder and reads only the headers and the fonts' first bytes.
//! A [`SheetBuilder`] also admits bitmap `.fnt` fonts ([`FntFile`]),
//! whose pages become whole-file images.
//! [`Sheet::atlas`] decodes the files of one [`Variant`] (1x or @2x) into
//! texture-array [`Page`]s with box-filtered mips and a [`UvTable`] that
//! finds each frame in them.
//! A [`Root`] gives a skin definition's reader the same confinement:
//! bounded reads of regular files and bounded folder listings, by names
//! that may step up with `..` but never above the folder.
//!
//! A [`Skin`] holds elements in draw order, each an image, a number, a
//! gauge, a judgment display, a line of text or the lanes widget on a
//! keyframe [`Track`] on a timer, placed in one of seven [`Region`]s, and
//! `when` timers. The target size gives the [`Canvas`]; the player's
//! [`Layout`] moves, scales and hides regions within the translation
//! clamp, and the scratch side and lane width re-lay the lanes
//! ([`Skin::placement`]). [`Skin::prepare`] makes one attempt's [`Play`];
//! [`Skin::text_requests`] lists the lines of text to rasterize, and
//! [`Skin::attach_text`] hands the play their images in a [`TextTable`];
//! [`Skin::attach_bitmap_text`] lays out the lines in bitmap fonts.
//! [`Skin::build`] runs each frame's drained effects through its reducer,
//! reads the play view's snapshot into the [`PLAY`] schema's fields,
//! starts the timers and pushes each drawn element's quads: an image's
//! current frame from the uv table, turned about its pivot; a number's
//! glyph cells; a gauge's lit and unlit segments; the judgment's grade
//! and combo; a text's line; a cover's paint moved by the covers and cut
//! at its trim line; or the lanes' notes, bar lines and judgment line.
//! From a chart's choice to its countdown the play screen draws a
//! [`Loading`] state instead, which [`Skin::prepare_loading`] makes
//! without a chart and [`Skin::build_loading`] builds from each
//! [`LoadingFrame`] through the same evaluation and drawing: `loading`
//! and `screen` run from the load's origin ([`LoadState`]),
//! `load.percent` reads its progress, the gauge draws its empty frame from
//! a [`LoadingGauge`], the lanes their judgment line, and an expression
//! reading a field loading lacks ([`loading_field`]) draws nothing. Its
//! text slots show the lines installed for the chosen chart
//! ([`Skin::install_loading_text`]) until they are cleared
//! ([`Skin::clear_loading_text`]).
//!
//! Expressions use integers only; floats appear only in a quad's geometry,
//! after evaluation. The cast lints below reject `as` casts that may
//! truncate, wrap, or lose the sign; they cover the library and its unit
//! tests, not integration tests.

#![forbid(unsafe_code)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod atlas;
mod canvas;
mod expr;
mod image;
mod layout;
mod place;
mod play;
mod quad;
mod schema;
mod skin;
mod track;
mod widget;

pub use atlas::uv::{FrameUv, UvTable};
pub use atlas::{
    ALIGN, Atlas, AtlasError, AtlasLimits, GUTTER, MAX_ATLAS_TEXELS, MAX_PAGE_SIDE, MAX_PAGES,
    MIN_PAGE_SIDE, MIP_LEVELS, Page, Variant,
};
pub use canvas::{Canvas, Fit, MAX_CANVAS};
pub use expr::{
    CompileError, ErrorKind, MAX_ELAPSED_MS, MAX_NAME_BYTES, MAX_NESTING, MAX_OPS, MAX_SKIN_OPS,
    MAX_SOURCE_BYTES, MAX_STACK, OptionDef, Program, Scope, SkinOps, Span, State, TIMER_OFF,
    compile,
};
pub use image::builder::{
    AdmitError, BitmapFontError, Charge, FntAdmitError, FntFileError, FntLimit, FontAdmitError,
    MAX_BITMAP_FONTS, MAX_FALLBACKS, Refused, SheetBuilder,
};
pub use image::fnt::{
    FntError, FntErrorKind, FntFile, FntGlyph, FntKerning, FntLimits, MAX_FNT_BYTES, MAX_FNT_PAGES,
    MAX_FNT_VALUE, MAX_FONT_GLYPHS, MAX_FONT_KERNINGS, MAX_SHEET_GLYPHS, MAX_SHEET_KERNINGS,
};
pub use image::font::{FontDef, FontError, MAX_FONT_BYTES, MAX_FONTS, MAX_TOTAL_FONT_BYTES};
pub use image::path::{Entry, FileId, Listing, Root, RootError};
pub use image::{
    CycleDef, FileAt, FileDef, HEADER_BYTES, ImageDef, ImageError, MAX_FILE_BYTES, MAX_FILES,
    MAX_IMAGE_FRAMES, MAX_IMAGE_TEXELS, MAX_IMAGES, MAX_PATH_BYTES, MAX_SIDE, MAX_SKIN_FRAMES,
    MAX_TOTAL_FILE_BYTES, MAX_TOTAL_TEXELS, Sheet, SolidImageDef,
};
pub use layout::{
    Adjust, Anchor, Controls, LANE_WIDTH_PCT, Layout, LayoutError, MAX_INSET, MAX_OFFSET, Region,
    RegionDef, SCALE_PCT,
};
pub use place::{
    Affine, Bind, Fill, Follow, MIN_VISIBLE, Native, Place, Placement, RegionPlacement,
};
pub use play::loading::{
    Loading, LoadingField, LoadingFrame, LoadingGauge, LoadingTimer, loading_field, loading_timer,
};
pub use play::schema::{
    COVER_PERMILLE, GAUGE_KIND_ENUM, GAUGE_PERMILLE, GREEN_MAX, Grade, LaneTimer, MAX_COUNT, PLAY,
    RATE_BASIS_POINTS, READOUT_MAX, field, lane_timer, timer,
};
pub use play::{
    Attempt, BpmRange, BuildError, CoverSettings, DurationRatio, FrameReadouts, GaugeDesc,
    LoadState, Play, PlayContext, PrepareError, SkinFrame,
};
pub use quad::{Blend, ClipId, ClipRect, Filter, Quad, QuadBatch, Run, RunKey};
pub use schema::{EnumDef, EnumId, Export, Field, FieldTy, Schema, SchemaError, TimerDef, Ty};
pub use skin::{
    ElementDef, LoadError, MAX_COORD, MAX_ELEMENTS, MAX_SKIN_KEYS, MAX_WHEN_TIMERS,
    MAX_WIDGET_QUADS, Skin, SkinDef, Source, TimerRef, WidgetDefs,
};
pub use track::{
    Ease, End, KeyDef, MAX_GEOMETRY, MAX_KEY_US, MAX_TRACK_KEYS, Prop, Sample, Track, TrackError,
};
pub use widget::bar::{BarGroupDef, BarGroupError, MAX_BAR_GROUPS};
pub use widget::cover_paint::{CoverImage, CoverMotion, CoverPaintDef, CoverPaintError, CoverTrim};
pub use widget::digits::{
    Align, DigitOffset, DigitsDef, GlyphSet, I32_DIGITS, MAX_CELLS, MAX_DIGIT_OFFSETS, NumberDef,
    NumberError, Pad,
};
pub use widget::gauge::{
    GaugeDef, GaugeError, GaugeLook, GaugePart, GaugeParts, MAX_SEGMENTS, Motion,
};
pub use widget::image_choice::{
    ImageChoiceDef, ImageChoiceError, ImageChoicePart, ImageSpan, MAX_IMAGE_CHOICE_REFS,
};
pub use widget::judge::{Attach, ComboPart, GradePart, JudgeDef, JudgeError, JudgePart, JudgeSlot};
pub use widget::lanes::{
    LANE_QUADS, LANE_RESERVED, LANE_VISITS, LanePart, LaneParts, LaneStyle, LaneView, LanesDef,
    LanesError, Side, SourceLanes,
};
pub use widget::scroll::Scale;
pub use widget::text::{
    ChartText, FontRef, MAX_TEXT_BYTES, MAX_TEXT_ELEMENTS, MAX_TEXT_EM, MAX_TEXT_EM_PX, Overflow,
    TextDef, TextError, TextField, TextImage, TextRequest, TextSlots, TextTable, TextTableError,
    TextValue,
};
