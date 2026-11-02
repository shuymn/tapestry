//! A self-made imported skin for the render tests, from its JSON
//! definition, bitmap font and page images through the importer
//! (`read_header`, `Config::defaults`, `import`), its atlas built for the
//! renderer and bundled as production bundles are: a fixed 1920×1080
//! canvas with lanes, a bitmap font of solid coloured glyphs, the chosen
//! chart's title, artist, genre and full title drawn in it, an untimed
//! background and animation, and LIFT's paint trimmed at the lanes'
//! bottom with an image following the lift.
//!
//! Every glyph is a 100×100 square of one colour, drawn 1:1 from the
//! top left of its text's 100×100 square, so a one-letter label paints
//! its square in its letter's colour ([`GLYPHS`]).

#![allow(dead_code)]

use std::fs;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use render::{ClipTable, Renderer, SkinBundle};
use skin_model::{Layout, Side};
use tempfile::TempDir;

/// The letters the font has and the colour each paints.
pub const GLYPHS: [(char, [u8; 3]); 5] = [
    ('A', [255, 0, 0]),
    ('B', [0, 255, 0]),
    ('C', [0, 0, 255]),
    ('D', [255, 255, 0]),
    ('E', [0, 255, 255]),
];

/// The colour letter `c` paints.
#[track_caller]
pub fn colour(c: char) -> [u8; 3] {
    GLYPHS.iter().find(|(g, _)| *g == c).unwrap().1
}

/// Where the title's line draws, in canvas pixels `[x, y, w, h]`, y down.
pub const TITLE: [i32; 4] = [600, 100, 100, 100];
/// Where the artist's line draws.
pub const ARTIST: [i32; 4] = [800, 100, 100, 100];
/// Where the genre's line draws.
pub const GENRE: [i32; 4] = [1_000, 100, 100, 100];
/// Where the full title's line draws.
pub const FULL_TITLE: [i32; 4] = [1_200, 100, 100, 100];
/// Where the untimed background draws, red.
pub const BACKGROUND: [i32; 4] = [0, 900, 100, 100];
/// Where the untimed animation starts, moving right 1 canvas
/// pixel per ms for 1 s, then held; red, as the declarative fixture's.
pub const ANIMATION: [i32; 4] = [200, 900, 100, 100];
/// LIFT's paint at rest: below the lanes, its top on their bottom (880),
/// where the trim line lies; it rises with the lift and draws only above
/// the line. Magenta.
pub const LIFT_PAINT: [i32; 4] = [20, 880, 430, 200];
/// The lanes' bottom: LIFT's trim line.
pub const TRIM_LINE: i32 = 880;
/// An image following the lift, at rest; it rises with the line. Red.
pub const FOLLOWER: [i32; 4] = [600, 800, 100, 50];
/// The lanes' length: a lift of `l` ‰ raises the line `l · 600 / 1000`.
pub const LANE_LENGTH: i32 = 600;

pub const MAGENTA: [u8; 3] = [255, 0, 255];
pub const RED: [u8; 3] = [255, 0, 0];

/// Writes a `w`×`h` RGBA PNG at `path`, texel `(x, y)` from `texel`.
fn png(path: &Path, w: u32, h: u32, texel: impl Fn(u32, u32) -> [u8; 4]) {
    let file = fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .flat_map(|(x, y)| texel(x, y))
        .collect();
    e.write_header().unwrap().write_image_data(&data).unwrap();
}

/// The font: size 100 with base 100, so every 100-tall glyph keeps the
/// line's top; one 512×128 page whose 100×100 cells are the glyphs.
fn font(dir: &Path) {
    let mut s = String::from(
        "info face=\"solid\" size=100 padding=0,0,0,0\n\
         common lineHeight=100 base=100 scaleW=512 scaleH=128 pages=1\n\
         page id=0 file=\"f.png\"\n",
    );
    for (i, (c, _)) in GLYPHS.iter().enumerate() {
        s.push_str(&format!(
            "char id={} x={} y=0 width=100 height=100 xoffset=0 yoffset=0 xadvance=100 \
             page=0 chnl=15\n",
            u32::from(*c),
            i * 100
        ));
    }
    fs::write(dir.join("f.fnt"), s).unwrap();
    png(&dir.join("f.png"), 512, 128, |x, y| {
        let cell = usize::try_from(x / 100).unwrap();
        match GLYPHS.get(cell) {
            Some((_, [r, g, b])) if y < 100 => [*r, *g, *b, 255],
            _ => [0; 4],
        }
    });
}

/// A destination of `id` over y-down canvas rect `rect`, with `extra`
/// fields and keys.
fn dst(id: &str, extra: &str, [x, y, w, h]: [i32; 4]) -> String {
    let y = 1_080 - y - h;
    format!(r#"{{"id": "{id}"{extra}, "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#)
}

/// Eight lanes: keys 50 wide from x 100, the scratch 80 wide at 20, from
/// y 280 down to the line at 880 (source y 200, 600 tall).
fn lanes() -> String {
    let mut v: Vec<String> = (0..7)
        .map(|i| format!(r#"{{"x": {}, "y": 200, "w": 50, "h": 600}}"#, 100 + 50 * i))
        .collect();
    v.push(r#"{"x": 20, "y": 200, "w": 80, "h": 600}"#.to_owned());
    v.join(", ")
}

/// The definition.
fn json() -> String {
    let eight = |id: &str| format!("[{}]", vec![format!("\"{id}\""); 8].join(", "));
    let texts = [("title", 10), ("artist", 14), ("genre", 13), ("full", 12)]
        .map(|(id, r)| format!(r#"{{"id": "{id}", "font": "f", "size": 100, "ref": {r}}}"#))
        .join(", ");
    let [ax, ay, aw, ah] = ANIMATION;
    let ay = 1_080 - ay - ah;
    let animation = format!(
        r#"{{"id": "w", "loop": 1000, "dst": [{{"time": 0, "x": {ax}, "y": {ay}, "w": {aw}, "h": {ah}}},
            {{"time": 1000, "x": {}, "y": {ay}, "w": {aw}, "h": {ah}}}]}}"#,
        ax + 1_000
    );
    let destinations = [
        r#"{"id": "notes", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#.to_owned(),
        dst("w", "", BACKGROUND),
        animation,
        dst("liftc", "", LIFT_PAINT),
        dst("w", r#", "offset": 3"#, FOLLOWER),
        dst("title", "", TITLE),
        dst("artist", "", ARTIST),
        dst("genre", "", GENRE),
        dst("full", "", FULL_TITLE),
    ]
    .join(", ");
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "w", "path": "w.png"}}, {{"id": "m", "path": "m.png"}}],
            "font": [{{"id": "f", "path": "f.fnt"}}],
            "image": [{{"id": "w", "src": "w", "x": 0, "y": 0, "w": 16, "h": 16}},
                      {{"id": "n", "src": "w", "x": 0, "y": 0, "w": 8, "h": 8}}],
            "liftCover": [{{"id": "liftc", "src": "m", "x": 0, "y": 0, "w": 16, "h": 16,
                            "disapearLine": 200}}],
            "note": {{"id": "notes", "note": {n}, "lnstart": {n}, "lnend": {n},
                      "lnbody": {n}, "lnactive": {n}, "hcnstart": {n}, "hcnend": {n},
                      "hcnbody": {n}, "hcnactive": {n}, "hcndamage": {n},
                      "hcnreactive": {n}, "mine": {n}, "dst": [{lanes}]}},
            "text": [{texts}],
            "destination": [{destinations}]}}"#,
        n = eight("n"),
        lanes = lanes(),
    )
}

/// The skin's folder, written: its definition, font and images.
pub fn folder() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    fs::write(d.join("s.json"), json()).unwrap();
    font(d);
    png(&d.join("w.png"), 16, 16, |_, _| [255, 0, 0, 255]);
    png(&d.join("m.png"), 16, 16, |_, _| [255, 0, 255, 255]);
    dir
}

/// The skin of `dir` imported with its header's defaults, nothing it
/// draws dropped.
pub fn import(dir: &Path) -> skin_beatoraja::Imported {
    let (mut def, header) = skin_beatoraja::read_header(dir, "s.json").unwrap();
    let config = skin_beatoraja::Config::defaults(&header);
    let imported = skin_beatoraja::import(&mut def, &header, &config).unwrap();
    let notes = &imported.report.notes;
    assert!(
        notes
            .iter()
            .all(|n| n.severity != skin_beatoraja::Severity::Dropped),
        "{notes:?}"
    );
    assert_eq!(imported.report.usage.texts, 4, "{notes:?}");
    imported
}

/// The skin of `dir` as a bundle for `r`: imported, its atlas built for
/// a 1920×1080 target with the author's layout, no fixed clip (JSON
/// destination clips stay unsupported), no rasterized-text producer.
pub fn bundle_in(r: &Renderer, dir: &Path) -> Arc<SkinBundle> {
    let imported = import(dir);
    let skin = imported.skin;
    let variant = skin.variant([1_920, 1_080], &Layout::AUTHOR);
    let atlas = skin.atlas(variant, r.atlas_limits()).unwrap();
    let scratch = imported.scratch.unwrap_or(Side::Left);
    Arc::new(SkinBundle::new(Arc::new(skin), atlas, ClipTable::default(), scratch).unwrap())
}

/// A fresh folder and its bundle for `r`.
pub fn bundle(r: &Renderer) -> (TempDir, Arc<SkinBundle>) {
    let dir = folder();
    let b = bundle_in(r, dir.path());
    (dir, b)
}

/// The RGB at the centre of canvas rect `rect` of a 1920-wide read back.
pub fn centre(pixels: &[u8], [x, y, w, h]: [i32; 4]) -> [u8; 3] {
    let at = |v: i32| u32::try_from(v).unwrap();
    rgb(pixels, (at(x + w / 2), at(y + h / 2)))
}

/// The colours at the title's, the artist's, the genre's and the full
/// title's squares of a 1920-wide read back.
pub fn text_at(pixels: &[u8]) -> [[u8; 3]; 4] {
    [TITLE, ARTIST, GENRE, FULL_TITLE].map(|rect| centre(pixels, rect))
}

/// The RGB at canvas pixel `(x, y)` of a 1920-wide read back.
pub fn rgb(pixels: &[u8], (x, y): (u32, u32)) -> [u8; 3] {
    let i = usize::try_from((y * 1_920 + x) * 4).unwrap();
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}
