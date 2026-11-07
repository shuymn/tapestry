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
//! A [`ScreenKind`] names the screen a skin is made for.
//!
//! A [`SelectionView`] is selection's snapshot: a bounded window of rows
//! around the cursor, each with its identity, kind, text, level and typed
//! score, the cursor's exact position, the place, the query and the selected
//! chart's labels, summary and score.
//!
//! A [`ResultView`] is the result screen's snapshot: one view's result,
//! its timing statistics and [`Timelines`], its best before the play and
//! the chart's facts, each value shown, hidden or unavailable. A result
//! skin ([`Skin::new_result`]) reads it through the [`RESULT`] schema: its
//! [`ResultScreen`] is filled per view ([`Skin::update_result`]) and built
//! per frame ([`Skin::build_result`]); a hidden value reads `i32::MIN`,
//! which a number does not draw, and an unavailable one has its validity
//! field false.
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
//! From a chart's choice to its READY state the play screen draws a
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
#[cfg(feature = "dev-tools")]
mod observation;
mod place;
mod play;
mod quad;
mod result;
mod schema;
mod screen;
mod select;
mod selection;
mod skin;
mod track;
mod widget;

pub use atlas::uv::{FrameUv, UvTable};
pub use atlas::{
    ALIGN, ATLAS_TABLE_BYTES, Atlas, AtlasError, AtlasLimits, CELL_TABLE_BYTES, FILE_TABLE_BYTES,
    GUTTER, MAX_ATLAS_TEXELS, MAX_PAGE_SIDE, MAX_PAGES, MIN_PAGE_SIDE, MIN_TEXT_SIDE, MIP_LEVELS,
    PAGE_SIDE_STEP, Page, Variant, cell_fits_page, downsample_rgba,
};
pub use canvas::{Canvas, Fit, MAX_CANVAS};
#[cfg(feature = "dev-tools")]
pub use expr::evaluations;
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
pub use image::font::{
    FontDef, FontError, FontReader, MAX_FONT_BYTES, MAX_FONTS, MAX_TOTAL_FONT_BYTES,
};
pub use image::path::{Entry, FileId, Listing, Root, RootError};
#[cfg(feature = "dev-tools")]
pub use image::source::SourceCounts;
pub use image::source::{CacheBounds, CacheUsage, FileSource, ResourceCache};
pub use image::{
    CycleDef, FileAt, FileDef, HEADER_BYTES, ImageDef, ImageError, MAX_FILE_BYTES, MAX_FILES,
    MAX_IMAGE_FRAMES, MAX_IMAGE_TEXELS, MAX_IMAGES, MAX_PATH_BYTES, MAX_SIDE, MAX_SKIN_FRAMES,
    MAX_STREAMED_TEXELS, MAX_TOTAL_FILE_BYTES, MAX_TOTAL_TEXELS, Sheet, SolidImageDef, TexelCap,
    Unbanded,
};
pub use layout::{
    Adjust, Anchor, Controls, LANE_WIDTH_PCT, Layout, LayoutError, MAX_INSET, MAX_OFFSET, Region,
    RegionDef, SCALE_PCT,
};
#[cfg(feature = "dev-tools")]
pub use observation::{
    DisplayList, ElementObservation, ListedElement, ListedQuad, Outcome, QuadSource,
};
pub use place::{
    Affine, Bind, Fill, Follow, MIN_VISIBLE, Native, Place, Placement, RegionPlacement,
};
pub use play::loading::{
    Loading, LoadingField, LoadingFrame, LoadingGauge, LoadingTimer, loading_field, loading_timer,
};
pub use play::playhead::ExactPlayhead;
pub use play::schema::{
    COVER_PERMILLE, DIFFICULTY_MAX, GAUGE_KIND_ENUM, GAUGE_PERMILLE, GREEN_MAX, Grade,
    JUDGE_CLASS_MAX, LaneTimer, MAX_COUNT, PLAY, PROGRESS_FULL, RATE_BASIS_POINTS, READOUT_MAX,
    field, lane_timer, timer,
};
pub use play::timers::native_values;
pub use play::{
    Attempt, BpmRange, BuildError, ChartMetadata, CoverSettings, DurationRatio, FrameReadouts,
    GaugeDesc, LoadState, NativeReadouts, NativeValues, Play, PlayContext, PrepareError,
    RoundedDuration, SkinFrame,
};
pub use quad::{Blend, ClipId, ClipRect, Filter, Quad, QuadBatch, Run, RunKey};
pub use result::schema::{
    CLEAR_MAX, GRADE_MAX, HIDDEN, RESULT, VALUE_MAX as RESULT_VALUE_MAX, field as result_field,
    timer as result_timer,
};
pub use result::view::{
    BestFigures, BestRate, BestView, MAX_TIMELINE_BINS, ResultBpm, ResultChart, ResultLabels,
    ResultNumber, ResultRate, ResultScore, ResultStatus, ResultView, TIMELINE_CHANNELS, Timelines,
    TimelinesError, TimingStats, Unavailable,
};
pub use result::{ResultError, ResultFields, ResultFrame, ResultScreen, ResultTimers};
pub use schema::{EnumDef, EnumId, Export, Field, FieldTy, Schema, SchemaError, TimerDef, Ty};
pub use screen::ScreenKind;
pub use select::schema::{SELECTION, VALUE_MAX as SELECTION_VALUE_MAX};
pub use select::schema::{field as selection_field, timer as selection_timer};
pub use select::{Selection, SelectionError, SelectionFrame, SelectionTimers};
pub use selection::{
    ChartLabels, FolderId, MAX_VISIBLE_ROWS, MasterVolume, OwnLamp, Ratio, RowFeatures, RowKey,
    RowKind, RowScore, RowView, SelectedChart, SelectionLabels, SelectionOrder, SelectionPlace,
    SelectionSettings, SelectionView, SpeedReference, SpeedView,
};
pub use skin::{
    ElementDef, LoadError, MAX_COORD, MAX_ELEMENTS, MAX_SKIN_KEYS, MAX_WHEN_TIMERS,
    MAX_WIDGET_QUADS, SelectionDefs, SelectionRequests, Skin, SkinDef, Source, TimerRef,
    WidgetDefs,
};
pub use track::{
    Ease, End, KeyDef, MAX_GEOMETRY, MAX_KEY_US, MAX_TRACK_KEYS, Prop, Sample, Track, TrackError,
};
pub use widget::bar::{BarGroupDef, BarGroupError, MAX_BAR_GROUPS};
pub use widget::cover_paint::{
    CoverImage, CoverMotion, CoverPaintDef, CoverPaintError, CoverTrim, Slider, SliderRate,
};
pub use widget::digits::{
    Align, DigitOffset, DigitsDef, GlyphSet, I32_DIGITS, MAX_CELLS, MAX_DIGIT_OFFSETS, NumberDef,
    NumberError, Pad,
};
pub use widget::gauge::{
    GaugeDef, GaugeError, GaugeLook, GaugePart, GaugeParts, MAX_SEGMENTS, Motion,
};
pub use widget::glyphs::BitmapLines;
pub use widget::image_choice::{
    ImageChoiceDef, ImageChoiceError, ImageChoicePart, ImageSpan, MAX_IMAGE_CHOICE_REFS, PastTable,
};
pub use widget::judge::{Attach, ComboPart, GradePart, JudgeDef, JudgeError, JudgePart, JudgeSlot};
pub use widget::lanes::{
    LANE_QUADS, LANE_RESERVED, LANE_VISITS, LanePart, LaneParts, LaneStyle, LaneView, LanesDef,
    LanesError, NoteOrder, Side, SourceLanes,
};
pub use widget::ratio_bar::{
    RatioBarDef, RatioBarError, RatioForm, RatioSource, SLIDER_STEPS, SliderCommand,
    SliderDirection,
};
pub use widget::scroll::{Gain, Scale};
pub use widget::song_list::{
    ImagePartDef, LEVEL_STYLES, LampKey, LevelDef, MAX_PLACEMENTS, PartDef, PlacementDef, RowLabel,
    RowStyle, RowTextDef, SongListDef, SongListError,
};
pub use widget::text::{
    ChartText, FontRef, MAX_TEXT_BYTES, MAX_TEXT_ELEMENTS, MAX_TEXT_EM, MAX_TEXT_EM_PX, Overflow,
    SelectionText, TextDef, TextError, TextField, TextImage, TextRequest, TextSlots, TextTable,
    TextTableError, TextValue,
};
