//! Self-made decision skins and their frames, over the selection
//! fixtures' sheet (every texel naming its own place), imported through
//! the production path (`import` or the Lua header's import, then
//! `prepare_decision`, `update_decision`, the text attachments and
//! `build_decision`).

#![allow(dead_code)]

use std::path::Path;

use bms_model::TimeUs;
use skin_beatoraja::{
    Cancel, Config, ImportError, ImportFacts, Imported, LuaImported, LuaOutcome, import,
    read_header, read_lua_header,
};
use skin_model::{
    Atlas, DecisionFrame, DecisionScreen, DecisionTimers, FrameUv, Layout, Quad, QuadBatch,
    ResultLabels, ScreenKind, Skin, TextImage, TextTable, Variant,
};
use tempfile::TempDir;

use super::play::{LIMITS, quad_texel};
use super::selection::{IMAGES, SHEET, lua_images};
use super::{coord, font, png_with, write};

/// A decision skin folder holding `s.json` with `body` after the type,
/// size, sources, fonts and the selection fixtures' images: `s.png`,
/// `f.ttf` and the bitmap font `g.fnt`.
pub fn folder(body: &str) -> TempDir {
    let d = sheet();
    write(d.path(), "s.json", json(body).as_bytes());
    d
}

/// A folder with only the sheet and the fonts.
pub fn sheet() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    png_with(d.path(), "s.png", SHEET, SHEET, coord);
    font(d.path(), "f.ttf");
    fnt(d.path(), "g.fnt");
    d
}

/// A bitmap font `name` of size 8 on `s.png`, its glyphs `A` and `B`.
fn fnt(root: &Path, name: &str) {
    let mut s = String::from(
        "info face=\"t\" size=8 padding=0,0,0,0\n\
         common lineHeight=8 base=8 scaleW=128 scaleH=128 pages=1\n\
         page id=0 file=\"s.png\"\n\
         chars count=2\n",
    );
    for code in [65, 66] {
        s += &format!(
            "char id={code} x=0 y=0 width=8 height=8 xoffset=0 yoffset=0 xadvance=8 page=0\n"
        );
    }
    write(root, name, s.as_bytes());
}

/// The definition [`folder`] writes.
pub fn json(body: &str) -> String {
    format!(
        r#"{{"type": 6, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}, {{"id": "g", "path": "g.fnt"}}],
            "image": [{IMAGES}], {body}}}"#
    )
}

/// `s.json` of `root` imported into the decision slot with the defaults.
pub fn import_decision(root: &Path) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "s.json")?;
    import(&mut def, &h, &Config::defaults(&h), ScreenKind::Decision)
}

/// A Lua decision skin in `root`'s `s.luaskin`: `pre` statements after the
/// sources, fonts and images (`ms` is `main_state`), then `body`
/// statements setting the skin's tables.
pub fn lua_folder(pre: &str, body: &str) -> TempDir {
    lua_folder_with("", pre, body)
}

/// [`lua_folder`] with `header` fields (each ending with a comma) in the
/// header's table.
pub fn lua_folder_with(header: &str, pre: &str, body: &str) -> TempDir {
    let d = sheet();
    let entry = format!(
        "local skin = {{ type = 6, name = \"t\", w = 1920, h = 1080, {header} }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }}, {{ id = \"g\", path = \"g.fnt\" }} }}\n\
         skin.image = {{ {images} }}\n\
         {pre}\n\
         {body}\n\
         return skin\n",
        images = lua_images()
    );
    write(d.path(), "s.luaskin", entry.as_bytes());
    d
}

/// Both passes of the Lua decision skin `s.luaskin` in `root` against
/// `facts`.
pub fn lua_outcome(root: &Path, facts: &ImportFacts) -> Result<LuaOutcome, ImportError> {
    let cancel = Cancel::new();
    let h = read_lua_header(root, "s.luaskin", &cancel)?;
    h.import(&Config::defaults(h.header()), facts, &cancel)
}

/// [`lua_outcome`] on the decision scene with every fact unavailable,
/// imported.
#[track_caller]
pub fn lua_decision(root: &Path) -> Result<LuaImported, ImportError> {
    match lua_outcome(root, &ImportFacts::unavailable(ScreenKind::Decision))? {
        LuaOutcome::Imported(i) => Ok(i),
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// The chosen chart's strings the tests pass, in the bitmap font's
/// glyphs.
pub fn labels() -> ResultLabels {
    ResultLabels {
        title: "AB".to_owned(),
        subtitle: "B".to_owned(),
        artist: "A".to_owned(),
        subartist: "BB".to_owned(),
        genre: "AA".to_owned(),
    }
}

/// An imported decision skin drawn: its atlas, state and batch, the
/// labels of [`labels`] given.
pub struct Dec {
    pub skin: Skin,
    pub atlas: Atlas,
    pub dec: DecisionScreen,
    pub batch: QuadBatch,
    pub timers: DecisionTimers,
}

impl Dec {
    pub fn new(skin: Skin) -> Self {
        let atlas = skin.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
        let mut dec = skin.prepare_decision();
        skin.update_decision(&mut dec, &labels()).unwrap();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            atlas,
            dec,
            batch,
            timers: DecisionTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
                input: None,
            },
        }
    }

    /// The outline-text lines the skin asks for, by slot.
    pub fn requests(&self) -> Vec<(u16, String)> {
        self.skin
            .decision_text_requests(&self.dec, [1_920, 1_080], &Layout::AUTHOR)
            .into_iter()
            .map(|r| (r.slot, r.text))
            .collect()
    }

    /// Attaches one line per requested slot, each an ink box 1×1 em.
    pub fn attach_lines(&mut self) {
        let mut lines = vec![None; self.skin.text_slots()];
        for (slot, _) in self.requests() {
            let uv = FrameUv {
                uv: [0.0, 0.0, 0.5, 0.5],
                layer: 1,
            };
            lines[slot as usize] = TextImage::new(uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        }
        self.skin
            .attach_decision_text(&mut self.dec, TextTable::new(lines))
            .unwrap();
    }

    /// The quads of the frame at `ms`.
    pub fn frame(&mut self, ms: i64) -> Vec<Quad> {
        self.frame_at(TimeUs(ms * 1_000))
    }

    /// The quads of the frame at `now`.
    pub fn frame_at(&mut self, now: TimeUs) -> Vec<Quad> {
        self.batch.clear();
        let frame = DecisionFrame {
            now,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            timers: self.timers,
        };
        self.skin
            .build_decision(&mut self.dec, &frame, self.atlas.uvs(), &mut self.batch)
            .unwrap();
        self.batch.quads().to_vec()
    }

    /// The image quads of the frame at `ms` (layer 1 onwards holds the
    /// sheet): each one's middle texel (its image) and its top.
    pub fn drawn(&mut self, ms: i64) -> Vec<([u8; 2], f32)> {
        let quads = self.frame(ms);
        quads
            .iter()
            .map(|q| {
                let [x, y, ..] = quad_texel(&self.atlas, q);
                ([x, y], q.rect[1])
            })
            .collect()
    }

    /// The tops of the quads drawn at `ms`, sorted.
    pub fn tops(&mut self, ms: i64) -> Vec<f32> {
        self.tops_at(TimeUs(ms * 1_000))
    }

    /// The tops of the quads drawn at `now`, sorted.
    pub fn tops_at(&mut self, now: TimeUs) -> Vec<f32> {
        let mut t: Vec<f32> = self.frame_at(now).iter().map(|q| q.rect[1]).collect();
        t.sort_by(f32::total_cmp);
        t
    }
}
