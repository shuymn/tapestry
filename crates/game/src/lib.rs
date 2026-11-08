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

mod adjust_keys;
pub mod adjustment;
pub mod app;
mod attempt;
mod audio_loop;
mod autoplay;
mod bests;
mod driver;
mod error;
mod hispeed;
mod keys;
pub mod live;
mod logic;
mod policy;
mod practice;
mod practice_screen;
mod prep;
mod prepare;
pub mod presets;
mod queue;
pub mod readouts;
mod ready_history;
mod render;
mod restore;
mod result_figures;
mod result_graphs;
pub mod result_screen;
mod results;
mod runner;
mod save;
mod session;
mod settings;
mod sound;
mod start_gate;
mod tempo;
mod tuning;
mod turntable;

pub use adjust_keys::{
    AdjustBinding, AdjustKeyError, AdjustmentConfig, AdjustmentKeys, AdjustmentPlan,
};
pub use app::{App, AppRig, IDLE_WAIT, LoadRefusal, Loading, Pick, Refused};
pub use attempt::{Attempt, AttemptKind, NoteEnd, Preparation, PreparationRefusal, song_total};
pub use audio_loop::{
    AudioControl, AudioCounters, AudioCtl, AudioLoadError, AudioProbe, AudioWaitError, AudioWorker,
    Probed, SWITCH_TIMEOUT, TransportGate, audio_control,
};
pub use autoplay::{Autoplay, AutoplayError, schedule_iter};
pub use bests::{
    BestCompare, BestFigures, BestKey, BestLamp, BestLookup, BestRate, BestScore, BestView,
    SavedPlay,
};
pub use driver::{AdjustDriver, InputPump};
pub use error::{Chain, IoFailure};
pub use eval::TABLE_BYTES_PER_OBJECT;
pub use hispeed::{
    Cover, CoverKind, Covers, GREEN_TARGETS, HiSpeedReference, HiSpeedSettings, MAX_COVER,
    PlayOptions, PlayOptionsError, Ratio, References, SpeedMode, Unrepresentable, duration_ms,
    green, solve, step_hispeed,
};
pub use keys::{
    Bound, CANCEL_KEY, KEEP_KEY, KeyConfigEnd, KeyConfigFlow, KeyConfigs, KeyScreen, LEARN_TIME,
    LearntRange, Press, ROLES, Refusal, SETTLE_TIME, capture_keys, capture_keys_within,
};
pub use live::{
    CLOSE_FLOOR, Closing, ClosingEnd, InputWaitError, Leave, LiveError, Opening, Polled, Rearm,
    Rig, closing_deadline, closing_time,
};
pub use logic::{
    AbortCause, Advance, EventSink, Logic, LogicConfig, LogicError, Record, SourceRead,
};
pub use policy::{
    DefaultPolicy, FailureAction, GaugeIndex, NoSessionEvents, SessionEvent, SessionEventSink,
    SessionPolicy,
};
pub use practice::{
    FADE, LEAD_IN, Practice, PracticeError, RESTORE_AHEAD, RESTORE_RETRY, RING_OUT, Reset, Unfit,
    chart_end, mid_chart_start, reset_notice,
};
pub use practice_screen::{
    LEAD_IN_STEP, MAX_LEAD_IN, MIN_LEAD_IN, PracticeActed, PracticeInput, PracticeRow,
    PracticeScreen, measures,
};
pub use prep::{Changes, PrepHandle, PrepKey, PrepPoll, Readiness, ScreenKey, Stale};
pub use prepare::{
    BmsIngest, ChartIngest, ChartLabels, ChartTransform, IngestError, Ingested, Passthrough,
    TransformId,
};
pub use presets::{
    Entry, POLL_INTERVAL, PollDeadline, PresetChange, PresetStep, Shelf, ShelfError, UnknownPreset,
};
pub use queue::{DEFAULT_QUEUED_PERIODS, QUEUED_PERIODS, QueueChoice, RAISED_QUEUED_PERIODS};
pub use ready_history::{HistoryFull, HistoryRecorder};
pub use render::{PlayView, RenderSide, RenderSink, ViewWriter, render_link};
pub use restore::{Restore, RestoreError, RestoreRefusal, Restored, Restorer};
pub use result_figures::{
    MAX_TIMELINE_BINS, Rate, ResultBasis, ResultFigures, TIMELINE_BIN_US, TIMELINE_CHANNELS,
    TIMING_RANGE_US, TimelineAxis, Timelines, TimingStats, judge_areas, mean_duration, next_rank,
    timing_sample,
};
pub use result_graphs::{
    Axis, DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeTrace, ResultGraphs, TIMING_BIN_US,
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
    AutoKeySounds, ChartSource, ClosingReport, Command, GateStorage, Loaded, MenuCommand, Parts,
    PlayEnd, PlayOutcome, PlayPlan, PlayReport, PlayScript, PlayStart, RECORDING_MEMORY,
    Reservation, SMALL_PARTS_BYTES, SYSTEM_CONTROL_INPUTS, Session, SessionConfig, SessionError,
    SessionPlayError, Settings, VIEW_NOTE_COPIES, resolve_displayed, wake_policy,
};
pub use settings::{
    ControllerDevice, CoverSettings, DEVICE_SETTINGS_VERSION, DataRoot, DeviceSettings,
    DeviceSettingsError, KeyConfig, KeyError, LaneKey, Language, LibrarySettings, NEXT_PRESET_KEY,
    OVERLAY_KEY, PREV_PRESET_KEY, PlayerSettings, PlayerSettingsError, Role, ScratchConfig,
    SettingsError, SettingsFile, SkinChoice, SkinChoices, load_settings, reserved_key,
    save_settings,
};
pub use sound::{AudioQueue, AudioSink, RunnerAudio, Wakes, audio_link, audio_link_at};
pub use tempo::{AttemptTempo, MAIN_HISTOGRAM_LIMIT, ReferenceError};
pub use tuning::{
    Adjust, MAX_VOLUME_PERCENT, OFFSET_LIMIT_US, OFFSET_STEP_US, SPEED_KEYS, Tuning,
    VOLUME_STEP_PERCENT,
};
pub use turntable::{
    AnalogTurn, AutoplayScratch, PlayState, PlayStates, ScratchInput, ScratchNote, ScratchNoteKind,
    Turn, TurntableClock, scratch_notes, state_ms,
};
