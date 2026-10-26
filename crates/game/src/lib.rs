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

mod audio_loop;
pub mod live;
mod logic;
mod policy;
mod prepare;
mod preset;
mod render;
mod results;
mod runner;
mod save;
mod session;
mod settings;
mod sound;

pub use audio_loop::{
    AudioControl, AudioCtl, AudioWaitError, AudioWorker, SWITCH_TIMEOUT, audio_control,
};
pub use live::{Closing, LiveError, Opening, Rig};
pub use logic::{
    AbortCause, Advance, EventSink, Logic, LogicConfig, LogicError, Record, SourceRead,
};
pub use policy::{
    DefaultPolicy, FailureAction, GaugeIndex, NoSessionEvents, SessionEvent, SessionEventSink,
    SessionPolicy,
};
pub use prepare::{
    BmsIngest, ChartIngest, ChartTransform, IngestError, Ingested, Passthrough, TransformId,
};
pub use preset::{PresetError, load_preset_toml, preset_to_toml};
pub use render::{PlayView, RenderSide, RenderSink};
pub use results::{
    MIN_OFFSET_SAMPLES, ReEvaluate, ReEvaluationError, RecommendedOffset, ResultContext,
    ResultExtension, ResultSummary,
};
pub use runner::{HANDOFF_WAKES, RunOutcome, Runner};
pub use save::{LocalRecords, RecordRef, RecordSink, SAVE_ATTEMPTS, SaveError, write_atomic};
pub use session::{
    Command, Loaded, Parts, PlayEnd, PlayOutcome, PlayPlan, PlayReport, PlayScript, PlayStart,
    ReloadError, Session, SessionConfig, SessionError, Settings,
};
pub use settings::{
    DataRoot, DeviceSettings, KeyConfig, LaneKey, PlayerSettings, SettingsError, load_settings,
    save_settings,
};
pub use sound::{AudioQueue, AudioSink, RunnerAudio, Wakes, audio_link, audio_link_at};
