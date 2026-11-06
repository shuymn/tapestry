//! A play skin built per chart for the tests that follow its text through
//! a play: it reads the chart's judgment class as it is built, and draws
//! bars of a red bitmap font, one per character of the chart's title.

use std::fs;
use std::path::Path;

/// The width of a character on a 1280×720 frame: 96 of the 1920 canvas.
const BAR: usize = 64;

/// Where a band's text sits, by its middle row on a 1280×720 frame.
#[derive(Clone, Copy)]
pub enum Band {
    /// A text with no op: its value is the title, or `bbb` without one.
    Always = 100,
    /// A text on op 80, the same value.
    Loading = 300,
    /// A text on op 81 and timer 40, the same value, fading over a minute.
    Loaded = 500,
}

/// Writes the skin into `dir` and chooses it for play in `appdata`.
pub fn write(dir: &Path, appdata: &Path) {
    fs::create_dir_all(dir).unwrap();
    let file = fs::File::create(dir.join("f.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let red: Vec<u8> = (0..16 * 16).flat_map(|_| [255, 0, 0, 255]).collect();
    e.write_header().unwrap().write_image_data(&red).unwrap();
    let glyph = |id: u32| {
        format!(
            "char id={id} x=0 y=0 width=16 height=16 xoffset=0 yoffset=0 xadvance=16 page=0 chnl=15\n"
        )
    };
    fs::write(
        dir.join("f.fnt"),
        format!(
            "info face=\"t\" size=16 padding=0,0,0,0\n\
             common lineHeight=16 base=16 scaleW=16 scaleH=16 pages=1\n\
             page id=0 file=\"f.png\"\nchars count=2\n{}{}",
            glyph(97),
            glyph(98)
        ),
    )
    .unwrap();
    fs::write(
        dir.join("play.luaskin"),
        "local skin = { type = 0, name = \"text\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         if ms.option(183) then skin.name = \"easy\" end\n\
         local function title()\n\
           local tx = ms.text(10)\n\
           if not tx or tx == \"\" then return \"bbb\" else return tx end\n\
         end\n\
         skin.font = { { id = \"f\", path = \"f.fnt\" } }\n\
         skin.text = {\n\
           { id = \"always\", font = \"f\", size = 96, value = title },\n\
           { id = \"loading\", font = \"f\", size = 96, value = title },\n\
           { id = \"loaded\", font = \"f\", size = 96, value = title }\n\
         }\n\
         skin.destination = {\n\
           { id = \"always\", dst = { { x = 0, y = 882, w = 1920, h = 96 } } },\n\
           { id = \"loading\", op = { 80 }, dst = { { x = 0, y = 582, w = 1920, h = 96 } } },\n\
           { id = \"loaded\", op = { 81 }, timer = 40, dst = {\n\
             { time = 0, x = 0, y = 282, w = 1920, h = 96, a = 255 }, { time = 60000, a = 0 } } }\n\
         }\n\
         return skin\n",
    )
    .unwrap();
    let root = appdata.join("tapestry");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            dir.display()
        ),
    )
    .unwrap();
}

/// How many characters `band` shows in the 1280×720 frame `png`.
pub fn bars(path: &Path, band: Band) -> usize {
    let mut reader = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut rgba).unwrap();
    assert_eq!(
        (info.width, info.height),
        (1_280, 720),
        "{}",
        path.display()
    );
    let row = band as usize * 1_280 * 4;
    let lit = rgba[row..row + 1_280 * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|&&[r, g, b, _]| r > 64 && g < 32 && b < 32)
        .count();
    (lit + BAR / 2) / BAR
}
