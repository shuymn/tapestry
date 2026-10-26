//! The audio thread loop (stage1.md D8): one engine step per buffer event
//! until asked to stop.

use std::sync::atomic::{AtomicBool, Ordering};

use rtrb::Consumer;

use crate::engine::{AudioEngine, SoundCmd};
use crate::output::{AudioOutput, OutputError};

/// What a run of the audio thread loop did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Engine steps taken (buffer events served).
    pub steps: u64,
    /// Frames written.
    pub frames: u64,
}

/// Runs `engine` into `output` until `stop` is set, taking sound commands
/// from `cmds`. `stop` is read before each step, so a stop requested while
/// the output waits for its buffer event takes effect when that wait
/// returns. Allocates nothing.
///
/// # Errors
///
/// The output's failure, which ends the loop.
pub fn run_audio(
    output: &mut impl AudioOutput,
    engine: &mut AudioEngine,
    cmds: &mut Consumer<SoundCmd>,
    stop: &AtomicBool,
) -> Result<RunReport, OutputError> {
    let mut report = RunReport::default();
    while !stop.load(Ordering::Acquire) {
        let step = engine.step(output, cmds)?;
        report.steps += 1;
        report.frames += u64::from(step.frames);
    }
    Ok(report)
}
