//! TrueType fonts written by the tests: just enough tables for the font
//! libraries (`head`, `hhea`, `maxp`, `hmtx`, `cmap`, `loca`, `glyf`,
//! `post`, `name`, optionally `OS/2` and a `GSUB` multiple substitution),
//! every glyph a rectangle or empty, and collections of them.

#![allow(dead_code)]

/// One glyph: its advance and, unless empty, its rectangle (left, bottom,
/// right, top) in font units.
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    pub advance: u16,
    pub rect: Option<[i16; 4]>,
}

/// The `OS/2` table a font has.
#[derive(Clone, Copy, Debug)]
pub enum Os2 {
    /// None.
    Absent,
    /// Version 1: no cap height.
    V1,
    /// Version 4 with this `sCapHeight`.
    V4 { cap_height: i16 },
}

/// A font to write.
#[derive(Clone, Debug)]
pub struct Spec {
    pub family: String,
    pub upem: u16,
    pub ascender: i16,
    pub descender: i16,
    /// Glyph 0 is `.notdef`.
    pub glyphs: Vec<Glyph>,
    /// Characters to glyphs.
    pub cmap: Vec<(char, u16)>,
    pub os2: Os2,
    /// A multiple substitution under `ccmp`: one glyph into a sequence.
    pub multiple: Option<(u16, Vec<u16>)>,
    /// Further tables, written as given.
    pub extra: Vec<([u8; 4], Vec<u8>)>,
    /// A `wght` axis and one glyph's variation along it.
    pub variation: Option<Variation>,
}

/// A `wght` axis from `min` to `max` with default `default`, along which
/// `glyph`, a rectangle, has its top raised by `rise` units at `min` (and
/// in proportion between `min` and `default`).
#[derive(Clone, Copy, Debug)]
pub struct Variation {
    pub min: i16,
    pub default: i16,
    pub max: i16,
    pub glyph: u16,
    pub rise: i16,
}

/// The glyph of the font's `n`-th Latin capital (`A` + n) in [`latin`].
pub const fn capital(n: u16) -> u16 {
    2 + n
}

/// A Latin face of 1000 units per em, named `family`: `.notdef` a 500 ×
/// 700 rectangle, the space empty, `A` to `Z` and `a` to `z` rectangles
/// from 50 units right of the pen on the baseline, each letter 12
/// (capitals, 700 tall, from 300 wide) or 10 (small letters, 500 tall,
/// from 250 wide) units wider than the one before and advancing 50 past
/// its right edge; cap height 700 in `OS/2` version 4.
pub fn latin(family: &str) -> Spec {
    let mut glyphs = vec![
        Glyph {
            advance: 600,
            rect: Some([50, 0, 550, 700]),
        },
        Glyph {
            advance: 300,
            rect: None,
        },
    ];
    let mut cmap = vec![(' ', 1)];
    for (i, c) in (0_i16..).zip('A'..='Z') {
        let right = 350 + 12 * i;
        glyphs.push(Glyph {
            advance: u16::try_from(right + 50).unwrap(),
            rect: Some([50, 0, right, 700]),
        });
        cmap.push((c, u16::try_from(2 + i).unwrap()));
    }
    for (i, c) in (0_i16..).zip('a'..='z') {
        let right = 300 + 10 * i;
        glyphs.push(Glyph {
            advance: u16::try_from(right + 50).unwrap(),
            rect: Some([50, 0, right, 500]),
        });
        cmap.push((c, u16::try_from(28 + i).unwrap()));
    }
    Spec {
        family: family.to_owned(),
        upem: 1_000,
        ascender: 800,
        descender: -200,
        glyphs,
        cmap,
        os2: Os2::V4 { cap_height: 700 },
        multiple: None,
        extra: Vec::new(),
        variation: None,
    }
}

/// A CJK face named `family`: [`latin`]'s glyphs and `あ` (U+3042) to
/// `ん` (U+3093), each a 900 × 900 square from -100 advancing 1000.
pub fn cjk(family: &str) -> Spec {
    let mut s = latin(family);
    for c in '\u{3042}'..='\u{3093}' {
        let g = u16::try_from(s.glyphs.len()).unwrap();
        s.glyphs.push(Glyph {
            advance: 1_000,
            rect: Some([50, -100, 950, 800]),
        });
        s.cmap.push((c, g));
    }
    s
}

fn be16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn bei16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// The font `spec` as a TrueType file.
pub fn build(spec: &Spec) -> Vec<u8> {
    let tables = tables(spec);
    let mut out = Vec::new();
    directory(&mut out, &tables, 0);
    out
}

/// A collection of `fonts`, face 0 first.
pub fn collection(fonts: &[Spec]) -> Vec<u8> {
    let header = 12 + 4 * fonts.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"ttcf");
    be32(&mut out, 0x0001_0000);
    be32(&mut out, u32::try_from(fonts.len()).unwrap());
    out.resize(header, 0);
    for (i, spec) in fonts.iter().enumerate() {
        let at = out.len();
        out[12 + 4 * i..16 + 4 * i].copy_from_slice(&u32::try_from(at).unwrap().to_be_bytes());
        let tables = tables(spec);
        let mut font = Vec::new();
        directory(&mut font, &tables, at);
        out.extend_from_slice(&font);
    }
    out
}

/// Writes the table directory and the tables, offsets counted from
/// `base` bytes before `out`'s start.
fn directory(out: &mut Vec<u8>, tables: &[([u8; 4], Vec<u8>)], base: usize) {
    let mut tables = tables.to_vec();
    tables.sort_by_key(|t| t.0);
    let n = u16::try_from(tables.len()).unwrap();
    be32(out, 0x0001_0000);
    be16(out, n);
    let mut pow = 1_u16;
    let mut log = 0_u16;
    while pow * 2 <= n {
        pow *= 2;
        log += 1;
    }
    be16(out, pow * 16);
    be16(out, log);
    be16(out, n * 16 - pow * 16);
    let mut at = 12 + 16 * tables.len();
    let mut body = Vec::new();
    for (tag, data) in &tables {
        out.extend_from_slice(tag);
        be32(out, 0);
        be32(out, u32::try_from(base + at + body.len()).unwrap());
        be32(out, u32::try_from(data.len()).unwrap());
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }
    at += body.len();
    let _ = at;
    out.extend_from_slice(&body);
}

fn tables(spec: &Spec) -> Vec<([u8; 4], Vec<u8>)> {
    let n = u16::try_from(spec.glyphs.len()).unwrap();
    let mut t = vec![
        (*b"head", head(spec)),
        (*b"hhea", hhea(spec, n)),
        (*b"maxp", maxp(n)),
        (*b"hmtx", hmtx(spec)),
        (*b"cmap", cmap(spec)),
        (*b"post", post()),
        (*b"name", name(&spec.family)),
    ];
    let (loca, glyf) = glyf(spec);
    t.push((*b"loca", loca));
    t.push((*b"glyf", glyf));
    if let Some(os2) = os2(spec) {
        t.push((*b"OS/2", os2));
    }
    if let Some((from, to)) = &spec.multiple {
        t.push((*b"GSUB", gsub(*from, to)));
    }
    if let Some(v) = spec.variation {
        t.push((*b"fvar", fvar(v)));
        t.push((*b"gvar", gvar(v, n)));
    }
    t.extend(spec.extra.iter().cloned());
    t
}

fn fixed(v: i16) -> u32 {
    u32::from_be_bytes((i32::from(v) << 16).to_be_bytes())
}

fn fvar(v: Variation) -> Vec<u8> {
    let mut o = Vec::new();
    for x in [1_u16, 0, 16, 2, 1, 20, 0, 8] {
        be16(&mut o, x);
    }
    o.extend_from_slice(b"wght");
    for x in [v.min, v.default, v.max] {
        be32(&mut o, fixed(x));
    }
    be16(&mut o, 0);
    be16(&mut o, 256);
    o
}

/// One tuple at the axis's minimum for `v.glyph`, applying to all its
/// points: its four corners and four phantom points, the top two raised.
fn gvar(v: Variation, n: u16) -> Vec<u8> {
    let mut data = Vec::new();
    be16(&mut data, 0x8001); // shared point numbers, one tuple
    be16(&mut data, 10); // the serialized data, after this header
    be16(&mut data, 18); // the tuple's data
    be16(&mut data, 0x8000); // an embedded peak
    be16(&mut data, 0xC000); // -1.0: the axis's minimum
    data.push(0); // shared points: all of them
    data.push(0x87); // x: eight zeros
    data.push(0x47); // y: eight words
    for dy in [0, 0, v.rise, v.rise, 0, 0, 0, 0] {
        bei16(&mut data, dy);
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let header = 20 + 4 * (usize::from(n) + 1);
    let mut o = Vec::new();
    be16(&mut o, 1);
    be16(&mut o, 0);
    be16(&mut o, 1);
    be16(&mut o, 0);
    be32(&mut o, u32::try_from(header).unwrap());
    be16(&mut o, n);
    be16(&mut o, 1); // long offsets
    be32(&mut o, u32::try_from(header).unwrap());
    for g in 0..=n {
        be32(
            &mut o,
            if g <= v.glyph {
                0
            } else {
                u32::try_from(data.len()).unwrap()
            },
        );
    }
    o.extend_from_slice(&data);
    o
}

fn head(spec: &Spec) -> Vec<u8> {
    let mut o = Vec::new();
    be32(&mut o, 0x0001_0000);
    be32(&mut o, 0x0001_0000);
    be32(&mut o, 0);
    be32(&mut o, 0x5F0F_3CF5);
    be16(&mut o, 0b11);
    be16(&mut o, spec.upem);
    o.extend_from_slice(&[0; 16]);
    for v in [0_i16, spec.descender, 1_000, spec.ascender] {
        bei16(&mut o, v);
    }
    be16(&mut o, 0);
    be16(&mut o, 8);
    bei16(&mut o, 2);
    bei16(&mut o, 1);
    bei16(&mut o, 0);
    o
}

fn hhea(spec: &Spec, n: u16) -> Vec<u8> {
    let mut o = Vec::new();
    be32(&mut o, 0x0001_0000);
    bei16(&mut o, spec.ascender);
    bei16(&mut o, spec.descender);
    bei16(&mut o, 0);
    be16(
        &mut o,
        spec.glyphs.iter().map(|g| g.advance).max().unwrap_or(0),
    );
    for v in [0_i16, 0, 1_000, 1, 0, 0, 0, 0, 0, 0, 0] {
        bei16(&mut o, v);
    }
    be16(&mut o, n);
    o
}

fn maxp(n: u16) -> Vec<u8> {
    let mut o = Vec::new();
    be32(&mut o, 0x0001_0000);
    be16(&mut o, n);
    for v in [16_u16, 2, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0] {
        be16(&mut o, v);
    }
    o
}

fn hmtx(spec: &Spec) -> Vec<u8> {
    let mut o = Vec::new();
    for g in &spec.glyphs {
        be16(&mut o, g.advance);
        bei16(&mut o, g.rect.map_or(0, |r| r[0]));
    }
    o
}

fn cmap(spec: &Spec) -> Vec<u8> {
    let mut map = spec.cmap.clone();
    map.sort_by_key(|(c, _)| *c);
    let mut o = Vec::new();
    be16(&mut o, 0);
    be16(&mut o, 1);
    be16(&mut o, 3);
    be16(&mut o, 10);
    be32(&mut o, 12);
    be16(&mut o, 12);
    be16(&mut o, 0);
    be32(&mut o, u32::try_from(16 + 12 * map.len()).unwrap());
    be32(&mut o, 0);
    be32(&mut o, u32::try_from(map.len()).unwrap());
    for (c, g) in map {
        be32(&mut o, u32::from(c));
        be32(&mut o, u32::from(c));
        be32(&mut o, u32::from(g));
    }
    o
}

fn post() -> Vec<u8> {
    let mut o = Vec::new();
    be32(&mut o, 0x0003_0000);
    o.extend_from_slice(&[0; 28]);
    o
}

fn name(family: &str) -> Vec<u8> {
    let utf16: Vec<u8> = family.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let ids = [1_u16, 4, 16];
    let mut o = Vec::new();
    be16(&mut o, 0);
    be16(&mut o, u16::try_from(ids.len()).unwrap());
    be16(&mut o, u16::try_from(6 + 12 * ids.len()).unwrap());
    for id in ids {
        for v in [3_u16, 1, 0x409, id, u16::try_from(utf16.len()).unwrap(), 0] {
            be16(&mut o, v);
        }
    }
    o.extend_from_slice(&utf16);
    o
}

fn os2(spec: &Spec) -> Option<Vec<u8>> {
    let version = match spec.os2 {
        Os2::Absent => return None,
        Os2::V1 => 1,
        Os2::V4 { .. } => 4,
    };
    let mut o = Vec::new();
    be16(&mut o, version);
    bei16(&mut o, 500);
    be16(&mut o, 400);
    be16(&mut o, 5);
    be16(&mut o, 0);
    o.extend_from_slice(&[0; 22]);
    o.extend_from_slice(&[0; 10]);
    o.extend_from_slice(&[0; 16]);
    o.extend_from_slice(b"TEST");
    be16(&mut o, 0x40);
    be16(&mut o, 0x20);
    be16(&mut o, 0xFFFF);
    bei16(&mut o, spec.ascender);
    bei16(&mut o, spec.descender);
    bei16(&mut o, 0);
    be16(&mut o, u16::try_from(spec.ascender).unwrap_or(0));
    be16(&mut o, u16::try_from(-spec.descender).unwrap_or(0));
    assert_eq!(o.len(), 78);
    o.extend_from_slice(&[0; 8]);
    if let Os2::V4 { cap_height } = spec.os2 {
        bei16(&mut o, 500);
        bei16(&mut o, cap_height);
        be16(&mut o, 0);
        be16(&mut o, 32);
        be16(&mut o, 1);
    }
    Some(o)
}

/// `loca` (long) and `glyf`.
fn glyf(spec: &Spec) -> (Vec<u8>, Vec<u8>) {
    let mut loca = Vec::new();
    let mut glyf = Vec::new();
    for g in &spec.glyphs {
        be32(&mut loca, u32::try_from(glyf.len()).unwrap());
        let Some([l, b, r, t]) = g.rect else {
            continue;
        };
        bei16(&mut glyf, 1);
        for v in [l, b, r, t] {
            bei16(&mut glyf, v);
        }
        be16(&mut glyf, 3);
        be16(&mut glyf, 0);
        glyf.extend_from_slice(&[1; 4]);
        // Counter-clockwise from the bottom left: (l, b), (r, b), (r, t),
        // (l, t), as deltas.
        for dx in [l, r - l, 0, l - r] {
            bei16(&mut glyf, dx);
        }
        for dy in [b, 0, t - b, 0] {
            bei16(&mut glyf, dy);
        }
        while glyf.len() % 4 != 0 {
            glyf.push(0);
        }
    }
    be32(&mut loca, u32::try_from(glyf.len()).unwrap());
    (loca, glyf)
}

/// A `GSUB` with one `ccmp` feature, for the `DFLT` and `latn` scripts,
/// whose one lookup replaces glyph `from` with `to`.
fn gsub(from: u16, to: &[u16]) -> Vec<u8> {
    // Script list.
    let mut script = Vec::new();
    be16(&mut script, 4); // default LangSys offset
    be16(&mut script, 0); // LangSys count
    be16(&mut script, 0); // lookupOrder
    be16(&mut script, 0xFFFF); // required feature
    be16(&mut script, 1);
    be16(&mut script, 0);
    let mut scripts = Vec::new();
    be16(&mut scripts, 2);
    let first = 2 + 2 * 6;
    for tag in [b"DFLT", b"latn"] {
        scripts.extend_from_slice(tag);
        be16(&mut scripts, u16::try_from(first).unwrap());
    }
    scripts.extend_from_slice(&script);
    // Feature list.
    let mut features = Vec::new();
    be16(&mut features, 1);
    features.extend_from_slice(b"ccmp");
    be16(&mut features, 8);
    be16(&mut features, 0);
    be16(&mut features, 1);
    be16(&mut features, 0);
    // Lookup list: one type 2 lookup.
    let mut sub = Vec::new();
    be16(&mut sub, 1);
    be16(&mut sub, 8); // coverage
    be16(&mut sub, 1);
    be16(&mut sub, 14); // sequence
    be16(&mut sub, 1);
    be16(&mut sub, 1);
    be16(&mut sub, from);
    be16(&mut sub, u16::try_from(to.len()).unwrap());
    for g in to {
        be16(&mut sub, *g);
    }
    let mut lookups = Vec::new();
    be16(&mut lookups, 1);
    be16(&mut lookups, 4);
    be16(&mut lookups, 2);
    be16(&mut lookups, 0);
    be16(&mut lookups, 1);
    be16(&mut lookups, 8);
    lookups.extend_from_slice(&sub);
    let mut o = Vec::new();
    be32(&mut o, 0x0001_0000);
    let s = 10;
    let f = s + scripts.len();
    let l = f + features.len();
    for v in [s, f, l] {
        be16(&mut o, u16::try_from(v).unwrap());
    }
    o.extend_from_slice(&scripts);
    o.extend_from_slice(&features);
    o.extend_from_slice(&lookups);
    o
}
