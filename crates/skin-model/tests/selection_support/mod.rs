//! Selection skins for the selection tests: a sheet whose every frame has
//! its own uv, so a quad tells its image and frame apart, the song list and
//! ratio bar definitions the tests vary, and selection views built in code.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bms_model::{ChartSummary, Tempo, TimeUs};
use image::{ImageFormat, Rgba, RgbaImage};
use skin_model::{
    Align, Anchor, AtlasLimits, Blend, ChartLabels, DigitsDef, Ease, ElementDef, End, FileDef,
    Filter, Fit, FontRef, GlyphSet, ImageDef, ImagePartDef, KeyDef, LEVEL_STYLES, LampKey, Layout,
    LevelDef, LoadError, Overflow, Pad, PartDef, Place, PlacementDef, Program, Quad, QuadBatch,
    Ratio, Region, RegionDef, RowFeatures, RowKey, RowKind, RowLabel, RowScore, RowTextDef,
    RowView, SELECTION, Scope, SelectedChart, Selection, SelectionDefs, SelectionFrame,
    SelectionLabels, SelectionPlace, SelectionTimers, SelectionView, Sheet, Skin, SkinDef,
    SongListDef, Source, TimerRef, Track, Ty, UvTable, Variant, WidgetDefs, compile,
    selection_timer,
};

/// The images, by index: [`PLAIN`] images of one frame each, then a strip
/// of ten glyphs.
pub const PLAIN: u32 = 24;
pub const DIGITS: u32 = PLAIN;
pub const IMAGES: u32 = PLAIN + 1;

pub const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 4,
    reserved_layers: 1,
};

pub const REGIONS: [RegionDef; Region::COUNT] = [RegionDef {
    anchor: Anchor::TopLeft,
    optional: false,
}; Region::COUNT];

/// Each image's frames.
pub fn frames_of(i: u32) -> u16 {
    if i == DIGITS { 10 } else { 1 }
}

fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("skin-model-selection");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..IMAGES {
            let tmp = tempfile::Builder::new()
                .suffix(".png")
                .tempfile_in(&dir)
                .unwrap();
            RgbaImage::from_pixel(2 * u32::from(frames_of(i)), 2, Rgba([255; 4]))
                .save_with_format(tmp.path(), ImageFormat::Png)
                .unwrap();
            let path = dir.join(format!("{i}.png"));
            if let Err(e) = tmp.persist(&path)
                && !path.is_file()
            {
                panic!("{path:?}: {e}");
            }
        }
        // A font file the sheet admits by its header alone, replaced whole like
        // the images: another test process may be reading it.
        let tmp = tempfile::Builder::new()
            .suffix(".ttf")
            .tempfile_in(&dir)
            .unwrap();
        std::fs::write(tmp.path(), [0, 1, 0, 0]).unwrap();
        let path = dir.join("o.ttf");
        if let Err(e) = tmp.persist(&path)
            && !path.is_file()
        {
            panic!("{path:?}: {e}");
        }
        dir
    })
}

/// [`sheet`] with the outline font file `o.ttf` as font 0.
pub fn sheet_with_font() -> Sheet {
    sheet()
        .with_fonts(vec![skin_model::FontDef {
            path: "o.ttf".to_owned(),
        }])
        .unwrap()
}

/// The sheet: image `i` is file `i`, cut into its frames.
pub fn sheet() -> Sheet {
    Sheet::new(
        root(),
        (0..IMAGES)
            .map(|i| FileDef {
                path: format!("{i}.png"),
                hi: None,
            })
            .collect(),
        (0..IMAGES)
            .map(|file| ImageDef {
                file,
                rect: None,
                grid: [frames_of(file), 1],
                cycle: None,
                filter: Filter::Linear,
            })
            .collect(),
    )
    .unwrap()
}

/// The image and frame of a quad by its uv; `None` for a text line.
pub fn frame_of(uvs: &UvTable, q: &Quad) -> Option<(u32, u16)> {
    let mut first = 0;
    for i in 0..IMAGES {
        for f in 0..frames_of(i) {
            if uvs
                .get(first + u32::from(f))
                .is_some_and(|u| u.uv == q.uv && u.layer == q.layer)
            {
                return Some((i, f));
            }
        }
        first += u32::from(frames_of(i));
    }
    None
}

/// A track of `keys`, each `(ms, [x, y, w, h])`, linear, holding after.
pub fn keys(keys: &[(i64, [i32; 4])]) -> Track {
    keys_ending(keys, End::Hold)
}

/// A track of `keys` ending as `end`.
pub fn keys_ending(keys: &[(i64, [i32; 4])], end: End) -> Track {
    let defs: Vec<KeyDef> = keys
        .iter()
        .map(|&(ms, [x, y, w, h])| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
            KeyDef {
                at: TimeUs(ms * 1_000),
                props,
            }
        })
        .collect();
    Track::new(&defs, Ease::Linear, end).unwrap()
}

/// A still track over `r`.
pub fn rect(r: [i32; 4]) -> Track {
    keys(&[(0, r)])
}

pub fn boolean(src: &str) -> Program {
    compile(src, &Scope::new(&SELECTION), Ty::Bool).unwrap()
}

pub fn int(src: &str) -> Program {
    compile(src, &Scope::new(&SELECTION), Ty::Int).unwrap()
}

/// A part over `r` on `screen`.
pub fn part(r: [i32; 4]) -> PartDef {
    PartDef {
        track: rect(r),
        timer: TimerRef::Builtin(selection_timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        pivot: Anchor::TopLeft,
    }
}

/// A placement over `r` showing `image` for every row style.
pub fn placement(image: u32, r: [i32; 4]) -> PlacementDef {
    PlacementDef {
        part: part(r),
        images: [Some(image); 3],
    }
}

/// An element drawing `source` over `r` on `screen`.
pub fn element(source: Source, r: [i32; 4]) -> ElementDef {
    ElementDef {
        source,
        track: rect(r),
        timer: TimerRef::Builtin(selection_timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// The song list element: its track only colors it.
pub fn list_element() -> ElementDef {
    element(Source::SongList, [0, 0, 0, 0])
}

/// A digits strip of `cells` over the glyph image.
pub fn digits(cells: u8) -> DigitsDef {
    DigitsDef {
        image: DIGITS,
        set: GlyphSet::Ten,
        cells,
        align: Align::Left,
        pad: Pad::Blank,
        space: 0,
        offsets: Vec::new(),
    }
}

/// A song list of `slots` placements 100 apart vertically, each 50 tall,
/// the unselected on image 0 and the selected on image 1, its center at
/// `center`, without styles.
pub fn list(slots: usize, center: u8) -> SongListDef {
    SongListDef {
        placements: (0..slots)
            .map(|i| {
                let y = 100 * i32::try_from(i).unwrap();
                [
                    placement(0, [10, y, 200, 50]),
                    placement(1, [10, y, 200, 50]),
                ]
            })
            .collect(),
        center,
        text: [None, None, None],
        level: std::array::from_fn(|_| None),
        lamp: std::array::from_fn(|_| None),
        label: [None, None, None],
    }
}

/// A lamp style per key, each on its own image (2 + key), over `r`.
pub fn lamps(r: [i32; 4]) -> [Option<ImagePartDef>; LampKey::COUNT] {
    std::array::from_fn(|k| {
        Some(ImagePartDef {
            part: part(r),
            image: 2 + u32::try_from(k).unwrap(),
        })
    })
}

/// A label style per label, each on its own image (10 + label), over `r`.
pub fn labels(r: [i32; 4]) -> [Option<ImagePartDef>; RowLabel::COUNT] {
    std::array::from_fn(|l| {
        Some(ImagePartDef {
            part: part(r),
            image: 10 + u32::try_from(l).unwrap(),
        })
    })
}

/// A level style per difficulty, each a strip of `cells`, over `r` moved
/// 1 000 right per difficulty.
pub fn levels(r: [i32; 4], cells: u8) -> [Option<LevelDef>; LEVEL_STYLES] {
    std::array::from_fn(|d| {
        let [x, y, w, h] = r;
        Some(LevelDef {
            part: part([x + 1_000 * i32::try_from(d).unwrap(), y, w, h]),
            digits: digits(cells),
        })
    })
}

/// A row text style in the default font over `r`.
pub fn row_text(r: [i32; 4]) -> RowTextDef {
    RowTextDef {
        part: part(r),
        font: FontRef::Default,
        align: Align::Left,
        overflow: Overflow::Spill,
    }
}

/// A sheet of one 2×2 image (image 0) and a bitmap font (font 0: `A` and
/// `B`, 4 wide and advancing 5, on one 16×16 page), its files in a new
/// folder that must live while the sheet is read or drawn.
pub fn bitmap_sheet() -> (tempfile::TempDir, Sheet) {
    use skin_model::{Root, SheetBuilder};
    let dir = tempfile::tempdir().unwrap();
    RgbaImage::from_pixel(2, 2, Rgba([255; 4]))
        .save_with_format(dir.path().join("0.png"), ImageFormat::Png)
        .unwrap();
    std::fs::write(
        dir.path().join("f.fnt"),
        "info face=\"t\" size=10 padding=0,0,0,0\n\
         common lineHeight=12 base=10 scaleW=16 scaleH=16 pages=1\n\
         page id=0 file=\"f.png\"\n\
         char id=65 x=0 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=5 page=0\n\
         char id=66 x=4 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=5 page=0\n",
    )
    .unwrap();
    RgbaImage::from_pixel(16, 16, Rgba([255; 4]))
        .save_with_format(dir.path().join("f.png"), ImageFormat::Png)
        .unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    let file = b
        .admit_file(FileDef {
            path: "0.png".into(),
            hi: None,
        })
        .unwrap();
    b.add_image(ImageDef {
        file,
        rect: None,
        grid: [1, 1],
        cycle: None,
        filter: Filter::Linear,
    })
    .unwrap();
    let f = b.admit_fnt("f.fnt", &mut |_| Ok(()), &|| false).unwrap();
    b.add_bitmap_font(f, &[]).unwrap();
    (dir, b.finish())
}

/// A song list of `slots` placements centered at `center` over a sheet of
/// one image and a bitmap font (`A` and `B`, 4 wide and advancing 5), its
/// chart rows' text in that font 20 above each placement's bottom; the
/// folder must live while the skin draws.
pub fn bitmap_list(slots: usize, center: u8) -> (tempfile::TempDir, Skin) {
    let (dir, sheet) = bitmap_sheet();
    let image = 0;
    let mut def = list(slots, center);
    for p in &mut def.placements {
        for arm in p.iter_mut() {
            arm.images = [Some(image); 3];
        }
    }
    let mut style = row_text([0, -20, 100, 10]);
    style.font = FontRef::Bitmap(0);
    def.text = [Some(style), None, None];
    let mut sd = skin_def(vec![list_element()], WidgetDefs::default());
    sd.sheet = sheet;
    let skin = Skin::new_selection(
        sd,
        SelectionDefs {
            song_list: Some(def),
            ..SelectionDefs::default()
        },
    )
    .unwrap();
    (dir, skin)
}

/// A selection skin of `elements` with `widgets` and `sel`.
pub fn load(
    elements: Vec<ElementDef>,
    widgets: WidgetDefs,
    sel: SelectionDefs,
) -> Result<Skin, LoadError> {
    Skin::new_selection(skin_def(elements, widgets), sel)
}

/// A fixed-canvas definition of `elements` with `widgets` over [`sheet`].
pub fn skin_def(elements: Vec<ElementDef>, widgets: WidgetDefs) -> SkinDef {
    SkinDef {
        fit: Fit::Fixed,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: sheet(),
        lanes: None,
        elements,
        widgets,
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    }
}

pub fn uvs(skin: &Skin) -> UvTable {
    skin.sheet()
        .atlas(Variant::Lo, LIMITS, &|| false)
        .unwrap()
        .into_parts()
        .1
}

/// The player's labels the tests pass.
pub fn labels_of() -> SelectionLabels {
    SelectionLabels {
        all_charts: "ALL".to_owned(),
        search_results: "FOUND".to_owned(),
    }
}

/// A drawn quad: its image and frame, `None` for a text line, and its
/// rect.
pub type Drawn = (Option<(u32, u16)>, [f32; 4]);

/// A selection skin with its state, uv table and batch.
pub struct Fx {
    pub skin: Skin,
    pub sel: Selection,
    pub uvs: UvTable,
    pub batch: QuadBatch,
    pub target: [u32; 2],
    pub layout: Layout,
    pub timers: SelectionTimers,
}

impl Fx {
    pub fn new(skin: Skin) -> Self {
        let sel = skin.prepare_selection();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            uvs: uvs(&skin),
            skin,
            sel,
            batch,
            target: [1_920, 1_080],
            layout: Layout::AUTHOR,
            timers: SelectionTimers {
                screen: Some(TimeUs(0)),
                ..SelectionTimers::default()
            },
        }
    }

    pub fn view(&mut self, view: &SelectionView) {
        self.skin
            .update_selection(&mut self.sel, view, &labels_of())
            .unwrap();
    }

    /// Builds the frame at `ms`.
    pub fn frame(&mut self, ms: i64) {
        self.batch.clear();
        let frame = SelectionFrame {
            now: TimeUs(ms * 1_000),
            target: self.target,
            layout: &self.layout,
            timers: self.timers,
        };
        self.skin
            .build_selection(&mut self.sel, &frame, &self.uvs, &mut self.batch)
            .unwrap();
    }

    /// The quads drawn: image, frame and rect.
    pub fn drawn(&self) -> Vec<Drawn> {
        self.batch
            .quads()
            .iter()
            .map(|q| (frame_of(&self.uvs, q), q.rect))
            .collect()
    }

    /// The images drawn, in order.
    pub fn images(&self) -> Vec<u32> {
        self.drawn()
            .into_iter()
            .filter_map(|(f, _)| f.map(|(i, _)| i))
            .collect()
    }
}

/// A chart row titled `title`.
pub fn chart(title: &str) -> RowView {
    RowView {
        key: RowKey::Chart(PathBuf::from(format!("{title}.bms"))),
        kind: RowKind::Chart,
        title: title.to_owned(),
        subtitle: String::new(),
        artist: String::new(),
        level: String::new(),
        level_value: 0,
        difficulty: None,
        score: RowScore::Unplayed,
        features: None,
        random: false,
    }
}

/// A folder row named `name` holding `charts`.
pub fn folder(name: &str, charts: u32) -> RowView {
    RowView {
        key: RowKey::Folder(skin_model::FolderId::new(PathBuf::from(name))),
        kind: RowKind::Folder { charts },
        title: name.to_owned(),
        ..chart("")
    }
}

/// The "all charts" row of `charts`.
pub fn all(charts: u32) -> RowView {
    RowView {
        key: RowKey::All,
        kind: RowKind::All { charts },
        ..chart("")
    }
}

/// A summary of `taps`, `longs` and `mines` lasting `length_s`, its
/// tempos `min` to `max` BPM.
pub fn summary(
    taps: u32,
    longs: u32,
    mines: u32,
    length_s: i64,
    min: u128,
    max: u128,
) -> ChartSummary {
    let bpm = |n: u128| Tempo::from_units(n * Tempo::UNITS_PER_BPM).unwrap();
    ChartSummary {
        taps,
        longs,
        mines,
        length: TimeUs(length_s * 1_000_000),
        tempo_start: bpm(min),
        tempo_min: bpm(min),
        tempo_max: bpm(max),
    }
}

/// A view of `rows`, the cursor on `cursor`, the selected chart from that
/// row when it is a chart, with `max_ex` and `summary`.
pub fn view(rows: Vec<RowView>, cursor: Option<usize>) -> SelectionView {
    let selected = cursor
        .and_then(|c| rows.get(c))
        .filter(|r| r.kind == RowKind::Chart)
        .map(|r| SelectedChart {
            labels: ChartLabels {
                title: r.title.clone(),
                subtitle: r.subtitle.clone(),
                artist: r.artist.clone(),
                level: r.level.clone(),
                level_value: r.level_value,
                difficulty: r.difficulty,
                ..ChartLabels::default()
            },
            summary: None,
            random: r.random,
            max_ex: None,
            score: r.score,
            judge_class: None,
        });
    SelectionView {
        position: None,
        rows,
        cursor,
        place: SelectionPlace::Top,
        query: String::new(),
        selected,
    }
}

/// [`RowFeatures`] with long notes and mines as given.
pub fn features(long_notes: bool, mines: bool) -> Option<RowFeatures> {
    Some(RowFeatures { long_notes, mines })
}

/// An exact ratio.
pub fn ratio(n: u64, d: u64) -> Option<Ratio> {
    Ratio::new(n, d)
}
