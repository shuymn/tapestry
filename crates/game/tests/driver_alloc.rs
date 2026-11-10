//! The adjustments' driver allocates nothing once its attempt is attached
//! (design.md "Scope of No Allocation"): drains of a real tap's presses,
//! releases, repeats and ticks with the attempt's tempo provider, an
//! opening's and a cancellation's markers, a loss and its recovery, the
//! window's speed keys, a provisional close with its decision either way,
//! and a display offset cut into the stream. This
//! file holds a single test, so the harness's other threads are not
//! tracked.

mod counting;

use bms_model::{ChartBuilder, TimeUs};
use eval::ControlId;
use game::{
    AdjustDriver, AdjustmentConfig, AttemptTempo, DeviceSettings, InputPump, InputWaitError,
    KeyConfigs, PlayOptions,
};
use input::{
    DeviceId, DeviceKind, Packet, PresentationFocus, SourceConfig, SourceSet, WindowKeyEvent,
    window_keys,
};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

struct Pump<'a>(&'a mut SourceSet, TimeUs);

impl InputPump for Pump<'_> {
    fn request_fence(&mut self, n: u64) {
        let now = self.1;
        if let Some(tap) = self.0.adjustment_mut() {
            tap.fence(n, now);
        }
    }
    fn wake(&mut self) {
        self.0.tick_presentation(self.1);
    }
    fn display_offset_us(&self) -> i64 {
        0
    }
    fn park(&mut self) -> Result<(), InputWaitError> {
        Err(InputWaitError::Timeout)
    }
}

fn batch(set: &mut SourceSet, at: i64, keys: &[(u32, i32)]) {
    set.begin_batch(TimeUs(at));
    for &(code, value) in keys {
        set.packet(&Packet {
            device: DeviceId(1),
            control: ControlId(code),
            value,
            background: false,
        });
    }
    set.end_batch(TimeUs(at + 1));
}

#[test]
fn the_driver_drains_without_allocating() {
    let device = DeviceSettings::default();
    let keys = KeyConfigs::from(&device);
    let plan = AdjustmentConfig::of(&device)
        .plan(&keys, None, None)
        .unwrap();
    let (mut driver, mut tap, mut reader) =
        AdjustDriver::new(&plan, PlayOptions::default(), 0).unwrap();
    let (mut window, receiver) = window_keys();
    let number = |code| {
        plan.bindings
            .controls
            .iter()
            .find(|(c, _)| *c == input::TapControl::Key(ControlId(code)))
            .map(|&(_, n)| n)
    };
    let [f1, f2] = game::SPEED_KEYS;
    tap.set_window_keys(receiver, [number(f1), number(f2)]);
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    let focus = PresentationFocus::new();
    set.set_presentation_focus(focus.clone());
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.end_batch(TimeUs(1));
    set.set_adjustment(tap);
    let chart = ChartBuilder::new(150_000)
        .bpm(TimeUs(5_000_000), 300_000)
        .build()
        .unwrap();
    driver
        .enter_attempt(Some(TimeUs(-2_000_000)), &mut Pump(&mut set, TimeUs(2)))
        .unwrap();
    driver
        .attach(
            AttemptTempo::prepare(&chart, None),
            TimeUs(-2_000_000),
            &mut Pump(&mut set, TimeUs(3)),
        )
        .unwrap();
    // The opening's storage is made before the count.
    set.begin_batch(TimeUs(10));
    assert!(
        set.open_candidate(TimeUs(10), TimeUs(2_000_010), 1)
            .is_some()
    );
    set.end_batch(TimeUs(11));
    let _ = driver.drain(0);

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut at = 1_000;
    for round in 0..20_i64 {
        // SELECT and the lane keys, START's double press, F1 and F2 on
        // the keyboard and from the window, the scratch held, ticks.
        batch(&mut set, at, &[(0x11, 1), (0x2c, 1), (0x2c, 0)]);
        batch(&mut set, at + 10_000, &[(0x11, 0), (0x10, 1), (0x10, 0)]);
        batch(
            &mut set,
            at + 20_000,
            &[(0x10, 1), (0x10, 0), (f2, 1), (f2, 0)],
        );
        batch(&mut set, at + 30_000, &[(0x2a, 1)]);
        window.key(0, WindowKeyEvent::Press, false);
        window.key(0, WindowKeyEvent::Release, false);
        for k in 1..=3 {
            set.tick_presentation(TimeUs(at + 30_000 + k * 50_000));
        }
        batch(&mut set, at + 200_000, &[(0x2a, 0), (f1, 1), (f1, 0)]);
        if round % 5 == 0 {
            focus.set(false);
            focus.set(true);
        }
        set.tick_presentation(TimeUs(at + 210_000));
        // The revalidation's provisional close, F2 waiting behind its
        // fence, then the decision: reversed, or kept and reopened.
        driver
            .close_provisionally(&mut Pump(&mut set, TimeUs(at + 220_000)))
            .unwrap();
        batch(&mut set, at + 230_000, &[(f2, 1), (f2, 0)]);
        if round % 2 == 0 {
            driver.reverse_close(&mut Pump(&mut set, TimeUs(at + 240_000)));
        } else {
            driver.commit_close(&mut Pump(&mut set, TimeUs(at + 240_000)));
            driver
                .reopen_acceptance(&mut Pump(&mut set, TimeUs(at + 250_000)))
                .unwrap();
        }
        let _ = driver.drain(round * 1_000);
        let _ = reader.read();
        at += 300_000;
    }
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0, "the driver allocated or freed {calls} times");
    assert_eq!(driver.diagnostics().out_of_order, 0);
    assert!(driver.snapshot().hispeed.is_some());
}
