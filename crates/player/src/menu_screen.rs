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
use skin_model::{BuildError, ChartText, TextField, TextSlots};

use crate::loading_state::LoadingUpdate;
use crate::loading_text::ChosenChart;
use crate::render_side::{Screen, refusal};

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

/// Where the load's lines start on the play screen while a chart loads:
/// right of the lanes, under the measurement overlay's place.
const LOADING_LEFT: f32 = 1_000.0;
const LOADING_TOP: f32 = 400.0;

/// The renderer, with the select screen's text when it draws menus.
pub struct MenuScreen {
    renderer: Renderer,
    /// The font, when the screen draws menus; read once per screen.
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
    /// The chosen chart's text last asked of the renderer for a loading
    /// lifetime; `None` before any.
    asked: Option<AskedText>,
}

/// The chosen chart's text last prepared for the loading presentation:
/// what it was prepared for, and the fields it was prepared from.
#[derive(Debug)]
struct AskedText {
    key: LoadingTextKey,
    target: [u32; 2],
    lifetime: u64,
    revision: u64,
    chart: Arc<ChosenChart>,
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
    /// `renderer`, drawing menus in `fonts` if given.
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
            asked: None,
        }
    }

    /// Brings the loading presentation's chosen-chart text to `update`,
    /// between frames: the presentation enters the update's lifetime, the
    /// text staged by an earlier call is installed (dropped if its
    /// installation, lifetime or revision has passed), and text for a
    /// lifetime, revision, installation or size not asked yet is prepared
    /// and staged for the next frame, replacing any staged. Within one
    /// lifetime and installation only the fields that changed are laid
    /// out again, their slots cleared until then; otherwise every slot.
    /// Preparing allocates: it runs only when the update asks for new
    /// text, never in a frame's build.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn chosen_text(&mut self, update: &LoadingUpdate, lifetime: u64) {
        self.renderer.enter_loading(lifetime);
        // Text whose installation, lifetime or revision has passed is
        // dropped as meant; only a table that does not fit is reported.
        if let Err(e @ LoadingTextError::Table(_)) = self.renderer.install_staged_loading_text() {
            eprintln!("render: the chosen chart's text was not installed: {e}");
        }
        let Some(key) = self.renderer.loading_text_key() else {
            self.asked = None;
            return;
        };
        let target = self.renderer.target();
        let which = match &self.asked {
            Some(a)
                if a.key == key
                    && a.target == target
                    && a.lifetime == lifetime
                    && a.revision == update.revision =>
            {
                return;
            }
            Some(a) if a.key == key && a.target == target && a.lifetime == lifetime => {
                let which = fields_changed(&a.chart, &update.chart);
                self.renderer.clear_loading_text(which);
                which
            }
            _ => TextSlots::ALL,
        };
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
        if let Err(e) = self
            .renderer
            .prepare_loading_text(lifetime, update.revision, which, &chart)
        {
            eprintln!("render: the chosen chart's text was not prepared: {e}");
        }
        self.asked = Some(AskedText {
            key,
            target,
            lifetime,
            revision: update.revision,
            chart: Arc::clone(&update.chart),
        });
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
    fn prepare(&mut self, page: &MenuPage, left: f32, top: f32) -> Result<(), RenderError> {
        let Some(fonts) = self.fonts.as_mut() else {
            // A screen made for plays only: no font was looked for.
            return Err(render::text::TextError::NoFont {
                dir: render::text::fonts_dir(),
            }
            .into());
        };
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
        self.underline = area
            .filter(|_| page.caret.is_some_and(|c| c.end > c.start))
            .map(|[x, y, w, h]| [x, y + h - 6.0, w, 2.0]);
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

    /// Stages `skin`'s chart-independent installation, answering at
    /// once; a refusal on a healthy device ([`RenderError::is_refusal`])
    /// keeps the installation in use.
    fn install(
        &mut self,
        skin: &Arc<SkinBundle>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        match self.renderer.install(skin, request) {
            Ok(_) => Ok(Preparation::Ready),
            Err(e) if e.is_refusal() => Ok(refusal(game::Chain(&e).to_string())),
            Err(e) => Err(e),
        }
    }

    fn installation(&self) -> Option<Installation> {
        self.renderer.installation()
    }

    fn restore(&mut self, installation: &Installation) -> Result<(), RenderError> {
        self.renderer.restore(installation)
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
            self.chosen_text(update, frame.lifetime);
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

    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let timing = self.menu_frame(page, changed)?;
        self.shown(timing.presented);
        Ok(timing)
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
        let cursor = self.cursor;
        let underline = self.underline;
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        self.renderer.menu_frame(
            |b| {
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
            },
            text,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

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
        };
        assert_eq!(fields_changed(&chart, &chart.clone()), TextSlots::NONE);
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

    /// Fading the launch screen lays nothing out again: once its glyphs are
    /// in the atlas, a step allocates only inside wgpu's write of the
    /// glyphs' vertices (its staging, as every frame's sprite upload has),
    /// a few, where laying the text out would allocate per glyph; a frame
    /// at the same opacity prepares nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_splash_fade_lays_nothing_out_again() {
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
}
