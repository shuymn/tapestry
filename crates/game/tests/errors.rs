//! The errors' source chains: each wrapper's message leaves its cause out,
//! `source()` returns the cause, a transparent wrapper forwards both, and
//! [`Chain`] prints a chain as anyhow's `{:#}` does.

use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use bms_model::{ModelError, TimeUs};
use bms_parser::LoadError;
use eval::EvalError;
use game::{
    AudioWaitError, Chain, DataRoot, DeviceSettingsError, IngestError, IoFailure, KeyError,
    LiveError, Loaded, LogicError, NameError, Parts, PlayerSettings, PlayerSettingsError,
    PresetError, PresetFileCause, PresetFileError, ReEvaluationError, Role, SaveError,
    SessionConfig, SessionError, Settings, SettingsError, ShelfError, UnknownPreset,
    load_preset_toml, load_settings,
};
use replay::{BuildProvenance, ReceiveMethod};
use rules::RuleError;

/// `e`'s source as a `T`, which must be what `source()` returns.
#[track_caller]
fn source<'a, T: Error + 'static>(e: &'a (dyn Error + 'static)) -> &'a T {
    let source = e.source().expect("a source");
    source.downcast_ref::<T>().unwrap_or_else(|| {
        panic!(
            "the source {source:?} is not a {}",
            std::any::type_name::<T>()
        )
    })
}

/// The messages of `e` and its sources, in order: where the chain ends.
fn messages(e: &(dyn Error + 'static)) -> Vec<String> {
    let mut out = vec![e.to_string()];
    let mut cause = e.source();
    while let Some(c) = cause {
        out.push(c.to_string());
        cause = c.source();
    }
    out
}

/// The source is `expected`, the chain is `chain`, and no message repeats
/// its cause.
#[track_caller]
fn assert_chain<T: Error + PartialEq + Debug + 'static>(
    e: &(dyn Error + 'static),
    expected: &T,
    chain: &[&str],
) {
    assert_eq!(source::<T>(e), expected);
    let messages = messages(e);
    assert_eq!(messages, chain);
    for pair in messages.windows(2) {
        if let [outer, inner] = pair {
            assert!(!outer.contains(inner), "{outer:?} repeats {inner:?}");
        }
    }
}

#[test]
fn ingest_load_and_model_errors_chain_to_the_model() {
    let load = LoadError::Model(ModelError::Overflow);
    let e = IngestError::Load(load.clone());
    assert_chain(
        &e,
        &load,
        &[
            "the chart cannot be loaded",
            "resolved chart is invalid",
            "time overflowed i64",
        ],
    );
    assert_eq!(source::<ModelError>(&load), &ModelError::Overflow);
}

#[test]
fn settings_errors_chain_to_their_causes() {
    let overlap = KeyError::Overlap {
        control: 0x2c,
        first: Role::Lane(1),
        second: Role::Lane(2),
    };
    let keys = DeviceSettingsError::KeyboardKeys(overlap);
    let e = SettingsError::InvalidDevice(keys.clone());
    assert_chain(
        &e,
        &keys,
        &[
            "the settings cannot be used: device.toml",
            "keyboard key config",
            "control 0x2c is bound to both key 1 and key 2",
        ],
    );
    assert_eq!(source::<KeyError>(&keys), &overlap);
    let controller = DeviceSettingsError::ControllerKeys(KeyError::NoSuchLane(9));
    assert_chain(
        &controller,
        &KeyError::NoSuchLane(9),
        &[
            "controller key config",
            "lane 9 does not exist (lanes are 0 to 7)",
        ],
    );
    let visible = PlayerSettingsError::VisibleTime(1);
    let e = SettingsError::InvalidPlayer(visible.clone());
    assert_chain(
        &e,
        &visible,
        &[
            "the settings cannot be used: player.toml",
            "visible_time_ms 1 is outside 100 to 4000",
        ],
    );
    let unknown = UnknownPreset("custom.toml".to_owned());
    let e = SettingsError::InvalidPlayer(PlayerSettingsError::Preset(unknown.clone()));
    assert_chain(
        &e,
        &PlayerSettingsError::Preset(unknown),
        &[
            "the settings cannot be used: player.toml",
            "`custom.toml` is neither a built-in preset (tapestry, beatoraja-style, lr2-style) \
             nor a user preset in the presets folder",
        ],
    );
    // The leaves of `validate`.
    for leaf in [
        DeviceSettingsError::ControllerUnnamed,
        DeviceSettingsError::Offset {
            name: "input_offset_us",
            value: 2_000_000,
        },
        DeviceSettingsError::MasterVolume {
            bits: 2.0_f32.to_bits(),
        },
        DeviceSettingsError::QueuedPeriods(9),
    ] {
        assert!(leaf.source().is_none(), "{leaf:?}");
    }
    assert_eq!(
        DeviceSettingsError::MasterVolume {
            bits: 1.5_f32.to_bits()
        }
        .to_string(),
        "master_volume 1.5 is outside 0.0 to 1.0"
    );
}

#[test]
fn io_failures_keep_the_system_message_as_the_source() {
    let os = || io::Error::new(io::ErrorKind::PermissionDenied, "access is denied");
    let failure = IoFailure::from(os());
    assert_eq!(failure.kind, io::ErrorKind::PermissionDenied);
    assert_eq!(failure.message(), "access is denied");
    assert!(failure.source().is_none());
    assert_chain(
        &SettingsError::from(os()),
        &failure,
        &["the settings file cannot be used", "access is denied"],
    );
    assert_chain(
        &SaveError::from(os()),
        &failure,
        &["the record cannot be saved", "access is denied"],
    );
}

#[test]
fn parse_errors_have_the_toml_error_as_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("player.toml");
    std::fs::write(&path, "volume = 3\n").unwrap();
    let e = load_settings::<PlayerSettings>(&path).unwrap_err();
    let SettingsError::Parse(toml) = &e else {
        panic!("{e:?}");
    };
    assert_chain(
        &e,
        toml,
        &["the settings file cannot be read", &toml.to_string()],
    );
    assert!(toml.message().contains("volume"), "{toml}");

    let e = load_preset_toml("mine", "id = [").unwrap_err();
    let PresetError::Parse(toml) = &e else {
        panic!("{e:?}");
    };
    let refused = "the preset file cannot be read (a file from an older version, one with an \
                   `id` among them, is refused, not converted; `eval-cli preset export \
                   <built-in> <path>` writes a built-in preset to a new file to edit)";
    assert_chain(&e, toml, &[refused, &toml.to_string()]);
    let e = PresetError::Invalid(RuleError::NoGauges);
    assert_chain(
        &e,
        &RuleError::NoGauges,
        &["the preset is invalid", &RuleError::NoGauges.to_string()],
    );
}

#[test]
fn evaluation_errors_chain_through_the_evaluator() {
    let invalid = EvalError::InvalidPreset(RuleError::NoGauges);
    let e = LogicError::Eval(invalid.clone());
    assert_chain(
        &e,
        &invalid,
        &[
            "evaluation cannot start",
            "invalid preset",
            &RuleError::NoGauges.to_string(),
        ],
    );
    let e = ReEvaluationError::Eval(EvalError::NoJudgeableNotes);
    assert_chain(
        &e,
        &EvalError::NoJudgeableNotes,
        &[
            "the record cannot be evaluated",
            "the chart has no notes to play",
        ],
    );
    assert!(ReEvaluationError::PresetMismatch.source().is_none());
    assert!(LogicError::StartAtMinimum.source().is_none());
}

/// A screen error of a test rig.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the surface cannot be made again")]
struct SurfaceLost;

#[test]
fn live_errors_have_each_side_as_source() {
    let logic = LogicError::TooManySources(9);
    let e = LiveError::<SurfaceLost>::Logic(logic.clone());
    assert_chain(
        &e,
        &logic,
        &[
            "the play cannot start",
            "9 sources exceed the source id range",
        ],
    );
    let e = LiveError::Screen(SurfaceLost);
    assert_chain(
        &e,
        &SurfaceLost,
        &[
            "the screen cannot draw the play",
            "the surface cannot be made again",
        ],
    );
    let e = LiveError::<SurfaceLost>::Audio(AudioWaitError::Gone);
    assert_chain(
        &e,
        &AudioWaitError::Gone,
        &["audio cannot play", "the audio thread is gone"],
    );
    for (cause, text) in [
        (game::InputWaitError::Gone, "the input thread is gone"),
        (
            game::InputWaitError::Timeout,
            "the input thread did not answer",
        ),
    ] {
        let e = LiveError::<SurfaceLost>::Input(cause);
        assert_chain(&e, &cause, &["the input thread failed", text]);
    }
}

/// `SessionError::Ingest` and `::Preset` and `PresetFileCause::Preset` are
/// transparent: the wrapped error's message, and its source, not itself.
#[test]
fn transparent_wrappers_forward_message_and_source() {
    let load = LoadError::TooManyLines { line: 4 };
    let ingest = IngestError::Load(load.clone());
    let e = SessionError::Ingest(ingest.clone());
    assert_eq!(e.to_string(), ingest.to_string());
    assert_chain(
        &e,
        &load,
        &["the chart cannot be loaded", "line 4: too many lines"],
    );
    // A preset file's error names the file; its cause forwards a preset
    // error's message and source.
    let file = PresetFileError {
        path: PathBuf::from("mine.toml"),
        cause: PresetFileCause::Preset(PresetError::Invalid(RuleError::NoGauges)),
    };
    let e = SessionError::Preset(ShelfError::Preset(file.clone()));
    assert_eq!(e.to_string(), file.to_string());
    assert_chain(
        &e,
        &file.cause,
        &[
            "the preset file mine.toml cannot be used",
            "the preset is invalid",
            &RuleError::NoGauges.to_string(),
        ],
    );
    let e = PresetFileError::io(PathBuf::from("mine.toml"), io::ErrorKind::NotFound);
    assert_chain(
        &e,
        &PresetFileCause::Io(io::ErrorKind::NotFound),
        &[
            "the preset file mine.toml cannot be used",
            "entity not found",
        ],
    );
    // A built-in's name as a stem: the file and the built-in are named.
    let e = PresetFileError {
        path: PathBuf::from("tapestry.toml"),
        cause: PresetFileCause::Preset(PresetError::Name(NameError::BuiltIn(
            "tapestry".to_owned(),
        ))),
    };
    assert_eq!(
        messages(&e),
        [
            "the preset file tapestry.toml cannot be used",
            "`tapestry` is the name of a built-in preset"
        ]
    );
    let unknown = UnknownPreset("custom.toml".to_owned());
    let e = SessionError::Preset(ShelfError::Unknown(unknown.clone()));
    assert_eq!(e.to_string(), unknown.to_string());
    assert!(e.source().is_none());
    let e = SessionError::InvalidStep(TimeUs(0));
    assert_eq!(e.to_string(), "a step of 0 us does not move the clock");
    assert!(e.source().is_none());
}

/// `Chain` prints what anyhow's `{:#}` prints, for chains of one to three
/// errors and through a transparent wrapper.
#[test]
fn chain_formats_as_anyhow_alternate() {
    let errors: [anyhow::Error; 6] = [
        SettingsError::InvalidDevice(DeviceSettingsError::ControllerUnnamed).into(),
        IngestError::Load(LoadError::Model(ModelError::Overflow)).into(),
        SessionError::Ingest(IngestError::Load(LoadError::TooManyLines { line: 4 })).into(),
        LogicError::Eval(EvalError::InvalidPreset(RuleError::NoGauges)).into(),
        AudioWaitError::Timeout.into(),
        LiveError::Screen(SurfaceLost).into(),
    ];
    for e in errors {
        let chained = Chain(e.as_ref()).to_string();
        assert_eq!(chained, format!("{e:#}"));
        assert_eq!(chained, messages(e.as_ref()).join(": "));
    }
}

/// The done-when of the error chains, without the player: a chart the
/// loader refuses reaches the report with every cause in its source chain,
/// and the report's line names each once.
#[test]
fn a_refused_chart_reports_each_cause_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().join("data"));
    let chart = dir.path().join("lntype2.bms");
    std::fs::write(
        &chart,
        "#PLAYER 1\n#BPM 120\n#LNTYPE 2\n#WAV01 click.wav\n#00151:0101\n",
    )
    .unwrap();
    let settings = Settings::load(&root).unwrap();
    let parts = Parts::standard(&root, &settings.player);
    let config = SessionConfig {
        lead_in: TimeUs(1_000_000),
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: 16,
        seed: 1,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    let Err(e) = Loaded::load(root, settings, &chart, config, parts) else {
        panic!("the chart loaded");
    };
    let unsupported = LoadError::Unsupported {
        line: 3,
        what: "#LNTYPE 2".to_owned(),
    };
    assert_eq!(source::<LoadError>(&e), &unsupported);
    let report = anyhow::Error::new(e).context(chart.display().to_string());
    let line = format!("{report:#}");
    assert_eq!(
        line,
        format!(
            "{}: the chart cannot be loaded: line 3: unsupported #LNTYPE 2",
            chart.display()
        )
    );
    for cause in report.chain() {
        let cause = cause.to_string();
        assert_eq!(line.matches(&cause).count(), 1, "{cause:?} in {line:?}");
    }
}
