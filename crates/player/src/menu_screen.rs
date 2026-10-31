//! The select screen as the render thread draws it (design.md "Rendering
//! and Skins"): lines of Japanese text laid out with `render::text` and
//! drawn in the menu pass, over a bar that marks the cursor's line.
//!
//! A [`MenuPage`] comes from the game thread whenever what it shows
//! changes; its lines are laid out and the screen's text prepared then,
//! and every frame in between draws what was prepared. A screen built
//! again after a failure lays out its page again. All of it runs on menus,
//! where allocation is allowed.

use bms_model::Chart;
use eval::Section;
use render::text::{Fonts, MenuText, TextBlock, TextStyle};
use render::{Blend, FrameTiming, RenderError, Renderer, Sprite, solid_uv};

use crate::render_side::Screen;

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
}

impl MenuScreen {
    /// `renderer`, drawing menus in `fonts` if given.
    pub const fn new(renderer: Renderer, fonts: Option<Fonts>) -> Self {
        Self {
            renderer,
            fonts,
            text: None,
            cursor: None,
        }
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
        let refs: Vec<&TextBlock> = blocks.iter().collect();
        let text = self
            .text
            .get_or_insert_with(|| MenuText::new(&self.renderer));
        text.prepare(&self.renderer, fonts, &refs)?;
        self.cursor = page
            .lines
            .iter()
            .take(PAGE_LINES)
            .position(|l| l.style == LineStyle::Cursor);
        Ok(())
    }
}

impl Screen for MenuScreen {
    fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError> {
        self.renderer.load(chart, section)
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
    fn build(&mut self, f: &render::FrameInput<'_>) {
        self.renderer.build(f);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        self.renderer.submit()
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
        self.renderer.text_frame(lines).map(|t| t.presented)
    }

    fn end(&mut self, page: &crate::render_side::EndPage) -> Result<bool, RenderError> {
        self.renderer
            .result_frame(&page.lines, page.graphs.as_deref())
            .map(|t| t.presented)
    }

    fn loading(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        if changed || self.text.is_none() {
            self.prepare(page, LOADING_LEFT, LOADING_TOP)?;
        }
        let Some(text) = &self.text else {
            return Err(RenderError::OtherRenderersText);
        };
        self.renderer.loading_frame(text)
    }

    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        if changed || self.text.is_none() {
            self.prepare(page, LEFT, TOP)?;
        }
        let cursor = self.cursor;
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
                        },
                    );
                }
            },
            text,
        )
    }

    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = self.renderer.size();
        let pixels = self.renderer.read_back();
        (!pixels.is_empty()).then_some((w, h, pixels))
    }
}
