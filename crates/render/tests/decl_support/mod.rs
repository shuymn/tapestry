//! Declarative skin bundles for the render tests, built here: a solid
//! image written as PNG into a temporary folder, a fixed 1920×1080 skin
//! drawing it, optionally under clip 0, with a text element whose
//! prepared line a supplied producer gives, with markers of the shown
//! gauge's look and of the judgment timer, and with a loading
//! presentation: an untimed background, an animation on the `loading`
//! timer and a mark on a `when` timer whose condition holds while loading;
//! the lanes and a gauge; the chosen chart's title, artist, genre and full
//! title; and marks following the load's progress.

#![allow(dead_code)]

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bms_model::TimeUs;
use render::{AttemptText, ClipTable, Renderer, SkinBundle, SkinError};
use skin_model::{
    Align, Anchor, Blend, ChartText, ClipId, Ease, ElementDef, End, FileDef, Filter, Fit, FontRef,
    FrameUv, GaugeDef, GaugeLook, GaugeParts, ImageDef, KeyDef, LaneParts, LaneStyle, LanesDef,
    Layout, Motion, Overflow, PLAY, Place, Region, RegionDef, Scope, Sheet, Side, Skin, SkinDef,
    Source, TextDef, TextField, TextImage, TextTable, TextValue, TimerRef, Track, Ty, Variant,
    WidgetDefs, compile,
};

/// Writes a `side`-texel-square PNG of `rgba` at `path`.
fn png(path: &Path, side: u32, rgba: [u8; 4]) {
    let w = BufWriter::new(File::create(path).unwrap());
    let mut e = png::Encoder::new(w, side, side);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = (0..side * side).flat_map(|_| rgba).collect();
    e.write_header().unwrap().write_image_data(&data).unwrap();
}

/// An element drawing image 0 over canvas `rect` from `ready` on, under
/// clip 0 when `clipped`.
fn image(rect: [i32; 4], clipped: bool) -> ElementDef {
    let [x, y, w, h] = rect;
    let mut props = [None; 9];
    props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
    ElementDef {
        source: Source::Image(0),
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(skin_model::timer::READY),
        visible: None,
        blend: Blend::Alpha,
        clip: clipped.then_some(ClipId(0)),
        place: Place::SCREEN,
    }
}

/// What a fixture bundle draws.
#[derive(Clone, Debug)]
pub struct Look {
    /// Its image's colour.
    pub rgba: [u8; 4],
    /// Its image's side in texels: past 256 its atlas pages are 512.
    pub side: u32,
    /// Where it draws, in canvas pixels.
    pub rect: [i32; 4],
    /// Clip 0, which the image draws under; none draws it unclipped.
    pub clip: Option<[f32; 4]>,
    /// More image elements, for a larger capacity: each draws the image
    /// under the same clip over the 10×10 canvas pixels at `rect`'s top
    /// left, so thousands of them stay cheap to rasterise on WARP, whose
    /// devices in one process wait on each other's work.
    pub copies: usize,
    /// A text element on the right half with its prepared line.
    pub text: bool,
    /// The text element on the untimed `screen` timer instead of
    /// `ready`, so loading frames draw it too.
    pub text_on_screen: bool,
    /// Two markers drawing the image unclipped: over [`GAUGE_MARKER`]
    /// while the shown gauge's look is `hard` (a survival gauge's), and
    /// over [`JUDGE_MARKER`] once a judgment started its timer.
    pub markers: bool,
    /// Three loading elements drawing the image unclipped: over
    /// [`BACKGROUND`] on the untimed `screen` timer, over a 100×100 square
    /// moving right from [`ANIMATION`] at 1 canvas pixel per ms of the
    /// `loading` timer, and over [`WHEN_MARK`] on `when` timer 0, whose
    /// condition (`load.percent >= 0`) holds in every loading frame.
    pub loading: bool,
    /// The lanes, their judgment line drawn over [`LINE_AT`], and a gauge
    /// of 50 segments over [`GAUGE`], every part drawing the image,
    /// both on the untimed `screen` timer.
    pub structure: bool,
    /// Four text elements on the untimed `screen` timer, each a 100×100
    /// square where its line draws: the title over [`TITLE`], the artist
    /// over [`ARTIST`], the genre over [`GENRE`] and the full title over
    /// [`FULL_TITLE`].
    pub chart_text: bool,
    /// With `loading`, two progress marks: over [`PERCENT_MARK`] while
    /// `load.percent` reads 40, and over [`PROGRESS_MARK`] on `when`
    /// timer 1 ([`PROGRESS_SLOT`]), whose condition is
    /// `load.percent >= 40`.
    pub progress: bool,
}

/// Where the untimed loading background draws, in canvas pixels.
pub const BACKGROUND: [i32; 4] = [0, 900, 100, 100];
/// Where the loading animation starts, at `loading` 0.
pub const ANIMATION: [i32; 4] = [200, 900, 100, 100];
/// Where the `when` timer's mark draws.
pub const WHEN_MARK: [i32; 4] = [1_700, 900, 100, 100];
/// The `when` timer's slot: the first after the built-in timers.
pub const WHEN_SLOT: u16 = skin_model::timer::BUILTIN;

/// A canvas pixel on the lanes' judgment line.
pub const LINE_AT: (u32, u32) = (300, 882);
/// Where the gauge draws.
pub const GAUGE: [i32; 4] = [100, 300, 500, 20];
/// Where the title's line draws.
pub const TITLE: [i32; 4] = [100, 600, 100, 100];
/// Where the artist's line draws.
pub const ARTIST: [i32; 4] = [300, 600, 100, 100];
/// Where the genre's line draws.
pub const GENRE: [i32; 4] = [500, 600, 100, 100];
/// Where the full title's line draws.
pub const FULL_TITLE: [i32; 4] = [700, 600, 100, 100];
/// Where the mark shown while `load.percent` reads 40 draws.
pub const PERCENT_MARK: [i32; 4] = [1_000, 600, 100, 100];
/// Where the mark on the progress `when` timer draws.
pub const PROGRESS_MARK: [i32; 4] = [1_200, 600, 100, 100];
/// The progress `when` timer's slot.
pub const PROGRESS_SLOT: u16 = skin_model::timer::BUILTIN + 1;

/// Where the gauge-look marker draws, in canvas pixels.
pub const GAUGE_MARKER: [i32; 4] = [1_200, 100, 100, 100];
/// Where the judgment marker draws.
pub const JUDGE_MARKER: [i32; 4] = [1_500, 100, 100, 100];

impl Look {
    /// `rgba` over the left half, under a clip of the left half.
    pub fn left(rgba: [u8; 4]) -> Self {
        Self {
            rgba,
            side: 16,
            rect: [0, 0, 1_920, 1_080],
            clip: Some([0.0, 0.0, 960.0, 1_080.0]),
            copies: 0,
            text: false,
            text_on_screen: false,
            markers: false,
            loading: false,
            structure: false,
            chart_text: false,
            progress: false,
        }
    }
}

/// A text producer giving every text slot whose string resolves to
/// something the image of the fixture's frame 0, and no line to the
/// others, counting its calls and noting each chart's title, artist and
/// genre, or refusing while `refuse` is set.
#[derive(Debug)]
pub struct Text {
    pub uv: FrameUv,
    pub calls: AtomicU32,
    pub refuse: AtomicBool,
    pub charts: Mutex<Vec<[String; 3]>>,
}

impl AttemptText for Text {
    fn prepare(
        &self,
        skin: &Skin,
        chart: &ChartText<'_>,
        target: [u32; 2],
    ) -> Result<Option<TextTable>, SkinError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.charts.lock().unwrap().push([
            chart.title.to_owned(),
            chart.artist.to_owned(),
            chart.genre.to_owned(),
        ]);
        if self.refuse.load(Ordering::SeqCst) {
            return Err(SkinError {
                reason: "the text cannot be prepared".to_owned(),
            });
        }
        let line = TextImage::new(self.uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        let mut slots = vec![None; skin.text_slots()];
        for request in skin.text_requests(chart, target, &Layout::AUTHOR) {
            if let Some(slot) = slots.get_mut(usize::from(request.slot)) {
                *slot = line;
            }
        }
        Ok(Some(TextTable::new(slots)))
    }
}

/// The compiled skin of `look`, its image written in `dir`.
pub fn skin(dir: &Path, look: &Look) -> Skin {
    let name = format!(
        "{:02x}{:02x}{:02x}-{}.png",
        look.rgba[0], look.rgba[1], look.rgba[2], look.side
    );
    png(&dir.join(&name), look.side, look.rgba);
    let sheet = Sheet::new(
        dir,
        vec![FileDef {
            path: name.as_str().into(),
            hi: None,
        }],
        vec![ImageDef {
            file: 0,
            rect: None,
            grid: [1, 1],
            cycle: None,
            filter: Filter::Nearest,
        }],
    )
    .unwrap();
    let clipped = look.clip.is_some();
    let [x, y, ..] = look.rect;
    let mut elements = vec![image(look.rect, clipped)];
    elements.extend(std::iter::repeat_n(
        image([x, y, 10, 10], clipped),
        look.copies,
    ));
    let mut widgets = WidgetDefs::default();
    if look.text {
        widgets.texts.push(TextDef {
            value: TextValue::Field(TextField::Title),
            font: FontRef::Default,
            align: Align::Left,
            overflow: Overflow::Spill,
        });
        let mut text = image([1_000, 500, 100, 100], false);
        text.source = Source::Text(0);
        if look.text_on_screen {
            text.timer = TimerRef::Builtin(skin_model::timer::SCREEN);
        }
        elements.push(text);
    }
    if look.markers {
        let scope = Scope::new(&PLAY);
        let hard = compile("gauge.valid && gauge.kind == hard", &scope, Ty::Bool).unwrap();
        elements.push(ElementDef {
            visible: Some(hard),
            ..image(GAUGE_MARKER, false)
        });
        elements.push(ElementDef {
            timer: TimerRef::Builtin(skin_model::timer::JUDGE),
            ..image(JUDGE_MARKER, false)
        });
    }
    if look.chart_text {
        for (field, rect) in [
            (TextField::Title, TITLE),
            (TextField::Artist, ARTIST),
            (TextField::Genre, GENRE),
            (TextField::FullTitle, FULL_TITLE),
        ] {
            let slot = u32::try_from(widgets.texts.len()).unwrap();
            widgets.texts.push(TextDef {
                value: TextValue::Field(field),
                font: FontRef::Default,
                align: Align::Left,
                overflow: Overflow::Spill,
            });
            elements.push(ElementDef {
                source: Source::Text(slot),
                timer: TimerRef::Builtin(skin_model::timer::SCREEN),
                ..image(rect, false)
            });
        }
    }
    let mut lanes = None;
    if look.structure {
        lanes = Some(LanesDef {
            style: LaneStyle::Simple,
            lanes: std::array::from_fn(|k| {
                let k = i32::try_from(k).unwrap();
                [160 + 64 * k, 64]
            }),
            scratch: Side::Left,
            top: 80,
            line: 880,
            bottom: 960,
            note_h: 16,
            mine_h: 8,
            line_h: 6,
            parts: LaneParts {
                note: [0; 8],
                head: [0; 8],
                tail: [0; 8],
                body_on: [0; 8],
                body_off: [0; 8],
                mine: [0; 8],
                line: Some(0),
            },
            bar_groups: Vec::new(),
        });
        let parts = GaugeParts {
            front: [0; 2],
            back: [0; 2],
            tip: [0; 2],
        };
        widgets.gauges.push(GaugeDef {
            segments: 50,
            looks: [parts; GaugeLook::COUNT],
            motion: Motion::Still,
        });
        elements.push(ElementDef {
            source: Source::Lanes,
            timer: TimerRef::Builtin(skin_model::timer::SCREEN),
            ..image([0; 4], false)
        });
        elements.push(ElementDef {
            source: Source::Gauge(0),
            timer: TimerRef::Builtin(skin_model::timer::SCREEN),
            ..image(GAUGE, false)
        });
    }
    let mut whens = Vec::new();
    if look.loading {
        let scope = Scope::new(&PLAY);
        whens.push(compile("load.percent >= 0", &scope, Ty::Bool).unwrap());
        elements.push(ElementDef {
            timer: TimerRef::Builtin(skin_model::timer::SCREEN),
            ..image(BACKGROUND, false)
        });
        let [x, y, w, h] = ANIMATION;
        let key = |at: i64, x: i32| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
            KeyDef {
                at: TimeUs(at),
                props,
            }
        };
        elements.push(ElementDef {
            track: Track::new(
                &[key(0, x), key(1_000_000, x + 1_000)],
                Ease::Linear,
                End::Hold,
            )
            .unwrap(),
            timer: TimerRef::Builtin(skin_model::timer::LOADING),
            ..image(ANIMATION, false)
        });
        elements.push(ElementDef {
            timer: TimerRef::When(0),
            ..image(WHEN_MARK, false)
        });
        if look.progress {
            whens.push(compile("load.percent >= 40", &scope, Ty::Bool).unwrap());
            elements.push(ElementDef {
                timer: TimerRef::Builtin(skin_model::timer::SCREEN),
                visible: Some(compile("load.percent == 40", &scope, Ty::Bool).unwrap()),
                ..image(PERCENT_MARK, false)
            });
            elements.push(ElementDef {
                timer: TimerRef::When(1),
                ..image(PROGRESS_MARK, false)
            });
        }
    }
    Skin::new(SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens,
        sheet,
        lanes,
        elements,
        widgets,
        bomb_up_to: None,
    })
    .unwrap()
}

/// A bundle of `look` for `r`'s device, its image in `dir`, with its text
/// producer when it has a text element.
pub fn bundle(r: &Renderer, dir: &Path, look: &Look) -> (Arc<SkinBundle>, Option<Arc<Text>>) {
    let skin = skin(dir, look);
    let atlas = skin.atlas(Variant::Lo, r.atlas_limits()).unwrap();
    let uv = atlas.uvs().get(0).unwrap();
    let clips = ClipTable::new(look.clip.into_iter().collect()).unwrap();
    let bundle = SkinBundle::new(Arc::new(skin), atlas, clips, Side::Left).unwrap();
    if !look.text && !look.chart_text {
        return (Arc::new(bundle), None);
    }
    let text = Arc::new(Text {
        uv,
        calls: AtomicU32::new(0),
        refuse: AtomicBool::new(false),
        charts: Mutex::new(Vec::new()),
    });
    let producer: Arc<dyn AttemptText> = text.clone();
    (Arc::new(bundle.with_text(producer)), Some(text))
}
