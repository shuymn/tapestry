//! Game core.
//!
//! [`Logic`] is the logic thread's work as a function that advances to a given
//! time: it reads every source's watermark, pops what each guarantees, merges
//! through the global watermark and advances the evaluator there (design.md
//! "Watermark"). Its events go to [`EventSink`]s: [`RenderSink`] queues them as
//! effects for rendering, [`AudioSink`] queues key sounds for the audio engine,
//! and a [`PlayView`] snapshot goes through a triple buffer. [`Runner`] drives
//! the whole system without threads, in the order production does: scripted
//! input, publication, logic, audio, render.
//!
//! The crate is portable: no platform code, and the logic step allocates
//! nothing after construction.

#![forbid(unsafe_code)]

pub mod app;
mod audio_loop;
mod autoplay;
mod error;
mod keys;
pub mod live;
mod logic;
mod policy;
mod prepare;
mod queue;
mod render;
mod results;
mod runner;
mod save;
mod session;
mod settings;
mod sound;
mod tuning;

pub use app::{App, AppRig, IDLE_WAIT, LoadRefusal, Refused};
pub use audio_loop::{
    AudioControl, AudioCounters, AudioCtl, AudioProbe, AudioWaitError, AudioWorker, Probed,
    SWITCH_TIMEOUT, audio_control,
};
pub use autoplay::{Autoplay, AutoplayError};
pub use error::{Chain, IoFailure};
pub use keys::{
    Bound, CANCEL_KEY, KEEP_KEY, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyScreen, LEARN_TIME,
    LearntRange, Press, ROLES, Refusal, capture_keys, capture_keys_within,
};
pub use live::{Closing, InputWaitError, Leave, LiveError, Opening, Rig};
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
pub use queue::{DEFAULT_QUEUED_PERIODS, QUEUED_PERIODS, QueueChoice, RAISED_QUEUED_PERIODS};
pub use render::{PlayView, RenderSide, RenderSink};
pub use results::{
    MIN_OFFSET_SAMPLES, ReEvaluate, ReEvaluationError, RecommendedOffset, ResultContext,
    ResultExtension, ResultSummary,
};
pub use rules_toml::{PresetError, load_preset_toml, preset_to_toml};
pub use runner::{HANDOFF_WAKES, RunOutcome, Runner};
pub use save::{LocalRecords, RecordRef, RecordSink, SAVE_ATTEMPTS, SaveError, write_atomic};
pub use session::{
    AutoKeySounds, Command, Loaded, MenuCommand, Parts, PlayEnd, PlayOutcome, PlayPlan, PlayReport,
    PlayScript, PlayStart, RECORDING_MEMORY, ReloadError, Reservation, SMALL_PARTS_BYTES,
    SYSTEM_CONTROL_INPUTS, Session, SessionConfig, SessionError, Settings, TABLE_BYTES_PER_OBJECT,
};
pub use settings::{
    ControllerDevice, DataRoot, DeviceSettings, DeviceSettingsError, KeyConfig, KeyError, LaneKey,
    LibrarySettings, OVERLAY_KEY, PlayerSettings, PlayerSettingsError, Role, ScratchConfig,
    SettingsError, load_settings, reserved_key, save_settings,
};
pub use sound::{AudioQueue, AudioSink, RunnerAudio, Wakes, audio_link, audio_link_at};
pub use tuning::{
    Adjust, DEFAULT_VISIBLE_TIME_MS, MAX_VOLUME_PERCENT, OFFSET_LIMIT_US, OFFSET_STEP_US, Tuning,
    VISIBLE_TIME_MS, VISIBLE_TIME_STEPS_MS, VOLUME_STEP_PERCENT,
};
