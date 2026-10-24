//! Procedural atlas and per-frame sprite building (design D2 rendering).
//!
//! Notes scroll from the chart; every judged press (`hit`, from the logic
//! thread's events, or from `Play::advance_to` offscreen) spawns the per-hit
//! effects (16-frame additive bomb over 250 ms, 24 particles, lane beam,
//! judgment bar, combo digits). Effects are pure functions of hit time, so
//! the only state is a fixed ring of recent hits. Building a frame writes
//! into preallocated instance buffers and never allocates.

use bytemuck::{Pod, Zeroable};
use spike_core::chart::{CHART_LEN_US, Chart, LANES, Note};
use spike_core::rng::SplitMix64;
use spike_core::run::Profile;
use spike_core::scroll::NoteTrack;

/// Layout space; the renderer letterboxes it to the output.
pub const LAYOUT_W: f32 = 1920.0;
pub const LAYOUT_H: f32 = 1080.0;
pub const ATLAS_SIZE: u32 = 2048;
/// Instance buffer capacity (design D2: preallocated 32k instances).
pub const MAX_INSTANCES: usize = 32_768;

const LANE_X0: f32 = 160.0;
const LANE_W: f32 = 72.0;
const JUDGE_Y: f32 = 920.0;
const LANE_H: f32 = 820.0;
const BOMB_US: i64 = 250_000;
const BEAM_US: i64 = 100_000;
const PARTICLES: usize = 24;
const HIT_RING: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Instance {
    /// x, y, w, h in layout pixels.
    pub rect: [f32; 4],
    /// u0, v0, u1, v1 in the atlas.
    pub uv: [f32; 4],
    pub color: [f32; 4],
}

/// Atlas regions in pixels: (x, y, w, h).
mod region {
    pub const BOMB: (u32, u32, u32, u32) = (0, 0, 1024, 1024);
    pub const NOTE: (u32, u32, u32, u32) = (1024, 0, 64, 16);
    pub const PARTICLE: (u32, u32, u32, u32) = (1024, 32, 16, 16);
    pub const BEAM: (u32, u32, u32, u32) = (1100, 0, 64, 256);
    pub const DIGITS: (u32, u32, u32, u32) = (1024, 300, 480, 80);
    pub const BAR: (u32, u32, u32, u32) = (1024, 400, 256, 48);
    pub const WHITE: (u32, u32, u32, u32) = (1600, 0, 4, 4);
}

fn uv((x, y, w, h): (u32, u32, u32, u32)) -> [f32; 4] {
    let s = ATLAS_SIZE as f32;
    [
        x as f32 / s,
        y as f32 / s,
        (x + w) as f32 / s,
        (y + h) as f32 / s,
    ]
}

/// RGBA8 atlas generated from a fixed seed (nothing copyrighted).
pub fn build_atlas() -> Vec<u8> {
    let n = ATLAS_SIZE as usize;
    let mut px = vec![0u8; n * n * 4];
    let mut put = |x: u32, y: u32, rgba: [f32; 4]| {
        let i = (y as usize * n + x as usize) * 4;
        for (k, c) in rgba.iter().enumerate() {
            px[i + k] = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    };
    // Bomb: 16 frames of an expanding, fading ring (4x4 grid of 256²).
    for f in 0..16u32 {
        let (ox, oy) = ((f % 4) * 256, (f / 4) * 256);
        let t = f as f32 / 15.0;
        let radius = 30.0 + 90.0 * t;
        for y in 0..256 {
            for x in 0..256 {
                let d = ((x as f32 - 128.0).powi(2) + (y as f32 - 128.0).powi(2)).sqrt();
                let ring = (1.0 - ((d - radius).abs() / 18.0)).max(0.0) * (1.0 - t);
                let core = (1.0 - d / 60.0).max(0.0) * (1.0 - t) * 0.6;
                let a = (ring + core).min(1.0);
                put(ox + x, oy + y, [a, a, a, a]);
            }
        }
    }
    let (nx, ny, nw, nh) = region::NOTE;
    for y in 0..nh {
        for x in 0..nw {
            let edge = (y == 0 || y == nh - 1 || x == 0 || x == nw - 1) as u8 as f32;
            let v = 0.75 + 0.25 * (y as f32 / nh as f32) - 0.3 * edge;
            put(nx + x, ny + y, [v, v, v, 1.0]);
        }
    }
    let (px0, py0, pw, ph) = region::PARTICLE;
    for y in 0..ph {
        for x in 0..pw {
            let d = ((x as f32 - 7.5).powi(2) + (y as f32 - 7.5).powi(2)).sqrt();
            let a = (1.0 - d / 8.0).max(0.0);
            put(px0 + x, py0 + y, [a, a, a, a]);
        }
    }
    let (bx, by, bw, bh) = region::BEAM;
    for y in 0..bh {
        for x in 0..bw {
            let fx = 1.0 - ((x as f32 - 31.5).abs() / 32.0);
            let a = fx * (y as f32 / bh as f32);
            put(bx + x, by + y, [a, a, a, a]);
        }
    }
    // Seven-segment digits 0–9, 48x80 each.
    const SEGS: [u8; 10] = [
        0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
        0b1111111, 0b1101111,
    ];
    let (dx, dy, _, _) = region::DIGITS;
    for (d, segs) in SEGS.iter().enumerate() {
        let ox = dx + d as u32 * 48;
        let on = |s: u8| segs & (1 << s) != 0;
        for y in 0..80u32 {
            for x in 0..48u32 {
                let hseg = |yc: u32| (8..40).contains(&x) && y + 3 >= yc && y < yc + 4;
                let vseg = |xc: u32, y0: u32| x + 3 >= xc && x < xc + 4 && y >= y0 && y < y0 + 34;
                let lit = (on(0) && hseg(6))
                    || (on(1) && vseg(40, 6))
                    || (on(2) && vseg(40, 40))
                    || (on(3) && hseg(74))
                    || (on(4) && vseg(6, 40))
                    || (on(5) && vseg(6, 6))
                    || (on(6) && hseg(40));
                if lit {
                    put(ox + x, dy + y, [1.0, 1.0, 1.0, 1.0]);
                }
            }
        }
    }
    let (jx, jy, jw, jh) = region::BAR;
    for y in 0..jh {
        for x in 0..jw {
            let a = 1.0 - ((y as f32 - 23.5).abs() / 24.0);
            put(jx + x, jy + y, [1.0, 1.0, 1.0, a]);
        }
    }
    let (wx, wy, ww, wh) = region::WHITE;
    for y in 0..wh {
        for x in 0..ww {
            put(wx + x, wy + y, [1.0, 1.0, 1.0, 1.0]);
        }
    }
    px
}

#[derive(Clone, Copy, Default)]
struct Hit {
    t_us: i64,
    lane: u8,
    seed: u32,
}

/// Load counters of one frame (P1 `SpikeFrame` fields).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameLoad {
    pub visible_notes: u32,
    pub effect_instances: u32,
    pub lanes_with_effects: u8,
    /// Instances dropped because a buffer was full (never expected).
    pub dropped: u32,
}

/// Instance buffers for one frame: alpha-blended, then additive.
pub struct Frame {
    pub alpha: Vec<Instance>,
    pub additive: Vec<Instance>,
}

impl Frame {
    pub fn new() -> Self {
        Self {
            alpha: Vec::with_capacity(MAX_INSTANCES / 2),
            additive: Vec::with_capacity(MAX_INSTANCES / 2),
        }
    }
}

fn push(v: &mut Vec<Instance>, i: Instance, load: &mut FrameLoad) {
    if v.len() < v.capacity() {
        v.push(i);
    } else {
        load.dropped += 1;
    }
}

const LANE_COLORS: [[f32; 3]; 8] = [
    [1.0, 0.3, 0.3],
    [0.9, 0.9, 0.9],
    [0.3, 0.6, 1.0],
    [0.9, 0.9, 0.9],
    [0.3, 0.6, 1.0],
    [0.9, 0.9, 0.9],
    [0.3, 0.6, 1.0],
    [0.9, 0.9, 0.9],
];

fn lane_x(lane: u8) -> f32 {
    LANE_X0 + f32::from(lane) * LANE_W
}

/// The effects a settings profile draws (design D2 profiles): standard
/// draws everything; streaming and no-bga turn particles off and keep one
/// of the four skin overlays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects {
    pub particles: bool,
    pub skin_layers: u32,
}

impl Effects {
    pub fn for_profile(p: Profile) -> Self {
        match p {
            Profile::Standard => Self {
                particles: true,
                skin_layers: 4,
            },
            Profile::Streaming | Profile::NoBga => Self {
                particles: false,
                skin_layers: 1,
            },
        }
    }
}

/// SHA-256 of the generated scene: the atlas and the layout and effect
/// constants (`scene_hash` in the conditions).
pub fn scene_hash() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(build_atlas());
    h.update(format!(
        "{LAYOUT_W} {LAYOUT_H} {LANE_X0} {LANE_W} {JUDGE_Y} {LANE_H} {BOMB_US} {BEAM_US} {PARTICLES} {HIT_RING} {MAX_INSTANCES}"
    ));
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Scene {
    chart: Chart,
    effects: Effects,
    track: NoteTrack,
    hits: [Hit; HIT_RING],
    hit_count: usize,
    combo: u32,
    /// Judged notes (the logic thread's snapshot), shown as the score.
    score: u32,
    rng: SplitMix64,
}

impl Scene {
    pub fn new(chart: Chart, effects: Effects) -> Self {
        let track = NoteTrack::new(&chart);
        Self {
            chart,
            effects,
            track,
            hits: [Hit::default(); HIT_RING],
            hit_count: 0,
            combo: 0,
            score: 0,
            rng: SplitMix64::new(0x5EED_0000_00FF),
        }
    }

    /// A judged press on `lane` at `t_us` (µs since play start).
    pub fn hit(&mut self, t_us: i64, lane: u8) {
        self.hits[self.hit_count % HIT_RING] = Hit {
            t_us,
            lane,
            seed: self.rng.next_u64() as u32,
        };
        self.hit_count += 1;
        self.combo += 1;
    }

    pub fn set_score(&mut self, judged: u32) {
        self.score = judged;
    }

    /// Builds frame `out` for play time `play_us` (negative during the
    /// countdown: only the persistent layers are drawn).
    pub fn build(&mut self, play_us: i64, out: &mut Frame) -> FrameLoad {
        out.alpha.clear();
        out.additive.clear();
        let mut load = FrameLoad::default();
        // Translucent full-screen skin layers (four in standard).
        for k in 0..self.effects.skin_layers {
            let i = Instance {
                rect: [0.0, 0.0, LAYOUT_W, LAYOUT_H],
                uv: uv(region::WHITE),
                color: [0.1 * k as f32, 0.05, 0.15, 0.06],
            };
            push(&mut out.alpha, i, &mut load);
        }
        // Lane backgrounds.
        for lane in 0..LANES {
            let i = Instance {
                rect: [lane_x(lane), JUDGE_Y - LANE_H, LANE_W - 4.0, LANE_H],
                uv: uv(region::WHITE),
                color: [0.0, 0.0, 0.0, 0.55],
            };
            push(&mut out.alpha, i, &mut load);
        }
        if play_us >= 0 {
            self.notes_and_effects(play_us, out, &mut load);
        }
        // Gauge and score digits.
        let gauge = Instance {
            rect: [LANE_X0, JUDGE_Y + 40.0, LANE_W * 8.0 * 0.8, 24.0],
            uv: uv(region::WHITE),
            color: [0.2, 0.9, 0.4, 0.9],
        };
        push(&mut out.alpha, gauge, &mut load);
        digits(
            u64::from(self.score) * 2,
            7,
            [1400.0, 80.0],
            40.0,
            &mut out.alpha,
            &mut load,
        );
        load
    }

    fn notes_and_effects(&mut self, play_us: i64, out: &mut Frame, load: &mut FrameLoad) {
        let chart_us = play_us.rem_euclid(CHART_LEN_US);
        let range = self.track.visible(chart_us);
        load.visible_notes = range.len() as u32;
        for i in range {
            let Note { lane, end_us, .. } = self.chart.notes[i];
            let y = JUDGE_Y - self.track.y(i, chart_us) as f32 * LANE_H;
            let c = LANE_COLORS[lane as usize];
            if let Some(end) = end_us {
                let end_y = JUDGE_Y
                    - ((self.track.map().beat_at(end) - self.track.map().beat_at(chart_us))
                        / self.track.map().visible_beats(chart_us)) as f32
                        * LANE_H;
                let top = end_y.max(JUDGE_Y - LANE_H);
                let body = Instance {
                    rect: [lane_x(lane) + 8.0, top, LANE_W - 20.0, (y - top).max(0.0)],
                    uv: uv(region::WHITE),
                    color: [c[0] * 0.5, c[1] * 0.5, c[2] * 0.5, 0.8],
                };
                push(&mut out.alpha, body, load);
            }
            let note = Instance {
                rect: [lane_x(lane), y - 16.0, LANE_W - 4.0, 16.0],
                uv: uv(region::NOTE),
                color: [c[0], c[1], c[2], 1.0],
            };
            push(&mut out.alpha, note, load);
        }
        // Effects of recent hits, newest first, until older than a bomb.
        let mut lanes_lit = 0u8;
        let mut latest_hit = None;
        let start = self.hit_count.saturating_sub(HIT_RING);
        for k in (start..self.hit_count).rev() {
            let hit = self.hits[k % HIT_RING];
            let dt = play_us - hit.t_us;
            if dt >= BOMB_US {
                break;
            }
            if dt < 0 {
                continue;
            }
            latest_hit.get_or_insert(dt);
            lanes_lit |= 1 << hit.lane;
            let c = LANE_COLORS[hit.lane as usize];
            let cx = lane_x(hit.lane) + LANE_W / 2.0;
            let frame = (dt * 16 / BOMB_US) as u32;
            let (bx, by, _, _) = region::BOMB;
            let bomb_uv = uv((bx + (frame % 4) * 256, by + (frame / 4) * 256, 256, 256));
            let before = out.additive.len();
            push(
                &mut out.additive,
                Instance {
                    rect: [cx - 128.0, JUDGE_Y - 128.0, 256.0, 256.0],
                    uv: bomb_uv,
                    color: [c[0], c[1], c[2], 1.0],
                },
                load,
            );
            let t = dt as f32 / BOMB_US as f32;
            let particles = if self.effects.particles { PARTICLES } else { 0 };
            for p in 0..particles {
                let a = (hit
                    .seed
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(p as u32 * 97)
                    % 6283) as f32
                    / 1000.0;
                let r = 40.0 + 260.0 * t * (0.5 + (p % 5) as f32 * 0.12);
                push(
                    &mut out.additive,
                    Instance {
                        rect: [
                            cx + r * a.cos() - 8.0,
                            JUDGE_Y + r * a.sin() * 0.6 - 8.0,
                            16.0,
                            16.0,
                        ],
                        uv: uv(region::PARTICLE),
                        color: [c[0], c[1], c[2], 1.0 - t],
                    },
                    load,
                );
            }
            if dt < BEAM_US {
                push(
                    &mut out.additive,
                    Instance {
                        rect: [lane_x(hit.lane), JUDGE_Y - LANE_H, LANE_W - 4.0, LANE_H],
                        uv: uv(region::BEAM),
                        color: [c[0], c[1], c[2], 0.5 * (1.0 - dt as f32 / BEAM_US as f32)],
                    },
                    load,
                );
            }
            load.effect_instances += (out.additive.len() - before) as u32;
        }
        load.lanes_with_effects = lanes_lit.count_ones() as u8;
        if let Some(dt) = latest_hit {
            let a = 1.0 - dt as f32 / BOMB_US as f32;
            push(
                &mut out.alpha,
                Instance {
                    rect: [LANE_X0 + 160.0, 600.0, 256.0, 48.0],
                    uv: uv(region::BAR),
                    color: [1.0, 0.85, 0.2, a],
                },
                load,
            );
            digits(
                u64::from(self.combo),
                5,
                [LANE_X0 + 190.0, 660.0],
                30.0,
                &mut out.alpha,
                load,
            );
            load.effect_instances += 6;
        }
    }
}

fn digits(
    mut value: u64,
    count: usize,
    [x, y]: [f32; 2],
    w: f32,
    out: &mut Vec<Instance>,
    load: &mut FrameLoad,
) {
    let (dx, dy, _, _) = region::DIGITS;
    for k in (0..count).rev() {
        let d = (value % 10) as u32;
        value /= 10;
        push(
            out,
            Instance {
                rect: [x + k as f32 * w, y, w * 0.9, w * 80.0 / 48.0],
                uv: uv((dx + d * 48, dy, 48, 80)),
                color: [1.0, 1.0, 1.0, 1.0],
            },
            load,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spike_core::chart::DEFAULT_SEED;
    use spike_core::play::Play;

    #[test]
    fn heavy_window_frames_light_all_lanes_without_overflow() {
        let chart = Chart::generate(DEFAULT_SEED);
        let mut play = Play::new(&chart, CHART_LEN_US);
        let mut scene = Scene::new(chart, Effects::for_profile(Profile::Standard));
        let mut frame = Frame::new();
        let mut max = 0;
        let mut all_lanes = 0;
        for f in 0..(150 * 60) {
            let t = f * 1_000_000 / 60;
            play.advance_to(t, |j| scene.hit(j.note_us + j.delta_us, j.lane));
            let load = scene.build(t, &mut frame);
            assert_eq!(load.dropped, 0, "at {t}");
            max = max.max(frame.alpha.len() + frame.additive.len());
            if (40_000_000..70_000_000).contains(&t) && load.lanes_with_effects == 8 {
                all_lanes += 1;
            }
        }
        assert!(all_lanes > 30 * 30, "{all_lanes}");
        assert!(max > 1_000 && max < MAX_INSTANCES, "{max}");
    }

    /// Instances of one frame at `t` under `profile`, after playing up to it.
    fn frame_at(profile: Profile, t: i64) -> (FrameLoad, usize, usize) {
        let chart = Chart::generate(DEFAULT_SEED);
        let mut play = Play::new(&chart, CHART_LEN_US);
        let mut scene = Scene::new(chart, Effects::for_profile(profile));
        let mut frame = Frame::new();
        play.advance_to(t, |j| scene.hit(j.note_us + j.delta_us, j.lane));
        let load = scene.build(t, &mut frame);
        (load, frame.alpha.len(), frame.additive.len())
    }

    #[test]
    fn streaming_profiles_draw_lighter_effects() {
        let t = 55_000_000; // inside H1
        let (std_load, std_alpha, std_add) = frame_at(Profile::Standard, t);
        for p in [Profile::Streaming, Profile::NoBga] {
            let (load, alpha, add) = frame_at(p, t);
            // Same chart state: same notes and lit lanes.
            assert_eq!(load.visible_notes, std_load.visible_notes);
            assert_eq!(load.lanes_with_effects, std_load.lanes_with_effects);
            // Three fewer skin layers, and no particles.
            assert_eq!(alpha, std_alpha - 3);
            assert!(add * 10 < std_add, "{add} vs {std_add}");
            assert!(load.effect_instances < std_load.effect_instances);
        }
    }
}
