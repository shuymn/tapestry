//! The simple skin: a hard-coded 16:9 layout authored at 1920×1080 and scaled
//! by the renderer (design.md "Rendering and Skins").
//!
//! Notes and bar lines sit at their stored display positions, and the
//! chart's [`ScrollMap`] gives the playhead at the expected display time
//! (design.md "Algorithms"). The hi-speed scale is fixed from the chart's
//! initial rate, so a note takes exactly `visible_time` from the lane top to
//! the judgment line at the start BPM. During play notes move through scroll
//! positions at that fixed scale, so BPM changes speed them up or slow them
//! down and STOPs freeze them, as the chart intends, and nothing ever jumps.
//! Only notes whose positions meet the visible range are visited, found per
//! lane by binary search on positions. A note judged by an effect is hidden
//! through a per-note bit set sized in `prepare`.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, LANE_COUNT, Lane, Note, NoteKind, Position, ScrollMap, TimeUs};
use eval::{JudgeEvent, JudgeEventKind};
use rules::{GaugeValue, Judgment};

use crate::atlas::{GLYPH_H, GLYPH_W, glyph_uv, solid_uv};
use crate::{Blend, FrameInput, Gpu, PlaySkin, SkinError, Sprite, SpriteBatch};

/// The layout, in layout pixels of the 1920×1080 base resolution, and the
/// placement of notes in it.
pub mod layout {
    use bms_model::{Lane, Position, ScrollMap, TimeUs};

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

    /// Left edge and width of `lane`.
    #[must_use]
    pub fn lane_x(lane: Lane) -> (f32, f32) {
        match lane.index() {
            0 => (LANE_LEFT, SCRATCH_W),
            k => (LANE_LEFT + SCRATCH_W + (k - 1) as f32 * KEY_W, KEY_W),
        }
    }

    /// Where the scroll stands for one frame: the playhead at the display
    /// time and the pixels per unit of position.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Scale {
        /// The display position at the judgment line.
        pub at_line: Position,
        /// Pixels per unit of position (2^-80 beat).
        pub px_per_pos: f64,
    }

    impl Scale {
        /// The scale at display time `at` with hi-speed `visible_time`. A
        /// visible time below 1 µs counts as 1 µs.
        ///
        /// The lane from [`LANE_TOP`] to [`JUDGE_Y`] holds the scroll of
        /// `visible_time` at the chart's initial rate. The pixels per
        /// position never change during a play, whatever the BPM at `at`.
        #[must_use]
        pub fn new(scroll: &ScrollMap, at: TimeUs, visible_time: TimeUs) -> Self {
            let visible = visible_time.0.max(1) as f64;
            let span = visible * scroll.initial_rate() as f64;
            Self {
                at_line: scroll.position_at(at),
                px_per_pos: f64::from(JUDGE_Y - LANE_TOP) / span,
            }
        }

        /// The vertical center of something at display position `p`.
        #[must_use]
        pub fn y(&self, p: Position) -> f32 {
            let ahead = p.minus(self.at_line) as f64;
            (f64::from(JUDGE_Y) - ahead * self.px_per_pos) as f32
        }

        /// The positions whose vertical center lies `below` pixels under
        /// the judgment line through `above` pixels over it, widened to
        /// whole units.
        #[must_use]
        pub fn positions(&self, below: f32, above: f32) -> (Position, Position) {
            let units = |px: f32| (f64::from(px) / self.px_per_pos).ceil() as i128;
            (
                Position(self.at_line.0.saturating_sub(units(below))),
                Position(self.at_line.0.saturating_add(units(above))),
            )
        }
    }

    /// The vertical center of something at display position `pos` in a
    /// frame displayed at `at` with hi-speed `visible_time`.
    #[must_use]
    pub fn note_y(scroll: &ScrollMap, pos: Position, at: TimeUs, visible_time: TimeUs) -> f32 {
        Scale::new(scroll, at, visible_time).y(pos)
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
/// A long note's body has two parts: active while its head was pressed and
/// the lane stays held, inactive otherwise (missed head, released, never
/// pressed), drawn darker.
const LN_BODY_ACTIVE: f32 = 0.6;
const LN_BODY_INACTIVE: f32 = 0.3;
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
    bar_lines: Vec<Position>,
    scroll: ScrollMap,
    /// Long notes whose head was held ([`JudgeEventKind::LnHeld`]): their
    /// press gave the feedback, so their judgment adds only its text.
    held: [Bits; LANE_COUNT],
    /// Notes hidden: judged taps and mines hit. A long note is never
    /// hidden; it is drawn until its end passes the judgment line.
    done: [Bits; LANE_COUNT],
    /// Per lane, the long note whose body is active: its head was pressed
    /// (held, or decided at a press before its end) and its visual hold has
    /// not ended ([`JudgeEventKind::LnHoldEnded`]). Every other body is
    /// inactive.
    active: [Option<usize>; LANE_COUNT],
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
    #[expect(
        clippy::expect_used,
        reason = "construction: 120 BPM without changes is a valid scroll"
    )]
    pub fn new() -> Self {
        Self {
            lanes: Default::default(),
            bar_lines: Vec::new(),
            scroll: ScrollMap::new(120_000_000, &[]).expect("120 BPM is a valid scroll"),
            held: Default::default(),
            done: Default::default(),
            active: [None; LANE_COUNT],
            pressed: [None; LANE_COUNT],
            hit: [None; LANE_COUNT],
            judgment: None,
        }
    }

    /// Takes `chart`'s notes, bar lines and scroll, and resets the play
    /// state. This is all of `prepare`; the skin needs nothing from the GPU.
    #[expect(
        clippy::indexing_slicing,
        reason = "loading before play: `from_fn` passes lane indices below LANE_COUNT"
    )]
    pub fn load(&mut self, chart: &Chart) {
        self.lanes = chart.lanes.clone();
        self.bar_lines = chart.bar_lines.clone();
        self.scroll = chart.scroll.clone();
        self.held = std::array::from_fn(|l| Bits::sized(self.lanes[l].len()));
        self.done = std::array::from_fn(|l| Bits::sized(self.lanes[l].len()));
        self.active = [None; LANE_COUNT];
        self.pressed = [None; LANE_COUNT];
        self.hit = [None; LANE_COUNT];
        self.judgment = None;
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "every index is a `Lane::index`, below LANE_COUNT, the length of the per-lane arrays"
    )]
    fn apply(&mut self, ev: &JudgeEvent) {
        match ev.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                release,
                ..
            } => {
                let l = lane.index();
                let end = match self.lanes[l].get(note).map(|n| n.kind) {
                    Some(NoteKind::Long { end, .. }) => Some(end),
                    Some(NoteKind::Tap | NoteKind::Mine { .. }) | None => None,
                };
                self.judgment = Some((judgment, ev.time));
                let Some(end) = end else {
                    self.done[l].set(note);
                    self.feedback(l, judgment, deviation.is_some(), ev.time);
                    return;
                };
                // A long note stays drawn when judged. Held earlier, its
                // press already gave its feedback; decided at a press
                // before its end (a BAD head), its body is active while the
                // lane stays held.
                if self.held[l].get(note) {
                    return;
                }
                if deviation.is_some() && release.is_none() && ev.time < end {
                    self.active[l] = Some(note);
                }
                self.feedback(l, judgment, deviation.is_some(), ev.time);
            }
            JudgeEventKind::LnHeld {
                lane,
                note,
                judgment,
                ..
            } => {
                let l = lane.index();
                self.held[l].set(note);
                self.active[l] = Some(note);
                self.judgment = Some((judgment, ev.time));
                self.feedback(l, judgment, true, ev.time);
            }
            JudgeEventKind::LnHoldEnded { lane, note } => {
                let l = lane.index();
                if self.active[l] == Some(note) {
                    self.active[l] = None;
                }
            }
            JudgeEventKind::MineHit { lane, note } => {
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

    /// A judgment's key beam and bomb on lane `l`: the bomb unless it is a
    /// POOR, the beam when a press made it.
    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass a `Lane::index`, below LANE_COUNT"
    )]
    fn feedback(&mut self, l: usize, judgment: Judgment, pressed: bool, at: TimeUs) {
        if judgment != Judgment::Poor {
            self.hit[l] = Some(at);
        }
        if pressed {
            self.pressed[l] = Some(at);
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

    fn bar_lines(&self, out: &mut SpriteBatch, scale: &Scale, from: Position, to: Position) {
        let start = self.bar_lines.partition_point(|&p| p < from);
        for &p in self.bar_lines.iter().skip(start).take_while(|&&p| p <= to) {
            let y = scale.y(p);
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

    fn notes(&self, out: &mut SpriteBatch, scale: &Scale, from: Position, to: Position) {
        for lane in Lane::all() {
            let l = lane.index();
            #[expect(
                clippy::indexing_slicing,
                reason = "`l` is a `Lane::index`, below LANE_COUNT"
            )]
            let notes = &self.lanes[l];
            let (x, w) = layout::lane_x(lane);
            let color = note_color(lane);
            // Positions never decrease along a lane (`Chart::validate`), so
            // the notes meeting [from, to] are one run.
            let start = notes.partition_point(|n| n.end_pos() < from);
            for (i, note) in notes.iter().enumerate().skip(start) {
                if note.pos > to {
                    break;
                }
                #[expect(
                    clippy::indexing_slicing,
                    reason = "`l` is a `Lane::index`, below LANE_COUNT"
                )]
                if self.done[l].get(i) {
                    continue;
                }
                let head = scale.y(note.pos);
                let pushed = match note.kind {
                    NoteKind::Tap => cap(out, x, w, head, NOTE_H, color),
                    NoteKind::Mine { .. } => cap(out, x, w, head, MINE_H, MINE),
                    NoteKind::Long { end_pos, .. } => {
                        // Judged or not, a long note is drawn until its end
                        // passes the judgment line, and once its head's time
                        // has passed the head stays on the line.
                        if end_pos < scale.at_line {
                            continue;
                        }
                        let tail = scale.y(end_pos);
                        let head = head.min(JUDGE_Y);
                        #[expect(
                            clippy::indexing_slicing,
                            reason = "`l` is a `Lane::index`, below LANE_COUNT"
                        )]
                        let body = if self.active[l] == Some(i) {
                            shade(color, LN_BODY_ACTIVE)
                        } else {
                            shade(color, LN_BODY_INACTIVE)
                        };
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
            #[expect(clippy::indexing_slicing, reason = "`Lane::index` is below LANE_COUNT")]
            if let Some(f) = self.pressed[lane.index()].and_then(|t| fade(at, t, BEAM_US)) {
                let c = [BEAM[0], BEAM[1], BEAM[2], BEAM[3] * f];
                rect(out, Blend::Additive, x, LANE_TOP, w, JUDGE_Y - LANE_TOP, c);
            }
            #[expect(clippy::indexing_slicing, reason = "`Lane::index` is below LANE_COUNT")]
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
        let (from, to) = scale.positions(LANE_BOTTOM - JUDGE_Y + half, JUDGE_Y - LANE_TOP + half);
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
        overlay(f.overlay, out);
    }
}

/// Left edge and top of the measurement overlay's first line, its glyph
/// scale and the distance between its lines, in layout pixels: right of
/// the lanes, glyphs 15×21, up to 50 characters a line.
const OVERLAY_LEFT: f32 = 1000.0;
const OVERLAY_TOP: f32 = 80.0;
const OVERLAY_SCALE: f32 = 3.0;
const OVERLAY_LINE: f32 = 28.0;

/// The measurement overlay (design.md "Latency Figures"): lines of text right
/// of the lanes.
fn overlay(lines: &[&str], out: &mut SpriteBatch) {
    for (i, line) in lines.iter().enumerate() {
        let y = OVERLAY_TOP + i as f32 * OVERLAY_LINE;
        text(out, OVERLAY_LEFT, y, OVERLAY_SCALE, TEXT, line.as_bytes());
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

/// The countdown before chart time 0 (the lead-in, design.md "Play Lifecycle"):
/// "READY" where the judgment shows, and under it the time left in seconds,
/// tenths rounded up, so it reads 0.1 until the chart starts.
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
#[expect(
    clippy::indexing_slicing,
    reason = "`i` counts down from the buffer's length, and a u32 has at most 10 digits"
)]
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
            overlay: &[],
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

    /// A long note's sprites in `lane`: active body, inactive body, caps.
    fn long_parts(batch: &SpriteBatch, l: Lane) -> [Vec<[f32; 4]>; 3] {
        let color = note_color(l);
        [
            in_lane(batch, l, shade(color, LN_BODY_ACTIVE)),
            in_lane(batch, l, shade(color, LN_BODY_INACTIVE)),
            in_lane(batch, l, color),
        ]
    }

    fn event(seq: u64, at: i64, origin: Option<i64>, kind: JudgeEventKind) -> JudgeEvent {
        JudgeEvent {
            seq,
            time: TimeUs(at),
            origin: origin.map(TimeUs),
            kind,
        }
    }

    fn ln_held(at: i64, note: usize) -> JudgeEvent {
        let kind = JudgeEventKind::LnHeld {
            lane: lane(2),
            note,
            judgment: Judgment::Great,
            deviation: TimeUs(0),
        };
        event(0, at, Some(at), kind)
    }

    fn ln_hold_ended(at: i64, note: usize) -> JudgeEvent {
        let kind = JudgeEventKind::LnHoldEnded {
            lane: lane(2),
            note,
        };
        event(1, at, Some(at), kind)
    }

    #[allow(clippy::too_many_arguments)]
    fn ln_judged(
        at: i64,
        note: usize,
        judgment: Judgment,
        deviation: Option<i64>,
        release: Option<i64>,
        origin: Option<i64>,
    ) -> JudgeEvent {
        let kind = JudgeEventKind::Judged {
            lane: lane(2),
            note,
            judgment,
            deviation: deviation.map(TimeUs),
            release: release.map(TimeUs),
            combo: 1,
        };
        event(2, at, origin, kind)
    }

    /// A long note on key 2 from 1 s to 1.3 s.
    fn long_note() -> Chart {
        ChartBuilder::new(BPM)
            .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
            .build()
            .unwrap()
    }

    #[test]
    fn a_long_note_stays_drawn_whatever_its_judgment_in_its_state() {
        // Displayed at 1.15 s, 800 px per 600 ms: the head 150 ms past the
        // line is pinned to it (y 880), the tail 150 ms ahead at y 680. The
        // body from 680 to 880 is active only while its head was pressed
        // and the lane stays held, whatever the judgment.
        let chart = long_note();
        let v = view();
        let caps = vec![[320.0, 672.0, 64.0, 16.0], [320.0, 872.0, 64.0, 16.0]];
        let body = vec![[320.0, 680.0, 64.0, 200.0]];
        let none = Vec::<[f32; 4]>::new();
        let cases: [(&str, Vec<JudgeEvent>, bool); 6] = [
            ("never pressed", vec![], false),
            (
                "head missed",
                vec![ln_judged(1_180 * MS, 0, Judgment::Poor, None, None, None)],
                false,
            ),
            ("held", vec![ln_held(1_000 * MS, 0)], true),
            (
                "released early",
                vec![
                    ln_held(1_000 * MS, 0),
                    ln_hold_ended(1_100 * MS, 0),
                    ln_judged(
                        1_100 * MS,
                        0,
                        Judgment::Bad,
                        Some(0),
                        Some(-200 * MS),
                        Some(1_100 * MS),
                    ),
                ],
                false,
            ),
            (
                "a BAD head decided at its press",
                vec![ln_judged(
                    850 * MS,
                    0,
                    Judgment::Bad,
                    Some(-150 * MS),
                    None,
                    Some(850 * MS),
                )],
                true,
            ),
            (
                "a BAD head released",
                vec![
                    ln_judged(
                        850 * MS,
                        0,
                        Judgment::Bad,
                        Some(-150 * MS),
                        None,
                        Some(850 * MS),
                    ),
                    ln_hold_ended(1_100 * MS, 0),
                ],
                false,
            ),
        ];
        for (name, effects, active) in cases {
            let mut s = skin(&chart);
            let batch = build(&mut s, &frame(&v, 1_150 * MS, &effects));
            let expected = if active {
                [body.clone(), none.clone(), caps.clone()]
            } else {
                [none.clone(), body.clone(), caps.clone()]
            };
            assert_eq!(long_parts(&batch, lane(2)), expected, "{name}");
        }
        // Before its head reaches the line, the head is drawn where it is:
        // at 0.85 s the head is 150 ms ahead (y 680) and the tail 450 ms
        // (y 280).
        let mut s = skin(&chart);
        let batch = build(&mut s, &frame(&v, 850 * MS, &[]));
        assert_eq!(
            long_parts(&batch, lane(2)),
            [
                none.clone(),
                vec![[320.0, 280.0, 64.0, 400.0]],
                vec![[320.0, 272.0, 64.0, 16.0], [320.0, 672.0, 64.0, 16.0]]
            ]
        );
        // A tap still disappears when judged.
        let tap = ChartBuilder::new(BPM)
            .tap(lane(3), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut s = skin(&tap);
        let judged = event(
            0,
            1_000 * MS,
            Some(1_000 * MS),
            JudgeEventKind::Judged {
                lane: lane(3),
                note: 0,
                judgment: Judgment::Great,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 1,
            },
        );
        let batch = build(&mut s, &frame(&v, 1_000 * MS, &[judged]));
        assert!(in_lane(&batch, lane(3), NOTE_WHITE).is_empty());
    }

    #[test]
    fn a_held_long_notes_judgment_adds_its_text_but_no_second_bomb() {
        let chart = long_note();
        let v = view();
        let mut s = skin(&chart);
        let bombs = |batch: &SpriteBatch| {
            batch
                .additive()
                .iter()
                .filter(|s| s.color[..3] == BOMB[..3])
                .count()
        };
        let held = build(&mut s, &frame(&v, 1_000 * MS, &[ln_held(1_000 * MS, 0)]));
        assert_eq!(bombs(&held), 1);
        assert_eq!(
            glyphs(&held, judgment_text(Judgment::Great).1),
            uvs(b"GREAT")
        );
        // Judged by its release 250 ms later: the press's bomb (200 ms) has
        // faded and no new one starts; the text shows the judgment, and the
        // note is still drawn, inactive.
        let judged = ln_judged(
            1_250 * MS,
            0,
            Judgment::Good,
            Some(0),
            Some(-50 * MS),
            Some(1_250 * MS),
        );
        let released = ln_hold_ended(1_250 * MS, 0);
        let batch = build(&mut s, &frame(&v, 1_250 * MS, &[released, judged]));
        assert_eq!(bombs(&batch), 0);
        assert_eq!(
            glyphs(&batch, judgment_text(Judgment::Good).1),
            uvs(b"GOOD")
        );
        let [active, inactive, caps] = long_parts(&batch, lane(2));
        assert!(active.is_empty());
        assert_eq!(inactive.len(), 1);
        assert_eq!(caps.len(), 2);
    }

    /// The long note's sprites drawn at `now` with `offset`, after `effects`.
    fn drawn(chart: &Chart, now: i64, offset: i64, effects: &[JudgeEvent]) -> usize {
        let v = view();
        let mut s = skin(chart);
        let f = FrameInput {
            display_offset: TimeUs(offset),
            ..frame(&v, now, effects)
        };
        long_parts(&build(&mut s, &f), lane(2))
            .iter()
            .map(Vec::len)
            .sum()
    }

    /// The events the evaluator emits up to logic time `to` for key 2
    /// pressed at `press` and released at `release` (µs), with the tapestry
    /// preset and one keyboard source.
    fn evaluated(chart: &Chart, press: i64, release: i64, to: i64) -> Vec<JudgeEvent> {
        let device = eval::DeviceProfile {
            sources: vec![eval::SourceProfile {
                input_offset: TimeUs(0),
                bindings: vec![eval::Binding {
                    control: eval::ControlId(2),
                    lane: lane(2),
                }],
                axes: Vec::new(),
            }],
        };
        let preset = rules::tapestry();
        let key = |at: i64, value: i32| eval::PhysicalInputEvent {
            received: TimeUs(at),
            source: eval::SourceId(0),
            kind: eval::InputKind::Control {
                control: eval::ControlId(2),
                value,
            },
        };
        let mut ev = eval::Evaluator::new(
            chart,
            &preset,
            &device,
            &eval::InputStart::default(),
            None,
            4,
        )
        .unwrap();
        ev.push_input(key(press, 1)).unwrap();
        ev.push_input(key(release, 0)).unwrap();
        let mut out = Vec::new();
        ev.advance_to(TimeUs(to), &mut out).unwrap();
        out
    }

    #[test]
    fn a_long_note_turns_inactive_at_its_logic_end_while_its_tail_still_shows() {
        // The long note from 1 s to 1.3 s, drawn with a display offset of
        // −100 ms, so its tail is still 100 ms ahead of the line when its
        // logic end passes. A GOOD-or-better head (on time) and a BAD head
        // (150 ms early), each released exactly at the end or held through
        // it (released at 1.5 s, judged by the end deadline): the body is
        // active at logic 1.29 s, and inactive from the logic end on, with
        // the tail drawn (at 1.31 s, 90 ms ahead: y 760).
        let chart = long_note();
        let v = view();
        let end = 1_300 * MS;
        let offset = -100 * MS;
        let tail = [320.0, 752.0, 64.0, 16.0];
        for press in [1_000 * MS, 850 * MS] {
            for release in [end, 1_500 * MS] {
                for (now, active) in [(end - 10 * MS, true), (end, false), (end + 10 * MS, false)] {
                    let effects = evaluated(&chart, press, release, now);
                    let mut s = skin(&chart);
                    let f = FrameInput {
                        display_offset: TimeUs(offset),
                        ..frame(&v, now, &effects)
                    };
                    let [on, off, caps] = long_parts(&build(&mut s, &f), lane(2));
                    let case = format!("press {press}, release {release}, at {now}");
                    assert_eq!(
                        (on.len(), off.len()),
                        (usize::from(active), usize::from(!active)),
                        "{case}"
                    );
                    if now == end + 10 * MS {
                        assert_eq!(caps[0], tail, "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_long_note_is_culled_once_its_tail_passes_below_the_line() {
        let chart = long_note();
        let end = 1_300 * MS;
        // Before the crossing: body and caps; at it, the tail on the line
        // and no body left: both caps; 1 µs after: nothing, judged or not.
        assert_eq!(drawn(&chart, end - 1, 0, &[]), 3);
        assert_eq!(drawn(&chart, end, 0, &[]), 2);
        assert_eq!(drawn(&chart, end + 1, 0, &[]), 0);
        let held = [ln_held(1_000 * MS, 0)];
        assert_eq!(drawn(&chart, end, 0, &held), 2);
        assert_eq!(drawn(&chart, end + 1, 0, &held), 0);
        // The display offset moves the crossing: 150 ms later on screen,
        // so 150 ms earlier on the frame's clock.
        let offset = 150 * MS;
        assert_eq!(drawn(&chart, end - offset, offset, &[]), 2);
        assert_eq!(drawn(&chart, end - offset + 1, offset, &[]), 0);
        // A STOP of 200 ms at the end holds the tail on the line for it.
        let stopped = ChartBuilder::new(BPM)
            .long(lane(2), TimeUs(1_000 * MS), TimeUs(end))
            .stop(TimeUs(end), TimeUs(200 * MS))
            .build()
            .unwrap();
        assert_eq!(drawn(&stopped, end - 1, 0, &[]), 3);
        assert_eq!(drawn(&stopped, end + 100 * MS, 0, &[]), 2);
        assert_eq!(drawn(&stopped, end + 200 * MS, 0, &[]), 2);
        assert_eq!(drawn(&stopped, end + 200 * MS + 1, 0, &[]), 0);
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

    /// Loads BMS `text` as a session does: the loader, then the transform
    /// and the validation of its output.
    fn loaded(text: &str) -> Chart {
        use game::ChartTransform;
        let dir = tempfile::tempdir().unwrap();
        let opts = bms_parser::LoadOptions::default();
        let chart = bms_parser::load_bms(text.as_bytes(), dir.path(), &opts)
            .unwrap()
            .chart;
        let chart = game::Passthrough.apply(chart, 0);
        chart.validate().unwrap();
        chart
    }

    #[test]
    fn loaded_notes_and_bar_lines_sit_at_their_exact_beats() {
        // Quarter-beat measures at 9,990,400 BPM: measure k starts at beat
        // k/4, k × 1.5014416... µs, so its notes sound on floored µs (1, 3,
        // 4, ...). Taps on key 1 at measures 1-6, a long note on key 2 from
        // measure 2 to 5, a last tap on key 3 at measure 9.
        let mut text = String::from("#BPM 9990400\n#LNTYPE 1\n#WAV01 a.wav\n");
        for m in 0..=9 {
            text.push_str(&format!("#{m:03}02:0.0625\n"));
        }
        for m in 1..=6 {
            text.push_str(&format!("#{m:03}11:01\n"));
        }
        text.push_str("#00252:01\n#00552:01\n#00913:01\n");
        let chart = loaded(&text);
        let mut s = skin(&chart);
        let v = view();
        // The playhead at 1 µs, 10 µs visible: 800 px over 10 µs of the
        // start BPM, so 80 px per µs of scroll at 9,990,400 BPM.
        let f = FrameInput {
            visible_time: TimeUs(10),
            ..frame(&v, 1, &[])
        };
        let batch = build(&mut s, &f);
        // By hand: beat k/4 is k × 15e6 / 9_990_400 µs of scroll, drawn
        // 80 px per µs above the line once past the playhead's 1 µs:
        // measure 1 at 880 - 80 × 0.5014416... = 839.8847 px, where its
        // floored time (1 µs) would put it on the line.
        let y = |k: i32| 880.0 - 80.0 * (f64::from(k) * 15e6 / 9_990_400.0 - 1.0);
        assert!((y(1) - 839.884_7).abs() < 1e-3);
        let centers = |rects: Vec<[f32; 4]>| -> Vec<f64> {
            rects.iter().map(|r| f64::from(r[1] + r[3] / 2.0)).collect()
        };
        let close = |got: Vec<f64>, want: Vec<f64>| {
            assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
            for (g, w) in got.iter().zip(&want) {
                assert!((g - w).abs() < 1e-3, "{got:?} vs {want:?}");
            }
        };
        close(
            centers(in_lane(&batch, lane(1), NOTE_WHITE)),
            (1..=6).map(y).collect(),
        );
        // The long note's end cap (measure 5), then its head (measure 2).
        close(
            centers(in_lane(&batch, lane(2), NOTE_BLUE)),
            vec![y(5), y(2)],
        );
        let bars: Vec<[f32; 4]> = batch
            .alpha()
            .iter()
            .filter(|s| s.color == BAR_LINE && s.rect[3] == BAR_LINE_H)
            .map(|s| s.rect)
            .collect();
        // Bar lines 1-7 lie wholly inside the lanes (y from 80 to 960).
        close(centers(bars), (1..=7).map(y).collect());
    }

    #[test]
    fn bar_lines_of_equal_measures_are_drawn_evenly() {
        // Quarter-beat measures at 9,990,400 BPM last 1.5014... µs each, so
        // their floored times step by 1 or 2 µs; their positions are
        // exactly a quarter beat apart. Over a 10 µs visible time a quarter
        // beat is 120.1... px.
        let mut text = String::from("#BPM 9990400\n#WAV01 a.wav\n");
        for m in 0..=64 {
            text.push_str(&format!("#{m:03}02:0.0625\n"));
        }
        text.push_str("#06411:01\n");
        let chart = loaded(&text);
        let mut s = skin(&chart);
        let v = view();
        let f = FrameInput {
            visible_time: TimeUs(10),
            ..frame(&v, 40, &[])
        };
        let batch = build(&mut s, &f);
        let ys: Vec<f32> = batch
            .alpha()
            .iter()
            .filter(|s| s.color == BAR_LINE && s.rect[3] == BAR_LINE_H)
            .map(|s| s.rect[1])
            .collect();
        assert!(ys.len() >= 6, "{ys:?}");
        let quarter = 800.0 / (10.0 * 9_990_400.0 / 60_000_000.0) / 4.0;
        for w in ys.windows(2) {
            let gap = w[0] - w[1];
            assert!((gap - quarter).abs() < 1e-2, "{ys:?}");
        }
    }

    #[test]
    fn drawn_notes_are_those_whose_span_meets_the_visible_range() {
        // 150 BPM: a beat is 400 ms, 1.5 beats over 800 px at 600 ms. Taps
        // every sixteenth beat on key 1 for 8 measures; a long note on key
        // 2 from measure 1 to measure 6; a tap on key 3 in measure 7.
        let mut text = String::from("#BPM 150\n#LNTYPE 1\n#WAV01 a.wav\n");
        let sixteenths = "01".repeat(64);
        for m in 0..8 {
            text.push_str(&format!("#{m:03}11:{sixteenths}\n"));
        }
        text.push_str("#00152:01\n#00652:01\n#00713:01\n");
        let chart = loaded(&text);
        let mut s = skin(&chart);
        let v = view();
        // Display at 3.31 s, beat 8.275: the judgment line is past measure
        // 2's start and the long note covers the lanes.
        let now = 3_310 * MS;
        let batch = build(&mut s, &frame(&v, now, &[]));
        // Independently: tap k sits at k/16 beat; its center is drawn
        // 800 px per 1.5 beats above the line for each beat ahead, and it
        // shows while its center is within half a note of the lanes.
        let playhead = 8.275;
        let expected = (0..8 * 64)
            .filter(|&k| {
                let ahead = f64::from(k) / 16.0 - playhead;
                let y = 880.0 - ahead * 800.0 / 1.5;
                y > f64::from(LANE_TOP - NOTE_H / 2.0) && y < f64::from(LANE_BOTTOM + NOTE_H / 2.0)
            })
            .count();
        assert!(expected > 20);
        assert_eq!(in_lane(&batch, lane(1), NOTE_WHITE).len(), expected);
        // The long note's head has passed the line, so it is pinned there:
        // its body runs from the lane top to the line, under its head cap.
        let body = shade(NOTE_BLUE, LN_BODY_INACTIVE);
        assert_eq!(
            in_lane(&batch, lane(2), body),
            [[320.0, LANE_TOP, 64.0, JUDGE_Y - LANE_TOP]]
        );
        assert_eq!(
            in_lane(&batch, lane(2), NOTE_BLUE),
            [[320.0, JUDGE_Y - NOTE_H / 2.0, 64.0, NOTE_H]]
        );
        assert!(in_lane(&batch, lane(3), NOTE_WHITE).is_empty());
    }

    #[test]
    fn visible_time_below_a_microsecond_counts_as_one() {
        let chart = ChartBuilder::new(BPM).build().unwrap();
        let one = layout::Scale::new(&chart.scroll, TimeUs(0), TimeUs(1));
        // 1 µs of 150 BPM (2.5 millionths of a beat) on 800 px.
        let beat = (1_u128 << 80) as f64;
        assert!((one.px_per_pos * beat * 2.5e-6 - 800.0).abs() < 1e-6);
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
                release: None,
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
    fn overlay_lines_draw_right_of_the_lanes() {
        let chart = ChartBuilder::new(120_000).build().unwrap();
        let mut s = skin(&chart);
        let v = view();
        let plain = build(&mut s, &frame(&v, 0, &[]));
        let lines = ["P5", "9"];
        let f = FrameInput {
            overlay: &lines,
            ..frame(&v, 0, &[])
        };
        let batch = build(&mut s, &f);
        let added: Vec<_> = batch.alpha()[plain.alpha().len()..]
            .iter()
            .map(|s| (s.rect, s.uv))
            .collect();
        // 3 px per texel: glyphs 15 × 21, advancing 18; lines 28 apart.
        assert_eq!(
            added,
            [
                ([1000.0, 80.0, 15.0, 21.0], glyph_uv(b'P')),
                ([1018.0, 80.0, 15.0, 21.0], glyph_uv(b'5')),
                ([1000.0, 108.0, 15.0, 21.0], glyph_uv(b'9')),
            ]
        );
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
