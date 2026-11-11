//! The select screen as the render thread draws it (design.md "Rendering
//! and Skins"): lines of Japanese text laid out with `render::text` and
//! drawn in the menu pass, over a bar that marks the cursor's line.
//!
//! A [`MenuPage`] comes from the game thread whenever what it shows
//! changes; its lines are laid out and the screen's text prepared then,
//! and every frame in between draws what was prepared. A screen built
//! again after a failure lays out its page again, and so does one whose
//! target changed size, since the text is prepared for the target's
//! pixels. All of it runs on menus, where allocation is allowed.
//!
//! A page with a text caret (the search box with focus) has the caret's
//! area measured as its line is laid out: the IME's composition is
//! underlined there, and the area goes to the window ([`WindowSink`]),
//! which reports it to the IME so its candidate window opens beside the
//! composition.
//!
//! A screen that has a window to tell draws a launch screen until the game
//! sends what to draw ([`Screen::splash`]), and tells the window when a
//! frame was presented: the window stays hidden until then, so the empty
//! window is never seen. With the menu font (the select screen's launch)
//! it is the game's name and version, fading in from black and out again
//! over [`SPLASH`], and the first page follows it; a screen for plays
//! only draws black, so `tapestry play` starts at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use std::time::Duration;

use bms_model::Chart;
use eval::Section;
use game::Preparation;
use render::text::{Fonts, MenuText, TextBlock, TextStyle};
use render::{
    AttemptInput, Blend, CommitError, FrameTiming, Installation, LoadingFrame, LoadingTextError,
    LoadingTextKey, PreparedToken, RenderError, Renderer, SkinBundle, Sprite, solid_uv,
};
use skin_model::{BuildError, ChartText, ScreenKind, TextField, TextSlots};

use crate::loading_state::LoadingUpdate;
use crate::loading_text::ChosenChart;
use crate::render_side::{Screen, refusal};
use crate::selection_skin::{SelectionDrawn, SelectionLogic, SelectionUpdate, StripText};

/// The size of a line's text, in layout pixels (1920×1080).
pub const TEXT_SIZE: f32 = 30.0;
/// The distance from one line's top to the next.
pub const LINE_STEP: f32 = 42.0;
/// Where the first line's top lies.
const TOP: f32 = 30.0;
/// The left edge of the text.
const LEFT: f32 = 60.0;
/// The lines a page holds: they fill the layout's height.
pub const PAGE_LINES: usize = 24;

/// How a line looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineStyle {
    /// The screen's title and state.
    Title,
    /// The search box.
    Search,
    /// A folder's heading in the list.
    Folder,
    /// A chart in the list.
    Chart,
    /// The chart under the cursor.
    Cursor,
    /// Why a chart was refused, or the library's trouble.
    Notice,
    /// The keys.
    Help,
}

impl LineStyle {
    const fn color(self) -> [u8; 4] {
        match self {
            Self::Title => [255, 255, 255, 255],
            Self::Search => [140, 220, 255, 255],
            Self::Folder => [255, 210, 120, 255],
            Self::Chart => [210, 210, 210, 255],
            Self::Cursor => [255, 255, 255, 255],
            Self::Notice => [255, 120, 110, 255],
            Self::Help => [150, 150, 150, 255],
        }
    }
}

/// One line of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuLine {
    /// What it says.
    pub text: String,
    /// How it looks.
    pub style: LineStyle,
}

/// What the select screen shows: at most [`PAGE_LINES`] lines, top down.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuPage {
    /// The lines.
    pub lines: Vec<MenuLine>,
    /// Where text is typed, when a text box has focus.
    pub caret: Option<Caret>,
}

/// Where text is typed on a page: the IME's composition in line `line`,
/// from byte `start` of its text to byte `end` (equal when nothing is
/// composed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
    /// The line, from the top.
    pub line: usize,
    /// Where the composition starts: the caret, before it.
    pub start: usize,
    /// Where the composition ends.
    pub end: usize,
}

/// Where the window learns what was drawn: the caret, which the IME's
/// windows open beside, and the first frame presented.
pub trait WindowSink: Send {
    /// The caret's area `[left, top, width, height]` in layout pixels
    /// (1920×1080), or `None` when no text box has focus.
    fn caret(&self, area: Option<[f32; 4]>);
    /// The flag a frame that reached the window sets: the window, which
    /// looks at it until it is shown, can show then. Setting it allocates
    /// nothing, so a play's frame may set it.
    fn presented(&self) -> Arc<AtomicBool>;
}

/// The launch screen's fade in from black, and its fade out.
const SPLASH_FADE: Duration = Duration::from_millis(300);
/// How long the launch screen shows, fades included: the select screen's
/// first page comes after it, or once startup is done if that is later.
pub const SPLASH: Duration = Duration::from_millis(1_500);

/// The launch screen's opacity `elapsed` after it began: up from 0 over
/// [`SPLASH_FADE`], 255 while it holds, down to 0 over the last
/// [`SPLASH_FADE`] of [`SPLASH`], and 0 after.
pub fn splash_alpha(elapsed: Duration) -> u8 {
    let fade = SPLASH_FADE.as_micros();
    let at = elapsed.as_micros();
    let left = SPLASH.as_micros().saturating_sub(at);
    let ramp = at.min(left).min(fade);
    // At most 255 × fade / fade.
    u8::try_from(ramp * 255 / fade).unwrap_or(u8::MAX)
}

/// The launch screen's text, laid out once, and the opacity last
/// prepared.
struct Splash {
    name: TextBlock,
    version: TextBlock,
    alpha: Option<u8>,
}

impl Splash {
    /// The game's name and version, centered.
    fn new(fonts: &mut Fonts) -> Self {
        let centered = |fonts: &mut Fonts, text: &str, top: f32, size: f32, color: [u8; 4]| {
            let style = |left| TextStyle {
                left,
                top,
                size,
                color,
            };
            let width = TextBlock::new(fonts, text, style(0.0)).width();
            TextBlock::new(fonts, text, style((1920.0 - width) / 2.0))
        };
        let version = format!("version {}", env!("CARGO_PKG_VERSION"));
        Self {
            name: centered(fonts, "tapestry", 420.0, 120.0, [255, 255, 255, 0]),
            version: centered(fonts, &version, 590.0, 30.0, [150, 150, 150, 0]),
            alpha: None,
        }
    }
}

/// The caret's area on `page`, `[left, top, width, height]` in layout
/// pixels: from where the composition starts to where it ends (at least a
/// pixel wide), over the line's height. `width` measures a prefix of the
/// caret's line as it is laid out. `None` without a caret, or with one
/// outside the page or its line's text.
pub fn caret_area(page: &MenuPage, mut width: impl FnMut(&str) -> f32) -> Option<[f32; 4]> {
    let caret = page.caret?;
    if caret.line >= PAGE_LINES {
        return None;
    }
    let text = &page.lines.get(caret.line)?.text;
    let start = width(text.get(..caret.start)?);
    let end = width(text.get(..caret.end.max(caret.start))?);
    Some([
        LEFT + start,
        line_top(caret.line),
        (end - start).max(1.0),
        LINE_STEP,
    ])
}

/// The top of line `i` of the select screen, in layout pixels.
fn line_top(i: usize) -> f32 {
    lines_from(TOP, i)
}

/// The top of line `i` of lines starting at `top`.
fn lines_from(top: f32, i: usize) -> f32 {
    // At most `PAGE_LINES` lines: exact in f32.
    top + LINE_STEP * i as f32
}

/// `page`'s lines laid out in `fonts` from `left` and `top`, and its
/// caret's area.
fn lay_out(
    fonts: &mut Fonts,
    page: &MenuPage,
    left: f32,
    top: f32,
) -> (Vec<TextBlock>, Option<[f32; 4]>) {
    let blocks: Vec<TextBlock> = page
        .lines
        .iter()
        .take(PAGE_LINES)
        .enumerate()
        .map(|(i, line)| {
            let style = TextStyle {
                left,
                top: lines_from(top, i),
                size: TEXT_SIZE,
                color: line.style.color(),
            };
            TextBlock::new(fonts, &line.text, style)
        })
        .collect();
    let area = caret_area(page, |prefix| {
        let style = TextStyle {
            left: LEFT,
            top: 0.0,
            size: TEXT_SIZE,
            color: [0; 4],
        };
        TextBlock::new(fonts, prefix, style).width()
    });
    (blocks, area)
}

/// The underline of `page`'s composition, from its caret's `area`: none
/// when nothing is composed.
fn underline(page: &MenuPage, area: Option<[f32; 4]>) -> Option<[f32; 4]> {
    area.filter(|_| page.caret.is_some_and(|c| c.end > c.start))
        .map(|[x, y, w, h]| [x, y + h - 6.0, w, 2.0])
}

/// What the selection skin's text on this screen was prepared for: text
/// prepared for another device, size, update or skin state is prepared
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionKey {
    device: u64,
    target: [u32; 2],
    lifetime: u64,
    revision: u64,
    state: u64,
}

/// The strip's sprites over a selection skin at `now`, in layout pixels:
/// a dark band under each line shown (the search line and the notice, and
/// the preset's name while it shows) and the composition's underline. At
/// most [`render::STRIP_SPRITES`]; allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn strip_sprites(
    lines: usize,
    preset: bool,
    underline: Option<[f32; 4]>,
    out: &mut [Sprite; render::STRIP_SPRITES],
) -> usize {
    let band = |i: usize| Sprite {
        rect: [0.0, line_top(i) - 3.0, 1_920.0, LINE_STEP],
        uv: solid_uv(),
        color: [0.0, 0.0, 0.0, 0.75],
        ..Sprite::default()
    };
    let shown = lines.min(2) + usize::from(preset);
    let mut n = 0;
    for (i, slot) in out.iter_mut().take(shown).enumerate() {
        *slot = band(i);
        n += 1;
    }
    if let (Some(rect), Some(slot)) = (underline, out.get_mut(n)) {
        *slot = Sprite {
            rect,
            uv: solid_uv(),
            color: [1.0; 4],
            ..Sprite::default()
        };
        n += 1;
    }
    n
}

/// The result strip's bands for `lines` lines, in layout pixels: one dark
/// band under each, from the top, at most
/// [`render::RESULT_STRIP_SPRITES`]. Allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn result_strip_sprites(lines: usize, out: &mut [Sprite; render::RESULT_STRIP_SPRITES]) -> usize {
    let mut n = 0;
    for (i, slot) in out.iter_mut().take(lines).enumerate() {
        *slot = Sprite {
            rect: [0.0, line_top(i) - 3.0, 1_920.0, LINE_STEP],
            uv: solid_uv(),
            color: [0.0, 0.0, 0.0, 0.75],
            ..Sprite::default()
        };
        n += 1;
    }
    n
}

/// Where the load's lines start on the play screen while a chart loads:
/// right of the lanes, under the measurement overlay's place.
const LOADING_LEFT: f32 = 1_000.0;
const LOADING_TOP: f32 = 400.0;

/// The renderer, with the select screen's text when it draws menus.
pub struct MenuScreen {
    renderer: Renderer,
    /// The font, read once per screen: when it is built on a screen that
    /// draws menus, and on a screen for plays only when its first page
    /// comes.
    fonts: Option<Fonts>,
    /// The text prepared for this renderer, and the page it shows.
    text: Option<MenuText>,
    /// The cursor's line of the prepared page.
    cursor: Option<usize>,
    /// The prepared page's composition, underlined: `[left, top, width,
    /// height]` in layout pixels.
    underline: Option<[f32; 4]>,
    /// Where the caret's area goes.
    sink: Option<Box<dyn WindowSink>>,
    /// What a presented frame sets, from the sink.
    presented: Option<Arc<AtomicBool>>,
    /// The caret's area last sent there; `None` before the first.
    sent: Option<Option<[f32; 4]>>,
    /// The launch screen, once drawn.
    splash: Option<Splash>,
    /// The target changed size since the text was last prepared: the
    /// page shown is prepared again before its next frame.
    stale: bool,
    /// The chosen chart's text as the loading presentation holds it.
    chosen: ChosenTextState,
    /// The strip's search line and notice over a selection skin.
    strip: Option<MenuText>,
    /// The strip's preset name, drawn while it shows.
    preset: Option<MenuText>,
    /// The strip's lines prepared.
    strip_lines: usize,
    /// The strip's lines last prepared, and whether the last selection
    /// frame drew the strip (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    strip_shown: (Vec<String>, bool),
    /// The strip's composition, underlined.
    strip_underline: Option<[f32; 4]>,
    /// What the selection skin's text and the strip were prepared for.
    selection_key: Option<SelectionKey>,
    /// The caret's area of the fallback page prepared with the selection
    /// skin, sent when a refusal draws the page.
    page_caret: Option<[f32; 4]>,
    /// The result strip's text: the one drawn, then the ones prepared for
    /// the commit point to take (the S10-09i design's D1), each with its
    /// lines' count.
    result_strip: [(Option<MenuText>, usize); 3],
    /// A frame's app work allocates once while set (tests: the allocation
    /// guards' positive control).
    #[cfg(all(test, feature = "dev-tools"))]
    alloc_probe: bool,
    /// The views the selection skin has taken (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    selection_views: u64,
}

/// One request for the chosen chart's text: the installation, size and
/// lifetime it is for (the installation by its key `K`), and the revision
/// of the fields it is prepared from.
#[derive(Clone, Debug)]
struct AskedText<K = LoadingTextKey> {
    key: K,
    target: [u32; 2],
    lifetime: u64,
    revision: u64,
    chart: Arc<ChosenChart>,
}

impl<K: PartialEq> AskedText<K> {
    /// Whether both are for the same installation, size and lifetime.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn same_place(&self, other: &Self) -> bool {
        self.key == other.key && self.target == other.target && self.lifetime == other.lifetime
    }

    /// Whether both are the same request.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn same_request(&self, other: &Self) -> bool {
        self.same_place(other) && self.revision == other.revision
    }
}

/// Where the chosen chart's text stands on this screen: the request whose
/// text every slot holds, the one staged for the next frame, and the last
/// one refused, which is not asked again.
#[derive(Debug, Default)]
struct ChosenTextState {
    /// What every slot holds; `None` when unknown, after a refusal.
    installed: Option<AskedText>,
    staged: Option<AskedText>,
    refused: Option<AskedText>,
}

/// What the chosen chart's text needs before a loading frame.
#[derive(Debug, PartialEq, Eq)]
enum TextPlan {
    /// Nothing: its text is installed, or it was refused.
    Keep,
    /// Clear the slots named, then prepare them.
    Prepare(TextSlots),
}

/// What request `asked` needs, the presentation holding `installed`'s
/// text, after `refused` was refused. A request installed or refused
/// needs nothing. In the place `installed` was prepared for, only the
/// fields that changed are cleared and prepared again; anywhere else, or
/// with what the slots hold unknown, every slot is cleared and prepared,
/// so no field kept from another installation, size or revision draws.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn text_plan<K: PartialEq>(
    installed: Option<&AskedText<K>>,
    refused: Option<&AskedText<K>>,
    asked: &AskedText<K>,
) -> TextPlan {
    if installed.is_some_and(|i| i.same_request(asked))
        || refused.is_some_and(|r| r.same_request(asked))
    {
        return TextPlan::Keep;
    }
    match installed {
        Some(i) if i.same_place(asked) => TextPlan::Prepare(fields_changed(&i.chart, &asked.chart)),
        _ => TextPlan::Prepare(TextSlots::ALL),
    }
}

/// The slots of the fields that differ between `old` and `new`, with the
/// composites they are part of: what a field change clears until its
/// replacement is installed.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn fields_changed(old: &ChosenChart, new: &ChosenChart) -> TextSlots {
    [
        (TextField::Title, &old.title, &new.title),
        (TextField::Subtitle, &old.subtitle, &new.subtitle),
        (TextField::Artist, &old.artist, &new.artist),
        (TextField::Subartist, &old.subartist, &new.subartist),
        (TextField::Genre, &old.genre, &new.genre),
    ]
    .into_iter()
    .filter(|(_, a, b)| a != b)
    .fold(TextSlots::NONE, |slots, (f, _, _)| {
        slots.with(TextSlots::field(f))
    })
}

impl MenuScreen {
    /// `renderer`, drawing menus in `fonts`; a screen for plays only has
    /// none, draws a black launch screen, and reads the font when it first
    /// has a page to draw.
    pub const fn new(renderer: Renderer, fonts: Option<Fonts>) -> Self {
        Self {
            renderer,
            fonts,
            text: None,
            cursor: None,
            underline: None,
            sink: None,
            presented: None,
            sent: None,
            splash: None,
            stale: false,
            chosen: ChosenTextState {
                installed: None,
                staged: None,
                refused: None,
            },
            strip: None,
            preset: None,
            strip_lines: 0,
            #[cfg(all(test, feature = "dev-tools"))]
            strip_shown: (Vec::new(), false),
            strip_underline: None,
            selection_key: None,
            page_caret: None,
            result_strip: [(None, 0), (None, 0), (None, 0)],
            #[cfg(all(test, feature = "dev-tools"))]
            alloc_probe: false,
            #[cfg(all(test, feature = "dev-tools"))]
            selection_views: 0,
        }
    }

    /// Brings the selection skin's text, the strip and the fallback page
    /// to `u` when it is not prepared for it on this screen: the page
    /// first (what a refusal draws), then the strip, then the skin's state
    /// and its lines. Allocates: on an update or a resize, outside a
    /// frame's build.
    ///
    /// # Errors
    ///
    /// The outer error is the device's or the font's: the screen is built
    /// again. The inner one is the skin's refusal of its text on a healthy
    /// device.
    fn prepare_selection(
        &mut self,
        u: &SelectionUpdate,
        logic: &mut SelectionLogic,
        changed: bool,
    ) -> Result<Result<(), String>, RenderError> {
        let key = SelectionKey {
            device: self.renderer.device_generation(),
            target: self.renderer.target(),
            lifetime: u.lifetime,
            revision: u.revision,
            state: logic.id,
        };
        if !changed && !self.stale && self.selection_key == Some(key) {
            return Ok(Ok(()));
        }
        self.selection_key = None;
        self.page_caret = self.prepare_page(&u.page, LEFT, TOP)?;
        let notice = logic.script_stop.map(crate::screens::ScriptStop::notice);
        self.prepare_strip(&u.strip, notice)?;
        let skin = Arc::clone(logic.bundle.skin());
        #[cfg(all(test, feature = "dev-tools"))]
        {
            self.selection_views += 1;
        }
        if let Err(e) = skin.update_selection(&mut logic.sel, &u.view, &u.labels) {
            return Ok(Err(e.to_string()));
        }
        match self.renderer.prepare_selection_text(&mut logic.sel) {
            Ok(()) => {}
            Err(e) if e.is_refusal() => return Ok(Err(game::Chain(&e).to_string())),
            Err(e) => return Err(e),
        }
        self.selection_key = Some(key);
        Ok(Ok(()))
    }

    /// The app's share of a selection frame of `u` at `now`: the skin's
    /// timers from the update's starts, the skin's frame and the strip's
    /// sprites staged, with no call into wgpu. Allocates nothing.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame: nothing is staged.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn build_selection_frame(
        &mut self,
        u: &SelectionUpdate,
        logic: &mut SelectionLogic,
        now: bms_model::TimeUs,
    ) -> Result<(), BuildError> {
        #[cfg(all(test, feature = "dev-tools"))]
        self.probe_alloc();
        let mut sprites = [Sprite::default(); render::STRIP_SPRITES];
        let n = strip_sprites(
            self.strip_lines,
            u.events.preset_shown(now),
            self.strip_underline,
            &mut sprites,
        );
        let frame = skin_model::SelectionFrame {
            now,
            target: self.renderer.target(),
            layout: &skin_model::Layout::AUTHOR,
            timers: u.events.timers(),
        };
        // The frame's preparation ends here (dev-tools: the S7-07d
        // gate's whole frames).
        #[cfg(feature = "dev-tools")]
        crate::dev::mark_build();
        self.renderer
            .build_selection(&mut logic.sel, &frame, sprites.get(..n).unwrap_or_default())
    }

    /// Brings the loading presentation's chosen-chart text to `update`.
    /// It runs inside [`loading`](Screen::loading), before the loading
    /// frame is built, on the render thread before recording opens: the
    /// presentation enters the update's lifetime, the text staged by the
    /// previous loading frame is installed (dropped if its installation,
    /// lifetime or revision has passed), and the text the update needs
    /// ([`text_plan`]) is cleared and prepared through the installation's
    /// producer, staged for the next loading frame. A request refused,
    /// by the producer or by its table, is not asked again until text
    /// installs after it; what the slots hold is then unknown, so the next
    /// request prepares every slot. Preparing allocates: it runs only when
    /// the update needs new text, never in a frame's build.
    ///
    /// # Errors
    ///
    /// The device failing while the text is prepared (not a refusal,
    /// [`RenderError::is_refusal`]): the screen is dropped and built again.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn chosen_text(&mut self, update: &LoadingUpdate, lifetime: u64) -> Result<(), RenderError> {
        self.renderer.enter_loading(lifetime);
        if let Some(staged) = self.chosen.staged.take() {
            match self.renderer.install_staged_loading_text() {
                // Every slot holds what was asked: a refusal before it no
                // longer describes them, so it is forgotten.
                Ok(true) => {
                    self.chosen.installed = Some(staged);
                    self.chosen.refused = None;
                }
                Ok(false) => {}
                Err(e @ LoadingTextError::Table(_)) => {
                    eprintln!("render: the chosen chart's text was not installed: {e}");
                    self.chosen.installed = None;
                    self.chosen.refused = Some(staged);
                }
                // Its installation, lifetime or revision has passed: what
                // the update needs is planned below.
                Err(_) => {}
            }
        }
        let Some(key) = self.renderer.loading_text_key() else {
            self.chosen = ChosenTextState::default();
            return Ok(());
        };
        let asked = AskedText {
            key,
            target: self.renderer.target(),
            lifetime,
            revision: update.revision,
            chart: Arc::clone(&update.chart),
        };
        let TextPlan::Prepare(which) = text_plan(
            self.chosen.installed.as_ref(),
            self.chosen.refused.as_ref(),
            &asked,
        ) else {
            return Ok(());
        };
        self.renderer.clear_loading_text(which);
        let c = &update.chart;
        fn field(f: &Option<String>) -> &str {
            f.as_deref().unwrap_or_default()
        }
        let chart = ChartText {
            title: field(&c.title),
            subtitle: field(&c.subtitle),
            artist: field(&c.artist),
            subartist: field(&c.subartist),
            genre: field(&c.genre),
        };
        match self
            .renderer
            .prepare_loading_text(lifetime, update.revision, which, &chart)
        {
            Ok(()) => self.chosen.staged = Some(asked),
            Err(e) if e.is_refusal() => {
                eprintln!(
                    "render: the chosen chart's text was not prepared: {}",
                    game::Chain(&e)
                );
                self.chosen.installed = None;
                self.chosen.refused = Some(asked);
            }
            // The device failed: the screen is dropped and built again.
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Tells `sink` of its first frame presented and the caret's area of
    /// each page prepared, and draws a splash page until told what to draw.
    #[must_use]
    pub fn with_sink(mut self, sink: Box<dyn WindowSink>) -> Self {
        self.presented = Some(sink.presented());
        self.sink = Some(sink);
        self
    }

    /// Lays `page` out from `left` and `top` and prepares it as this
    /// renderer's text.
    ///
    /// # Errors
    ///
    /// As the renderer's text, and [`Fonts::runtime`] on a screen for plays
    /// only, whose first page is the first that needs a font.
    fn prepare(&mut self, page: &MenuPage, left: f32, top: f32) -> Result<(), RenderError> {
        let area = self.prepare_page(page, left, top)?;
        self.send(area);
        Ok(())
    }

    /// [`prepare`](Self::prepare), its caret's area returned instead of
    /// sent.
    fn prepare_page(
        &mut self,
        page: &MenuPage,
        left: f32,
        top: f32,
    ) -> Result<Option<[f32; 4]>, RenderError> {
        let fonts = match &mut self.fonts {
            Some(fonts) => fonts,
            none => none.insert(Fonts::runtime()?),
        };
        let (blocks, area) = lay_out(fonts, page, left, top);
        let refs: Vec<&TextBlock> = blocks.iter().collect();
        let text = self
            .text
            .get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &refs)?;
        self.stale = false;
        self.cursor = page
            .lines
            .iter()
            .take(PAGE_LINES)
            .position(|l| l.style == LineStyle::Cursor);
        self.underline = underline(page, area);
        Ok(area)
    }

    /// The runtime of `logic`, the selection state shown, stopped: the
    /// stop is kept in the state, whose strip shows its notice from this
    /// frame for as long as the state lasts (a screen built again lays it
    /// out from the state), and said once on standard error.
    #[cold]
    fn script_stopped(
        &mut self,
        u: &SelectionUpdate,
        logic: &mut SelectionLogic,
        stop: crate::screens::ScriptStop,
    ) -> Result<(), RenderError> {
        eprintln!("render: selection skin: {}", stop.notice());
        logic.script_stop = Some(stop);
        self.prepare_strip(&u.strip, Some(stop.notice()))
    }

    /// Lays out the strip `strip` over a selection skin: its search line
    /// and notice from the top, the preset's name in a text of its own
    /// under them, which a frame draws only while it shows; the caret's
    /// area goes to the window. `script`, the runtime's stop notice, is
    /// the notice line while `strip` has none.
    fn prepare_strip(
        &mut self,
        strip: &StripText,
        script: Option<&str>,
    ) -> Result<(), RenderError> {
        let fonts = match &mut self.fonts {
            Some(fonts) => fonts,
            none => none.insert(Fonts::runtime()?),
        };
        let mut page = strip.page();
        if let Some(text) = script
            && strip.notice.is_none()
        {
            page.lines.push(MenuLine {
                text: text.to_owned(),
                style: LineStyle::Notice,
            });
        }
        #[cfg(all(test, feature = "dev-tools"))]
        {
            self.strip_shown.0 = page.lines.iter().map(|l| l.text.clone()).collect();
        }
        let (blocks, area) = lay_out(fonts, &page, LEFT, TOP);
        let refs: Vec<&TextBlock> = blocks.iter().collect();
        let text = self
            .strip
            .get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &refs)?;
        let preset = MenuPage {
            lines: vec![MenuLine {
                text: strip.preset.clone(),
                style: LineStyle::Title,
            }],
            caret: None,
        };
        let below = lines_from(TOP, page.lines.len());
        let (blocks, _) = lay_out(fonts, &preset, LEFT, below);
        let refs: Vec<&TextBlock> = blocks.iter().collect();
        let text = self
            .preset
            .get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &refs)?;
        self.strip_lines = page.lines.len();
        self.strip_underline = underline(&page, area);
        self.send(area);
        Ok(())
    }

    /// Sends the caret's `area` to the sink when it changed.
    fn send(&mut self, area: Option<[f32; 4]>) {
        if self.sent == Some(area) {
            return;
        }
        if let Some(sink) = &self.sink {
            sink.caret(area);
        }
        self.sent = Some(area);
    }

    /// The strip's lines the next result frame draws.
    fn result_strip(&self) -> usize {
        self.result_strip
            .first()
            .map_or(0, |(t, n)| if t.is_some() { *n } else { 0 })
    }

    /// Its renderer, for the tests that draw on it directly.
    #[cfg(test)]
    pub(crate) const fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Tells the window a frame was `presented`, with nothing allocated (a
    /// play's frame calls it); passes it on.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn shown(&self, presented: bool) -> bool {
        if presented && let Some(flag) = &self.presented {
            flag.store(true, Ordering::Release);
        }
        presented
    }
}

impl Screen for MenuScreen {
    fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError> {
        self.renderer.load(chart, section)
    }

    /// Stages `skin` with the attempt's fresh state, or readies the skin
    /// in use in place, answering at once; a refusal on a healthy device
    /// ([`RenderError::is_refusal`]) refuses the attempt and keeps the
    /// installation in use.
    fn prepare_attempt(
        &mut self,
        a: &AttemptInput<'_>,
        skin: Option<&Arc<SkinBundle>>,
        request: u64,
    ) -> Option<Result<Preparation, RenderError>> {
        let done = match skin {
            Some(bundle) => self.renderer.stage(bundle, a, request).map(|_| ()),
            None => self.renderer.prepare_attempt(a),
        };
        Some(match done {
            Ok(()) => Ok(Preparation::Ready),
            // The whole chain: the skin's or the device's own reason too.
            Err(e) if e.is_refusal() => Ok(refusal(game::Chain(&e).to_string())),
            Err(e) => Err(e),
        })
    }

    fn device(&self) -> u64 {
        self.renderer.device_generation()
    }

    fn commit(&mut self, token: PreparedToken) -> Result<(), CommitError> {
        if self.renderer.staged().is_some() {
            self.renderer.commit(token)
        } else if token.device == self.renderer.device_generation() {
            Ok(())
        } else {
            Err(CommitError::Device)
        }
    }

    fn discard(&mut self) {
        self.renderer.discard();
    }

    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        self.renderer.publish_chart_images(target, images)
    }

    fn publish_chart_absent(
        &mut self,
        target: render::ChartTarget,
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        self.renderer.publish_chart_absent(target)
    }

    fn invalidate_chart_images(&mut self) {
        self.renderer.invalidate_chart_images();
    }

    /// Stages `skin`'s chart-independent installation of `kind`,
    /// answering at once; a refusal on a healthy device
    /// ([`RenderError::is_refusal`]), the texels the screens share
    /// included, keeps every installation in use.
    fn install(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<SkinBundle>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        match self.renderer.install(kind, skin, request) {
            Ok(_) => Ok(Preparation::Ready),
            Err(e) if e.is_refusal() => Ok(refusal(game::Chain(&e).to_string())),
            Err(e) => Err(e),
        }
    }

    fn installation(&self, kind: ScreenKind) -> Option<Installation> {
        self.renderer.installation(kind)
    }

    fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &Installation,
    ) -> Result<(), RenderError> {
        self.renderer.restore(kind, installation)
    }

    fn remove(&mut self, kind: ScreenKind) {
        self.renderer.remove(kind);
    }

    fn install_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        match self.renderer.install_pages(kind, pages, request) {
            Ok(_) => Ok(Preparation::Ready),
            Err(e) if e.is_refusal() => Ok(refusal(game::Chain(&e).to_string())),
            Err(e) => Err(e),
        }
    }

    fn restore_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
    ) -> Result<(), RenderError> {
        self.renderer.restore_pages(kind, pages)
    }

    fn disable_result(&mut self) {
        self.renderer.disable_result();
    }

    fn drain_retired(&mut self) -> Option<skin_model::AtlasId> {
        self.renderer.drain_retired()
    }

    fn installed_pages(&self, kind: ScreenKind) -> Option<Arc<skin_model::PackedPages>> {
        self.renderer.installed_pages(kind).cloned()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn retained_atlases(&self, kind: ScreenKind) -> [Option<skin_model::AtlasId>; 3] {
        self.renderer.retained_atlases(kind)
    }

    fn prepare_result(
        &mut self,
        skin: &Arc<SkinBundle>,
        state: &mut skin_model::ResultScreen,
        view: &skin_model::ResultView,
        labels: &skin_model::ResultLabels,
    ) -> Result<Result<(), String>, RenderError> {
        if let Err(e) = skin.skin().update_result(state, view, labels) {
            return Ok(Err(e.to_string()));
        }
        match self.renderer.prepare_result_text(state) {
            Ok(()) => Ok(Ok(())),
            Err(e) if e.is_refusal() => Ok(Err(game::Chain(&e).to_string())),
            Err(e) => Err(e),
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn install_result_text(&mut self) -> bool {
        self.renderer.install_staged_result_text()
    }

    fn prepare_result_strip(&mut self, which: usize, lines: &[&str]) -> Result<(), RenderError> {
        let fonts = match &mut self.fonts {
            Some(fonts) => fonts,
            none => none.insert(Fonts::runtime()?),
        };
        let page = MenuPage {
            lines: lines
                .iter()
                .map(|l| MenuLine {
                    text: (*l).to_owned(),
                    style: LineStyle::Notice,
                })
                .collect(),
            caret: None,
        };
        let (blocks, _) = lay_out(fonts, &page, LEFT, TOP);
        let refs: Vec<&TextBlock> = blocks.iter().collect();
        let Some((text, n)) = self.result_strip.get_mut(which) else {
            return Ok(());
        };
        let text = text.get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &refs)?;
        *n = lines.len().min(PAGE_LINES);
        Ok(())
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn use_result_strip(&mut self, which: usize) {
        if let [drawn, rest @ ..] = &mut self.result_strip
            && let Some(slot) = which.checked_sub(1).and_then(|i| rest.get_mut(i))
        {
            std::mem::swap(drawn, slot);
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn build_result(
        &mut self,
        state: &mut skin_model::ResultScreen,
        frame: &skin_model::ResultFrame<'_>,
    ) -> Result<(), BuildError> {
        #[cfg(all(test, feature = "dev-tools"))]
        self.probe_alloc();
        let [(text, lines), ..] = &self.result_strip;
        let lines = if text.is_some() { *lines } else { 0 };
        let mut sprites = [Sprite::default(); render::RESULT_STRIP_SPRITES];
        let n = result_strip_sprites(lines, &mut sprites);
        self.renderer
            .build_result(state, frame, sprites.get(..n).unwrap_or_default())
    }

    fn submit_result(&mut self) -> Result<FrameTiming, RenderError> {
        let lines = self.result_strip();
        let timing = match self.result_strip.first().and_then(|(t, _)| t.as_ref()) {
            Some(t) if lines > 0 => self.renderer.submit_result(&[t]),
            _ => self.renderer.submit_result(&[]),
        }?;
        self.shown(timing.presented);
        Ok(timing)
    }

    #[cfg(feature = "dev-tools")]
    fn atlas_uploads(&self) -> u64 {
        self.renderer.atlas_uploads()
    }

    #[cfg(feature = "dev-tools")]
    fn take_upload_timing(&mut self) -> Option<render::UploadTiming> {
        self.renderer.take_upload_timing()
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn result_quads(&self) -> Option<Vec<skin_model::Quad>> {
        self.renderer.result_quads()
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn result_strip(&self) -> usize {
        MenuScreen::result_strip(self)
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn refuse_next_result(&mut self, text: bool) {
        self.renderer.refuse_next_result(text);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait(&self) -> bool {
        self.renderer.wait_for_frame(100)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn paced(&self) -> bool {
        self.renderer.has_frame_waitable()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn rearm(&mut self) {
        self.renderer.rearm();
    }

    fn carry(&self) -> Option<skin_model::Carry<'_>> {
        self.renderer.carry()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        self.renderer.build(f)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        let timing = self.renderer.submit()?;
        self.shown(timing.presented);
        Ok(timing)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn statistics(&self) -> Option<render::measure::FrameStatistics> {
        self.renderer.frame_statistics()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn presentation(&self) -> Option<render::measure::Presentation> {
        self.renderer.presentation()
    }

    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
        let presented = self.renderer.text_frame(lines)?.presented;
        Ok(self.shown(presented))
    }

    fn end(&mut self, page: &crate::render_side::EndPage) -> Result<bool, RenderError> {
        let presented = self
            .renderer
            .result_frame(&page.lines, page.graphs.as_deref())?
            .presented;
        Ok(self.shown(presented))
    }

    fn splash(&mut self, elapsed: Duration) -> Option<Result<bool, RenderError>> {
        self.sink.as_ref()?;
        Some(if self.fonts.is_some() {
            self.fade_splash(splash_alpha(elapsed))
                .and_then(|()| self.splash_frame())
        } else {
            // A screen for plays only: black, so the window shows.
            self.text(&[])
        })
    }

    /// The skin's loading frame `frame`, then the lines over it. The
    /// chosen chart's text is brought to the lifetime's state `update`
    /// ([`chosen_text`](Self::chosen_text)) and the lines are laid out
    /// before the frame is built, outside its work.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn loading(
        &mut self,
        page: &MenuPage,
        update: Option<&LoadingUpdate>,
        changed: bool,
        frame: &LoadingFrame<'_>,
    ) -> Result<Result<FrameTiming, BuildError>, RenderError> {
        if let Some(update) = update {
            self.chosen_text(update, frame.lifetime)?;
        }
        if changed || self.stale || self.text.is_none() {
            self.prepare(page, LOADING_LEFT, LOADING_TOP)?;
        }
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        let timing = self.renderer.loading_frame(frame, text)?;
        if let Ok(t) = &timing {
            self.shown(t.presented);
        }
        Ok(timing)
    }

    /// The cleared canvas with the load's lines where a skin's loading
    /// frame has them; no skin's frame.
    fn pending_loading(
        &mut self,
        page: &MenuPage,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        if changed || self.stale || self.text.is_none() {
            self.prepare(page, LOADING_LEFT, LOADING_TOP)?;
        }
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        let timing = self.renderer.menu_frame(|_| {}, text)?;
        self.shown(timing.presented);
        Ok(timing)
    }

    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let timing = self.menu_frame(page, changed)?;
        self.shown(timing.presented);
        Ok(timing)
    }

    /// The skin installed for selection draws `u` with `logic`, the strip
    /// over it; without one (or with a state of another installation) the
    /// page draws. A refusal of the skin's text or frame draws the page
    /// prepared with the update in the same frame, which prepares nothing
    /// more.
    fn selection(
        &mut self,
        u: &SelectionUpdate,
        logic: Option<&mut SelectionLogic>,
        changed: bool,
        now: bms_model::TimeUs,
    ) -> Result<SelectionDrawn, RenderError> {
        let installed = self.renderer.selection_bundle().cloned();
        let logic = logic.filter(|l| installed.is_some_and(|b| Arc::ptr_eq(&b, &l.bundle)));
        let Some(logic) = logic else {
            // A page laid out for the skin's screen sent no caret: it is
            // laid out again with its own.
            let changed = changed || self.selection_key.take().is_some();
            let timing = self.menu(&u.page, changed)?;
            return Ok(SelectionDrawn {
                timing,
                refused: None,
            });
        };
        let refused = match self.prepare_selection(u, logic, changed)? {
            // The frame's timers, then the runtime over that state, then
            // the build from it (docs/stage3.md S7-07).
            Ok(()) => match crate::screens::selection_visit(
                &mut logic.script,
                logic.bundle.skin(),
                &mut logic.sel,
                now,
                &u.events.timers(),
            ) {
                Ok(stop) => {
                    if let Some(stop) = stop {
                        self.script_stopped(u, logic, stop)?;
                    }
                    self.build_selection_frame(u, logic, now)
                        .err()
                        .map(|e| e.to_string())
                }
                Err(e) => Some(e.to_string()),
            },
            Err(why) => Some(why),
        };
        if refused.is_some() {
            // The page prepared with the update draws, its caret with it
            // in place of the strip's.
            self.selection_key = None;
            self.send(self.page_caret);
            let timing = self.menu_frame(&u.page, false)?;
            self.shown(timing.presented);
            return Ok(SelectionDrawn { timing, refused });
        }
        let preset = u.events.preset_shown(now);
        let texts = [
            self.strip.as_ref().filter(|_| self.strip_lines > 0),
            self.preset.as_ref().filter(|_| preset),
        ];
        let mut drawn: [Option<&MenuText>; 2] = [None; 2];
        let mut n = 0;
        for t in texts.into_iter().flatten() {
            if let Some(slot) = drawn.get_mut(n) {
                *slot = Some(t);
                n += 1;
            }
        }
        #[cfg(all(test, feature = "dev-tools"))]
        {
            self.strip_shown.1 = drawn[0].is_some() && self.strip_lines > 0;
        }
        let timing = match drawn {
            [Some(a), Some(b)] => self.renderer.submit_selection(&[a, b]),
            [Some(a), None] => self.renderer.submit_selection(&[a]),
            _ => self.renderer.submit_selection(&[]),
        }?;
        self.shown(timing.presented);
        Ok(SelectionDrawn {
            timing,
            refused: None,
        })
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_requests(&self) -> (Vec<String>, usize) {
        let (lines, rasterized) = self.renderer.selection_requests();
        (lines.to_vec(), rasterized)
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_strip(&self) -> (Vec<String>, bool) {
        self.strip_shown.clone()
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_quads(&self) -> Option<Vec<skin_model::Quad>> {
        self.renderer.selection_quads()
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_observations(&self) -> Option<Vec<skin_model::ElementObservation>> {
        self.renderer.selection_observations()
    }

    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_views(&self) -> u64 {
        self.selection_views
    }

    /// A new size invalidates the text prepared: the page shown, or the
    /// launch screen, is prepared again before its next frame.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn resize(&mut self, width: u32, height: u32) {
        let before = self.renderer.target();
        self.renderer.resize(width, height);
        if self.renderer.target() != before {
            self.stale = true;
            if let Some(splash) = &mut self.splash {
                splash.alpha = None;
            }
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn target(&self) -> [u32; 2] {
        self.renderer.target()
    }

    fn atlas_limits(&self) -> Option<skin_model::AtlasLimits> {
        Some(self.renderer.atlas_limits())
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn minimized(&self) -> bool {
        self.renderer.is_minimized()
    }

    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = self.renderer.size();
        let pixels = self.renderer.read_back();
        (!pixels.is_empty()).then_some((w, h, pixels))
    }

    #[cfg(feature = "dev-tools")]
    fn display_list(&self) -> Option<skin_model::DisplayList> {
        self.renderer.display_list()
    }
}

impl MenuScreen {
    /// Prepares the launch screen at opacity `alpha`, laid out the first
    /// time only and prepared again only when `alpha` changed.
    fn fade_splash(&mut self, alpha: u8) -> Result<(), RenderError> {
        let Some(fonts) = self.fonts.as_mut() else {
            return Ok(());
        };
        let splash = self.splash.get_or_insert_with(|| Splash::new(fonts));
        if splash.alpha == Some(alpha) {
            return Ok(());
        }
        splash.name.set_alpha(alpha);
        splash.version.set_alpha(alpha);
        let text = self
            .text
            .get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &[&splash.name, &splash.version])?;
        splash.alpha = Some(alpha);
        self.stale = false;
        // The page prepared before, if any, is gone from the text.
        self.cursor = None;
        self.underline = None;
        Ok(())
    }

    /// Draws the launch screen as last prepared.
    fn splash_frame(&mut self) -> Result<bool, RenderError> {
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        let presented = self.renderer.menu_frame(|_| {}, text)?.presented;
        Ok(self.shown(presented))
    }

    /// Draws `page`, laid out again when `changed`.
    fn menu_frame(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        if changed || self.stale || self.text.is_none() {
            self.prepare(page, LEFT, TOP)?;
        }
        let start = self.build_page_frame();
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        self.renderer.submit_menu(start, text)
    }

    /// The app's share of a frame of the page prepared last: its cursor
    /// and composition sprites staged, with no call into wgpu. Allocates
    /// nothing. Returns when the frame's work began.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn build_page_frame(&mut self) -> std::time::Instant {
        #[cfg(all(test, feature = "dev-tools"))]
        self.probe_alloc();
        let cursor = self.cursor;
        let underline = self.underline;
        self.renderer.build_menu(|b| {
            if let Some(i) = cursor {
                b.push(
                    Blend::Alpha,
                    Sprite {
                        rect: [LEFT - 20.0, line_top(i) - 3.0, 1_840.0, LINE_STEP - 2.0],
                        uv: solid_uv(),
                        color: [0.15, 0.3, 0.6, 1.0],
                        ..Sprite::default()
                    },
                );
            }
            if let Some(rect) = underline {
                b.push(
                    Blend::Alpha,
                    Sprite {
                        rect,
                        uv: solid_uv(),
                        color: [1.0; 4],
                        ..Sprite::default()
                    },
                );
            }
        })
    }

    /// Allocates once when the probe is set (tests): the positive control
    /// of the allocation guards over a frame's app work.
    #[cfg(all(test, feature = "dev-tools"))]
    fn probe_alloc(&self) {
        if self.alloc_probe {
            drop(std::hint::black_box(Box::new(0_u8)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// A request installed or refused needs nothing; in the place its
    /// text was installed for, only the changed fields are cleared and
    /// prepared; for another installation, size or lifetime, or with
    /// what the slots hold unknown, every slot is cleared, so a field
    /// that became unavailable never draws from what they kept.
    #[test]
    fn a_request_clears_what_it_cannot_vouch_for() {
        use std::sync::Arc;

        use skin_model::{TextField, TextSlots};

        use super::{AskedText, TextPlan, text_plan};
        use crate::loading_text::ChosenChart;

        let chart = |artist: Option<&str>| {
            Arc::new(ChosenChart {
                title: Some("T".to_owned()),
                artist: artist.map(str::to_owned),
                ..ChosenChart::default()
            })
        };
        let asked = |key: u64, target: [u32; 2], revision: u64, artist: Option<&str>| AskedText {
            key,
            target,
            lifetime: 1,
            revision,
            chart: chart(artist),
        };
        let full = [1_920, 1_080];
        let installed = asked(1, full, 1, Some("x"));
        let all = TextPlan::Prepare(TextSlots::ALL);
        assert_eq!(
            text_plan(Some(&installed), None, &installed.clone()),
            TextPlan::Keep
        );
        let artist = TextSlots::field(TextField::Artist);
        assert_eq!(
            text_plan(Some(&installed), None, &asked(1, full, 2, None)),
            TextPlan::Prepare(artist)
        );
        // Another size, another installation, nothing known.
        assert_eq!(
            text_plan(Some(&installed), None, &asked(1, [960, 540], 2, None)),
            all
        );
        assert_eq!(
            text_plan(Some(&installed), None, &asked(2, full, 1, Some("x"))),
            all
        );
        assert_eq!(text_plan(None, None, &asked(1, full, 2, None)), all);
        // A refused request is not asked again; the next one is.
        let refused = asked(1, full, 2, Some("y"));
        assert_eq!(
            text_plan(None, Some(&refused), &refused.clone()),
            TextPlan::Keep
        );
        assert_eq!(
            text_plan(None, Some(&refused), &asked(1, full, 3, Some("y"))),
            all
        );
    }

    /// Only the fields that differ name their slots, each with the
    /// composites it is part of; equal fields name none, and a field
    /// becoming unavailable names its own.
    #[test]
    fn only_changed_fields_name_their_slots() {
        use skin_model::{TextField, TextSlots};

        use super::fields_changed;
        use crate::loading_text::ChosenChart;

        let chart = ChosenChart {
            title: Some("T".to_owned()),
            subtitle: Some("s".to_owned()),
            artist: Some("a".to_owned()),
            subartist: None,
            genre: Some("g".to_owned()),
            play_level: Some(12),
            difficulty: Some(4),
            bga: Some(true),
            length_us: Some(bms_model::TimeUs(125_400_000)),
        };
        assert_eq!(fields_changed(&chart, &chart.clone()), TextSlots::NONE);
        // The play level, the difficulty, the BGA flag and the length are
        // no text: they name no slot.
        let level = ChosenChart {
            play_level: None,
            difficulty: None,
            bga: None,
            length_us: None,
            ..chart.clone()
        };
        assert_eq!(fields_changed(&chart, &level), TextSlots::NONE);
        let artist = ChosenChart {
            artist: Some("b".to_owned()),
            ..chart.clone()
        };
        assert_eq!(
            fields_changed(&chart, &artist),
            TextSlots::field(TextField::Artist)
        );
        let gone = ChosenChart {
            subtitle: None,
            genre: None,
            ..chart.clone()
        };
        assert_eq!(
            fields_changed(&chart, &gone),
            TextSlots::field(TextField::Subtitle).with(TextSlots::field(TextField::Genre))
        );
    }

    use render::text::Fonts;
    use render::{AdapterChoice, Renderer, Target};

    use super::{LINE_STEP, MenuScreen, WindowSink, caret_area, line_top};
    use crate::render_side::Screen;
    use crate::select::{MenuInput, MenuKey, SelectScreen};

    /// A select screen whose search box has focus, holding `typed` and
    /// composing `composing`.
    fn searching(typed: &str, composing: &str) -> SelectScreen {
        let mut s = SelectScreen::new(Vec::new());
        s.input(MenuInput::Key(MenuKey::Tab));
        s.input(MenuInput::Text(typed.to_owned()));
        s.input(MenuInput::Preedit(composing.to_owned()));
        s
    }

    /// The caret's areas a screen sent.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<Option<[f32; 4]>>>>, Arc<AtomicBool>);

    impl WindowSink for Sent {
        fn caret(&self, area: Option<[f32; 4]>) {
            self.0.lock().unwrap().push(area);
        }

        fn presented(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.1)
        }
    }

    fn warp() -> Renderer {
        Renderer::new(
            Target::Offscreen {
                width: 1920,
                height: 1080,
            },
            AdapterChoice::Warp,
        )
        .unwrap()
    }

    /// The launch screen fades in from black over 0.3 s, holds, and fades
    /// out over its last 0.3 s, black after.
    #[test]
    fn the_splash_fades_in_and_out() {
        use std::time::Duration;
        let at = |ms| super::splash_alpha(Duration::from_millis(ms));
        assert_eq!(at(0), 0);
        assert_eq!(at(150), 127);
        assert_eq!(at(300), 255);
        assert_eq!(at(750), 255);
        assert_eq!(at(1_200), 255);
        assert_eq!(at(1_350), 127);
        assert_eq!(at(1_500), 0);
        assert_eq!(at(5_000), 0);
    }

    /// A screen with a window to tell draws the launch screen, the game's
    /// name at the opacity of its time, and its presented frame tells the
    /// window, black ones included; one for plays only draws black, and
    /// one without a window draws no launch screen.
    #[test]
    fn the_splash_tells_the_window_of_its_frame() {
        use std::time::Duration;
        let lit = |screen: &MenuScreen| {
            screen
                .renderer
                .read_back()
                .chunks(4)
                .filter(|p| p[0] > 200)
                .count()
        };
        let sent = Sent::default();
        let fonts = Some(Fonts::runtime().unwrap());
        let mut screen = MenuScreen::new(warp(), fonts).with_sink(Box::new(sent.clone()));
        assert!(!sent.1.load(Ordering::Acquire));
        assert!(matches!(screen.splash(Duration::ZERO), Some(Ok(true))));
        assert_eq!(lit(&screen), 0);
        assert!(sent.1.load(Ordering::Acquire));
        assert!(matches!(
            screen.splash(Duration::from_millis(750)),
            Some(Ok(true))
        ));
        assert!(lit(&screen) > 100);
        // A page after it draws the page alone.
        let page = SelectScreen::new(Vec::new()).page("");
        screen.menu(&page, true).unwrap();

        let sent = Sent::default();
        let mut screen = MenuScreen::new(warp(), None).with_sink(Box::new(sent.clone()));
        assert!(matches!(
            screen.splash(Duration::from_millis(750)),
            Some(Ok(true))
        ));
        assert_eq!(lit(&screen), 0);
        assert!(sent.1.load(Ordering::Acquire));
        let mut screen = MenuScreen::new(warp(), None);
        assert!(screen.splash(Duration::ZERO).is_none());
    }

    /// A screen for plays only draws the skin's wait and the load's lines
    /// on the cleared canvas: it reads the font for the first page it has.
    #[test]
    fn a_screen_for_plays_only_draws_its_pages() {
        let lit = |screen: &MenuScreen| {
            screen
                .renderer
                .read_back()
                .chunks(4)
                .filter(|p| p[0] > 200 || p[1] > 200)
                .count()
        };
        let mut screen = MenuScreen::new(warp(), None);
        let wait = crate::loading_text::skin_wait_page();
        screen.menu(&wait, true).unwrap();
        assert!(lit(&screen) > 100);

        let mut screen = MenuScreen::new(warp(), None);
        screen.pending_loading(&wait, true).unwrap();
        assert!(lit(&screen) > 100);
    }

    /// A screen for plays only opens no font for what shows no page, its
    /// launch screen and its text frames: only its first page does.
    #[test]
    fn a_screen_for_plays_only_opens_no_font_without_a_page() {
        use std::time::Duration;
        let mut screen = MenuScreen::new(warp(), None).with_sink(Box::new(Sent::default()));
        assert!(matches!(
            screen.splash(Duration::from_millis(750)),
            Some(Ok(true))
        ));
        screen.text(&["play"]).unwrap();
        assert!(screen.fonts.is_none());
        let wait = crate::loading_text::skin_wait_page();
        screen.menu(&wait, true).unwrap();
        assert!(screen.fonts.is_some());
    }

    /// Fading the launch screen lays nothing out again: once its glyphs are
    /// in the atlas, a step allocates only inside wgpu's write of the
    /// glyphs' vertices (its staging, as every frame's sprite upload has),
    /// a few, where laying the text out would allocate per glyph; a frame
    /// at the same opacity prepares nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_splash_fade_lays_nothing_out_again_alloc() {
        let _warp = crate::warp_turn::take();
        let mut screen = MenuScreen::new(warp(), Some(Fonts::runtime().unwrap()));
        screen.fade_splash(255).unwrap();
        screen.fade_splash(254).unwrap();
        let before = crate::alloc_count::this_thread();
        for _ in 0..100 {
            screen.fade_splash(254).unwrap();
        }
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
        let mut steps = Vec::with_capacity(254);
        for alpha in (0..=253).rev() {
            let before = crate::alloc_count::this_thread();
            screen.fade_splash(alpha).unwrap();
            steps.push(crate::alloc_count::this_thread() - before);
        }
        assert!(steps.iter().all(|&n| n <= 8), "{steps:?}");
    }

    /// The search box's caret lies after the typed text and spans the
    /// composition shown there, measured on its line as laid out (here 30
    /// layout pixels a character).
    #[test]
    fn the_caret_spans_the_composition_in_the_search_line() {
        let by_chars = |s: &str| 30.0 * s.chars().count() as f32;
        let page = searching("ab", "にほ").page("");
        assert_eq!(page.lines[1].text, "検索: abにほ_");
        // "検索: ab" is 6 characters.
        assert_eq!(
            caret_area(&page, by_chars),
            Some([60.0 + 180.0, line_top(1), 60.0, LINE_STEP])
        );
        // Nothing composed: the caret, a pixel wide.
        let page = searching("ab", "").page("");
        assert_eq!(
            caret_area(&page, by_chars),
            Some([240.0, line_top(1), 1.0, LINE_STEP])
        );
        // No focus, no caret.
        let page = SelectScreen::new(Vec::new()).page("");
        assert_eq!(caret_area(&page, by_chars), None);
    }

    /// Drawing the page measures the caret in the font, sends its area to
    /// the window once per change, and underlines the composition.
    #[test]
    fn a_drawn_page_sends_its_caret_and_underlines_the_composition() {
        let sent = Sent::default();
        let mut screen = MenuScreen::new(warp(), Some(Fonts::runtime().unwrap()))
            .with_sink(Box::new(sent.clone()));
        let page = searching("ab", "にほ").page("");
        screen.menu(&page, true).unwrap();
        screen.menu(&page, true).unwrap();
        let areas = sent.0.lock().unwrap().clone();
        assert_eq!(areas.len(), 1, "{areas:?}");
        let [x, y, w, h] = areas[0].unwrap();
        // After "検索: ab", over the two kana: about a character each.
        assert!(x > 60.0 + 100.0 && x < 60.0 + 200.0, "{x}");
        assert!(w > 40.0 && w < 70.0, "{w}");
        assert_eq!((y, h), (line_top(1), LINE_STEP));

        // The underline: a white row under the composition.
        let pixels = screen.renderer.read_back();
        let row = (y + h - 5.0) as usize;
        let white = (x as usize + 1..(x + w) as usize - 1)
            .filter(|&col| {
                let i = (row * 1920 + col) * 4;
                pixels[i..i + 3].iter().all(|&c| c > 200)
            })
            .count();
        assert!(white as f32 > (w - 2.0) * 0.5, "{white} of {w}");

        // Leaving the box clears the caret.
        let page = SelectScreen::new(Vec::new()).page("");
        screen.menu(&page, true).unwrap();
        assert_eq!(sent.0.lock().unwrap().last(), Some(&None));
    }

    #[cfg(feature = "dev-tools")]
    mod skinned;
}
