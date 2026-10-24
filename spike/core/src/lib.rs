//! Pure logic of the stage 0 spike (issue #3).
//!
//! Everything here is deterministic and free of I/O, FFmpeg and Windows APIs,
//! so it builds and tests on Linux too. Times are integer microseconds
//! relative to the run origin unless a name says otherwise.

pub mod chart;
pub mod evaluate;
pub mod export;
pub mod heavy;
pub mod obslog;
pub mod presentmon;
pub mod rng;
pub mod run;
pub mod scroll;
pub mod stats;
#[doc(hidden)]
pub mod synth;
pub mod time;
