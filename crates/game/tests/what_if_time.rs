//! How long a what-if takes on the result screen (design.md "Extension
//! Boundaries"): a chart near the loader's 500,000-object limit, its every
//! note hit, evaluated under another preset. An indicator against the
//! 250 ms target, not a gate; run it in release:
//! `cargo test --release -p game --test what_if_time -- --ignored --nocapture`.

mod common;

use std::time::Instant;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, lane};
use game::{ReEvaluate, ResultContext};
use replay::{
    BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete, PlayRecord,
    PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions, WakePolicy,
};
use rules::Implementation;

#[test]
#[ignore = "an indicator: run in release with --ignored"]
fn what_if_on_a_chart_near_the_object_limit() {
    // 490,000 notes over the 7 keys and the scratch, 12 ms apart on each
    // lane, every 10th a long note: about 12 minutes.
    let notes: i64 = 490_000;
    let mut b = ChartBuilder::new(150_000);
    for i in 0..notes {
        let l = lane(u8::try_from(i % 8).unwrap());
        let t = 1_000 * MS + (i / 8) * 12 * MS + (i % 8) * MS;
        b = if i % 10 == 9 {
            b.long(l, TimeUs(t), TimeUs(t + 6 * MS))
        } else {
            b.tap(l, TimeUs(t))
        };
    }
    let chart = b.build().unwrap();
    let played = rules::tapestry();
    let dev = device(&[0]);
    let source = &dev.sources[0];
    let events = input::autoplay(&chart, source).unwrap();
    let finalized =
        eval::Evaluator::new(&chart, &played, &dev, &eval::InputStart::default(), None, 0)
            .unwrap()
            .final_deadline();
    let inputs = events.len();
    let record = PlayRecord {
        versions: Versions {
            evaluator: Implementation::STANDARD_VERSION,
            app: "0.0.0-test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(&played),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: source.clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
        start: eval::InputStart {
            opened_at: TimeUs(-1_000 * MS),
            held: Vec::new(),
        },
        events,
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized,
            incomplete: Incomplete::default(),
        },
    };
    let ctx = ResultContext {
        record: &record,
        chart: &chart,
        preset: &played,
        live: &[],
    };
    for what_if in [rules::beatoraja_style(), rules::lr2_style()] {
        let began = Instant::now();
        let evaluated = ReEvaluate::under(&ctx, &what_if).unwrap();
        let took = began.elapsed();
        assert!(!evaluated.as_played);
        assert!(evaluated.evaluation.completed);
        println!(
            "indicator: what-if under {} of {notes} notes and {inputs} inputs in {} ms",
            what_if.id,
            took.as_millis()
        );
    }
}
