//! The session: one chart played any number of times, through the extension
//! points (design.md "Play Lifecycle", "Extension Boundaries").
//!
//! Each play runs prepare → countdown → play → evaluation complete → audio
//! tail → save → result. Retry, quit, focus loss, removal of the last
//! device and suspend abort it from any state: it ends at the watermark the
//! evaluator had already finalized, and what was recorded after it stays in
//! the record unevaluated.
//!
//! Each play has its own timeline: its chart time 0 lies `lead_in` after
//! its countdown starts, and recording opens at the countdown. The session
//! clock, which the audio output follows, only moves forward across plays.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ::audio::{AudioOutput, OutputError};
use bms_model::{BgmEvent, Chart, ModelError, TimeSource, TimeUs, VirtualClock};
use eval::{ControlId, Evaluator, GaugeState, InputStart, JudgeEvent, Tally};
use input::hid::is_axis;
use input::{Arrival as InputArrival, DeviceKind, InputAdapter, ScriptedInput, SourceSet};
use replay::{
    Arrival, BuildProvenance, ChartRecord, DeviceRecord, EndStatus, Incomplete, PlayRecord,
    PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};
use rules::{GaugeKind, RulePreset};

use crate::autoplay::{Autoplay, AutoplayError};
use crate::live::{Closing, InputWaitError, LiveError, Opening, Rig};
use crate::logic::{AbortCause, LogicConfig, LogicError, Record};
use crate::policy::{
    DefaultPolicy, FailureAction, GaugeIndex, NoSessionEvents, SessionEvent, SessionEventSink,
    SessionPolicy, SharedEvents,
};
use crate::prepare::{BmsIngest, ChartIngest, ChartTransform, IngestError, Passthrough};
use crate::render::PlayView;
use crate::render::RenderSide;
use crate::results::{ReEvaluate, ResultContext, ResultExtension, ResultSummary};
use crate::save::{LocalRecords, RecordRef, RecordSink, SaveError, write_atomic};
use crate::settings::{DataRoot, DeviceSettings, KeyConfig, PlayerSettings, SettingsError};
use crate::sound::{AudioQueue, RunnerAudio, Wakes};
use rules_toml::{PresetError, load_preset_toml, preset_to_toml};

/// Every extension point the session calls through, each with one
/// implementation in stage 1.
pub struct Parts {
    /// Reads the chart file.
    pub ingest: Box<dyn ChartIngest>,
    /// Transforms the chart before evaluation.
    pub transform: Box<dyn ChartTransform>,
    /// Decides what the evaluator does not.
    pub policy: Box<dyn SessionPolicy>,
    /// Receives screen and play changes.
    pub events: Box<dyn SessionEventSink>,
    /// Summarizes each play.
    pub results: Box<dyn ResultExtension>,
    /// Saves each play's record; `None` for a session that saves nothing
    /// (autoplay), whose records stay in memory for the result.
    pub records: Option<Box<dyn RecordSink>>,
}

impl fmt::Debug for Parts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts")
            .field("transform", &self.transform.id())
            .field("policy", &self.policy.id())
            .finish_non_exhaustive()
    }
}

impl Parts {
    /// The stage 1 parts: BMS ingestion, the pass-through transform, the
    /// default policy showing the player's gauge, no session events,
    /// re-evaluation, and local records under `root`.
    #[must_use]
    pub fn standard(root: &DataRoot, player: &PlayerSettings) -> Self {
        Self {
            records: Some(Box::new(LocalRecords::new(root.records()))),
            ..Self::unsaved(player)
        }
    }

    /// The stage 1 parts without a record sink: nothing a play records is
    /// saved, and each record is still re-evaluated for its result
    /// (design.md "Autoplay").
    #[must_use]
    pub fn unsaved(player: &PlayerSettings) -> Self {
        Self {
            ingest: Box::new(BmsIngest),
            transform: Box::new(Passthrough),
            policy: Box::new(DefaultPolicy {
                displayed: player.displayed_gauge,
            }),
            events: Box::new(NoSessionEvents),
            results: Box::new(ReEvaluate),
            records: None,
        }
    }
}

/// The settings a session runs with.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// `device.toml`.
    pub device: DeviceSettings,
    /// `player.toml`.
    pub player: PlayerSettings,
}

impl Settings {
    /// Reads both files from `root`; a missing file gives its defaults.
    ///
    /// # Errors
    ///
    /// The first file that exists but cannot be read, then
    /// [`SettingsError::InvalidDevice`] or
    /// [`SettingsError::InvalidPlayer`] for a value that cannot be used
    /// ([`DeviceSettings::validate`], [`PlayerSettings::validate`]).
    pub fn load(root: &DataRoot) -> Result<Self, SettingsError> {
        let s = Self {
            device: crate::settings::load_settings(&root.device_settings())?,
            player: crate::settings::load_settings(&root.player_settings())?,
        };
        s.device.validate().map_err(SettingsError::InvalidDevice)?;
        s.player.validate().map_err(SettingsError::InvalidPlayer)?;
        Ok(s)
    }

    /// Writes both files to `root`, each atomically.
    ///
    /// # Errors
    ///
    /// The first file that cannot be written.
    pub fn save(&self, root: &DataRoot) -> Result<(), SettingsError> {
        crate::settings::save_settings(&root.device_settings(), &self.device)?;
        crate::settings::save_settings(&root.player_settings(), &self.player)
    }
}

/// Fixed choices of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    /// How long the countdown runs before chart time 0 (design.md "Play
    /// Lifecycle").
    pub lead_in: TimeUs,
    /// How far the clock moves per step.
    pub step: TimeUs,
    /// Where the session clock starts: the audio output's timeline.
    pub start: TimeUs,
    /// Effects the render queue holds.
    pub effects: usize,
    /// The seed for `#RANDOM` and the chart transform.
    pub seed: u64,
    /// The app's version, as recorded.
    pub app_version: String,
    /// Which kind of build records.
    pub provenance: BuildProvenance,
    /// How the sources' input is received, as recorded.
    pub receive: ReceiveMethod,
    /// Whether the session plays by itself (design.md "Autoplay"), and
    /// how its key sounds reach the mixer: every play is planned for the
    /// generated keyboard, with the built-in key config
    /// ([`KeyConfig::autoplay`]) and an input offset of 0 whatever the
    /// settings say, and a recording window of exactly its events.
    pub autoplay: Option<AutoKeySounds>,
}

/// How an autoplay session's key sounds reach the mixer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoKeySounds {
    /// Scheduled ahead, merged with the BGM, and none sent from judgments:
    /// `play --auto`.
    Scheduled,
    /// Sent from judgments as in a device play, the engine playing the
    /// chart's own BGM: the perfect measurement (design.md "Autoplay").
    #[cfg(feature = "dev-tools")]
    Judged,
}

/// Why a session could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// No ingestion adapter reads the chart file.
    #[error("no adapter reads this chart format")]
    UnsupportedChart(PathBuf),
    /// The configured step does not move the clock forward.
    #[error("a step of {} us does not move the clock", .0.0)]
    InvalidStep(TimeUs),
    /// A file could not be read or written.
    #[error("a file cannot be used: {kind}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The kind of failure.
        kind: io::ErrorKind,
    },
    /// The chart could not be ingested.
    #[error(transparent)]
    Ingest(IngestError),
    /// The preset file could not be used.
    #[error(transparent)]
    Preset(PresetError),
    /// The chart transform's output breaks a chart invariant
    /// ([`Chart::validate`]).
    #[error("the transformed chart is invalid")]
    Transform(#[source] ModelError),
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> SessionError + '_ {
    move |e| SessionError::Io {
        path: path.to_owned(),
        kind: e.kind(),
    }
}

/// A system control, at a time on the play's timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start the chart over.
    Retry,
    /// Leave the chart.
    Quit,
    /// The system suspends.
    Suspend,
}

impl Command {
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) const fn cause(self) -> AbortCause {
        match self {
            Self::Retry => AbortCause::Retry,
            Self::Quit => AbortCause::Quit,
            Self::Suspend => AbortCause::Suspend,
        }
    }
}

/// A command on the result screen, separate from the in-play [`Command`]
/// (design.md "Play Lifecycle").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    /// Play the chart again.
    Retry,
    /// Leave the chart.
    Back,
    /// Show the result under the next preset; the play preset is unchanged.
    NextPreset,
    /// Show the result under the previous preset; the play preset is
    /// unchanged.
    PrevPreset,
}

/// What one play receives: device input and system controls, both on the
/// play's timeline (chart time; the countdown starts at `-lead_in`).
/// Script items before the countdown bootstrap the devices: connections and
/// controls held before recording opens.
#[derive(Debug)]
pub struct PlayScript {
    /// The device input.
    pub input: ScriptedInput,
    /// System controls, in time order.
    pub commands: Vec<(TimeUs, Command)>,
}

/// How a play ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayEnd {
    /// Evaluation completed.
    Completed,
    /// The play was aborted before completing.
    Aborted(AbortCause),
}

/// Why a reload of the preset at a retry was not used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReloadError {
    /// The file could not be read.
    #[error("the preset file cannot be read: {0}")]
    Io(io::ErrorKind),
    /// The file is not a valid preset.
    #[error(transparent)]
    Preset(PresetError),
}

/// What happened in one play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayReport {
    /// The play's number in the session, from 1.
    pub attempt: u32,
    /// How the play ended.
    pub end: PlayEnd,
    /// What ended the audio tail of a completed play early, if anything;
    /// the play stays complete.
    pub tail_cut: Option<AbortCause>,
    /// Why the preset file changed at this retry but was not used; the
    /// previous preset played instead.
    pub reload_error: Option<ReloadError>,
    /// The audio output's failure, if it stopped the play.
    pub output_error: Option<OutputError>,
    /// The input thread's failure, if it ended the play, its recording or
    /// its tail.
    pub input_error: Option<InputWaitError>,
    /// The live event stream.
    pub live: Vec<JudgeEvent>,
    /// The live totals.
    pub tally: Tally,
    /// The gauge the policy shows.
    pub displayed_gauge: GaugeIndex,
    /// The record, unless the play ended before anything was finalized.
    pub record: Option<PlayRecord>,
    /// Where the record was saved, or why it was not; `None` when there is
    /// no record or the session saves nothing.
    pub saved: Option<Result<RecordRef, SaveError>>,
    /// The result extension's summary of a recorded play.
    pub result: Option<ResultSummary>,
    /// The preset the play was evaluated under: after a reload at a
    /// retry, the new one.
    pub preset: RulePreset,
}

/// A loaded chart, its preset and settings, before audio is attached: the
/// sound bank is built from [`Loaded::chart`].
///
/// It also holds what every play of the chart shares, whichever driver
/// runs the plays: [`Session::play`] headlessly, or the game's threads.
/// Each play goes [`begin_play`](Self::begin_play) →
/// [`plan`](Self::plan) once recording has opened → [`admitted`](Self::admitted)
/// once its logic step is built → evaluation →
/// [`finish_play`](Self::finish_play).
#[derive(Debug)]
pub struct Loaded {
    root: DataRoot,
    settings: Settings,
    config: SessionConfig,
    parts: Parts,
    chart: Chart,
    chart_record: ChartRecord,
    warnings: Vec<String>,
    preset: RulePreset,
    preset_path: PathBuf,
    preset_modified: Option<SystemTime>,
    attempt: u32,
    /// Where the session events go, out of the parts so a play can report
    /// its start while its logic step borrows the chart.
    events: SharedEvents,
}

/// The start of one play: its number, and whether the preset file it
/// should have reloaded was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayStart {
    /// The play's number in the session, from 1.
    pub attempt: u32,
    /// Why the preset file changed at this retry but was not used; the
    /// previous preset plays instead.
    pub reload_error: Option<ReloadError>,
}

/// What one play is sized and evaluated with, once its participating
/// sources are known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayPlan {
    /// The device profile, indexed by source id.
    pub device: eval::DeviceProfile,
    /// The logic step's sizes: the recording window's capacity (design.md
    /// "Recording Capacity").
    pub logic: LogicConfig,
}

/// How one play went, as its driver saw it: what
/// [`Loaded::finish_play`] turns into the record, the saved file and the
/// result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayOutcome {
    /// From [`Loaded::begin_play`].
    pub start: PlayStart,
    /// How the play ended.
    pub end: PlayEnd,
    /// What ended the audio tail of a completed play early, if anything.
    pub tail_cut: Option<AbortCause>,
    /// The audio output's failure, if it stopped the play or its tail.
    pub output_error: Option<OutputError>,
    /// The input thread's failure, if it ended the play, its recording or
    /// its tail.
    pub input_error: Option<InputWaitError>,
    /// The live event stream.
    pub live: Vec<JudgeEvent>,
    /// The play view after the last step.
    pub view: PlayView,
    /// What the logic step recorded.
    pub record: Record,
    /// The participating sources' kinds, indexed by source id.
    pub kinds: Vec<DeviceKind>,
    /// Devices that arrived while recording was open.
    pub arrivals: Vec<InputArrival>,
    /// How many axis values each source reported outside the range their
    /// conversion uses, by source id; recorded with the source.
    pub axis_out_of_range: Vec<u64>,
    /// Why the record may be incomplete. Suspend is set by
    /// [`Loaded::finish_play`] from `end`.
    pub incomplete: Incomplete,
}

impl Loaded {
    /// Reads the preset named in `player.toml`, writing the custom preset
    /// there first if the file does not exist, then ingests the chart and
    /// applies the transform.
    ///
    /// # Errors
    ///
    /// [`SessionError`] for a step that is not positive, an unreadable
    /// preset or chart, or a chart no adapter reads.
    pub fn load(
        root: DataRoot,
        settings: Settings,
        chart_path: &Path,
        config: SessionConfig,
        mut parts: Parts,
    ) -> Result<Self, SessionError> {
        let events = SharedEvents::new(std::mem::replace(
            &mut parts.events,
            Box::new(NoSessionEvents),
        ));
        Self::load_reporting_to(root, settings, chart_path, config, parts, events)
    }

    /// [`load`](Self::load), with the session events going to `events`
    /// instead of the parts' sink.
    pub(crate) fn load_reporting_to(
        root: DataRoot,
        settings: Settings,
        chart_path: &Path,
        config: SessionConfig,
        parts: Parts,
        events: SharedEvents,
    ) -> Result<Self, SessionError> {
        if config.step.0 <= 0 {
            return Err(SessionError::InvalidStep(config.step));
        }
        let preset_path = root.preset(&settings.player.preset);
        if !preset_path.exists() {
            if let Some(dir) = preset_path.parent() {
                fs::create_dir_all(dir).map_err(io_error(dir))?;
            }
            let text = preset_to_toml(&rules::custom());
            write_atomic(&preset_path, text.as_bytes()).map_err(io_error(&preset_path))?;
        }
        let (preset, preset_modified) = read_preset(&preset_path).map_err(|e| match e {
            ReloadError::Io(kind) => SessionError::Io {
                path: preset_path.clone(),
                kind,
            },
            ReloadError::Preset(e) => SessionError::Preset(e),
        })?;
        if !parts.ingest.accepts(chart_path) {
            return Err(SessionError::UnsupportedChart(chart_path.to_owned()));
        }
        // The file is untrusted: read one byte past the adapter's limit at
        // most, so a larger file is refused by the adapter without being
        // read whole.
        let limit = u64::try_from(parts.ingest.max_bytes()).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        fs::File::open(chart_path)
            .and_then(|f| f.take(limit.saturating_add(1)).read_to_end(&mut bytes))
            .map_err(io_error(chart_path))?;
        let dir = chart_path.parent().unwrap_or_else(|| Path::new("."));
        let ingested = parts
            .ingest
            .ingest(&bytes, dir, config.seed)
            .map_err(SessionError::Ingest)?;
        let identity = ingested.chart.identity.clone();
        let chart = parts.transform.apply(ingested.chart, config.seed);
        // Rendering culls by position and evaluation walks lanes in time
        // order: a transform must keep both orders (design.md "Chart
        // Model").
        chart.validate().map_err(SessionError::Transform)?;
        let chart_record = ChartRecord {
            identity,
            random_seed: config.seed,
            transform: parts.transform.id().record(),
        };
        events.on_event(&SessionEvent::Load);
        Ok(Self {
            root,
            settings,
            config,
            parts,
            chart,
            chart_record,
            warnings: ingested.warnings,
            preset,
            preset_path,
            preset_modified,
            attempt: 0,
            events,
        })
    }

    /// The chart to be played, after the transform.
    #[must_use]
    pub const fn chart(&self) -> &Chart {
        &self.chart
    }

    /// What the loader skipped or assumed.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The preset plays use now.
    #[must_use]
    pub const fn preset(&self) -> &RulePreset {
        &self.preset
    }

    /// The settings the session runs with.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Reports `ev` to the session event sink.
    pub(crate) fn emit(&self, ev: &SessionEvent) {
        self.events.on_event(ev);
    }

    /// The settings, to change between plays.
    pub(crate) const fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// The session's fixed choices.
    #[must_use]
    pub const fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The data root.
    #[must_use]
    pub const fn root(&self) -> &DataRoot {
        &self.root
    }

    /// Attaches audio and the input side, ready to play. The engine takes
    /// the device settings' volume and audio offset.
    #[must_use]
    pub fn start<O: AudioOutput + Wakes>(
        self,
        mut audio: RunnerAudio<O>,
        sources: SourceSet,
    ) -> Session<O> {
        self.settings.device.apply_to(audio.engine_mut());
        Session {
            clock: VirtualClock::new(self.config.start),
            loaded: self,
            audio,
            sources,
        }
    }

    /// Numbers the next play, before recording opens; its start is reported
    /// once it is [`admitted`](Self::admitted). Every play after the first
    /// is a retry: a preset file changed since the previous play is used
    /// from this one on.
    pub fn begin_play(&mut self) -> PlayStart {
        self.attempt += 1;
        let attempt = self.attempt;
        let reload_error = if attempt > 1 {
            self.reload_preset()
        } else {
            None
        };
        PlayStart {
            attempt,
            reload_error,
        }
    }

    /// The play `start` was admitted: recording opened, the play was
    /// planned and its logic step built. Reports its start.
    pub fn admitted(&self, start: &PlayStart) {
        self.events.on_event(&SessionEvent::Start {
            attempt: start.attempt,
        });
    }

    /// At a retry: uses the preset file from now on if its modification
    /// time changed; a file that fails to load leaves the current preset.
    fn reload_preset(&mut self) -> Option<ReloadError> {
        let modified = fs::metadata(&self.preset_path).and_then(|m| m.modified());
        match modified {
            Ok(m) if Some(m) == self.preset_modified => None,
            _ => match read_preset(&self.preset_path) {
                Ok((preset, modified)) => {
                    self.preset = preset;
                    self.preset_modified = modified;
                    None
                }
                Err(e) => Some(e),
            },
        }
    }

    /// The device profile and the logic step's sizes for a play whose
    /// participating sources have `kinds` and declare `axes`, indexed by source
    /// id. The capacity is the recording window's (design.md "Recording
    /// Capacity"), never cut.
    ///
    /// # Errors
    ///
    /// [`LogicError::RecordingTooLarge`] when the window needs more than
    /// the recording policy reserves ([`Reservation`]): the play is refused
    /// right after recording opens, before it is handed to audio and the
    /// screen, rather than cut off midway.
    ///
    /// An autoplay session ([`SessionConfig::autoplay`]) plans with the
    /// built-in keyboard key config and an input offset of 0, whatever the
    /// settings hold, and its window holds exactly the generated events;
    /// the reservation also counts the session's own buffers
    /// ([`Reservation::with_autoplay`]).
    pub fn plan(
        &self,
        kinds: &[DeviceKind],
        axes: &[Vec<input::hid::AxisCap>],
    ) -> Result<PlayPlan, LogicError> {
        if self.config.autoplay.is_some() {
            return self.autoplay_plan(kinds, axes);
        }
        let device = self.settings.device.profile(kinds, axes);
        // Per source: its recorded lane buttons and keys, and its recorded
        // axes it declares (no other axis is read), played at their rates;
        // and its system controls, a fixed allowance each.
        let mut system = 0_usize;
        let rates: Vec<(usize, usize)> = kinds
            .iter()
            .enumerate()
            .map(|(i, &k)| {
                let keys = self.settings.device.keys(k);
                let declared = axes.get(i).map_or(&[][..], Vec::as_slice);
                let axis = |c: u32| k == DeviceKind::Controller && is_axis(ControlId(c));
                let bound = usize::from(keys.retry.is_some()) + usize::from(keys.quit.is_some());
                system = system.saturating_add(bound.saturating_mul(SYSTEM_CONTROL_INPUTS));
                (
                    keys.lanes.iter().filter(|l| !axis(l.control)).count(),
                    keys.lanes
                        .iter()
                        .filter(|l| axis(l.control))
                        .filter(|l| declared.iter().any(|c| c.control.0 == l.control))
                        .count(),
                )
            })
            .collect();
        // One probe sizes both the window and the reservation; a profile
        // the evaluator refuses is left for the play to report.
        let probe = Evaluator::new(
            &self.chart,
            &self.preset,
            &device,
            &InputStart::default(),
            None,
            0,
        );
        let end = probe.as_ref().map_or(TimeUs(0), Evaluator::final_deadline);
        let inputs =
            window_inputs(&device, end, self.config.lead_in, &rates).saturating_add(system);
        if let Ok(probe) = &probe
            && let r = Reservation::from_probe(probe, &self.chart, &device, self.config.effects)
            && !r.fits(inputs)
        {
            return Err(LogicError::RecordingTooLarge {
                needed: inputs,
                allowed: r.capacity(),
            });
        }
        let logic = LogicConfig {
            inputs,
            effects: self.config.effects,
        };
        Ok(PlayPlan { device, logic })
    }

    /// The device settings an autoplay play is planned with: the loaded
    /// ones with the built-in keyboard key config and an input offset of 0.
    /// The loaded settings, and so `device.toml`, keep their own.
    fn autoplay_device(&self) -> DeviceSettings {
        DeviceSettings {
            keyboard: KeyConfig::autoplay(),
            input_offset_us: 0,
            ..self.settings.device.clone()
        }
    }

    /// [`plan`](Self::plan) for an autoplay session.
    fn autoplay_plan(
        &self,
        kinds: &[DeviceKind],
        axes: &[Vec<input::hid::AxisCap>],
    ) -> Result<PlayPlan, LogicError> {
        let device = self.autoplay_device().profile(kinds, axes);
        let generated = generated();
        let inputs = input::autoplay_count(&self.chart, &generated);
        let schedule = match self.config.autoplay {
            None | Some(AutoKeySounds::Scheduled) => crate::autoplay::schedule_len(&self.chart),
            #[cfg(feature = "dev-tools")]
            Some(AutoKeySounds::Judged) => self.chart.bgm.len(),
        };
        // A profile the evaluator refuses is reported here, before the
        // session makes its stream: autoplay is never admitted unchecked.
        let probe = Evaluator::new(
            &self.chart,
            &self.preset,
            &device,
            &InputStart::default(),
            None,
            0,
        )
        .map_err(LogicError::Eval)?;
        let r = Reservation::from_probe(&probe, &self.chart, &device, self.config.effects)
            .with_autoplay(inputs, schedule);
        if !r.fits(inputs) {
            return Err(LogicError::RecordingTooLarge {
                needed: inputs,
                allowed: r.capacity(),
            });
        }
        let logic = LogicConfig {
            inputs,
            effects: self.config.effects,
        };
        Ok(PlayPlan { device, logic })
    }

    /// What an autoplay session plays (design.md "Autoplay"), made once
    /// before its first play: the generated input of the built-in keyboard
    /// key config with an input offset of 0, and the audio schedule: the
    /// chart's BGM, merged with the key sounds it presses unless they are
    /// judged ([`AutoKeySounds`]). Its plays are planned as
    /// [`plan`](Self::plan) plans an autoplay session's, and the plan is
    /// checked first, so a chart whose recording reservation (autoplay's
    /// own buffers included) is over budget is refused before anything of
    /// it is allocated.
    ///
    /// # Errors
    ///
    /// [`AutoplayError::Plan`] for a chart the evaluator refuses or one over
    /// the budget, and
    /// [`AutoplayError::Time`] for a note too late to press.
    pub fn autoplay(&self) -> Result<Autoplay, AutoplayError> {
        self.autoplay_plan(&[DeviceKind::Keyboard], &[Vec::new()])?;
        let generated = generated();
        let events = input::autoplay(&self.chart, &generated)?;
        Ok(Autoplay {
            events,
            schedule: match self.config.autoplay {
                None | Some(AutoKeySounds::Scheduled) => crate::autoplay::schedule(&self.chart),
                #[cfg(feature = "dev-tools")]
                Some(AutoKeySounds::Judged) => self.chart.bgm.clone(),
            },
            recorded: KeyConfig::autoplay().recorded(),
        })
    }

    /// Whether the session policy ends the play with `gauges` (every
    /// gauge in the preset's order): a survival gauge failed and the policy
    /// does not keep playing.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn ends_play(&self, gauges: &[GaugeState]) -> bool {
        let failed = gauges
            .iter()
            .zip(&self.preset.gauges)
            .any(|(g, rule)| rule.kind == GaugeKind::Survival && g.failed_at.is_some());
        failed && self.parts.policy.on_survival_failure() == FailureAction::EndPlay
    }

    /// Ends a play: builds its record (unless nothing was finalized),
    /// saves it, summarizes it through the result extension, and reports
    /// the session events.
    pub fn finish_play(&mut self, outcome: PlayOutcome) -> PlayReport {
        let PlayOutcome {
            start,
            end,
            tail_cut,
            output_error,
            input_error,
            live,
            view,
            record,
            kinds,
            arrivals,
            axis_out_of_range,
            mut incomplete,
        } = outcome;
        incomplete.suspend |= end == PlayEnd::Aborted(AbortCause::Suspend);
        let PlayStart {
            attempt,
            reload_error,
        } = start;
        let status = match end {
            PlayEnd::Completed => EndStatus::PlayedToEnd,
            PlayEnd::Aborted(_) => EndStatus::EndedMidway,
        };
        let (config, settings, preset) = (&self.config, &self.settings, &self.preset);
        let play_record = record.finalized.map(|finalized| PlayRecord {
            versions: Versions {
                evaluator: preset.implementation.version(),
                app: config.app_version.clone(),
                provenance: config.provenance,
            },
            chart: self.chart_record.clone(),
            preset: PresetRecord::of(preset),
            session_policy: self.parts.policy.id().to_owned(),
            devices: DeviceRecord {
                sources: kinds
                    .iter()
                    .zip(&record.device.sources)
                    .enumerate()
                    .map(|(i, (&kind, profile))| SourceRecord {
                        kind: replay_kind(kind),
                        receive: config.receive,
                        profile: profile.clone(),
                        axis_out_of_range: axis_out_of_range.get(i).copied().unwrap_or(0),
                    })
                    .collect(),
                audio_offset: TimeUs(settings.device.audio_offset_us),
                display_offset: TimeUs(settings.device.display_offset_us),
            },
            play_type: PlayType::Full,
            start: record.start,
            events: record.events,
            arrivals: arrivals.iter().map(arrival).collect(),
            status: Status {
                end: status,
                finalized,
                incomplete,
            },
        });
        let parts = &mut self.parts;
        let saved = parts
            .records
            .as_mut()
            .zip(play_record.as_ref())
            .map(|(sink, r)| sink.save(r));
        let result = play_record.as_ref().map(|r| {
            parts.results.on_play_end(&ResultContext {
                record: r,
                chart: &self.chart,
                preset,
                live: &live,
            })
        });
        let retried =
            end == PlayEnd::Aborted(AbortCause::Retry) || tail_cut == Some(AbortCause::Retry);
        if retried {
            self.events.on_event(&SessionEvent::Retry { attempt });
        }
        self.events.on_event(&SessionEvent::End { attempt, status });
        PlayReport {
            attempt,
            end,
            tail_cut,
            reload_error,
            output_error,
            input_error,
            live,
            tally: view.tally,
            displayed_gauge: parts.policy.displayed_gauge(&view.gauges),
            record: play_record,
            saved,
            result,
            preset: preset.clone(),
        }
    }
}

/// The generated keyboard's profile: the built-in key config and an input
/// offset of 0.
fn generated() -> eval::SourceProfile {
    eval::SourceProfile {
        input_offset: TimeUs(0),
        bindings: KeyConfig::autoplay().bindings(DeviceKind::Keyboard),
        axes: Vec::new(),
    }
}

/// Reads and validates a preset file, with its modification time.
fn read_preset(path: &Path) -> Result<(RulePreset, Option<SystemTime>), ReloadError> {
    // Content and time come from one open file, so a save that replaces
    // the file in between cannot pair old content with the new time.
    let io = |e: io::Error| ReloadError::Io(e.kind());
    let mut file = fs::File::open(path).map_err(io)?;
    let modified = file.metadata().and_then(|m| m.modified()).ok();
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io)?;
    let preset = load_preset_toml(&text).map_err(ReloadError::Preset)?;
    Ok((preset, modified))
}

/// The session: a loaded chart, its audio and input, played any number of
/// times without threads, on a virtual clock.
#[derive(Debug)]
pub struct Session<O> {
    loaded: Loaded,
    audio: RunnerAudio<O>,
    sources: SourceSet,
    clock: VirtualClock,
}

impl<O: AudioOutput + Wakes> Session<O> {
    /// The preset plays use now.
    #[must_use]
    pub const fn preset(&self) -> &RulePreset {
        self.loaded.preset()
    }

    /// Calls `f` with each sound start the engine scheduled and kept (the
    /// engine's schedule log) since the last drain.
    pub fn drain_scheduled(&mut self, f: impl FnMut(&::audio::Scheduled)) {
        self.audio.engine_mut().drain_scheduled(f);
    }

    /// The session clock: the audio output's timeline.
    #[must_use]
    pub fn now(&self) -> TimeUs {
        self.clock.now()
    }

    /// The data root.
    #[must_use]
    pub const fn root(&self) -> &DataRoot {
        self.loaded.root()
    }

    /// The audio side.
    #[must_use]
    pub const fn audio(&self) -> &RunnerAudio<O> {
        &self.audio
    }

    /// Plays the chart once with `script`, from the countdown to the
    /// result, or until a system control, focus loss, removal of the last
    /// device or an audio failure ends it, through the lifecycle every
    /// driver shares ([`crate::live`]). Every play after the first is a
    /// retry: a preset file changed since the previous play is used from
    /// this one on.
    ///
    /// # Errors
    ///
    /// What [`Logic::new`] refuses (for example a key config binding one
    /// control twice); recording is closed again and nothing is played.
    pub fn play(&mut self, script: PlayScript) -> Result<PlayReport, LogicError> {
        let Self {
            loaded,
            audio,
            sources,
            clock,
        } = self;
        let mut rig = SessionRig {
            step: loaded.config.step,
            clock,
            sources,
            audio,
            script,
            next_command: 0,
            origin: None,
            participants: Vec::new(),
            sounds: None,
            screen: None,
            failure: None,
            sealed: false,
        };
        match crate::live::play(loaded, &mut rig) {
            Ok(Some(report)) => Ok(report),
            Ok(None) => unreachable!("a session never quits before recording opens"),
            Err(LiveError::Logic(e)) => Err(e),
            Err(LiveError::Screen(e)) => match e {},
            Err(LiveError::Audio(_)) => {
                unreachable!("a session has no audio thread to fail")
            }
            Err(LiveError::Input(_)) => {
                unreachable!("a session has no input thread to fail")
            }
        }
    }
}

/// The session's rig: one step of the virtual clock per wait, the script
/// fed on the play's timeline, and the audio stepped through every buffer
/// event before the step's logic (a buffer event at the step's own time
/// runs at the next wait, after the logic, as production orders them).
struct SessionRig<'s, O> {
    step: TimeUs,
    clock: &'s mut VirtualClock,
    sources: &'s mut SourceSet,
    audio: &'s mut RunnerAudio<O>,
    script: PlayScript,
    next_command: usize,
    origin: Option<TimeUs>,
    /// Each participant with its connection at the opening.
    participants: Vec<(input::DeviceId, Option<u64>)>,
    sounds: Option<AudioQueue>,
    screen: Option<RenderSide>,
    failure: Option<OutputError>,
    sealed: bool,
}

impl<O: AudioOutput + Wakes> SessionRig<'_, O> {
    /// Every buffer event due before `now` (with `through`, at `now` too),
    /// until the output fails.
    fn step_audio(&mut self, now: TimeUs, through: bool) {
        if let Some(sounds) = &mut self.sounds
            && self.failure.is_none()
            && let Err(e) = self.audio.step(now, through, sounds)
        {
            self.failure = Some(e);
        }
    }
}

impl<O> SessionRig<'_, O> {
    fn play_time(&self) -> Option<TimeUs> {
        self.origin.map(|o| TimeUs(self.clock.now().0 - o.0))
    }
}

impl<O: AudioOutput + Wakes> Rig for SessionRig<'_, O> {
    type Reader = input::SourceReader;
    type ScreenError = std::convert::Infallible;

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(self.step);
        let global = self.clock.now();
        if let Some(now) = self.play_time() {
            self.script.input.poll(now, self.sources);
        }
        if !self.sealed {
            self.step_audio(global, false);
        }
        if let Some(screen) = &mut self.screen {
            screen.drain_effects(|_| {});
        }
    }

    fn stepped(&mut self, aborted: bool) {
        if !aborted {
            self.step_audio(self.clock.now(), true);
        }
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.play_time()?;
        let &(at, c) = self.script.commands.get(self.next_command)?;
        (at <= now).then(|| {
            self.next_command += 1;
            c
        })
    }

    fn quitting(&self) -> bool {
        false
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<Self::Reader>, InputWaitError> {
        // The countdown starts now; chart time 0 is `lead_in` later.
        let origin = TimeUs(self.clock.now().0 + lead_in.0);
        let open_at = TimeUs(-lead_in.0);
        self.script.input.poll(TimeUs(open_at.0 - 1), self.sources);
        self.sources.gate_mut().reset();
        let start = self.sources.open_recording(open_at);
        let sources = &*self.sources;
        // One removed and connected again since counts as removed.
        self.participants = sources
            .participants()
            .map(|(d, _)| (d, sources.connection(d)))
            .collect();
        let kinds = sources.participants().map(|(_, k)| k).collect();
        let axes = sources
            .participants()
            .map(|(d, _)| sources.axes(d).to_vec())
            .collect();
        self.origin = Some(origin);
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds,
            axes,
        })
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        self.audio.engine_mut().restart(origin);
        self.sounds = Some(sounds);
        self.screen = Some(screen);
    }

    fn focus_lost(&self) -> bool {
        self.sources.gate().is_tripped()
    }

    fn connected(&self) -> bool {
        self.participants
            .iter()
            .any(|&(d, c)| c.is_some() && self.sources.connection(d) == c)
    }

    fn audio_failure(&self) -> Option<OutputError> {
        self.failure
    }

    fn screen_failed(&self) -> bool {
        false
    }

    fn cancel_audio(&mut self) {
        if let Some(sounds) = &mut self.sounds {
            self.audio.cancel(sounds);
        }
    }

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn audio_done(&self) -> bool {
        let engine = self.audio.engine();
        self.sealed
            && self.sounds.as_ref().is_none_or(AudioQueue::is_empty)
            && engine.mixer().pending() == 0
            && engine.mixer().sounding() == 0
            && engine.bgm_left() == 0
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        let closing = Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: self.sources.axis_out_of_range().collect(),
        };
        self.sources.close_recording();
        Ok(closing)
    }

    fn ended(&mut self, _report: &PlayReport) {}

    fn show_end(&mut self, _report: &PlayReport) {}
}

const fn replay_kind(kind: DeviceKind) -> replay::DeviceKind {
    match kind {
        DeviceKind::Keyboard => replay::DeviceKind::Keyboard,
        DeviceKind::Controller => replay::DeviceKind::Controller,
    }
}

fn arrival(a: &InputArrival) -> Arrival {
    Arrival {
        at: a.at,
        kind: replay_kind(a.kind),
    }
}

/// The memory one play may reserve for its recording, right after
/// recording opens and before the play is handed to audio and the screen
/// (design.md "Recording Capacity"): what [`Reservation`] counts.
pub const RECORDING_MEMORY: usize = 512 << 20;

/// The bytes the evaluator's own tables may take per chart object (its
/// judgeable notes, their flags and its deadlines), an allowance on top of
/// what [`Reservation`] counts exactly.
pub const TABLE_BYTES_PER_OBJECT: usize = 64;

/// An allowance for the logic step's small fixed parts (its per-source
/// bookkeeping, the play view's buffers and the gauges).
pub const SMALL_PARTS_BYTES: usize = 64 << 10;

/// What a play reserves right after recording opens, before it is handed
/// to audio and the screen, as a function of its input
/// capacity: `fixed + per_input × inputs` bytes (design.md "Recording
/// Capacity").
///
/// It counts what the constructors reserve: [`Logic::new`](crate::Logic::new)
/// sizes the evaluator's input ring, the merge and the record for the
/// input budget (the capacity plus a disconnect per source), each source's
/// staging for the capacity plus one, and its output for
/// [`Evaluator::max_events`] of the budget; a live play adds an audio
/// command queue and a copy of the events, each also of
/// [`Evaluator::max_events`]. The fixed part holds the chart's share of
/// those events (its notes, mines and song end), the render effect queue,
/// and the allowances for tables and small parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// Bytes reserved whatever the capacity.
    pub fixed: usize,
    /// Bytes reserved per input of the capacity.
    pub per_input: usize,
}

impl Reservation {
    /// The reservation of a play of `chart` under `preset` with the
    /// sources of `device` and a render effect queue of `effects`; `None`
    /// when the evaluator refuses them, which the play then reports.
    #[must_use]
    pub fn of(
        chart: &Chart,
        preset: &RulePreset,
        device: &eval::DeviceProfile,
        effects: usize,
    ) -> Option<Self> {
        let probe = Evaluator::new(chart, preset, device, &InputStart::default(), None, 0).ok()?;
        Some(Self::from_probe(&probe, chart, device, effects))
    }

    /// [`of`](Self::of), with an evaluator built for the play with no
    /// input capacity.
    #[must_use]
    pub fn from_probe(
        probe: &Evaluator<'_>,
        chart: &Chart,
        device: &eval::DeviceProfile,
        effects: usize,
    ) -> Self {
        let sources = device.sources.len();
        let event = 2 * size_of::<JudgeEvent>() + size_of::<::audio::SoundCmd>();
        let input = size_of::<eval::PhysicalInputEvent>();
        let slot = size_of::<(TimeUs, eval::PhysicalInputEvent)>();
        let total = |inputs: usize| {
            let budget = inputs + sources;
            probe.max_events(budget) * event
                + sources * (inputs + 1) * input
                + 2 * budget * input
                + budget * slot
        };
        let objects: usize = chart.lanes.iter().map(Vec::len).sum();
        let empty = total(0);
        Self {
            fixed: empty
                + effects * size_of::<JudgeEvent>()
                + objects * TABLE_BYTES_PER_OBJECT
                + SMALL_PARTS_BYTES,
            per_input: total(1) - empty,
        }
    }

    /// The bytes reserved for a capacity of `inputs`.
    #[must_use]
    pub const fn bytes(&self, inputs: usize) -> usize {
        self.fixed
            .saturating_add(self.per_input.saturating_mul(inputs))
    }

    /// This reservation with an autoplay session's own buffers added to
    /// its fixed part (design.md "Autoplay"): the source queue holding the
    /// whole stream of `events`, the generated events kept for every play,
    /// and the merged audio schedule of `schedule` entries. All three are
    /// allocated at once while the session starts, so this is also their
    /// peak.
    #[must_use]
    pub const fn with_autoplay(self, events: usize, schedule: usize) -> Self {
        let event = size_of::<eval::PhysicalInputEvent>();
        let own = events
            .saturating_mul(2 * event)
            .saturating_add(schedule.saturating_mul(size_of::<BgmEvent>()));
        Self {
            fixed: self.fixed.saturating_add(own),
            per_input: self.per_input,
        }
    }

    /// Whether a capacity of `inputs` fits in [`RECORDING_MEMORY`], fixed
    /// costs included: a chart whose fixed part alone exceeds it fits no
    /// capacity, not even 0.
    #[must_use]
    pub const fn fits(&self, inputs: usize) -> bool {
        self.bytes(inputs) <= RECORDING_MEMORY
    }

    /// The largest capacity within [`RECORDING_MEMORY`]; 0 also when not
    /// even 0 fits ([`fits`](Self::fits)).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        RECORDING_MEMORY.saturating_sub(self.fixed)
            / if self.per_input == 0 {
                1
            } else {
                self.per_input
            }
    }
}

/// The inputs each bound system control (retry, quit) of a source gets in the
/// recording window, whatever its length: a press and a release. Its first
/// press ends the play, so it is not played at a rate; a key held across a
/// retry and auto-repeating, which the window takes no further press from, may
/// use up the window, which ends the play at its last finalized watermark with
/// the log incomplete (design.md "Recording Capacity").
pub const SYSTEM_CONTROL_INPUTS: usize = 2;

/// The recording window's input capacity (design.md "Recording Capacity",
/// [`eval::window_inputs`]): each source's lane-bound buttons pressed and
/// released 30 times a second each, up to 240 edges a second together, and its
/// lane-bound axes it declares reporting 1000 times a second each, from the
/// countdown to the chart's final deadline plus the largest input offset, in
/// whole seconds, for sources recording `(buttons, axes)` each. The caller adds
/// [`SYSTEM_CONTROL_INPUTS`] per bound system control.
fn window_inputs(
    device: &eval::DeviceProfile,
    end: TimeUs,
    lead_in: TimeUs,
    sources: &[(usize, usize)],
) -> usize {
    let offset = device
        .sources
        .iter()
        .map(|s| s.input_offset.0)
        .max()
        .unwrap_or(0)
        .max(0);
    let span = lead_in
        .0
        .saturating_add(end.0.max(0))
        .saturating_add(offset);
    eval::window_inputs(TimeUs(span), sources)
}
