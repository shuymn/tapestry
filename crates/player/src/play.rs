//! The game thread: plays the loaded chart on the real threads, from the
//! countdown to the end screen, until the player quits (D8, D13).
//!
//! Each play has its own timeline (`game::Session` does the same over a
//! virtual clock): the input thread opens recording at a boundary and puts
//! chart time 0 `lead_in` after it on the QPC clock. The source set moves
//! every recorded time onto the play's timeline, the logic step advances
//! to the clock minus that origin, the audio engine and the key sounds'
//! origins sit on the clock with the play's origin, and the render thread
//! draws at the clock minus the origin. This thread runs the logic step,
//! woken by the input thread after each publication.

use std::thread;
use std::time::{Duration, Instant};

use bms_model::{TimeSource, TimeUs};
use game::{AbortCause, Command, Loaded, Logic, PlayEnd, PlayOutcome, PlayReport, audio_link_at};
use input::QpcClock;
use replay::Incomplete;

use crate::audio_side::{AudioLink, AudioMsg};
use crate::controls::Controls;
use crate::end_screen::{self, LoadTimes};
use crate::input_side::{InputLink, Request};
use crate::render_side::{RenderLink, RenderMsg};

/// How long the logic step parks at most without a wake from the input
/// thread.
const PARK: Duration = Duration::from_millis(2);
/// How often the tail and the end screen look at the controls.
const POLL: Duration = Duration::from_millis(2);

/// The threads a run plays through.
pub struct Threads {
    /// The input thread.
    pub input: InputLink,
    /// The audio thread.
    pub audio: AudioLink,
    /// The render thread.
    pub render: RenderLink,
}

impl Threads {
    /// Ends the render and audio threads; the input side stops with its
    /// owner.
    pub fn shut_down(self) {
        self.render.exit();
        self.audio.exit();
    }
}

/// How the run ends after a play's end screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterEnd {
    /// Wait for retry or quit.
    Wait,
    /// Quit once the end screen has been shown for this long (scripted
    /// runs).
    #[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
    QuitAfter(Duration),
}

/// What the run reports per play, for the console.
fn print_report(report: &PlayReport) {
    let result = report.result.as_ref();
    let counts = result
        .and_then(|r| r.evaluation.as_ref().ok())
        .map(|e| e.tally.counts);
    match counts {
        Some(c) => println!(
            "play {}: {:?}; perfect {} great {} good {} bad {} poor {}; re-evaluation matches live: {}",
            report.attempt,
            report.end,
            c.perfect,
            c.great,
            c.good,
            c.bad,
            c.poor,
            result.is_some_and(|r| r.matches_live)
        ),
        None => println!(
            "play {}: {:?}; nothing recorded",
            report.attempt, report.end
        ),
    }
    match &report.saved {
        Some(Ok(r)) => println!("saved: {}", r.path.display()),
        Some(Err(e)) => eprintln!("not saved: {e}"),
        None => {}
    }
}

/// Plays until the player quits or the window closes. `started` is when
/// the process started, for the first load time.
pub fn run(
    loaded: &mut Loaded,
    threads: &Threads,
    controls: &Controls,
    clock: QpcClock,
    started: Instant,
    after_end: AfterEnd,
) {
    let lead_in = loaded.config().lead_in;
    let device = loaded.settings().device.clone();
    let gauge = loaded.settings().player.displayed_gauge;
    let mut times = LoadTimes::default();
    let mut retry_at: Option<Instant> = None;
    let mut generation = 0;
    loop {
        controls.clear();
        generation += 1;
        let Some(report) = play_once(
            loaded,
            threads,
            controls,
            clock,
            Once {
                lead_in,
                gauge,
                generation,
            },
            |opened_at| match retry_at {
                Some(t) => times.retry = Some(opened_at.duration_since(t)),
                None => times.first = opened_at.duration_since(started),
            },
        ) else {
            return;
        };
        print_report(&report);
        println!(
            "load: first {} ms, retry {}",
            times.first.as_millis(),
            times
                .retry
                .map_or_else(|| "-".to_owned(), |d| format!("{} ms", d.as_millis()))
        );
        if controls.closing() {
            return;
        }
        let ended_by = |c| report.end == PlayEnd::Aborted(c) || report.tail_cut == Some(c);
        if ended_by(AbortCause::Quit) {
            return;
        }
        if ended_by(AbortCause::Retry) {
            retry_at = Some(Instant::now());
            continue;
        }
        let lines = end_screen::lines(&report, loaded.preset(), device.input_offset_us, times);
        threads.render.send(RenderMsg::Text(lines));
        let shown = Instant::now();
        loop {
            thread::sleep(POLL);
            match controls.take() {
                Some(Command::Retry) => {
                    retry_at = Some(Instant::now());
                    break;
                }
                Some(Command::Quit) => return,
                _ => {}
            }
            if let AfterEnd::QuitAfter(d) = after_end
                && shown.elapsed() >= d
            {
                return;
            }
        }
    }
}

struct Once {
    lead_in: TimeUs,
    gauge: usize,
    generation: u64,
}

/// One play, from opening the recording to its report; `None` if the logic
/// step could not start (the reason is printed).
fn play_once(
    loaded: &mut Loaded,
    threads: &Threads,
    controls: &Controls,
    clock: QpcClock,
    once: Once,
    mut on_open: impl FnMut(Instant),
) -> Option<PlayReport> {
    let start = loaded.begin_play();
    let keys = &loaded.settings().device;
    let opened = threads.input.open(Request::Open {
        lead_in: once.lead_in,
        logic: thread::current(),
        keyboard: keys.keyboard.recorded(),
        controller: keys.controller.recorded(),
    });
    on_open(Instant::now());
    let origin = opened.origin;
    let plan = loaded.plan(&opened.kinds);
    let (mut logic, render) = match Logic::new(
        loaded.chart(),
        loaded.preset(),
        &plan.device,
        &opened.start,
        opened.readers,
        plan.logic,
        Vec::new(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            threads.input.close();
            eprintln!("the play cannot start: {e}");
            return None;
        }
    };
    let (sink, queue) = audio_link_at(logic.max_events(), origin);
    logic.add_sink(Box::new(sink));
    threads.audio.send(AudioMsg::Play {
        origin,
        queue,
        generation: once.generation,
    });
    threads.render.send(RenderMsg::Play {
        origin,
        side: render,
        gauge: once.gauge,
    });
    let mut live = Vec::with_capacity(logic.max_events());
    let mut output_error = None;
    let now = || TimeUs(clock.now().0 - origin.0);

    let end = loop {
        thread::park_timeout(PARK);
        // A system control, focus loss, suspend or audio failure seen in a
        // step is handled before the step advances (D13).
        if let Some(c) = controls.take() {
            logic.abort(abort_cause(c));
        } else if threads.input.tripped() {
            logic.abort(AbortCause::FocusLoss);
        } else if let Some(e) = threads.audio.failure() {
            output_error = Some(e);
            logic.abort(AbortCause::OutputFailed);
        }
        let advance = logic.advance_to(now());
        live.extend_from_slice(logic.events());
        if let Some(cause) = advance.aborted {
            threads.audio.send(AudioMsg::Cancel);
            break PlayEnd::Aborted(cause);
        }
        if advance.complete {
            break PlayEnd::Completed;
        }
        if loaded.ends_play(logic.gauges()) {
            logic.abort(AbortCause::GaugeFailed);
            threads.audio.send(AudioMsg::Cancel);
            break PlayEnd::Aborted(AbortCause::GaugeFailed);
        }
    };

    // Recording closes at the end of evaluation (D13).
    let closed = threads.input.close();
    let incomplete = Incomplete {
        overflow: logic.overflowed() || closed.overflowed,
        source_limit: closed.source_limited,
        suspend: false,
    };
    let record = logic.record();
    let view = logic.view();
    drop(logic);

    // The audio tail of a completed play: nothing is recorded; a system
    // control, focus loss, removal of the last device or an audio failure
    // cuts it short, and the record stays complete.
    let mut tail_cut = None;
    while end == PlayEnd::Completed && !threads.audio.silent(once.generation) {
        tail_cut = if let Some(c) = controls.take() {
            Some(abort_cause(c))
        } else if threads.input.tripped() {
            Some(AbortCause::FocusLoss)
        } else if !threads.input.connected() {
            Some(AbortCause::LastSourceRemoved)
        } else if let Some(e) = threads.audio.failure() {
            output_error = Some(e);
            Some(AbortCause::OutputFailed)
        } else {
            None
        };
        if tail_cut.is_some() {
            threads.audio.send(AudioMsg::Cancel);
            break;
        }
        thread::sleep(POLL);
    }

    Some(loaded.finish_play(PlayOutcome {
        start,
        end,
        tail_cut,
        output_error,
        live,
        view,
        record,
        kinds: opened.kinds,
        arrivals: closed.arrivals,
        incomplete,
    }))
}

const fn abort_cause(c: Command) -> AbortCause {
    match c {
        Command::Retry => AbortCause::Retry,
        Command::Quit => AbortCause::Quit,
        Command::Suspend => AbortCause::Suspend,
    }
}
