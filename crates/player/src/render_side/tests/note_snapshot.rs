//! #83 through the render thread: a real logic step whose effect queue
//! holds one effect, the render thread's acquisition, and both skins'
//! builds, `SimpleSkin` and the model's declarative skin through
//! [`render::skin_frame`], from the very `FrameInput` the thread makes.
//! Every effect but one is dropped, and each skin still shows each note's
//! state from the acquired view's note facts; a rearm reaches the screen
//! before the first frame of its candidate, whose fresh view shows every
//! note again.

use std::sync::Mutex;

use eval::{Binding, ControlId, InputKind, PhysicalInputEvent, SourceId, SourceProfile};
use render::{PlaySkin, SimpleSkin, SpriteBatch, layout};
use skin_model::{AtlasLimits, LaneTimer, Play, QuadBatch, Skin, UvTable, Variant, lane_timer};

use super::*;

const MS: i64 = 1_000;
/// The chart time both skins draw every frame at, whatever the clock: the
/// notes are all on screen, ahead of the line.
const AT: TimeUs = TimeUs(950 * MS);

fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

/// What one frame showed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    /// The acquired view's generation and events.
    generation: u64,
    events: u64,
    /// The screen's rearms before this frame.
    rearms: u64,
    /// The sequence numbers of the frame's effects.
    effects: Vec<u64>,
    /// The simple skin: key 1's tap and key 3's mine drawn, and whether
    /// the long notes of keys 2, 4 and 5 draw their body active.
    simple: (usize, usize, [bool; 3]),
    /// The model: key 1's tap and key 3's mine drawn, and the `hold`
    /// timers of keys 2, 4 and 5.
    model: (usize, usize, [Option<TimeUs>; 3]),
}

/// A screen that builds both skins from each frame the render thread
/// acquires, at [`AT`], and notes what they show.
struct Skins {
    simple: SimpleSkin,
    /// The same skin of a fresh attempt, never told anything: the body
    /// colours of notes nobody holds.
    plain: SimpleSkin,
    model: Skin,
    uvs: UvTable,
    play: Option<Play>,
    rearms: u64,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Alpha sprites in `l`'s column `h` high and as wide as the lane.
fn simple_parts(batch: &SpriteBatch, l: Lane, h: f32) -> Vec<render::Sprite> {
    let (x, w) = layout::lane_x(l);
    batch
        .blended(render::Blend::Alpha)
        .filter(|s| s.rect[0] == x && s.rect[2] == w && s.rect[3] == h)
        .copied()
        .collect()
}

/// The colour of `l`'s long-note body: the one sprite of the lane's width
/// taller than a cap and shorter than the lane's background.
fn body(batch: &SpriteBatch, l: Lane) -> Option<[f32; 4]> {
    let (x, w) = layout::lane_x(l);
    batch
        .blended(render::Blend::Alpha)
        .find(|s| {
            s.rect[0] == x
                && s.rect[2] == w
                && s.rect[3] > layout::NOTE_H
                && s.rect[3] < layout::LANE_BOTTOM - layout::LANE_TOP
        })
        .map(|s| s.color)
}

/// The model's quads in key `n`'s column (`[160 + 64n, 64]`) `h` high.
fn model_parts(batch: &QuadBatch, n: u8, h: f32) -> usize {
    let x = 160.0 + 64.0 * f32::from(n);
    batch
        .quads()
        .iter()
        .filter(|q| {
            let r = q.rect;
            r[0] == x && r[3] == h
        })
        .count()
}

impl Screen for Skins {
    fn load(&mut self, chart: &Chart, section: Option<&eval::Section>) -> Result<(), RenderError> {
        self.simple.prepare_attempt(chart, section).unwrap();
        self.plain.prepare_attempt(chart, section).unwrap();
        let ctx = crate::render_common::context(&crate::render_common::GAUGES, false);
        self.play = Some(self.model.prepare(chart, &ctx).unwrap());
        Ok(())
    }
    fn wait(&self) -> bool {
        true
    }
    fn paced(&self) -> bool {
        false
    }
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        let f = FrameInput {
            now: AT,
            display_offset: TimeUs(0),
            ..*f
        };
        let mut batch = SpriteBatch::with_capacity(4_096);
        self.simple.build(&f, &mut batch)?;
        let fresh = game::PlayView {
            notes: eval::NoteFacts::default(),
            ..f.view.clone()
        };
        let mut plain = SpriteBatch::with_capacity(4_096);
        self.plain.build(
            &FrameInput {
                view: &fresh,
                effects: &[],
                ..f
            },
            &mut plain,
        )?;
        let active = [2, 4, 5].map(|n| {
            let (Some(drawn), Some(idle)) = (body(&batch, lane(n)), body(&plain, lane(n))) else {
                panic!("key {n}'s body is not drawn");
            };
            drawn != idle
        });
        let simple = (
            simple_parts(&batch, lane(1), layout::NOTE_H).len(),
            simple_parts(&batch, lane(3), 8.0).len(),
            active,
        );
        let play = self.play.as_mut().unwrap();
        let mut quads =
            QuadBatch::with_capacity(self.model.quad_capacity(), self.model.run_capacity());
        self.model
            .build(play, &render::skin_frame(&f), &self.uvs, &mut quads)?;
        let model = (
            model_parts(&quads, 1, 16.0),
            model_parts(&quads, 3, 8.0),
            [2, 4, 5].map(|n| play.timer(lane_timer(LaneTimer::Hold, lane(n)))),
        );
        self.seen.lock().unwrap().push(Seen {
            generation: f.view.generation,
            events: f.view.events,
            rearms: self.rearms,
            effects: f.effects.iter().map(|e| e.seq).collect(),
            simple,
            model,
        });
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(timing(true))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn rearm(&mut self) {
        self.rearms += 1;
        self.simple.rearm();
        if let Some(play) = &mut self.play {
            play.rearm();
        }
    }
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

/// Key 1's tap at 1 s, key 3's mine at 1.05 s, and long notes from 1 s to
/// 5 s on keys 2, 4 and 5.
fn chart() -> Chart {
    ChartBuilder::new(120_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .mine(lane(3), TimeUs(1_050 * MS), 10)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(5_000 * MS))
        .long(lane(4), TimeUs(1_000 * MS), TimeUs(5_000 * MS))
        .long(lane(5), TimeUs(1_000 * MS), TimeUs(5_000 * MS))
        .build()
        .unwrap()
}

/// One source binding control `n` to lane `n`.
fn device() -> DeviceProfile {
    DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: ControlId(u32::try_from(l.index()).unwrap()),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}

fn key(ms: i64, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(ms * MS),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(u32::from(n)),
            value,
        },
    }
}

/// With an effect queue of one, the logic step drops every effect but the
/// first of a play that judges key 1's tap 40 ms early, hits key 3's mine
/// while held, holds key 2's long note, takes key 4's head BAD (150 ms
/// early) and holds it, and releases key 5's long note 800 ms early: the
/// frames the render thread acquires show all of it in both skins, from
/// the view's note facts. A rearm then reaches the screen before any frame
/// of its candidate, whose fresh view shows every note again and no hold.
#[test]
fn both_skins_show_the_snapshot_when_effects_are_dropped() {
    let chart = chart();
    let (rule, device) = (rules::tapestry(), device());
    let (mut writer, reader) = input::source_queue(64);
    let (mut logic, side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &device,
        &InputStart::default(),
        vec![reader],
        LogicConfig {
            inputs: 64,
            effects: 1,
        },
        Vec::new(),
    )
    .unwrap();
    for ev in [
        key(850, 4, 1),
        key(900, 3, 1),
        key(960, 1, 1),
        key(1_000, 1, 0),
        key(1_000, 2, 1),
        key(1_000, 5, 1),
        key(1_100, 3, 0),
        key(1_200, 5, 0),
    ] {
        writer.push(ev).unwrap();
    }
    writer.publish(TimeUs(2_000 * MS));
    // Before the render thread takes the side: nothing pops, so the queue
    // keeps the first effect and drops the rest.
    let advance = logic.advance_to(TimeUs(2_000 * MS));
    let events = u64::try_from(advance.emitted).unwrap();
    assert!(events > 10, "{events}");
    assert_eq!(side.dropped(), events - 1);

    let dir = tempfile::tempdir().unwrap();
    let look = crate::render_decl::Look {
        structure: true,
        ..crate::render_decl::Look::left([255, 255, 255, 255])
    };
    let model = crate::render_decl::skin(dir.path(), &look);
    let limits = AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    let uvs = model.atlas(Variant::Lo, limits).unwrap().into_parts().1;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&seen);
    let made = Mutex::new(Some((model, uvs)));
    let make: MakeScreen = Box::new(move || {
        let (model, uvs) = made.lock().unwrap().take().unwrap();
        Ok(Box::new(Skins {
            simple: SimpleSkin::new(),
            plain: SimpleSkin::new(),
            model,
            uvs,
            play: None,
            rearms: 0,
            seen: Arc::clone(&noted),
        }) as Box<dyn Screen>)
    });
    let clock = QpcClock::start();
    let shared = Arc::new(chart.clone());
    let link = spawn(
        make,
        Some(Arc::clone(&shared)),
        None,
        clock,
        view(),
        measure(clock),
        Box::new(NoFrames),
    )
    .unwrap();
    link.send(RenderMsg::Play {
        side,
        gauge: 0,
        measure: play_measure(clock.now()),
    });
    assert!(wait_until(|| seen.lock().unwrap().len() >= 3));
    // The next candidate.
    logic.rearm(&InputStart::default(), TimeUs(clock.now().0 + 10_000_000));
    assert!(wait_until(|| seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.generation == 1)
        .count()
        >= 3));
    let _ = link.finish(clock.now());
    link.exit();

    let seen = seen.lock().unwrap();
    let held = |ms: i64| Some(TimeUs(ms * MS));
    let played = Seen {
        generation: 0,
        events,
        rearms: 0,
        effects: Vec::new(),
        simple: (0, 0, [true, true, false]),
        model: (0, 0, [held(1_000), held(850), None]),
    };
    let fresh = Seen {
        generation: 1,
        events: 0,
        rearms: 1,
        effects: Vec::new(),
        simple: (1, 1, [false; 3]),
        model: (1, 1, [None; 3]),
    };
    // The one effect kept is the first, handed to the first frame only.
    let (first, rest) = seen.split_first().unwrap();
    assert_eq!(
        *first,
        Seen {
            effects: vec![0],
            ..played.clone()
        }
    );
    for s in rest {
        let want = if s.generation == 0 { &played } else { &fresh };
        assert_eq!(s, want);
    }
}
