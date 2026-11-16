//! A play frame's one sample and its view's acquisition (#626): the frame
//! reads the clock for its chart time after it acquires the newest view,
//! so whatever that view finalized, through an input publication made
//! before the publication of the view, is before the frame's chart time.
//! A scenario's capture writes both beside the frame (`chart_us` from
//! `Drawn::play`, `finalized_us` from `Drawn::view`).

use std::sync::Mutex;

use crate::measure::Drawn;

use super::super::FrameSink;
use super::*;

const MS: i64 = 1_000;

/// Each frame's chart time and its view's finalized watermark.
type Stamped = Arc<Mutex<Vec<(Option<TimeUs>, Option<TimeUs>)>>>;

struct Stamps(Stamped);

impl FrameSink for Stamps {
    fn frame(&mut self, _: &dyn Screen, drawn: &Drawn<'_>) {
        self.0
            .lock()
            .unwrap()
            .push((drawn.play, drawn.view.and_then(|v| v.finalized)));
    }
}

/// A screen that draws every frame it is asked for, a millisecond apart.
struct Frames;

impl Screen for Frames {
    fn load(&mut self, _: &Chart, _: Option<&eval::Section>) -> Result<(), RenderError> {
        Ok(())
    }
    fn wait(&self) -> bool {
        std::thread::sleep(Duration::from_millis(1));
        true
    }
    fn paced(&self) -> bool {
        false
    }
    fn build(&mut self, _: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(timing(true))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

/// A view published while a play frame has begun and not yet acquired its
/// view, finalized through the clock read there less 1 µs (as the input
/// thread's publication gives it, the logic step finalizing there before
/// it publishes the view), is the view that frame draws, and the frame's
/// chart time is after the view's finalized watermark: the frame reads
/// the clock after the acquisition, not at its beginning, 2 ms before.
#[test]
fn a_play_frame_s_time_is_after_what_its_view_finalized() {
    let clock = QpcClock::start();
    let origin = clock.now();
    let mut initial = crate::render_common::view(0, 0);
    initial.origin = Some(origin);
    let (_sink, mut writer, side) = game::render_link(16, &initial);
    let stamped: Stamped = Arc::new(Mutex::new(Vec::new()));
    let make: MakeScreen = Box::new(|| Ok(Box::new(Frames) as Box<dyn Screen>));
    let chart = Arc::new(
        ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000 * MS))
            .build()
            .unwrap(),
    );
    let link = spawn(
        make,
        Some(Arc::clone(&chart)),
        None,
        clock,
        view(),
        measure(clock),
        Box::new(Stamps(Arc::clone(&stamped))),
    )
    .unwrap();
    let ready = link.prepare_attempt(super::attempt(&chart, None), || false);
    assert_eq!(ready.unwrap(), Preparation::Ready);
    let [held, holding] = &link.shared.play_frame_hold;
    held.store(true, Ordering::SeqCst);
    link.send(RenderMsg::Play {
        state_origin: origin,
        side,
        gauge: 0,
        measure: play_measure(origin),
    });
    assert!(wait_until(|| holding.load(Ordering::SeqCst)));
    // The frame has begun and holds before its acquisition. The input
    // thread reads the clock and publishes up to that read less 1 µs; the
    // logic step finalizes there and publishes the view.
    std::thread::sleep(Duration::from_millis(2));
    let finalized = TimeUs(clock.now().0 - origin.0 - 1);
    let v = writer.input_buffer_mut();
    v.generation = 0;
    v.origin = Some(origin);
    v.finalized = Some(finalized);
    writer.publish();
    held.store(false, Ordering::SeqCst);
    let first = || {
        stamped
            .lock()
            .unwrap()
            .iter()
            .find(|(_, f)| *f == Some(finalized))
            .copied()
    };
    assert!(wait_until(|| first().is_some()));
    let (play, _) = first().unwrap();
    link.exit();
    // The frames before it drew the initial view, finalized nowhere.
    let before: Vec<_> = stamped
        .lock()
        .unwrap()
        .iter()
        .take_while(|(_, f)| *f != Some(finalized))
        .map(|&(_, f)| f)
        .collect();
    assert!(before.iter().all(Option::is_none), "{before:?}");
    let play = play.expect("a play frame");
    assert!(
        finalized < play,
        "the frame's chart time {play:?} is not after its view's finalized {finalized:?}"
    );
}
