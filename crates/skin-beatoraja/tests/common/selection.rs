//! Self-made selection skins and their frames: a JSON skin over one sheet
//! whose every texel names its own place, imported through the production
//! path (`import`, then `prepare_selection`, `update_selection` and
//! `build_selection`), each drawn quad told apart by the texel at its
//! middle.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use bms_model::{ChartSummary, Tempo, TimeUs};
use skin_beatoraja::{Config, ImportError, Imported, import, read_header};
use skin_model::{
    Atlas, ChartLabels, GaugeLook, Layout, OwnLamp, Quad, QuadBatch, Ratio, RowFeatures, RowKey,
    RowKind, RowScore, RowView, ScreenKind, SelectedChart, Selection, SelectionFrame,
    SelectionLabels, SelectionPlace, SelectionTimers, SelectionView, Skin, Variant,
};
use tempfile::TempDir;

use super::play::{LIMITS, quad_texel};
use super::{coord, font, png_with, write};

/// The sheet `s.png`: 128×128 texels, each naming its column and row.
pub const SHEET: u32 = 128;

/// The images every fixture declares, each a cut of `s.png` at its own
/// column and row so its middle texel names it: the bar images 0, 1 and 5,
/// a lamp, a label, a ratio image and a plain image.
pub const IMAGES: &str = r#"
    {"id": "b0", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8},
    {"id": "b1", "src": "s", "x": 8, "y": 0, "w": 8, "h": 8},
    {"id": "b5", "src": "s", "x": 16, "y": 0, "w": 8, "h": 8},
    {"id": "lamp", "src": "s", "x": 24, "y": 0, "w": 8, "h": 8},
    {"id": "lab", "src": "s", "x": 32, "y": 0, "w": 8, "h": 8},
    {"id": "bar", "src": "s", "x": 40, "y": 0, "w": 8, "h": 8},
    {"id": "dot", "src": "s", "x": 48, "y": 0, "w": 8, "h": 8}"#;

/// The selection fixtures' images as a Lua table's entries.
pub fn lua_images() -> String {
    [
        ("b0", 0),
        ("b1", 8),
        ("b5", 16),
        ("lamp", 24),
        ("lab", 32),
        ("bar", 40),
        ("dot", 48),
    ]
    .iter()
    .map(|(id, x)| format!("{{ id = \"{id}\", src = \"s\", x = {x}, y = 0, w = 8, h = 8 }}"))
    .collect::<Vec<_>>()
    .join(", ")
}

/// The middle texel of an image's cut, by its left column: what
/// [`Sel::drawn`] names a quad by.
pub fn image_at(x: u32, y: u32) -> [u8; 2] {
    [u8::try_from(x + 4).unwrap(), u8::try_from(y + 4).unwrap()]
}

/// The bar images' names, by the middle texel of their cut.
pub const B0: [u8; 2] = [4, 4];
pub const B1: [u8; 2] = [12, 4];
pub const B5: [u8; 2] = [20, 4];
pub const LAMP: [u8; 2] = [28, 4];
pub const LABEL: [u8; 2] = [36, 4];
pub const BAR: [u8; 2] = [44, 4];
pub const DOT: [u8; 2] = [52, 4];

/// The digit strip `d`: ten glyphs 4×8 from row 32, glyph `k` at column
/// `4k`, and its middle texel's column `4k + 2`.
pub const DIGITS: &str = r#""src": "s", "x": 0, "y": 32, "w": 40, "h": 8, "divx": 10"#;

/// The glyph a digit quad shows, by its middle texel.
pub fn digit_of(t: [u8; 2]) -> Option<u8> {
    (t[1] == 36 && t[0] >= 2 && t[0] < 40).then(|| (t[0] - 2) / 4)
}

/// A selection skin folder holding `s.json` with `body`, its members after
/// the type, size, sources, fonts and [`IMAGES`]: `s.png` and `f.ttf`.
pub fn folder(body: &str) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    png_with(d.path(), "s.png", SHEET, SHEET, coord);
    font(d.path(), "f.ttf");
    write(d.path(), "s.json", json(body).as_bytes());
    d
}

/// The definition [`folder`] writes.
pub fn json(body: &str) -> String {
    format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{IMAGES}], {body}}}"#
    )
}

/// `s.json` of `root` imported into the selection slot with the default
/// configuration.
pub fn import_selection(root: &Path) -> Result<Imported, ImportError> {
    import_selection_with(root, |_| {})
}

/// [`import_selection`] with `edit` applied to the defaults.
pub fn import_selection_with(
    root: &Path,
    edit: impl FnOnce(&mut Config),
) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "s.json")?;
    let mut config = Config::defaults(&h);
    edit(&mut config);
    import(&mut def, &h, &config, ScreenKind::Selection)
}

/// A song list whose placements are `liston` and `listoff` (each a list
/// of `dst` arrays drawing the image set `set`), center `center`, and
/// further members `rest` (its `text`, `level`, `lamp`, `label`).
pub fn song_list(liston: &[&str], listoff: &[&str], center: usize, rest: &str) -> String {
    let arm = |d: &&str| format!(r#"{{"id": "set", "dst": [{d}]}}"#);
    let on: Vec<String> = liston.iter().map(arm).collect();
    let off: Vec<String> = listoff.iter().map(arm).collect();
    let rest = if rest.is_empty() {
        String::new()
    } else {
        format!(", {rest}")
    };
    format!(
        r#""songlist": {{"id": "list", "center": {center}, "liston": [{}], "listoff": [{}]{rest}}}"#,
        on.join(","),
        off.join(","),
    )
}

/// A title style `text[k]` entry list: style `t` at index 0.
pub const TITLE: &str = r#""text": [{"id": "t", "dst": [{"x": 2, "y": 3, "w": 100, "h": 10}]}]"#;

/// The text `t` the title styles name.
pub const TEXT_T: &str = r#"{"id": "t", "font": "f", "size": 10, "ref": 10}"#;

/// The image set every placement draws: images 0, 1 and 5.
pub const SET: &str = r#"{"id": "set", "images": ["b0", "b1", "b0", "b0", "b0", "b5"]}"#;

/// A minimal valid selection skin: one placement, its title, the list
/// drawn once, and `more` members.
pub fn minimal(more: &str) -> String {
    let list = song_list(
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        0,
        TITLE,
    );
    let more = if more.is_empty() {
        String::new()
    } else {
        format!(", {more}")
    };
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}], {list},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}]{more}"#
    )
}

/// The player's labels the tests pass.
pub fn labels() -> SelectionLabels {
    SelectionLabels {
        all_charts: "ALL".to_owned(),
        search_results: "FOUND".to_owned(),
    }
}

/// A text element's line: its slot and string.
pub type Line = (u16, String);

/// A row's line: its slot, string and em in pixels.
pub type RowLine = (u16, String, f32);

/// An imported selection skin drawn: its atlas, state and batch.
pub struct Sel {
    pub skin: Skin,
    pub atlas: Atlas,
    pub uvs: skin_model::UvTable,
    pub sel: Selection,
    pub batch: QuadBatch,
    pub timers: SelectionTimers,
    /// The application's readouts each frame gives.
    pub app: skin_model::AppReadouts,
    /// The pointer each frame gives, in target pixels.
    pub pointer: Option<[f32; 2]>,
}

impl Sel {
    pub fn new(skin: Skin) -> Self {
        let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
        let uvs = atlas.uvs().clone();
        let sel = skin.prepare_selection();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            atlas,
            uvs,
            sel,
            batch,
            timers: SelectionTimers {
                screen: Some(TimeUs(0)),
                ..SelectionTimers::default()
            },
            app: skin_model::AppReadouts::LAUNCH,
            pointer: None,
        }
    }

    /// The skin of `root`'s `s.json`, imported, ready to draw.
    #[track_caller]
    pub fn of(root: &Path) -> Self {
        Self::new(
            import_selection(root)
                .unwrap_or_else(|e| panic!("{e:?}"))
                .skin,
        )
    }

    pub fn view(&mut self, view: &SelectionView) {
        self.skin
            .update_selection(&mut self.sel, view, &labels())
            .unwrap();
    }

    /// The lines the last view asks for: the text elements' strings, by
    /// slot, and the rows' strings with their em, by row slot.
    pub fn requests(&self) -> (Vec<Line>, Vec<RowLine>) {
        let r = self
            .skin
            .selection_text_requests(&self.sel, [1_920, 1_080], &Layout::AUTHOR);
        (
            r.texts.into_iter().map(|t| (t.slot, t.text)).collect(),
            r.rows
                .into_iter()
                .map(|t| (t.slot, t.text, t.em_px))
                .collect(),
        )
    }

    /// The quads of the frame at `ms`.
    pub fn frame(&mut self, ms: i64) -> Vec<Quad> {
        self.batch.clear();
        let frame = SelectionFrame {
            now: TimeUs(ms * 1_000),
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            timers: self.timers,
            app: self.app,
            pointer: self.pointer,
        };
        self.skin
            .build_selection(&mut self.sel, &frame, &self.uvs, &mut self.batch)
            .unwrap();
        self.batch.quads().to_vec()
    }

    /// The frame at `ms`: each quad's middle texel (its image) and its
    /// rect, exactly.
    pub fn drawn(&mut self, ms: i64) -> Vec<([u8; 2], [f32; 4])> {
        let quads = self.frame(ms);
        quads
            .iter()
            .map(|q| {
                let [x, y, ..] = quad_texel(&self.atlas, q);
                ([x, y], q.rect)
            })
            .collect()
    }

    /// The images drawn at `ms`, in order.
    pub fn images(&mut self, ms: i64) -> Vec<[u8; 2]> {
        self.drawn(ms).into_iter().map(|(t, _)| t).collect()
    }

    /// The digits of the digit quads drawn at `ms`, in order.
    pub fn digits(&mut self, ms: i64) -> String {
        self.images(ms)
            .into_iter()
            .filter_map(digit_of)
            .map(|d| char::from(b'0' + d))
            .collect()
    }
}

/// A chart row titled `title`.
pub fn chart(title: &str) -> RowView {
    RowView {
        key: RowKey::Chart(PathBuf::from(format!("{title}.bms"))),
        kind: RowKind::Chart,
        index: None,
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
pub fn folder_row(name: &str, charts: u32) -> RowView {
    RowView {
        key: RowKey::Folder(skin_model::FolderId::new(PathBuf::from(name))),
        kind: RowKind::Folder { charts },
        title: name.to_owned(),
        ..chart("")
    }
}

/// The "all charts" row of `charts`.
pub fn all_row(charts: u32) -> RowView {
    RowView {
        key: RowKey::All,
        kind: RowKind::All { charts },
        ..chart("")
    }
}

/// A best of `ex`, `bp` and `plays`, cleared on a normal gauge.
pub fn best(ex: u32, bp: u32, plays: u64) -> RowScore {
    RowScore::Best {
        plays,
        ex,
        bp,
        lamp: OwnLamp::Cleared {
            gauge: 0,
            rank: 1,
            look: GaugeLook::Normal,
        },
    }
}

/// A summary of `taps` and `longs` lasting `length_s`, its tempos `min` to
/// `max` BPM.
pub fn summary(taps: u32, longs: u32, length_s: i64, min: u128, max: u128) -> ChartSummary {
    let bpm = |n: u128| Tempo::from_units(n * Tempo::UNITS_PER_BPM).unwrap();
    ChartSummary {
        taps,
        longs,
        mines: 0,
        length: TimeUs(length_s * 1_000_000),
        tempo_start: bpm(min),
        tempo_min: bpm(min),
        tempo_max: bpm(max),
    }
}

/// A view of `rows`, the cursor on `cursor`, the selected chart from that
/// row when it is a chart, with `max_ex` and `summary`.
pub fn view(
    rows: Vec<RowView>,
    cursor: Option<usize>,
    max_ex: Option<u64>,
    summary: Option<ChartSummary>,
) -> SelectionView {
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
            summary,
            random: r.random,
            max_ex,
            score: r.score,
            judge_class: None,
            clears: 0,
            best: None,
        });
    SelectionView {
        position: None,
        rows,
        cursor,
        place: SelectionPlace::Top,
        query: String::new(),
        selected,
        list_revision: 0,
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

/// `x` as an `f32` rect component.
#[expect(clippy::cast_precision_loss, reason = "small test values")]
pub fn rect(r: [i32; 4]) -> [f32; 4] {
    r.map(|v| v as f32)
}
