//! Game core.
//!
//! [`Logic`] is the logic thread's work as a function that advances to a
//! given time: it reads every source's watermark, pops what each guarantees,
//! merges through the global watermark and advances the evaluator there
//! (D8). Its events go to [`EventSink`]s: [`RenderSink`] queues them as
//! effects for rendering, [`AudioSink`] queues key sounds for the audio
//! engine, and a [`PlayView`] snapshot goes through a triple
//! buffer. [`Runner`] drives the whole system without threads, in the order
//! production does: scripted input, publication, logic, audio, render.
//!
//! The crate is portable: no platform code, and the logic step allocates
//! nothing after construction.

#![forbid(unsafe_code)]

mod logic;
mod render;
mod runner;
mod sound;

pub use logic::{
    AbortCause, Advance, EventSink, Logic, LogicConfig, LogicError, Record, SourceRead,
};
pub use render::{PlayView, RenderSide, RenderSink};
pub use runner::{RunOutcome, Runner};
pub use sound::{AudioQueue, AudioSink, RunnerAudio, Wakes, audio_link};
