//! Lane input (S9-02, #157) through the render thread: a real logic step
//! fed by source queues, the render thread's acquisition, and both skins'
//! builds from the very `FrameInput` the thread makes, `SimpleSkin`'s key
//! beams and the model's `press` and `release` timers, with a 150 ms
//! display offset that moves neither. The fixed-clock tests run with an
//! effect queue of one, whose first step runs before the render thread
//! drains anything and so drops all but one of its effects (later steps
//! drop whatever the frames have not drained yet), and with a roomy one:
//! the beams and timers are the same, since they come from the view. A
//! lifecycle test keeps the thread's own frame clock through real opening,
//! rearm and cancelled-wait boundaries.

use std::sync::Mutex;

use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, PhysicalInputEvent, ScratchSettings,
    SourceId, SourceProfile,
};
use render::{PlaySkin, SimpleSkin, SpriteBatch, layout};
use skin_model::{AtlasLimits, LaneTimer, Play, Skin, UvTable, Variant, lane_timer, timer};

use super::*;

const MS: i64 = 1_000;
const AXIS: ControlId = ControlId(0x1_0030);
/// The key beam's alpha at full light (`render::simple`'s `BEAM`).
const BEAM_ALPHA: f32 = 0.8;

fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

/// What one frame showed.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    generation: u64,
    finalized: Option<TimeUs>,
    /// How far the view's conversion went: its finalized watermark in
    /// play, beyond it in the audio tail.
    converted: Option<TimeUs>,
    /// Each lane's key beam alpha in `SimpleSkin`, `None` unlit.
    beams: [Option<f32>; 8],
    /// Each lane's model `press` and `release` starts.
    keys: [(Option<TimeUs>, Option<TimeUs>); 8],
    /// The model's `ready` start.
    ready: Option<TimeUs>,
}

/// What one frame's clock was.
#[derive(Clone, Copy, Debug)]
struct Clocked {
    /// The frame's chart time.
    now: TimeUs,
    /// The QPC read as the frame was built.
    at: TimeUs,
    /// The model's `ready` reading in ms.
    ready_ms: i32,
    /// Each lane's model `press` and `release` readings in ms.
    elapsed: [(i32, i32); 8],
}

/// A screen that builds both skins from each frame the render thread
/// acquires and notes what they show: at the chart time `now` gives the
/// frame's view's conversion watermark when set (the logic step
/// publishes each step's view once, converted through its watermark), otherwise at the thread's own frame time,
/// with the display offset it is given.
struct Skins {
    simple: SimpleSkin,
    model: Skin,
    uvs: UvTable,
    play: Option<Play>,
    now: Option<fn(Option<TimeUs>) -> TimeUs>,
    clock: QpcClock,
    seen: Arc<Mutex<Vec<(Seen, Clocked)>>>,
}

/// Each lane's key beam alpha: the additive sprite of the lane's width
/// from the lanes' top to the judgment line.
fn beams(batch: &SpriteBatch) -> [Option<f32>; 8] {
    std::array::from_fn(|l| {
        let (x, w) = layout::lane_x(lane(u8::try_from(l).unwrap()));
        batch
            .blended(render::Blend::Add)
            .find(|s| s.rect == [x, layout::LANE_TOP, w, layout::JUDGE_Y - layout::LANE_TOP])
            .map(|s| s.color[3])
    })
}

impl Screen for Skins {
    fn load(&mut self, chart: &Chart, section: Option<&eval::Section>) -> Result<(), RenderError> {
        self.simple.prepare_attempt(chart, section).unwrap();
        let ctx = crate::render_common::context(&crate::render_common::GAUGES, false);
        self.play = Some(self.model.prepare(chart, &ctx).unwrap());
        Ok(())
    }
    /// The attempt's own context, its lead-in and READY start included.
    fn prepare_attempt(
        &mut self,
        a: &render::AttemptInput<'_>,
        _skin: Option<&Arc<render::SkinBundle>>,
        _request: u64,
    ) -> Option<Result<Preparation, RenderError>> {
        self.simple.prepare_attempt(a.chart, a.section()).unwrap();
        self.play = Some(self.model.prepare(a.chart, a.context).unwrap());
        Some(Ok(Preparation::Ready))
    }
    fn wait(&self) -> bool {
        true
    }
    fn paced(&self) -> bool {
        false
    }
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        let f = match self.now {
            Some(now) => FrameInput {
                now: now(f.view.input.converted()),
                display_offset: TimeUs(150 * MS),
                ..*f
            },
            None => *f,
        };
        let at = self.clock.now();
        let mut batch = SpriteBatch::with_capacity(4_096);
        self.simple.build(&f, &mut batch)?;
        let play = self.play.as_mut().unwrap();
        let mut quads = skin_model::QuadBatch::with_capacity(
            self.model.quad_capacity(),
            self.model.run_capacity(),
        );
        self.model
            .build(play, &render::skin_frame(&f), &self.uvs, &mut quads)?;
        let keys = std::array::from_fn(|l| {
            let l = lane(u8::try_from(l).unwrap());
            (
                play.timer(lane_timer(LaneTimer::Press, l)),
                play.timer(lane_timer(LaneTimer::Release, l)),
            )
        });
        let reading = |slot: u16| play.state().timers[usize::from(slot)];
        let elapsed = std::array::from_fn(|l| {
            let l = lane(u8::try_from(l).unwrap());
            (
                reading(lane_timer(LaneTimer::Press, l)),
                reading(lane_timer(LaneTimer::Release, l)),
            )
        });
        let clocked = Clocked {
            now: f.now,
            at,
            ready_ms: reading(timer::READY),
            elapsed,
        };
        let seen = Seen {
            generation: f.view.generation,
            finalized: f.view.finalized,
            converted: f.view.input.converted(),
            beams: beams(&batch),
            keys,
            ready: play.timer(timer::READY),
        };
        self.seen.lock().unwrap().push((seen, clocked));
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(timing(true))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn rearm(&mut self) {
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

/// Source 0 binding control `n` to lane `n` for the keys, control 9 to
/// key 4 too, and an absolute axis on the scratch; source 1 binding
/// control `n` to key `n`.
fn device() -> DeviceProfile {
    let keys = || {
        (1..8)
            .map(|n| Binding {
                control: ControlId(u32::from(n)),
                lane: lane(n),
            })
            .collect::<Vec<_>>()
    };
    let mut first = keys();
    first.push(Binding {
        control: ControlId(9),
        lane: lane(4),
    });
    DeviceProfile {
        sources: vec![
            SourceProfile {
                input_offset: TimeUs(0),
                bindings: first,
                axes: vec![AxisBinding {
                    control: AXIS,
                    lane: Lane::SCRATCH,
                    scratch: ScratchSettings {
                        mode: AxisMode::Absolute { wrap: 256 },
                        ..ScratchSettings::DEFAULT
                    },
                }],
            },
            SourceProfile {
                input_offset: TimeUs(0),
                bindings: keys(),
                axes: Vec::new(),
            },
        ],
    }
}

fn set(ms: i64, source: u16, control: u32, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(ms * MS),
        source: SourceId(source),
        kind: InputKind::Control {
            control: ControlId(control),
            value,
        },
    }
}

fn key(ms: i64, n: u32, value: i32) -> PhysicalInputEvent {
    set(ms, 0, n, value)
}

fn axis(ms: i64, value: i32) -> PhysicalInputEvent {
    set(ms, 0, AXIS.0, value)
}

/// One logic step: the events each source receives (source 1's after
/// source 0's), its watermark, whether source 1 is closed with it, and
/// the frame's chart time.
struct Step {
    events: Vec<PhysicalInputEvent>,
    watermark: i64,
    close_second: bool,
}

/// Plays `steps` over `chart` from the opening `start` with an effect
/// queue of `effects`, then rearms with `rearm` held at the next opening
/// if given; each frame is drawn at the chart time `now` gives its view.
/// Returns the last frame seen of each step, and of the rearmed
/// candidate. The first step runs before the render thread takes its
/// side, so a queue of one drops all but one of its effects.
fn played(
    chart: &Chart,
    device: &DeviceProfile,
    start: &InputStart,
    steps: &[Step],
    effects: usize,
    rearm: Option<&InputStart>,
    now: fn(Option<TimeUs>) -> TimeUs,
) -> Vec<Seen> {
    let rule = rules::tapestry();
    let sources = device.sources.len();
    let (mut writers, readers): (Vec<_>, Vec<_>) =
        (0..sources).map(|_| input::source_queue(64)).unzip();
    let (mut logic, side) = Logic::<SourceReader>::new(
        chart,
        &rule,
        device,
        start,
        readers,
        LogicConfig {
            inputs: 64,
            effects,
        },
        Vec::new(),
    )
    .unwrap();
    let mut step = |logic: &mut Logic<'_, SourceReader>, s: &Step| {
        for ev in &s.events {
            writers[usize::from(ev.source.0)].push(*ev).unwrap();
        }
        for (i, w) in writers.iter_mut().enumerate() {
            if i == 1 && s.close_second {
                w.close(TimeUs(s.watermark * MS));
            } else {
                w.publish(TimeUs(s.watermark * MS));
            }
        }
        let a = logic.advance_to(TimeUs(s.watermark * MS));
        assert_eq!(a.watermark, Some(TimeUs(s.watermark * MS)));
        u64::try_from(a.emitted).unwrap()
    };
    let (first, rest) = steps.split_first().unwrap();
    let emitted = step(&mut logic, first);
    if effects == 1 {
        assert!(emitted > 1, "{emitted}");
        assert_eq!(side.dropped(), emitted - 1);
    }

    let clock = QpcClock::start();
    let (seen, make) = screen(Some(now), clock);
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
    let last = |generation: u64, finalized: Option<TimeUs>| {
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| (s.generation, s.finalized) == (generation, finalized))
            .count()
            >= 2));
        let seen = seen.lock().unwrap();
        let frames: Vec<&Seen> = seen
            .iter()
            .map(|(s, _)| s)
            .filter(|s| (s.generation, s.finalized) == (generation, finalized))
            .collect();
        // Every frame of one view shows the same, whatever was drained.
        assert!(frames.windows(2).all(|w| w[0] == w[1]), "{frames:?}");
        frames[0].clone()
    };
    let mut out = vec![last(0, Some(TimeUs(first.watermark * MS)))];
    for s in rest {
        step(&mut logic, s);
        out.push(last(0, Some(TimeUs(s.watermark * MS))));
    }
    if let Some(start) = rearm {
        logic.rearm(start, TimeUs(clock.now().0 + 10_000_000));
        out.push(last(1, None));
    }
    let _ = link.finish(clock.now());
    link.exit();
    out
}

/// Frames noted by the screen [`Skins`] makes, at `now` if given, with
/// a model skin of its own.
#[expect(clippy::type_complexity, reason = "a test's shared log")]
fn screen(
    now: Option<fn(Option<TimeUs>) -> TimeUs>,
    clock: QpcClock,
) -> (Arc<Mutex<Vec<(Seen, Clocked)>>>, MakeScreen) {
    let dir = tempfile::tempdir().unwrap();
    let model = crate::render_decl::skin(dir.path(), &crate::render_decl::Look::left([255; 4]));
    let limits = AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    let uvs = model.atlas(Variant::Lo, limits).unwrap().into_parts().1;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&seen);
    let made = Mutex::new(Some((model, uvs, dir)));
    let make: MakeScreen = Box::new(move || {
        let (model, uvs, _dir) = made.lock().unwrap().take().unwrap();
        Ok(Box::new(Skins {
            simple: SimpleSkin::new(),
            model,
            uvs,
            play: None,
            now,
            clock,
            seen: Arc::clone(&noted),
        }) as Box<dyn Screen>)
    });
    (seen, make)
}

/// A beam at `share` of full light.
fn lit(share: f32) -> Option<f32> {
    Some(BEAM_ALPHA * share)
}

fn close(a: [Option<f32>; 8], b: [Option<f32>; 8]) -> bool {
    a.iter().zip(&b).all(|(a, b)| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-6,
        (None, None) => true,
        _ => false,
    })
}

fn at(ms: i64) -> Option<TimeUs> {
    Some(TimeUs(ms * MS))
}

/// Lanes with and without notes, a silent note, a key held at the
/// opening, a key's repeat, two controls on one lane, a second source, a
/// scratch hold moving its release deadline, a reversal, the deadline's
/// expiry with no input, a disconnect and a rearm, each frame drawn 0 to
/// 40 ms after its step's watermark: each lane's beam is whole while it
/// is held, past 100 ms included, and fades from its release, and its
/// key-on and key-off exclude each other.
#[test]
fn lane_input_reaches_both_skins() {
    // Key 7's taps at 100, 200 and 300 ms are judged (effects every
    // step); key 2's note at 1 s has no sound; key 3 has no notes.
    let chart = ChartBuilder::new(150_000)
        .tap(lane(7), TimeUs(100 * MS))
        .tap(lane(7), TimeUs(200 * MS))
        .tap(lane(7), TimeUs(300 * MS))
        .tap(lane(2), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    assert!(chart.lanes[2].iter().all(|n| n.sound.is_none()));
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), ControlId(6), 1)],
    };
    let steps = [
        Step {
            events: vec![
                key(100, 3, 1),
                axis(100, 10),
                key(100, 7, 1),
                key(130, 7, 0),
                key(150, 3, 1),
                key(200, 4, 1),
                axis(200, 12),
                key(200, 7, 1),
                key(230, 7, 0),
                key(250, 9, 1),
                axis(290, 13),
                key(300, 4, 0),
                key(300, 7, 1),
            ],
            watermark: 320,
            close_second: false,
        },
        Step {
            events: vec![axis(350, 11)],
            watermark: 400,
            close_second: false,
        },
        Step {
            events: vec![set(450, 1, 5, 1), key(500, 3, 0), key(550, 6, 0)],
            watermark: 600,
            close_second: false,
        },
        Step {
            events: vec![
                key(700, 3, 1),
                key(850, 2, 1),
                PhysicalInputEvent {
                    received: TimeUs(800 * MS),
                    source: SourceId(1),
                    kind: InputKind::Disconnected,
                },
            ],
            watermark: 900,
            close_second: true,
        },
    ];
    let rearmed = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), ControlId(1), 1)],
    };
    let now = |finalized: Option<TimeUs>| match finalized.map(|t| t.0 / MS) {
        Some(320) => TimeUs(320 * MS),
        Some(400) => TimeUs(400 * MS),
        Some(600) => TimeUs(560 * MS),
        Some(900) => TimeUs(880 * MS),
        _ => TimeUs(100 * MS),
    };
    for effects in [1, 4_096] {
        let seen = played(
            &chart,
            &device(),
            &start,
            &steps,
            effects,
            Some(&rearmed),
            now,
        );
        let ready = seen[0].ready;
        assert!(ready.is_some());
        let none = (None, None);
        let on = |ms| (at(ms), None);
        let off = |ms| (None, at(ms));
        // At 320 ms: key 6 held since the opening, its key-on from READY;
        // key 3 held 220 ms through its repeat; key 4 by its second
        // control; key 7 pressed again; the scratch held past its first
        // deadline (300 ms) by the hold at 290 ms.
        let want = [
            ((on(200)), lit(1.0)),
            (none, None),
            (none, None),
            (on(100), lit(1.0)),
            (on(200), lit(1.0)),
            (none, None),
            ((ready, None), lit(1.0)),
            (on(300), lit(1.0)),
        ];
        // At 400 ms: the scratch reversed at 350 ms, held from there.
        let mut second = want;
        second[0] = (on(350), lit(1.0));
        // At 560 ms: the scratch released at its 450 ms deadline (faded),
        // key 3 at 500 ms (60 ms along), key 6 at 550 ms (10 ms); key 5
        // held from source 1.
        let mut third = second;
        third[0] = (off(450), None);
        third[3] = (off(500), lit(0.4));
        third[6] = (off(550), lit(0.9));
        third[5] = (on(450), lit(1.0));
        // At 880 ms: key 3 pressed again at 700 ms, whole; key 5 released
        // by its source's removal at 800 ms; key 2's silent note pressed
        // at 850 ms; key 6 faded.
        let mut fourth = third;
        fourth[3] = (on(700), lit(1.0));
        fourth[5] = (off(800), lit(0.2));
        fourth[2] = (on(850), lit(1.0));
        fourth[6] = (off(550), None);
        // The rearmed candidate: key 1 held at its opening, the rest
        // fresh.
        let mut fifth = [(none, None); 8];
        fifth[1] = ((ready, None), lit(1.0));
        for (i, (s, want)) in seen
            .iter()
            .zip([want, second, third, fourth, fifth])
            .enumerate()
        {
            let keys: [_; 8] = std::array::from_fn(|l| want[l].0);
            let beams: [_; 8] = std::array::from_fn(|l| want[l].1);
            assert_eq!(s.keys, keys, "effects {effects}, frame {i}");
            assert!(
                close(s.beams, beams),
                "effects {effects}, frame {i}: {:?} {beams:?}",
                s.beams
            );
        }
        assert_eq!(seen.len(), 5);
    }
}

/// Autoplay's generated stream: a tap held 50 ms, a soundless tap, and a
/// long note held to its end each light their beam whole while held and
/// fade from the generated release.
#[test]
fn autoplays_stream_lights_the_beams() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(1_000 * MS))
        .long(lane(3), TimeUs(1_000 * MS), TimeUs(1_500 * MS))
        .build()
        .unwrap();
    let mut device = device();
    device.sources.truncate(1);
    let events = input::autoplay(&chart, &device.sources[0]).unwrap();
    let upto = |from: i64, to: i64| {
        events
            .iter()
            .filter(|e| e.received.0 > from * MS && e.received.0 <= to * MS)
            .copied()
            .collect::<Vec<_>>()
    };
    let steps = [
        Step {
            events: upto(i64::MIN / MS, 1_020),
            watermark: 1_020,
            close_second: false,
        },
        Step {
            events: upto(1_020, 1_080),
            watermark: 1_080,
            close_second: false,
        },
        Step {
            events: upto(1_080, 1_600),
            watermark: 1_600,
            close_second: false,
        },
    ];
    let now = |finalized: Option<TimeUs>| match finalized.map(|t| t.0 / MS) {
        Some(1_600) => TimeUs(1_550 * MS),
        Some(t) => TimeUs(t * MS),
        None => TimeUs(0),
    };
    for effects in [1, 4_096] {
        let seen = played(
            &chart,
            &device,
            &InputStart::default(),
            &steps,
            effects,
            None,
            now,
        );
        let beams = |b: [Option<f32>; 3]| {
            let mut all = [None; 8];
            all[1..4].copy_from_slice(&b);
            all
        };
        let expect = [
            beams([lit(1.0), lit(1.0), lit(1.0)]),
            beams([lit(0.7), lit(0.7), lit(1.0)]),
            beams([None, None, lit(0.5)]),
        ];
        for (i, (s, want)) in seen.iter().zip(expect).enumerate() {
            assert!(
                close(s.beams, want),
                "effects {effects}, frame {i}: {:?} {want:?}",
                s.beams
            );
        }
        assert_eq!(seen[1].keys[1], (None, at(1_050)));
        assert_eq!(seen[1].keys[3], (at(1_000), None));
        assert_eq!(seen[2].keys[3], (None, at(1_500)));
    }
}

/// The real lifecycle's clocks, the render thread's own frame time kept:
/// a candidate opened 1 s before its chart time 0 (the attempt's
/// lead-in, so its opening is its READY start) with key 6 held, a press,
/// a cancellation whose READY wait lasts 2 s with no step, a rearm from
/// the wait's opening with key 5 held, a press and 2 s of steps, then a
/// rearm straight from them with key 7 held. Each candidate's frames run
/// on its own origin (checked against the QPC read as each frame is
/// built), `ready` starts at its opening, a key held at an opening runs
/// key-on from there, and every reading is the frame's time since its
/// start. Prints the time from just before each step's advance (or
/// rearm) to the first frame built with its view; no bound is asserted.
#[test]
fn key_timers_follow_the_lifecycles_own_clocks() {
    const LEAD: i64 = 1_000 * MS;
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(60_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let mut device = device();
    device.sources.truncate(1);
    let clock = QpcClock::start();
    // An opening snapshot on its candidate's chart clock, as production's:
    // the opening lies the lead-in before chart time 0.
    let held = |n: u32| InputStart {
        opened_at: TimeUs(-LEAD),
        held: vec![(SourceId(0), ControlId(n), 1)],
    };
    let opened = clock.now();
    let mut origin = TimeUs(opened.0 + LEAD);
    let (mut writer, reader) = input::source_queue(64);
    let (mut logic, side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &device,
        &held(6),
        vec![reader],
        LogicConfig {
            inputs: 64,
            effects: 16,
        },
        Vec::new(),
    )
    .unwrap();
    logic.reserve_start(4);
    let (seen, make) = screen(None, clock);
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
    let ready = link.prepare_attempt(super::attempt(&shared, None), || false);
    assert_eq!(ready.unwrap(), Preparation::Ready);
    link.send(RenderMsg::Play {
        side,
        gauge: 0,
        measure: play_measure(origin),
    });
    // The first frame of `generation` built with a view finalized at
    // `finalized`.
    let frame = |generation: u64, finalized: Option<TimeUs>| {
        assert!(wait_until(|| seen.lock().unwrap().iter().any(|(s, _)| (
            s.generation,
            s.finalized
        )
            == (generation, finalized))));
        let seen = seen.lock().unwrap();
        seen.iter()
            .find(|(s, _)| (s.generation, s.finalized) == (generation, finalized))
            .cloned()
            .unwrap()
    };
    // The frame was drawn on `origin`; READY started at the opening, key
    // `held` held since it and each of `pressed` from its press, nothing
    // released; every reading is the frame's time since its start.
    let check = |(s, c): &(Seen, Clocked), origin: TimeUs, held: u8, pressed: &[(u8, TimeUs)]| {
        let lag = c.at.0 - origin.0 - c.now.0;
        assert!((0..1_000 * MS).contains(&lag), "{c:?} on {origin:?}");
        let ms = |start: TimeUs| i32::try_from((c.now.0 - start.0).max(0) / MS).unwrap();
        let ready_at = TimeUs(-LEAD);
        assert_eq!(s.ready, Some(ready_at), "{s:?}");
        assert_eq!(c.ready_ms, ms(ready_at), "{c:?}");
        for l in 0..8_u8 {
            let i = usize::from(l);
            let start = if l == held {
                Some(ready_at)
            } else {
                pressed.iter().find(|&&(p, _)| p == l).map(|&(_, t)| t)
            };
            assert_eq!(s.keys[i], (start, None), "key {l}: {s:?}");
            let want = start.map_or(skin_model::TIMER_OFF, ms);
            assert_eq!(
                c.elapsed[i],
                (want, skin_model::TIMER_OFF),
                "key {l}: {c:?}"
            );
        }
    };
    // One step at the candidate's chart time now, pressing key `n` there
    // if given: the time, and the QPC just before the advance.
    let step = |logic: &mut Logic<'_, SourceReader>,
                writer: &mut input::SourceWriter,
                origin: TimeUs,
                press: Option<u32>| {
        let at = TimeUs(clock.now().0 - origin.0);
        if let Some(n) = press {
            writer
                .push(PhysicalInputEvent {
                    received: at,
                    ..key(0, n, 1)
                })
                .unwrap();
        }
        writer.publish(at);
        let before = clock.now();
        logic.advance_to(at);
        (at, before)
    };
    let mut latencies = Vec::new();
    // Candidate 0: key 3 pressed in READY.
    let (pressed, before) = step(&mut logic, &mut writer, origin, Some(3));
    let first = frame(0, Some(pressed));
    latencies.push(first.1.at.0 - before.0);
    check(&first, origin, 6, &[(3, pressed)]);
    // Cancelled: no step for 2 s, the readers lent; the frames go on
    // drawing the cancelled candidate on its origin, its timers ageing.
    drop(logic.lend_readers());
    let waited_until = TimeUs(pressed.0 + 2_000 * MS);
    let past = |(s, c): &(Seen, Clocked)| s.generation == 0 && c.now >= waited_until;
    assert!(wait_until(|| seen.lock().unwrap().iter().any(past)));
    let waited = seen
        .lock()
        .unwrap()
        .iter()
        .find(|f| past(f))
        .cloned()
        .unwrap();
    check(&waited, origin, 6, &[(3, pressed)]);
    // Released: the next candidate opens now, with key 5 held, on a
    // reopened queue.
    let (w, reader) = input::source_queue(64);
    writer = w;
    logic.restore_readers(vec![reader]);
    let rearm = |logic: &mut Logic<'_, SourceReader>, n: u32, origin: &mut TimeUs| {
        let opening = clock.now();
        let previous = *origin;
        *origin = TimeUs(opening.0 + LEAD);
        assert!(origin.0 - previous.0 >= 2_000 * MS);
        let before = clock.now();
        logic.rearm(&held(n), *origin);
        before
    };
    let before = rearm(&mut logic, 5, &mut origin);
    let rearmed = frame(1, None);
    latencies.push(rearmed.1.at.0 - before.0);
    check(&rearmed, origin, 5, &[]);
    // Key 2 pressed, then 2 s of steps.
    let (at, before) = step(&mut logic, &mut writer, origin, Some(2));
    let pressed = frame(1, Some(at));
    latencies.push(pressed.1.at.0 - before.0);
    check(&pressed, origin, 5, &[(2, at)]);
    let until = clock.now().0 + 2_000 * MS;
    while clock.now().0 < until {
        let (w, before) = step(&mut logic, &mut writer, origin, None);
        let f = frame(1, Some(w));
        latencies.push(f.1.at.0 - before.0);
        check(&f, origin, 5, &[(2, at)]);
        std::thread::sleep(Duration::from_millis(50));
    }
    // Rearmed straight from the steps, key 7 held at its opening.
    let readers = logic.lend_readers();
    logic.restore_readers(readers);
    let before = rearm(&mut logic, 7, &mut origin);
    let rearmed = frame(2, None);
    latencies.push(rearmed.1.at.0 - before.0);
    check(&rearmed, origin, 7, &[]);
    let _ = link.finish(clock.now());
    link.exit();
    latencies.sort_unstable();
    println!(
        "lane input publication to frame, µs: {} views, min {}, median {}, max {}",
        latencies.len(),
        latencies[0],
        latencies[latencies.len() / 2],
        latencies[latencies.len() - 1]
    );
}

/// The audio tail (S9-03, #159) through the render thread and the audio
/// thread's loop: once the play completed and its audio was sealed, the
/// feedback goes on converting what the queue takes, so key 3 pressed in
/// the tail (no notes) lights whole in `SimpleSkin` with the model's
/// key-on from its press, and fades from its release with key-off, with an
/// effect queue of one, frames drawn at each step's conversion watermark;
/// key 1 pressed there sounds its note's sound, which the audio thread
/// takes, and the audio is reported done only once that sound has ended
/// with the BGM; the record and the view's scored part stay as the
/// completion froze them.
#[test]
fn tail_presses_reach_both_skins_and_the_audio() {
    use ::audio::{AudioEngine, Gain, MAX_VOICES, SimConfig, SimDevice, Sound, SoundBank};
    const RATE: u32 = 48_000;
    let mut chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("key.wav")
        .bgm(TimeUs(0), bms_model::SoundId(0))
        .tap(lane(1), TimeUs(100 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(bms_model::SoundId(1));
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![900; RATE as usize]),
            Sound::mono(vec![500; 480]),
        ],
    ));
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, 480, MAX_VOICES, 0).unwrap();
    let (mut audio, mut worker) = game::audio_control(engine);
    let mut out = SimDevice::new(SimConfig::steady(RATE, 480, TimeUs(0)));
    let mut audio_to = |at: i64| {
        while out.next_wake().0 <= at {
            worker.step(&mut out).unwrap();
        }
    };
    let rule = rules::tapestry();
    let mut device = device();
    device.sources.truncate(1);
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
    let (sink, queue) = game::audio_link_at(logic.max_events(), TimeUs(0));
    let accepted = queue.accepted_count();
    logic.add_sink(Box::new(sink));
    audio
        .play_at(TimeUs(0), queue, Gain::HEADROOM, None)
        .unwrap();
    let mut feed = |events: &[PhysicalInputEvent], ms: i64| {
        for ev in events {
            writer.push(*ev).unwrap();
        }
        writer.publish(TimeUs(ms * MS));
    };
    // Key 1's note hit, the play complete at 400 ms.
    feed(&[key(100, 1, 1), key(120, 1, 0)], 400);
    assert!(logic.advance_to(TimeUs(400 * MS)).complete);
    logic.stop_playing();
    audio.seal();
    let frozen = logic.view();
    let record = logic.record();

    let clock = QpcClock::start();
    let (seen, make) = screen(Some(|c| c.unwrap_or(TimeUs(0))), clock);
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
    let frame = |converted: i64| {
        let at = Some(TimeUs(converted * MS));
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| s.converted == at)));
        let seen = seen.lock().unwrap();
        seen.iter()
            .map(|(s, _)| s)
            .find(|s| s.converted == at)
            .cloned()
            .unwrap()
    };
    let beams = |b1: Option<f32>, b3: Option<f32>| {
        let mut b = [None; 8];
        b[1] = b1;
        b[3] = b3;
        b
    };
    assert_eq!(frame(400).keys[3], (None, None));
    // The tail: keys 1 and 3 pressed at 450 ms.
    feed(&[key(450, 1, 1), key(450, 3, 1)], 500);
    assert_eq!(logic.feedback_to(), Some(TimeUs(500 * MS)));
    let pressed = frame(500);
    assert_eq!(pressed.keys[3], (at(450), None));
    assert_eq!(pressed.keys[1], (at(450), None));
    assert!(
        close(pressed.beams, beams(lit(1.0), lit(1.0))),
        "{pressed:?}"
    );
    audio_to(500 * MS);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 2);
    assert!(!audio.done());
    // Both released at 520 ms: 80 ms along their fade at 600 ms.
    feed(&[key(520, 3, 0), key(520, 1, 0)], 600);
    logic.feedback_to().unwrap();
    let released = frame(600);
    assert_eq!(released.keys[3], (None, at(520)));
    assert!(
        close(released.beams, beams(lit(0.2), lit(0.2))),
        "{released:?}"
    );
    // The BGM's 1 s ends; then the audio is done, every sound taken.
    audio_to(900 * MS);
    assert!(!audio.done());
    audio_to(1_200 * MS);
    assert!(audio.done());
    let v = logic.view();
    assert_eq!(
        (v.tally, v.gauges, v.notes, v.finalized, v.events),
        (
            frozen.tally,
            frozen.gauges,
            frozen.notes,
            frozen.finalized,
            frozen.events
        )
    );
    assert_eq!(logic.record(), record);
    let _ = link.finish(clock.now());
    link.exit();
}
