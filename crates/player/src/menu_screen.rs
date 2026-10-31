//! The select screen as the render thread draws it (design.md "Rendering
//! and Skins"): lines of Japanese text laid out with `render::text` and
//! drawn in the menu pass, over a bar that marks the cursor's line.
//!
//! A [`MenuPage`] comes from the game thread whenever what it shows
//! changes; its lines are laid out and the screen's text prepared then,
//! and every frame in between draws what was prepared. A screen built
//! again after a failure lays out its page again. All of it runs on menus,
//! where allocation is allowed.
//!
//! A page with a text caret (the search box with focus) has the caret's
//! area measured as its line is laid out: the IME's composition is
//! underlined there, and the area goes to the window ([`WindowSink`]),
//! which reports it to the IME so its candidate window opens beside the
//! composition.

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

/// Where the window learns of the caret: the area the IME's windows open
/// beside.
pub trait WindowSink: Send {
    /// The caret's area `[left, top, width, height]` in layout pixels
    /// (1920×1080), or `None` when no text box has focus.
    fn caret(&self, area: Option<[f32; 4]>);
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

/// The top of line `i`, in layout pixels.
fn line_top(i: usize) -> f32 {
    // At most `PAGE_LINES` lines: exact in f32.
    TOP + LINE_STEP * i as f32
}

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
    /// The caret's area last sent there; `None` before the first.
    sent: Option<Option<[f32; 4]>>,
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
            sent: None,
        }
    }

    /// Sends the caret's area of each page prepared to `sink`.
    #[must_use]
    pub fn with_sink(mut self, sink: Box<dyn WindowSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Lays `page` out and prepares it as this renderer's text.
    fn prepare(&mut self, page: &MenuPage) -> Result<(), RenderError> {
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
                    left: LEFT,
                    top: line_top(i),
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

    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        if changed || self.text.is_none() {
            self.prepare(page)?;
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

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
    struct Sent(Arc<Mutex<Vec<Option<[f32; 4]>>>>);

    impl WindowSink for Sent {
        fn caret(&self, area: Option<[f32; 4]>) {
            self.0.lock().unwrap().push(area);
        }
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
        let r = Renderer::new(
            Target::Offscreen {
                width: 1920,
                height: 1080,
            },
            AdapterChoice::Warp,
        )
        .unwrap();
        let sent = Sent::default();
        let mut screen =
            MenuScreen::new(r, Some(Fonts::runtime().unwrap())).with_sink(Box::new(sent.clone()));
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
