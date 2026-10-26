//! The simple skin: a hard-coded 16:9 layout authored at 1920×1080 and
//! scaled by the renderer (D9; design.md "Rendering and Skins").
//!
//! Notes are placed from the expected display time through the chart's
//! [`ScrollMap`] and the hi-speed visible time (D9): the scale is fixed
//! from the chart's initial BPM, so a note takes exactly `visible_time` from
//! the lane top to the judgment line at the start BPM. During play notes
//! move through scroll positions at that fixed scale, so BPM changes speed
//! them up or slow them down and STOPs freeze them, as the chart intends,
//! and nothing ever jumps. Only notes inside the visible range are visited,
//! found per lane by binary search. A note judged by an effect is hidden
//! through a per-note bit set sized in `prepare`.

use bms_model::{Chart, LANE_COUNT, Lane, Note, NoteKind, Position, ScrollMap, TimeUs};
use eval::{JudgeEvent, JudgeEventKind};
use rules::{GaugeValue, Judgment};

use crate::atlas::{GLYPH_H, GLYPH_W, glyph_uv, solid_uv};
use crate::{Blend, FrameInput, Gpu, PlaySkin, SkinError, Sprite, SpriteBatch};

/// The layout, in layout pixels of the 1920×1080 base resolution, and the
/// placement of notes in it.
pub mod layout {
    use bms_model::{Lane, ScrollMap, TimeUs};

    /// Base width the layout is authored at.
    pub const WIDTH: f32 = 1920.0;
    /// Base height the layout is authored at.
    pub const HEIGHT: f32 = 1080.0;
    /// Left edge of the scratch lane.
    pub const LANE_LEFT: f32 = 160.0;
    /// Width of the scratch lane.
    pub const SCRATCH_W: f32 = 96.0;
    /// Width of each key lane.
    pub const KEY_W: f32 = 64.0;
    /// Right edge of the last key lane.
    pub const LANES_RIGHT: f32 = LANE_LEFT + SCRATCH_W + 7.0 * KEY_W;
    /// Top of the lanes, where a note appears `visible_time` before it
    /// reaches the judgment line.
    pub const LANE_TOP: f32 = 80.0;
    /// The judgment line.
    pub const JUDGE_Y: f32 = 880.0;
    /// Bottom of the lanes: a late note keeps falling below the judgment
    /// line until here, or until its miss is judged.
    pub const LANE_BOTTOM: f32 = 960.0;
    /// Height of a note; a note's position is its vertical center.
    pub const NOTE_H: f32 = 16.0;
    /// Microseconds per minute, which turns a BPM into a scroll rate.
    const US_PER_MINUTE: f64 = 60_000_000.0;

    /// Left edge and width of `lane`.
    #[must_use]
    pub fn lane_x(lane: Lane) -> (f32, f32) {
        match lane.index() {
            0 => (LANE_LEFT, SCRATCH_W),
            k => (LANE_LEFT + SCRATCH_W + (k - 1) as f32 * KEY_W, KEY_W),
        }
    }

    /// Where the scroll stands for one frame: the display position at the
    /// display time and the pixels per unit of position.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Scale {
        /// The display position at the judgment line.
        pub at_line: i64,
        /// Pixels per millionth of a beat.
        pub px_per_pos: f64,
    }

    impl Scale {
        /// The scale at display time `at` with hi-speed `visible_time`. A
        /// visible time below 1 µs counts as 1 µs.
        ///
        /// The lane from [`LANE_TOP`] to [`JUDGE_Y`] holds the scroll of
        /// `visible_time` at the chart's initial BPM. The pixels per
        /// position never change during a play, whatever the BPM at `at`.
        #[must_use]
        pub fn new(scroll: &ScrollMap, at: TimeUs, visible_time: TimeUs) -> Self {
            let visible = visible_time.0.max(1) as f64;
            let bpm_micro = scroll.initial_bpm_micro() as f64;
            // Position units per µs are bpm_micro / US_PER_MINUTE.
            let span = visible * bpm_micro / US_PER_MINUTE;
            Self {
                at_line: scroll.position_at(at).0,
                px_per_pos: f64::from(JUDGE_Y - LANE_TOP) / span,
            }
        }

        /// The vertical center of something at display position `p`.
        #[must_use]
        pub fn y(&self, p: i64) -> f32 {
            let ahead = (i128::from(p) - i128::from(self.at_line)) as f64;
            (f64::from(JUDGE_Y) - ahead * self.px_per_pos) as f32
        }

        /// The positions whose vertical center lies `below` pixels under
        /// the judgment line through `above` pixels over it, widened to
        /// whole units.
        #[must_use]
        pub fn positions(&self, below: f32, above: f32) -> (i64, i64) {
            let units = |px: f32| (f64::from(px) / self.px_per_pos).ceil() as i64;
            (
                self.at_line.saturating_sub(units(below)),
                self.at_line.saturating_add(units(above)),
            )
        }
    }

    /// The vertical center of a note at `time` in a frame displayed at `at`
    /// with hi-speed `visible_time`.
    #[must_use]
    pub fn note_y(scroll: &ScrollMap, time: TimeUs, at: TimeUs, visible_time: TimeUs) -> f32 {
        Scale::new(scroll, at, visible_time).y(scroll.position_at(time).0)
    }
}

use layout::{JUDGE_Y, LANE_BOTTOM, LANE_LEFT, LANE_TOP, LANES_RIGHT, NOTE_H, Scale};

type Rgba = [f32; 4];

const LANE_BG: Rgba = [0.04, 0.04, 0.05, 1.0];
const SCRATCH_BG: Rgba = [0.07, 0.04, 0.04, 1.0];
const SEPARATOR: Rgba = [0.18, 0.18, 0.2, 1.0];
const BAR_LINE: Rgba = [0.5, 0.5, 0.5, 1.0];
const JUDGE_LINE: Rgba = [1.0, 0.2, 0.2, 1.0];
const NOTE_WHITE: Rgba = [0.95, 0.95, 0.95, 1.0];
const NOTE_BLUE: Rgba = [0.35, 0.55, 1.0, 1.0];
const NOTE_SCRATCH: Rgba = [1.0, 0.35, 0.35, 1.0];
const MINE: Rgba = [0.6, 0.1, 0.1, 1.0];
const LN_BODY_SHADE: f32 = 0.6;
const GAUGE_BG: Rgba = [0.12, 0.12, 0.12, 1.0];
const GAUGE_FILL: Rgba = [0.2, 0.85, 0.3, 1.0];
const TEXT: Rgba = [0.9, 0.9, 0.9, 1.0];
const COUNTDOWN: Rgba = [1.0, 0.9, 0.4, 1.0];
const BEAM: Rgba = [0.25, 0.3, 0.55, 0.8];
const BOMB: Rgba = [1.0, 0.6, 0.15, 1.0];

/// Height of a mine.
const MINE_H: f32 = 8.0;
/// Height of the judgment line.
const JUDGE_LINE_H: f32 = 6.0;
/// Height of a bar line.
const BAR_LINE_H: f32 = 2.0;
/// How long a key beam fades after a press.
const BEAM_US: i64 = 100_000;
/// How long a bomb fades after a hit.
const BOMB_US: i64 = 200_000;
/// Side of a bomb at its start.
const BOMB_SIDE: f32 = 112.0;
/// How long the judgment text stays.
const JUDGMENT_US: i64 = 600_000;
/// Center of the judgment text, and its scale in layout pixels per texel.
const JUDGMENT_X: f32 = (LANE_LEFT + LANES_RIGHT) / 2.0;
const JUDGMENT_Y: f32 = 560.0;
const JUDGMENT_SCALE: f32 = 5.0;
/// Top of the combo, below the judgment text.
const COMBO_Y: f32 = 610.0;
/// The gauge bar under the lanes.
const GAUGE_Y: f32 = 1000.0;
const GAUGE_H: f32 = 24.0;
/// The score, right of the lanes.
const SCORE_X: f32 = 760.0;
const SCORE_Y: f32 = 120.0;
const SCORE_SCALE: f32 = 4.0;
const SMALL_SCALE: f32 = 3.0;

/// One bit per note of a lane.
#[derive(Clone, Debug, Default)]
struct Bits(Vec<u64>);

impl Bits {
    fn sized(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    /// Sets bit `i`; an index past the lane is ignored.
    fn set(&mut self, i: usize) {
        if let Some(w) = self.0.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
    }

    fn get(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

/// The hard-coded play screen.
#[derive(Debug)]
pub struct SimpleSkin {
    lanes: [Vec<Note>; LANE_COUNT],
    bar_lines: Vec<TimeUs>,
    scroll: ScrollMap,
    /// Heads of long notes judged and held.
    held: [Bits; LANE_COUNT],
    /// Notes finished: judged taps, missed heads, finished long notes and
    /// mines hit.
    done: [Bits; LANE_COUNT],
    /// Latest press per lane, for the key beam.
    pressed: [Option<TimeUs>; LANE_COUNT],
    /// Latest hit per lane, for the bomb.
    hit: [Option<TimeUs>; LANE_COUNT],
    /// Latest judgment shown, and when.
    judgment: Option<(Judgment, TimeUs)>,
}

impl Default for SimpleSkin {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleSkin {
    /// A skin with no chart loaded: it draws the empty lanes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lanes: Default::default(),
            bar_lines: Vec::new(),
            scroll: ScrollMap::new(120_000_000, &[]).expect("120 BPM is a valid scroll"),
            held: Default::default(),
            done: Default::default(),
            pressed: [None; LANE_COUNT],
            hit: [None; LANE_COUNT],
            judgment: None,
        }
    }

    /// Takes `chart`'s notes, bar lines and scroll, and resets the play
    /// state. This is all of `prepare`; the skin needs nothing from the GPU.
    pub fn load(&mut self, chart: &Chart) {
        self.lanes = chart.lanes.clone();
        self.bar_lines = chart.bar_lines.clone();
        self.scroll = chart.scroll.clone();
        self.held = std::array::from_fn(|l| Bits::sized(self.lanes[l].len()));
        self.done = std::array::from_fn(|l| Bits::sized(self.lanes[l].len()));
        self.pressed = [None; LANE_COUNT];
        self.hit = [None; LANE_COUNT];
        self.judgment = None;
    }

    fn apply(&mut self, ev: &JudgeEvent) {
        match ev.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                ..
            } => {
                let l = lane.index();
                let long = matches!(
                    self.lanes[l].get(note).map(|n| n.kind),
                    Some(NoteKind::Long { .. })
                );
                // A missed head finishes its long note too.
                if long && judgment != Judgment::Poor {
                    self.held[l].set(note);
                } else {
                    self.done[l].set(note);
                }
                if judgment != Judgment::Poor {
                    self.hit[l] = Some(ev.time);
                }
                if deviation.is_some() {
                    self.pressed[l] = Some(ev.time);
                }
                self.judgment = Some((judgment, ev.time));
            }
            JudgeEventKind::LnEnd { lane, note, .. } | JudgeEventKind::MineHit { lane, note } => {
                self.done[lane.index()].set(note);
            }
            JudgeEventKind::EmptyPoor { lane } => {
                self.pressed[lane.index()] = Some(ev.time);
                self.judgment = Some((Judgment::Poor, ev.time));
            }
            JudgeEventKind::PlaySound { lane, .. } => {
                self.pressed[lane.index()] = Some(ev.time);
            }
            JudgeEventKind::Gauge { .. } | JudgeEventKind::SongEnd => {}
        }
    }

    fn lanes_and_lines(out: &mut SpriteBatch) {
        for lane in Lane::all() {
            let (x, w) = layout::lane_x(lane);
            let bg = if lane == Lane::SCRATCH {
                SCRATCH_BG
            } else {
                LANE_BG
            };
            rect(
                out,
                Blend::Alpha,
                x,
                LANE_TOP,
                w,
                LANE_BOTTOM - LANE_TOP,
                bg,
            );
            rect(
                out,
                Blend::Alpha,
                x,
                LANE_TOP,
                1.0,
                LANE_BOTTOM - LANE_TOP,
                SEPARATOR,
            );
        }
        rect(
            out,
            Blend::Alpha,
            LANES_RIGHT,
            LANE_TOP,
            1.0,
            LANE_BOTTOM - LANE_TOP,
            SEPARATOR,
        );
    }

    fn bar_lines(&self, out: &mut SpriteBatch, scale: &Scale, from: TimeUs, to: TimeUs) {
        let start = self.bar_lines.partition_point(|&t| t < from);
        for &t in self.bar_lines[start..].iter().take_while(|&&t| t <= to) {
            let y = scale.y(self.scroll.position_at(t).0);
            if !cap(
                out,
                LANE_LEFT,
                LANES_RIGHT - LANE_LEFT,
                y,
                BAR_LINE_H,
                BAR_LINE,
            ) {
                return;
            }
        }
    }

    fn notes(&self, out: &mut SpriteBatch, scale: &Scale, from: TimeUs, to: TimeUs) {
        for lane in Lane::all() {
            let l = lane.index();
            let notes = &self.lanes[l];
            let (x, w) = layout::lane_x(lane);
            let color = note_color(lane);
            let start = notes.partition_point(|n| n.end() < from);
            for (i, note) in notes.iter().enumerate().skip(start) {
                if note.time > to {
                    break;
                }
                if self.done[l].get(i) {
                    continue;
                }
                let head = scale.y(self.scroll.position_at(note.time).0);
                let pushed = match note.kind {
                    NoteKind::Tap => cap(out, x, w, head, NOTE_H, color),
                    NoteKind::Mine { .. } => cap(out, x, w, head, MINE_H, MINE),
                    NoteKind::Long { end, .. } => {
                        let tail = scale.y(self.scroll.position_at(end).0);
                        // A held head stays on the judgment line.
                        let (head, tail) = if self.held[l].get(i) {
                            (head.min(JUDGE_Y), tail.min(JUDGE_Y))
                        } else {
                            (head, tail)
                        };
                        let body = shade(color, LN_BODY_SHADE);
                        let body_ok = match clip(tail, head) {
                            Some((y0, y1)) => rect(out, Blend::Alpha, x, y0, w, y1 - y0, body),
                            None => true,
                        };
                        body_ok
                            && cap(out, x, w, tail, NOTE_H, color)
                            && cap(out, x, w, head, NOTE_H, color)
                    }
                };
                if !pushed {
                    // Full: the rest of the frame is dropped, so the work
                    // stays bounded by the batch.
                    return;
                }
            }
        }
    }

    fn effects(&self, out: &mut SpriteBatch, at: TimeUs) {
        for lane in Lane::all() {
            let (x, w) = layout::lane_x(lane);
            if let Some(f) = self.pressed[lane.index()].and_then(|t| fade(at, t, BEAM_US)) {
                let c = [BEAM[0], BEAM[1], BEAM[2], BEAM[3] * f];
                rect(out, Blend::Additive, x, LANE_TOP, w, JUDGE_Y - LANE_TOP, c);
            }
            if let Some(f) = self.hit[lane.index()].and_then(|t| fade(at, t, BOMB_US)) {
                let side = BOMB_SIDE * (0.5 + 0.5 * f);
                let cx = x + w / 2.0;
                let c = [BOMB[0], BOMB[1], BOMB[2], f];
                rect(
                    out,
                    Blend::Additive,
                    cx - side / 2.0,
                    JUDGE_Y - side / 2.0,
                    side,
                    side,
                    c,
                );
            }
        }
    }

    fn hud(&self, f: &FrameInput<'_>, out: &mut SpriteBatch, at: TimeUs) {
        if let Some((j, t)) = self.judgment
            && fade(at, t, JUDGMENT_US).is_some()
        {
            let (name, color) = judgment_text(j);
            let width = text_width(name.len(), JUDGMENT_SCALE);
            text(
                out,
                JUDGMENT_X - width / 2.0,
                JUDGMENT_Y,
                JUDGMENT_SCALE,
                color,
                name,
            );
            let combo = f.view.tally.combo;
            if combo > 0 && j != Judgment::Poor {
                let mut buf = [0; 10];
                let digits = decimal(combo, &mut buf);
                let width = text_width(digits.len(), JUDGMENT_SCALE);
                text(
                    out,
                    JUDGMENT_X - width / 2.0,
                    COMBO_Y,
                    JUDGMENT_SCALE,
                    TEXT,
                    digits,
                );
            }
        }
        if f.now.0 < 0 {
            countdown(out, f.now);
        }
        let mut buf = [0; 10];
        text(out, SCORE_X, SCORE_Y, SCORE_SCALE, TEXT, b"EX");
        let score = decimal(f.view.tally.ex_score, &mut buf);
        let x = SCORE_X + text_width(3, SCORE_SCALE);
        text(out, x, SCORE_Y, SCORE_SCALE, TEXT, score);

        let w = LANES_RIGHT - LANE_LEFT;
        rect(out, Blend::Alpha, LANE_LEFT, GAUGE_Y, w, GAUGE_H, GAUGE_BG);
        if let Some(g) = f.view.gauges.get(f.gauge) {
            let v = g.value.0.clamp(0, GaugeValue::FULL.0);
            let filled = w * (v as f32 / GaugeValue::FULL.0 as f32);
            rect(
                out,
                Blend::Alpha,
                LANE_LEFT,
                GAUGE_Y,
                filled,
                GAUGE_H,
                GAUGE_FILL,
            );
            let percent = u32::try_from(v / GaugeValue::PERCENT).unwrap_or(0);
            let digits = decimal(percent, &mut buf);
            let x = LANES_RIGHT + 16.0;
            let y = GAUGE_Y + 2.0;
            text(out, x, y, SMALL_SCALE, GAUGE_FILL, digits);
            let x = x + text_width(digits.len(), SMALL_SCALE);
            text(out, x, y, SMALL_SCALE, GAUGE_FILL, b"%");
        }
    }
}

impl PlaySkin for SimpleSkin {
    fn prepare(&mut self, _gpu: &Gpu, chart: &Chart) -> Result<(), SkinError> {
        self.load(chart);
        Ok(())
    }

    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch) {
        for ev in f.effects {
            self.apply(ev);
        }
        let at = f.display_time();
        let scale = Scale::new(&self.scroll, at, f.visible_time);
        let half = NOTE_H / 2.0;
        let (lo, hi) = scale.positions(LANE_BOTTOM - JUDGE_Y + half, JUDGE_Y - LANE_TOP + half);
        let (from, to) = self.scroll.time_range(Position(lo), Position(hi));
        Self::lanes_and_lines(out);
        self.bar_lines(out, &scale, from, to);
        let w = LANES_RIGHT - LANE_LEFT;
        let y = JUDGE_Y - JUDGE_LINE_H / 2.0;
        rect(out, Blend::Alpha, LANE_LEFT, y, w, JUDGE_LINE_H, JUDGE_LINE);
        self.notes(out, &scale, from, to);
        // Feedback ages on the frame's own clock: the display offset moves
        // notes only.
        self.hud(f, out, f.now);
        self.effects(out, f.now);
    }
}

/// The share of an effect started at `t` still shown at `at`, from 1 down,
/// or `None` once it has faded. An effect stamped after `at` shows whole.
fn fade(at: TimeUs, t: TimeUs, len: i64) -> Option<f32> {
    let age = at.0.saturating_sub(t.0).max(0);
    (age < len).then(|| 1.0 - age as f32 / len as f32)
}

/// The part of `[y0, y1)` inside the lanes, if any.
fn clip(y0: f32, y1: f32) -> Option<(f32, f32)> {
    let (y0, y1) = (y0.max(LANE_TOP), y1.min(LANE_BOTTOM));
    (y1 > y0).then_some((y0, y1))
}

/// Pushes a note-like cap of height `h` centered at `y`, clipped to the
/// lanes. Returns `false` only when the batch is full.
fn cap(out: &mut SpriteBatch, x: f32, w: f32, y: f32, h: f32, color: Rgba) -> bool {
    match clip(y - h / 2.0, y + h / 2.0) {
        Some((y0, y1)) => rect(out, Blend::Alpha, x, y0, w, y1 - y0, color),
        None => true,
    }
}

/// Pushes a solid rectangle; `false` when the batch is full.
fn rect(out: &mut SpriteBatch, blend: Blend, x: f32, y: f32, w: f32, h: f32, c: Rgba) -> bool {
    out.push(
        blend,
        Sprite {
            rect: [x, y, w, h],
            uv: solid_uv(),
            color: c,
        },
    )
}

fn text_width(chars: usize, scale: f32) -> f32 {
    chars as f32 * (GLYPH_W + 1) as f32 * scale
}

fn text(out: &mut SpriteBatch, x: f32, y: f32, scale: f32, color: Rgba, s: &[u8]) {
    let advance = (GLYPH_W + 1) as f32 * scale;
    for (i, &c) in s.iter().enumerate() {
        if c == b' ' {
            continue;
        }
        out.push(
            Blend::Alpha,
            Sprite {
                rect: [
                    x + i as f32 * advance,
                    y,
                    GLYPH_W as f32 * scale,
                    GLYPH_H as f32 * scale,
                ],
                uv: glyph_uv(c),
                color,
            },
        );
    }
}

/// Left edge and top of a text screen's first line, in layout pixels.
const SCREEN_LEFT: f32 = 80.0;
const SCREEN_TOP: f32 = 80.0;
/// Scale of a text screen's glyphs, and the distance between its lines.
const SCREEN_SCALE: f32 = 4.0;
const SCREEN_LINE: f32 = 40.0;

/// A screen of text lines in the bitmap font, top left, one line every 40
/// layout pixels (glyphs 20×28): the end screen. Characters the font does
/// not have draw as spaces, and lines below the layout are left out.
pub fn text_screen(lines: &[&str], out: &mut SpriteBatch) {
    for (i, line) in lines.iter().enumerate() {
        let y = SCREEN_TOP + i as f32 * SCREEN_LINE;
        if y >= layout::HEIGHT {
            return;
        }
        text(out, SCREEN_LEFT, y, SCREEN_SCALE, TEXT, line.as_bytes());
    }
}

/// The countdown before chart time 0 (the lead-in, D13): "READY" where the
/// judgment shows, and under it the time left in seconds, tenths rounded
/// up, so it reads 0.1 until the chart starts.
fn countdown(out: &mut SpriteBatch, now: TimeUs) {
    let width = text_width(5, JUDGMENT_SCALE);
    let x = JUDGMENT_X - width / 2.0;
    text(out, x, JUDGMENT_Y, JUDGMENT_SCALE, COUNTDOWN, b"READY");
    let tenths = now.0.unsigned_abs().div_ceil(100_000);
    let mut buf = [0; 10];
    let whole = decimal(u32::try_from(tenths / 10).unwrap_or(u32::MAX), &mut buf);
    let n = whole.len();
    let width = text_width(n + 2, JUDGMENT_SCALE);
    let x = JUDGMENT_X - width / 2.0;
    text(out, x, COMBO_Y, JUDGMENT_SCALE, COUNTDOWN, whole);
    let fraction = [b'.', b'0' + (tenths % 10) as u8];
    let x = x + text_width(n, JUDGMENT_SCALE);
    text(out, x, COMBO_Y, JUDGMENT_SCALE, COUNTDOWN, &fraction);
}

/// `n` in decimal, written at the end of `buf`.
fn decimal(mut n: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}

const fn note_color(lane: Lane) -> Rgba {
    match lane.index() {
        0 => NOTE_SCRATCH,
        k if k % 2 == 1 => NOTE_WHITE,
        _ => NOTE_BLUE,
    }
}

fn shade(c: Rgba, k: f32) -> Rgba {
    [c[0] * k, c[1] * k, c[2] * k, c[3]]
}

const fn judgment_text(j: Judgment) -> (&'static [u8], Rgba) {
    match j {
        Judgment::Perfect => (b"PERFECT", [0.6, 0.95, 1.0, 1.0]),
        Judgment::Great => (b"GREAT", [1.0, 0.85, 0.3, 1.0]),
        Judgment::Good => (b"GOOD", [0.5, 1.0, 0.5, 1.0]),
        Judgment::Bad => (b"BAD", [0.9, 0.5, 1.0, 1.0]),
        Judgment::Poor => (b"POOR", [1.0, 0.4, 0.4, 1.0]),
    }
}

#[cfg(test)]
mod tests {
    use bms_model::ChartBuilder;
    use eval::{GaugeState, Tally};
    use game::PlayView;

    use super::*;

    const MS: i64 = 1_000;
    /// 150 BPM, in thousandths.
    const BPM: i64 = 150_000;
    const VISIBLE: TimeUs = TimeUs(600 * MS);

    fn lane(i: u8) -> Lane {
        Lane::new(i).unwrap()
    }

    fn view() -> PlayView {
        PlayView {
            finalized: None,
            tally: Tally {
                counts: rules::PerJudgment {
                    perfect: 0,
                    great: 0,
                    good: 0,
                    bad: 0,
                    poor: 0,
                },
                ex_score: 0,
                bp: 0,
                combo: 0,
                max_combo: 0,
                empty_poor: 0,
                ln_breaks: 0,
            },
            gauges: Vec::new(),
            complete: false,
            events: 0,
        }
    }

    fn frame<'a>(view: &'a PlayView, now: i64, effects: &'a [JudgeEvent]) -> FrameInput<'a> {
        FrameInput {
            now: TimeUs(now),
            display_offset: TimeUs(0),
            visible_time: VISIBLE,
            view,
            effects,
            gauge: 0,
        }
    }

    /// The alpha sprites drawn in `lane`'s column with `color`.
    fn in_lane(batch: &SpriteBatch, lane: Lane, color: Rgba) -> Vec<[f32; 4]> {
        let (x, _) = layout::lane_x(lane);
        batch
            .alpha()
            .iter()
            .filter(|s| s.rect[0] == x && s.color == color)
            .map(|s| s.rect)
            .collect()
    }

    fn skin(chart: &Chart) -> SimpleSkin {
        let mut s = SimpleSkin::new();
        s.load(chart);
        s
    }

    fn build(skin: &mut SimpleSkin, f: &FrameInput<'_>) -> SpriteBatch {
        let mut batch = SpriteBatch::with_capacity(4096);
        skin.build(f, &mut batch);
        batch
    }

    #[test]
    fn note_on_the_line_and_at_the_top() {
        // 150 BPM over 600 ms: 1.5 beats on 800 px. A note at the line is
        // centered on y = 880; one exactly 600 ms ahead is centered on the
        // lane top (80) and shows its lower half, 80..88.
        let chart = ChartBuilder::new(BPM)
            .tap(lane(3), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut s = skin(&chart);
        let v = view();
        let at_line = build(&mut s, &frame(&v, 1_000 * MS, &[]));
        assert_eq!(
            in_lane(&at_line, lane(3), NOTE_WHITE),
            [[384.0, 872.0, 64.0, 16.0]]
        );
        let at_top = build(&mut s, &frame(&v, 400 * MS, &[]));
        assert_eq!(
            in_lane(&at_top, lane(3), NOTE_WHITE),
            [[384.0, 80.0, 64.0, 8.0]]
        );
        // 6 ms earlier the note sits 8 px higher (800 px / 600 ms), wholly
        // above the lane.
        let above = build(&mut s, &frame(&v, 394 * MS, &[]));
        assert_eq!(in_lane(&above, lane(3), NOTE_WHITE), Vec::<[f32; 4]>::new());
    }

    #[test]
    fn held_long_note_head_stays_on_the_line() {
        // An LN from 1 s to 1.3 s, displayed at 1.15 s: the head is 150 ms
        // past the line (200 px below, y = 1080, out of the lanes), the
        // tail 150 ms ahead (y = 680).
        let chart = ChartBuilder::new(BPM)
            .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
            .build()
            .unwrap();
        let mut s = skin(&chart);
        let v = view();
        let body = shade(NOTE_BLUE, LN_BODY_SHADE);
        let free = build(&mut s, &frame(&v, 1_150 * MS, &[]));
        assert_eq!(in_lane(&free, lane(2), body), [[320.0, 680.0, 64.0, 280.0]]);
        assert_eq!(
            in_lane(&free, lane(2), NOTE_BLUE),
            [[320.0, 672.0, 64.0, 16.0]]
        );
        let judged = JudgeEvent {
            seq: 0,
            time: TimeUs(1_000 * MS),
            origin: Some(TimeUs(999 * MS)),
            kind: JudgeEventKind::Judged {
                lane: lane(2),
                note: 0,
                judgment: Judgment::Great,
                deviation: Some(TimeUs(0)),
                combo: 1,
            },
        };
        let held = build(&mut s, &frame(&v, 1_150 * MS, &[judged]));
        assert_eq!(in_lane(&held, lane(2), body), [[320.0, 680.0, 64.0, 200.0]]);
        assert_eq!(
            in_lane(&held, lane(2), NOTE_BLUE),
            [[320.0, 672.0, 64.0, 16.0], [320.0, 872.0, 64.0, 16.0]]
        );
        let end = JudgeEvent {
            seq: 1,
            time: TimeUs(1_300 * MS),
            origin: None,
            kind: JudgeEventKind::LnEnd {
                lane: lane(2),
                note: 0,
                outcome: eval::LnOutcome::Completed,
            },
        };
        let finished = build(&mut s, &frame(&v, 1_150 * MS, &[end]));
        assert!(in_lane(&finished, lane(2), body).is_empty());
        assert!(in_lane(&finished, lane(2), NOTE_BLUE).is_empty());
    }

    #[test]
    fn dense_notes_below_a_pixel_stay_bounded_by_the_batch() {
        // 3 000 taps 100 µs apart in each of two lanes: at 800 px per
        // 600 ms they are 0.1333 px apart, and 6 000 are visible.
        let mut b = ChartBuilder::new(BPM);
        for i in 0..3_000 {
            b = b.tap(lane(1), TimeUs(1_000 * MS + i * 100));
            b = b.tap(lane(7), TimeUs(1_000 * MS + i * 100));
        }
        let chart = b.build().unwrap();
        let mut s = skin(&chart);
        let v = view();
        let f = frame(&v, 1_000 * MS, &[]);
        let mut batch = SpriteBatch::with_capacity(4_000);
        s.build(&f, &mut batch);
        assert!(batch.is_full());
        assert!(batch.dropped() >= 1);
        let lane1 = in_lane(&batch, lane(1), NOTE_WHITE);
        assert_eq!(lane1.len(), 3_000);
        let gap = lane1[0][1] - lane1[1][1];
        assert!((gap - 800.0 / 6_000.0).abs() < 1e-3, "gap {gap}");
        let mut roomy = SpriteBatch::with_capacity(8_192);
        s.build(&f, &mut roomy);
        assert_eq!(roomy.dropped(), 0);
        assert_eq!(in_lane(&roomy, lane(7), NOTE_WHITE).len(), 3_000);
    }

    #[test]
    fn visible_time_below_a_microsecond_counts_as_one() {
        let chart = ChartBuilder::new(BPM).build().unwrap();
        let one = layout::Scale::new(&chart.scroll, TimeUs(0), TimeUs(1));
        // 1 µs of 150 BPM is 2.5 millionths of a beat on 800 px.
        assert!((one.px_per_pos - 320.0).abs() < 1e-9);
        for v in [0, -5, i64::MIN] {
            assert_eq!(layout::Scale::new(&chart.scroll, TimeUs(0), TimeUs(v)), one);
        }
    }

    #[test]
    fn effects_for_unknown_notes_and_gauges_are_ignored() {
        let chart = ChartBuilder::new(BPM)
            .tap(lane(5), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut s = skin(&chart);
        let mut v = view();
        let stray = JudgeEvent {
            seq: 0,
            time: TimeUs(500 * MS),
            origin: None,
            kind: JudgeEventKind::MineHit {
                lane: lane(5),
                note: 4_000,
            },
        };
        let batch = build(&mut s, &frame(&v, 700 * MS, &[stray]));
        assert_eq!(in_lane(&batch, lane(5), NOTE_WHITE).len(), 1);
        // No gauge at index 0: only the bar's background.
        assert!(batch.alpha().iter().all(|s| s.color != GAUGE_FILL));
        v.gauges.push(GaugeState {
            value: GaugeValue(GaugeValue::PERCENT * 40),
            reference: GaugeValue(GaugeValue::PERCENT * 41),
            failed_at: None,
        });
        let batch = build(&mut s, &frame(&v, 700 * MS, &[]));
        let fill: Vec<_> = batch
            .alpha()
            .iter()
            .filter(|s| s.color == GAUGE_FILL && s.uv == solid_uv())
            .map(|s| s.rect)
            .collect();
        // 40 % of the 544 px bar.
        assert_eq!(fill, [[160.0, 1000.0, 217.6, 24.0]]);
    }

    #[test]
    fn display_offset_moves_notes_but_not_feedback() {
        // A press judged GREAT at 1 s, drawn at 1 s with a 150 ms display
        // offset: the 100 ms beam and the 200 ms bomb are fresh, though the
        // display time is past both lifetimes' halves.
        let chart = ChartBuilder::new(BPM)
            .tap(lane(3), TimeUs(1_200 * MS))
            .tap(lane(6), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut s = skin(&chart);
        let v = view();
        let press = JudgeEvent {
            seq: 0,
            time: TimeUs(1_000 * MS),
            origin: Some(TimeUs(999 * MS)),
            kind: JudgeEventKind::Judged {
                lane: lane(6),
                note: 0,
                judgment: Judgment::Great,
                deviation: Some(TimeUs(0)),
                combo: 1,
            },
        };
        let effects = [press];
        let f = FrameInput {
            display_offset: TimeUs(150 * MS),
            ..frame(&v, 1_000 * MS, &effects)
        };
        let batch = build(&mut s, &f);
        let (x, _) = layout::lane_x(lane(6));
        let beam: Vec<f32> = batch
            .additive()
            .iter()
            .filter(|s| s.rect[0] == x && s.color[..3] == BEAM[..3])
            .map(|s| s.color[3])
            .collect();
        assert_eq!(beam, [BEAM[3]]);
        let bombs: Vec<f32> = batch
            .additive()
            .iter()
            .filter(|s| s.color[..3] == BOMB[..3])
            .map(|s| s.rect[2])
            .collect();
        assert_eq!(bombs, [BOMB_SIDE]);
        // The lane 3 note is 200 ms ahead of 1 s but 50 ms ahead of the
        // display time: 66.67 px above the line.
        let note = in_lane(&batch, lane(3), NOTE_WHITE);
        assert_eq!(note.len(), 1);
        assert!((note[0][1] + NOTE_H / 2.0 - (880.0 - 200.0 / 3.0)).abs() < 1e-3);
    }

    /// The glyphs drawn in `color`, in drawing order.
    fn glyphs(batch: &SpriteBatch, color: Rgba) -> Vec<[f32; 4]> {
        batch
            .alpha()
            .iter()
            .filter(|s| s.color == color)
            .map(|s| s.uv)
            .collect()
    }

    fn uvs(s: &[u8]) -> Vec<[f32; 4]> {
        s.iter().map(|&c| glyph_uv(c)).collect()
    }

    #[test]
    fn countdown_shows_the_time_left_before_chart_zero() {
        let chart = ChartBuilder::new(BPM).build().unwrap();
        let mut s = skin(&chart);
        let v = view();
        // 350 ms left: 0.4 s, tenths rounded up; 1 µs left still 0.1.
        for (now, left) in [(-350 * MS, b"0.4"), (-1, b"0.1"), (-1_000 * MS, b"1.0")] {
            let batch = build(&mut s, &frame(&v, now, &[]));
            let mut expected = uvs(b"READY");
            expected.extend(uvs(left));
            assert_eq!(glyphs(&batch, COUNTDOWN), expected, "at {now}");
        }
        let batch = build(&mut s, &frame(&v, 0, &[]));
        assert!(glyphs(&batch, COUNTDOWN).is_empty());
    }

    #[test]
    fn text_screen_draws_lines_down_to_the_layout_bottom() {
        let mut batch = SpriteBatch::with_capacity(256);
        text_screen(&["AB", "C"], &mut batch);
        let rects: Vec<_> = batch.alpha().iter().map(|s| (s.rect, s.uv)).collect();
        // 4 px per texel: glyphs 20 × 28, advancing 24.
        assert_eq!(
            rects,
            [
                ([80.0, 80.0, 20.0, 28.0], glyph_uv(b'A')),
                ([104.0, 80.0, 20.0, 28.0], glyph_uv(b'B')),
                ([80.0, 120.0, 20.0, 28.0], glyph_uv(b'C')),
            ]
        );
        // Line 25 would start at y = 1080: lines 0 to 24 fit.
        let lines = ["Z"; 30];
        let mut batch = SpriteBatch::with_capacity(256);
        text_screen(&lines, &mut batch);
        let ys: Vec<f32> = batch.alpha().iter().map(|s| s.rect[1]).collect();
        assert_eq!(ys.len(), 25);
        assert_eq!(ys[24], 1040.0);
    }

    #[test]
    fn decimal_writes_every_digit() {
        let mut buf = [0; 10];
        assert_eq!(decimal(0, &mut buf), b"0");
        assert_eq!(decimal(4_096, &mut buf), b"4096");
        assert_eq!(decimal(u32::MAX, &mut buf), b"4294967295");
    }

    #[test]
    fn effects_fade_and_future_stamps_show_whole() {
        assert_eq!(fade(TimeUs(100), TimeUs(100), 1_000), Some(1.0));
        assert_eq!(fade(TimeUs(600), TimeUs(100), 1_000), Some(0.5));
        assert_eq!(fade(TimeUs(1_100), TimeUs(100), 1_000), None);
        assert_eq!(fade(TimeUs(50), TimeUs(100), 1_000), Some(1.0));
    }
}
