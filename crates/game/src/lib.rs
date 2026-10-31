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
mod bests;
mod error;
mod keys;
pub mod live;
mod logic;
mod policy;
mod practice;
mod practice_screen;
mod prepare;
pub mod presets;
mod queue;
mod render;
mod restore;
mod result_graphs;
pub mod result_screen;
mod results;
mod runner;
mod save;
mod session;
mod settings;
mod sound;
mod tuning;

pub use app::{App, AppRig, IDLE_WAIT, LoadRefusal, Pick, Refused};
pub use audio_loop::{
    AudioControl, AudioCounters, AudioCtl, AudioLoadError, AudioProbe, AudioWaitError, AudioWorker,
    Probed, SWITCH_TIMEOUT, audio_control,
};
pub use autoplay::{Autoplay, AutoplayError, schedule_iter};
pub use bests::{BestScore, BestView, SavedPlay};
pub use error::{Chain, IoFailure};
pub use eval::TABLE_BYTES_PER_OBJECT;
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
pub use practice::{
    FADE, LEAD_IN, Practice, PracticeError, RESTORE_AHEAD, RESTORE_RETRY, RING_OUT, chart_end,
    mid_chart_start,
};
pub use practice_screen::{
    LEAD_IN_STEP, MAX_LEAD_IN, MIN_LEAD_IN, PracticeActed, PracticeInput, PracticeRow,
    PracticeScreen, measures,
};
pub use prepare::{
    BmsIngest, ChartIngest, ChartTransform, IngestError, Ingested, Passthrough, TransformId,
};
pub use presets::{
    Entry, POLL_INTERVAL, PollDeadline, PresetChange, Shelf, ShelfError, UnknownPreset,
};
pub use queue::{DEFAULT_QUEUED_PERIODS, QUEUED_PERIODS, QueueChoice, RAISED_QUEUED_PERIODS};
pub use render::{PlayView, RenderSide, RenderSink};
pub use restore::{Restore, RestoreError, RestoreRefusal, Restored, Restorer};
pub use result_graphs::{
    Axis, GRAPH_COLUMNS, GaugeColumn, GaugeMark, GaugeTrace, ResultGraphs, TIMING_BIN_US,
    TIMING_BINS, Timing, condense, listed,
};
pub use result_screen::{
    EntryId, GaugeEnd, NOTICES, Notice, RESULT_CACHE, ResultScreen, SHOWN_GAUGES, SUMMARY_BYTES,
    ViewEntry, ViewStatus, ViewSummary, what_if_bytes,
};
pub use results::{
    MIN_OFFSET_SAMPLES, ReEvaluate, ReEvaluationError, RecommendedOffset, ResultContext,
    ResultExtension, ResultSummary, press_deviations,
};
pub use rules_toml::{
    NameError, PresetError, PresetFileCause, PresetFileError, check_user_preset_name,
    load_preset_toml, preset_to_toml,
};
pub use runner::{HANDOFF_WAKES, RunOutcome, Runner};
pub use save::{LocalRecords, RecordRef, RecordSink, SAVE_ATTEMPTS, SaveError, write_atomic};
pub use session::{
    AutoKeySounds, Command, Loaded, MenuCommand, Parts, PlayEnd, PlayOutcome, PlayPlan, PlayReport,
    PlayScript, PlayStart, RECORDING_MEMORY, Reservation, SMALL_PARTS_BYTES, SYSTEM_CONTROL_INPUTS,
    Session, SessionConfig, SessionError, SessionPlayError, Settings, wake_policy,
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
