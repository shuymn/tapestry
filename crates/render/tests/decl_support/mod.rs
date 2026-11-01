//! Declarative skin bundles for the render tests, built here: a solid
//! image written as PNG into a temporary folder, a fixed 1920×1080 skin
//! drawing it, optionally under clip 0 and with a text element whose
//! prepared line a supplied producer gives.

#![allow(dead_code)]

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bms_model::TimeUs;
use render::{AttemptText, ClipTable, Renderer, SkinBundle, SkinError};
use skin_model::{
    Align, Anchor, Blend, ChartText, ClipId, Ease, ElementDef, End, FileDef, Filter, Fit, FontRef,
    FrameUv, ImageDef, KeyDef, Overflow, Place, Region, RegionDef, Sheet, Side, Skin, SkinDef,
    Source, TextDef, TextField, TextImage, TextTable, TextValue, TimerRef, Track, Variant,
    WidgetDefs,
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
    /// More copies of the image element, for a larger capacity.
    pub copies: usize,
    /// A text element on the right half with its prepared line.
    pub text: bool,
}

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
        }
    }
}

/// A text producer giving slot 0 the image of the fixture's frame 0 and
/// counting its calls, or refusing while `refuse` is set.
#[derive(Debug)]
pub struct Text {
    pub uv: FrameUv,
    pub calls: AtomicU32,
    pub refuse: AtomicBool,
}

impl AttemptText for Text {
    fn prepare(
        &self,
        skin: &Skin,
        _chart: &ChartText<'_>,
        _target: [u32; 2],
    ) -> Result<Option<TextTable>, SkinError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse.load(Ordering::SeqCst) {
            return Err(SkinError {
                reason: "the text cannot be prepared".to_owned(),
            });
        }
        let line = TextImage::new(self.uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        let mut slots = vec![None; skin.text_slots()];
        if let Some(slot) = slots.first_mut() {
            *slot = line;
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
    let mut elements = vec![image(look.rect, clipped); 1 + look.copies];
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
        elements.push(text);
    }
    Skin::new(SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet,
        lanes: None,
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
    if !look.text {
        return (Arc::new(bundle), None);
    }
    let text = Arc::new(Text {
        uv,
        calls: AtomicU32::new(0),
        refuse: AtomicBool::new(false),
    });
    let producer: Arc<dyn AttemptText> = text.clone();
    (Arc::new(bundle.with_text(producer)), Some(text))
}
