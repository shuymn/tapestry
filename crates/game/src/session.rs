//! The session: one chart played any number of times, through the
//! extension points (stage1.md D13; design.md "Extension Boundaries").
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
use bms_model::{Chart, TimeSource, TimeUs, VirtualClock};
use eval::{Evaluator, InputStart, JudgeEvent, Tally};
use input::{Arrival as InputArrival, DeviceKind, InputAdapter, ScriptedInput, SourceSet};
use replay::{
    Arrival, BuildProvenance, ChartRecord, DeviceRecord, EndStatus, Incomplete, PlayRecord,
    PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};
use rules::{GaugeKind, RulePreset};

use crate::logic::{AbortCause, Logic, LogicConfig, LogicError, Record};
use crate::policy::{
    DefaultPolicy, FailureAction, GaugeIndex, NoSessionEvents, SessionEvent, SessionEventSink,
    SessionPolicy,
};
use crate::prepare::{BmsIngest, ChartIngest, ChartTransform, IngestError, Passthrough};
use crate::preset::{PresetError, load_preset_toml, preset_to_toml};
use crate::render::PlayView;
use crate::results::{ReEvaluate, ResultContext, ResultExtension, ResultSummary};
use crate::save::{LocalRecords, RecordRef, RecordSink, SaveError, write_atomic};
use crate::settings::{DataRoot, DeviceSettings, PlayerSettings, SettingsError};
use crate::sound::{RunnerAudio, Wakes, audio_link_at};

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
    /// Saves each play's record.
    pub records: Box<dyn RecordSink>,
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
            ingest: Box::new(BmsIngest),
            transform: Box::new(Passthrough),
            policy: Box::new(DefaultPolicy {
                displayed: player.displayed_gauge,
            }),
            events: Box::new(NoSessionEvents),
            results: Box::new(ReEvaluate),
            records: Box::new(LocalRecords::new(root.records())),
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
    /// The first file that exists but cannot be read.
    pub fn load(root: &DataRoot) -> Result<Self, SettingsError> {
        Ok(Self {
            device: crate::settings::load_settings(&root.device_settings())?,
            player: crate::settings::load_settings(&root.player_settings())?,
        })
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
    /// How long the countdown runs before chart time 0 (D13).
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
}

/// Why a session could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// No ingestion adapter reads the chart file.
    UnsupportedChart(PathBuf),
    /// The configured step does not move the clock forward.
    InvalidStep(TimeUs),
    /// A file could not be read or written.
    Io {
        /// The file.
        path: PathBuf,
        /// The kind of failure.
        kind: io::ErrorKind,
    },
    /// The chart could not be ingested.
    Ingest(IngestError),
    /// The preset file could not be used.
    Preset(PresetError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedChart(_) => f.write_str("no adapter reads this chart format"),
            Self::InvalidStep(step) => write!(f, "a step of {} us does not move the clock", step.0),
            Self::Io { kind, .. } => write!(f, "a file cannot be used: {kind}"),
            Self::Ingest(e) => e.fmt(f),
            Self::Preset(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SessionError {}

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
    const fn cause(self) -> AbortCause {
        match self {
            Self::Retry => AbortCause::Retry,
            Self::Quit => AbortCause::Quit,
            Self::Suspend => AbortCause::Suspend,
        }
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadError {
    /// The file could not be read.
    Io(io::ErrorKind),
    /// The file is not a valid preset.
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
    /// The live event stream.
    pub live: Vec<JudgeEvent>,
    /// The live totals.
    pub tally: Tally,
    /// The gauge the policy shows.
    pub displayed_gauge: GaugeIndex,
    /// The record, unless the play ended before anything was finalized.
    pub record: Option<PlayRecord>,
    /// Where the record was saved, or why it was not.
    pub saved: Option<Result<RecordRef, SaveError>>,
    /// The result extension's summary of a recorded play.
    pub result: Option<ResultSummary>,
}

/// A loaded chart, its preset and settings, before audio is attached: the
/// sound bank is built from [`Loaded::chart`].
///
/// It also holds what every play of the chart shares, whichever driver
/// runs the plays: [`Session::play`] headlessly, or the game's threads.
/// Each play goes [`begin_play`](Self::begin_play) →
/// [`plan`](Self::plan) once recording has opened → evaluation →
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
    /// The logic step's sizes: the recording window's capacity (D8).
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
        let chart_record = ChartRecord {
            identity,
            random_seed: config.seed,
            transform: parts.transform.id().record(),
        };
        parts.events.on_event(&SessionEvent::Load);
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

    /// Starts the next play. Every play after the first is a retry: a
    /// preset file changed since the previous play is used from this one
    /// on.
    pub fn begin_play(&mut self) -> PlayStart {
        self.attempt += 1;
        let attempt = self.attempt;
        let reload_error = if attempt > 1 {
            self.reload_preset()
        } else {
            None
        };
        self.parts.events.on_event(&SessionEvent::Start { attempt });
        PlayStart {
            attempt,
            reload_error,
        }
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
    /// participating sources have `kinds`, indexed by source id.
    #[must_use]
    pub fn plan(&self, kinds: &[DeviceKind]) -> PlayPlan {
        let device = self.settings.device.profile(kinds);
        let buttons: usize = kinds
            .iter()
            .map(|&k| self.settings.device.keys(k).recorded().len())
            .sum();
        let logic = LogicConfig {
            inputs: window_inputs(
                &self.chart,
                &self.preset,
                &device,
                self.config.lead_in,
                buttons,
            ),
            effects: self.config.effects,
        };
        PlayPlan { device, logic }
    }

    /// Whether the session policy ends the play at `view`: a survival
    /// gauge failed and the policy does not keep playing.
    #[must_use]
    pub fn ends_play(&self, view: &PlayView) -> bool {
        let failed = view
            .gauges
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
            live,
            view,
            record,
            kinds,
            arrivals,
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
                    .map(|(&kind, profile)| SourceRecord {
                        kind: replay_kind(kind),
                        receive: config.receive,
                        profile: profile.clone(),
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
        let saved = play_record.as_ref().map(|r| parts.records.save(r));
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
            parts.events.on_event(&SessionEvent::Retry { attempt });
        }
        parts
            .events
            .on_event(&SessionEvent::End { attempt, status });
        PlayReport {
            attempt,
            end,
            tail_cut,
            reload_error,
            output_error,
            live,
            tally: view.tally,
            displayed_gauge: parts.policy.displayed_gauge(&view.gauges),
            record: play_record,
            saved,
            result,
        }
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
    /// device or an audio failure ends it. Every play after the first is a
    /// retry: a preset file changed since the previous play is used from
    /// this one on.
    ///
    /// # Errors
    ///
    /// What [`Logic::new`] refuses (for example a key config binding one
    /// control twice); recording is closed again and nothing is played.
    #[allow(clippy::too_many_lines)]
    pub fn play(&mut self, mut script: PlayScript) -> Result<PlayReport, LogicError> {
        let start = self.loaded.begin_play();
        let Self {
            loaded,
            audio,
            sources,
            clock,
        } = self;
        let lead_in = loaded.config.lead_in;
        let step = loaded.config.step;

        // The countdown starts now; chart time 0 is `lead_in` later.
        let origin = TimeUs(clock.now().0 + lead_in.0);
        let open_at = TimeUs(-lead_in.0);
        script.input.poll(TimeUs(open_at.0 - 1), sources);
        sources.gate_mut().reset();
        let input_start = sources.open_recording(open_at);
        let kinds: Vec<DeviceKind> = sources.participants().map(|(_, k)| k).collect();
        // Each participant with its connection at the opening: one removed
        // and connected again since counts as removed in the tail.
        let participants: Vec<_> = sources
            .participants()
            .map(|(d, _)| (d, sources.connection(d)))
            .collect();
        let plan = loaded.plan(&kinds);
        let readers = sources.take_readers();
        let (mut logic, mut render) = match Logic::new(
            &loaded.chart,
            &loaded.preset,
            &plan.device,
            &input_start,
            readers,
            plan.logic,
            Vec::new(),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                sources.close_recording();
                return Err(e);
            }
        };
        let (sink, mut sounds) = audio_link_at(logic.max_events(), origin);
        logic.add_sink(Box::new(sink));
        audio.engine_mut().restart(origin);
        let mut live = Vec::with_capacity(logic.max_events());
        let mut commands = script.commands.iter().copied().peekable();
        let mut output_error = None;
        let mut due = |now: TimeUs| commands.next_if(|&(at, _)| at <= now).map(|(_, c)| c);

        let end = loop {
            clock.advance(step);
            let global = clock.now();
            let now = TimeUs(global.0 - origin.0);
            script.input.poll(now, sources);
            if let Some(c) = due(now) {
                logic.abort(c.cause());
            } else if sources.gate().is_tripped() {
                logic.abort(AbortCause::FocusLoss);
            } else if let Err(e) = audio.step(global, false, &mut sounds) {
                output_error = Some(e);
                logic.abort(AbortCause::OutputFailed);
            }
            let advance = logic.advance_to(now);
            live.extend_from_slice(logic.events());
            let failure = match advance.aborted {
                Some(_) => None,
                None => audio.step(global, true, &mut sounds).err(),
            };
            let failed = failure.is_some();
            output_error = failure.or(output_error);
            if advance.aborted.is_some() || failed {
                audio.cancel(&mut sounds);
            }
            render.drain_effects(|_| {});
            if let Some(cause) = advance.aborted {
                break PlayEnd::Aborted(cause);
            }
            // Completion is final: an audio failure or a gauge failure in
            // the same advance does not turn a completed play into an
            // aborted one; the failure only ends the tail, which is then
            // not played.
            if advance.complete {
                break PlayEnd::Completed;
            }
            if failed {
                logic.abort(AbortCause::OutputFailed);
                break PlayEnd::Aborted(AbortCause::OutputFailed);
            }
            if loaded.ends_play(render.view()) {
                logic.abort(AbortCause::GaugeFailed);
                audio.cancel(&mut sounds);
                break PlayEnd::Aborted(AbortCause::GaugeFailed);
            }
        };

        // Recording closes at the end of evaluation (D13): the input side
        // appends nothing more, and the record is frozen here.
        let arrivals = sources.arrivals().to_vec();
        let incomplete = Incomplete {
            overflow: logic.overflowed() || sources.overflowed(),
            source_limit: sources.source_limited(),
            suspend: false,
        };
        sources.close_recording();
        let record = logic.record();
        let view = render.view().clone();
        drop(logic);

        // The audio tail of a completed play: input still reaches the
        // devices' state, but nothing is recorded. A system control, focus
        // loss, removal of the last participating device or an audio
        // failure cuts it short; the record stays complete.
        let mut tail_cut = None;
        if end == PlayEnd::Completed && output_error.is_some() {
            tail_cut = Some(AbortCause::OutputFailed);
        }
        while end == PlayEnd::Completed && tail_cut.is_none() {
            let silent = sounds.is_empty()
                && audio.engine().mixer().pending() == 0
                && audio.engine().mixer().sounding() == 0
                && audio.engine().bgm_left() == 0;
            if silent {
                break;
            }
            clock.advance(step);
            let global = clock.now();
            let now = TimeUs(global.0 - origin.0);
            script.input.poll(now, sources);
            tail_cut = if let Some(c) = due(now) {
                Some(c.cause())
            } else if sources.gate().is_tripped() {
                Some(AbortCause::FocusLoss)
            } else if !participants
                .iter()
                .any(|&(d, c)| c.is_some() && sources.connection(d) == c)
            {
                Some(AbortCause::LastSourceRemoved)
            } else if let Err(e) = audio.step(global, true, &mut sounds) {
                output_error = Some(e);
                Some(AbortCause::OutputFailed)
            } else {
                None
            };
            if tail_cut.is_some() {
                audio.cancel(&mut sounds);
            }
        }

        Ok(loaded.finish_play(PlayOutcome {
            start,
            end,
            tail_cut,
            output_error,
            live,
            view,
            record,
            kinds,
            arrivals,
            incomplete,
        }))
    }
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

/// The recording window's input capacity (D8): every recorded control
/// pressed and released 30 times a second, from the countdown to the
/// chart's final deadline plus the largest input offset, in whole seconds.
fn window_inputs(
    chart: &Chart,
    preset: &RulePreset,
    device: &eval::DeviceProfile,
    lead_in: TimeUs,
    buttons: usize,
) -> usize {
    let end = Evaluator::new(chart, preset, device, &InputStart::default(), 0)
        .map_or(TimeUs(0), |e| e.final_deadline());
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
    let secs = u64::try_from(span).unwrap_or(0).div_ceil(1_000_000);
    buttons
        .saturating_mul(60)
        .saturating_mul(usize::try_from(secs).unwrap_or(usize::MAX))
}
