//! The same scripted play through real input and logic threads, woken as
//! in production (`unpark` after each publication, `park_timeout` on the
//! logic side), ends with the stream of the threadless runner.
//!
//! Prints the input → judgment latency and the finalization delay as
//! indicators: `cargo test -p game --test threaded -- --nocapture`.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bms_model::{TimeSource, TimeUs};
use common::{MS, autoplay_script, chart, count_judged, device, window_capacity};
use eval::InputStart;
use game::{Logic, LogicConfig, RunOutcome, Runner};
use input::{InputAdapter, SourceSet};
use rules::Judgment;

/// The wall clock on the judgment timeline, reading `base` at creation.
struct Wall {
    origin: Instant,
    base: i64,
}

impl TimeSource for Wall {
    fn now(&self) -> TimeUs {
        let us = i64::try_from(self.origin.elapsed().as_micros()).unwrap();
        TimeUs(self.base + us)
    }
}

fn summary(name: &str, samples: &mut [i64]) {
    assert!(!samples.is_empty(), "no {name} samples");
    samples.sort_unstable();
    let at = |q: usize| samples[(samples.len() - 1) * q / 100];
    println!(
        "indicator: {name}: n={} p50={} us p99={} us max={} us",
        samples.len(),
        at(50),
        at(99),
        samples[samples.len() - 1]
    );
}

#[test]
fn threaded_logic_matches_threadless() {
    const START: TimeUs = TimeUs(-200_000);
    // 200 notes 11 ms apart, every 5th a 30 ms long note: about 2.4 s.
    let chart = chart(200, 11 * MS, 30 * MS);
    let rule = rules::custom();
    let dev = device(&[0]);
    let config = LogicConfig {
        inputs: window_capacity(START, TimeUs(4_000 * MS), 8),
        effects: 1 << 16,
    };

    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let mut runner = Runner::new(&chart, &rule, &dev, sources, script, config, START).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    let expected = runner.stream().to_vec();
    assert_eq!(count_judged(&expected, Judgment::Perfect), 200);

    let (_, mut script) = autoplay_script(&chart, &dev);
    let mut sources = SourceSet::new(common::sources()).unwrap();
    script.poll(START, &mut sources);
    let start: InputStart = sources.open_recording(START);
    let readers = sources.take_readers();
    let (mut logic, mut render) =
        Logic::new(&chart, &rule, &dev, &start, readers, config, Vec::new()).unwrap();
    let mut stream = Vec::with_capacity(logic.max_events());
    let mut latency = Vec::with_capacity(1 << 20);
    let mut delay = Vec::with_capacity(1 << 20);
    let clock = Wall {
        origin: Instant::now(),
        base: START.0,
    };
    let done = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(30);

    thread::scope(|s| {
        let logic_thread = s.spawn(|| {
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::park_timeout(Duration::from_millis(2));
                let now = clock.now();
                let a = logic.advance_to(now);
                stream.extend_from_slice(logic.events());
                if let Some(d) = a.finalization_delay
                    && delay.len() < delay.capacity()
                {
                    delay.push(d.0);
                }
                if let Some(l) = a.judgment_latency
                    && latency.len() < latency.capacity()
                {
                    latency.push(l.0);
                }
                if a.complete {
                    done.store(true, Ordering::Release);
                }
            }
        });
        let waker = logic_thread.thread().clone();
        let (done, clock, script, sources) = (&done, &clock, &mut script, &mut sources);
        s.spawn(move || {
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                script.poll(clock.now(), sources);
                waker.unpark();
                thread::sleep(Duration::from_millis(1));
            }
        });
    });

    assert!(
        done.load(Ordering::Acquire),
        "the threaded play did not complete"
    );
    assert_eq!(count_judged(&stream, Judgment::Perfect), 200);
    assert_eq!(stream, expected);
    let mut effects = Vec::with_capacity(stream.len());
    render.drain_effects(|e| effects.push(*e));
    assert_eq!(effects, stream);
    summary("input -> judgment", &mut latency);
    summary("finalization delay", &mut delay);
}
