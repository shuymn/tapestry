//! What the chroma-location tests share (design.md "Rendering and
//! Skins", video frames): H.264 streams (Annex B) built in test code, their
//! pictures in I_PCM macroblocks so they decode to exactly the samples
//! authored, their sequences declaring a chroma location in their VUI or
//! none; the AVI file that carries them; and the measure of where a step's
//! rise lies.

#![allow(dead_code, reason = "each test file uses part of what the tests share")]

/// Bits written most significant first, as H.264's syntax writes them.
#[derive(Default)]
pub struct Bits {
    bytes: Vec<u8>,
    used: u32,
}

impl Bits {
    pub fn bit(&mut self, one: bool) -> &mut Self {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if one {
            *self.bytes.last_mut().unwrap() |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
        self
    }

    pub fn u(&mut self, n: u32, v: u64) -> &mut Self {
        for i in (0..n).rev() {
            self.bit(v >> i & 1 == 1);
        }
        self
    }

    /// An unsigned Exp-Golomb code.
    pub fn ue(&mut self, v: u64) -> &mut Self {
        let x = v + 1;
        let n = 64 - x.leading_zeros();
        self.u(n - 1, 0).u(n, x)
    }

    /// A signed Exp-Golomb code.
    pub fn se(&mut self, v: i64) -> &mut Self {
        let m = v.unsigned_abs() * 2;
        self.ue(if v > 0 { m - 1 } else { m })
    }

    /// Zeros to the byte.
    pub fn align(&mut self) -> &mut Self {
        while self.used != 0 {
            self.bit(false);
        }
        self
    }

    /// The payload: the bits, a stop bit, and zeros to the byte.
    pub fn rbsp(&mut self) -> Vec<u8> {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
        std::mem::take(&mut self.bytes)
    }
}

/// A NAL unit with `header` and `rbsp`, after a start code, with H.264's
/// emulation prevention.
pub fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, header];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// The VUI of an H.264 sequence (ITU-T H.264 E.1.1).
#[derive(Clone, Copy, Debug)]
pub enum Vui {
    /// None.
    None,
    /// One with no chroma location (`chroma_loc_info_present_flag` 0).
    Unsited,
    /// One with this chroma sample location type, both fields alike.
    Sited(u64),
}

/// [`h264_sets`] with `vui`.
pub fn h264_sets_with(coded: (u32, u32), shown: (u32, u32), vui: Vui) -> (Vec<u8>, Vec<u8>) {
    // Baseline, level 5.1; frame numbers of 4 bits, picture order type 2,
    // one reference; frames only, cropped on the right and at the bottom
    // in units of two pixels; then the VUI.
    let mut sps = Bits::default();
    sps.u(8, 66)
        .u(8, 0)
        .u(8, 51)
        .ue(0)
        .ue(0)
        .ue(2)
        .ue(1)
        .bit(false)
        .ue(u64::from(coded.0 / 16 - 1))
        .ue(u64::from(coded.1 / 16 - 1))
        .bit(true)
        .bit(true)
        .bit(true)
        .ue(0)
        .ue(u64::from((coded.0 - shown.0) / 2))
        .ue(0)
        .ue(u64::from((coded.1 - shown.1) / 2));
    match vui {
        Vui::None => {
            sps.bit(false);
        }
        Vui::Unsited | Vui::Sited(_) => {
            // No aspect ratio, overscan or video signal type.
            sps.bit(true).bit(false).bit(false).bit(false);
            match vui {
                Vui::Sited(t) => sps.bit(true).ue(t).ue(t),
                _ => sps.bit(false),
            };
            // No timing, HRD, picture structure or bitstream restriction.
            sps.bit(false).bit(false).bit(false).bit(false).bit(false);
        }
    }
    let sps = sps.rbsp();
    // CAVLC, one slice group, one reference, no weighting, QP 26, the
    // deblocking control present.
    let pps = Bits::default()
        .ue(0)
        .ue(0)
        .bit(false)
        .bit(false)
        .ue(0)
        .ue(0)
        .ue(0)
        .bit(false)
        .u(2, 0)
        .se(0)
        .se(0)
        .se(0)
        .bit(true)
        .bit(false)
        .bit(false)
        .rbsp();
    (sps, pps)
}

/// The payload of IDR slice `i` of a picture of `coded` pixels in I_PCM
/// macroblocks, which decodes to exactly the samples `sample(plane, x, y)`
/// gives (plane 0 the luma, 1 Cb, 2 Cr, at that plane's coordinates): an I
/// slice of the whole picture, deblocking off.
pub fn h264_pcm(i: u64, coded: (u32, u32), sample: &dyn Fn(usize, u32, u32) -> u8) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.ue(0)
        .ue(7)
        .ue(0)
        .u(4, 0)
        .ue(i)
        .bit(false)
        .bit(false)
        .se(0)
        .ue(1);
    for my in 0..coded.1 / 16 {
        for mx in 0..coded.0 / 16 {
            // I_PCM, then its samples from the next byte.
            bits.ue(25).align();
            for y in 0..16 {
                for x in 0..16 {
                    bits.u(8, sample(0, mx * 16 + x, my * 16 + y).into());
                }
            }
            for plane in [1, 2] {
                for y in 0..8 {
                    for x in 0..8 {
                        bits.u(8, sample(plane, mx * 8 + x, my * 8 + y).into());
                    }
                }
            }
        }
    }
    bits.rbsp()
}

/// The access units of an H.264 stream (Annex B) of I_PCM pictures in
/// parts, each a sequence that codes `coded` pixels and crops them to
/// `shown`, with its VUI and its frames' samples, its parameter sets before
/// its first.
pub fn h264_sited(
    coded: (u32, u32),
    shown: (u32, u32),
    parts: &[(Vui, u64)],
    sample: &dyn Fn(u64, usize, u32, u32) -> u8,
) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    for &(vui, frames) in parts {
        let (sps, pps) = h264_sets_with(coded, shown, vui);
        for f in 0..frames {
            let mut unit = Vec::new();
            if f == 0 {
                unit.extend(nal(0x67, &sps));
                unit.extend(nal(0x68, &pps));
            }
            let i = u64::try_from(units.len()).unwrap();
            unit.extend(nal(
                0x65,
                &h264_pcm(i, coded, &|p, x, y| sample(i, p, x, y)),
            ));
            units.push(unit);
        }
    }
    units
}

/// An AVI file of one video stream of `fourcc` at `size` and `rate`
/// frames a second, its frames `frames`, without an index.
pub fn avi(size: (u32, u32), rate: u32, fourcc: &[u8; 4], frames: &[Vec<u8>]) -> Vec<u8> {
    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        c.extend(data);
        if data.len() % 2 == 1 {
            c.push(0);
        }
        c
    }
    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        chunk(b"LIST", &[kind.as_slice(), body].concat())
    }
    let count = u32::try_from(frames.len()).unwrap();
    let words = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let avih = words(&[
        1_000_000 / rate,
        0,
        0,
        0,
        count,
        0,
        1,
        1 << 20,
        size.0,
        size.1,
        0,
        0,
        0,
        0,
    ]);
    let strh = [
        b"vids".as_slice(),
        fourcc,
        &words(&[0, 0, 0, 1, rate, 0, count, 1 << 20, u32::MAX, 0]),
        &[0; 8],
    ]
    .concat();
    let strf = [
        words(&[40, size.0, size.1]),
        vec![1, 0, 24, 0],
        fourcc.to_vec(),
        words(&[size.0 * size.1 * 3, 0, 0, 0, 0]),
    ]
    .concat();
    let strl = list(
        b"strl",
        &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat(),
    );
    let hdrl = list(b"hdrl", &[chunk(b"avih", &avih), strl].concat());
    let movi: Vec<u8> = frames.iter().flat_map(|f| chunk(b"00dc", f)).collect();
    chunk(
        b"RIFF",
        &[b"AVI ".as_slice(), &hdrl, &list(b"movi", &movi)].concat(),
    )
}

/// The centre of a step's rise along `row`: the mean of the positions
/// between neighbours (`k + 0.5`), each weighted by the rise there. A
/// filter symmetric about each output sample's place keeps it where the
/// step was.
pub fn rise_centre(row: &[u8]) -> f64 {
    let (mut sum, mut weight) = (0.0, 0.0);
    for (k, pair) in row.windows(2).enumerate() {
        let rise = f64::from(pair[1]) - f64::from(pair[0]);
        sum += (k as f64 + 0.5) * rise;
        weight += rise;
    }
    sum / weight
}
