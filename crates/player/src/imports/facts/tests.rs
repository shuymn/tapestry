//! The play level's parse, and an attempt's facts from loaded chart files
//! under the play presets' header policies.

use skin_beatoraja::{ChartRank, Fact, ImportFacts, JudgeClass};
use skin_model::ScreenKind;

use super::{attempt_facts, level_of};

/// `text` loaded as `a.bms` under the play preset `preset`, the chart one
/// note after `text`'s headers.
pub(in crate::imports) fn loaded(text: &str, preset: &str) -> game::Loaded {
    let dir = tempfile::tempdir().unwrap();
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings {
            preset: preset.to_owned(),
            ..game::PlayerSettings::default()
        },
    };
    let root = game::DataRoot::new(dir.path().join("root"));
    let chart_path = dir.path().join("a.bms");
    std::fs::write(&chart_path, format!("{text}#BPM 120\n#00111:01\n")).unwrap();
    let config = game::SessionConfig {
        lead_in: bms_model::TimeUs(1_000_000),
        step: bms_model::TimeUs(1_000),
        start: bms_model::TimeUs(0),
        effects: 64,
        seed: 1,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: replay::BuildProvenance::Development,
        receive: replay::ReceiveMethod::Scripted,
        autoplay: None,
    };
    let parts = game::Parts::unsaved(&settings.player);
    game::Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// The source's `Integer.parseInt` on `#PLAYLEVEL`: an optional sign,
/// then decimal digits of any script in the Basic Multilingual Plane,
/// within `i32`; anything else 0.
#[test]
fn the_level_parses_as_the_source_does() {
    for (text, level) in [
        ("12", 12),
        ("+12", 12),
        ("-1", -1),
        ("０１２", 12),
        ("١٢", 12),
        ("1٢", 12),
        ("", 0),
        (" 12", 0),
        ("12 ", 0),
        ("12a", 0),
        ("1.5", 0),
        ("+", 0),
        ("-", 0),
        ("+-1", 0),
        ("2147483647", 2_147_483_647),
        ("2147483648", 0),
        ("-2147483648", -2_147_483_648),
        ("-2147483649", 0),
        ("99999999999999999999", 0),
        ("0000000000000000000012", 12),
        // Mathematical bold digit one and two: outside the BMP, so two
        // surrogates each, neither a digit.
        ("\u{1D7CF}\u{1D7D0}", 0),
        ("Ⅻ", 0),
        ("½", 0),
    ] {
        assert_eq!(level_of(text), level, "{text:?}");
    }
}

/// The level an attempt reads is the loader's: its value trimmed of
/// Unicode whitespace, and the last declaration in the `#RANDOM` branches
/// the load took.
#[test]
fn an_attempt_reads_the_loaders_level() {
    for (text, level) in [
        ("#PLAYLEVEL   12  \n", 12),
        ("#PLAYLEVEL \u{3000}１２\u{3000}\n", 12),
        ("#PLAYLEVEL 7\n#PLAYLEVEL 9\n", 9),
        (
            "#PLAYLEVEL 1\n#SETRANDOM 2\n#IF 1\n#PLAYLEVEL 3\n#ENDIF\n\
             #IF 2\n#PLAYLEVEL 11\n#ENDIF\n#ENDRANDOM\n",
            11,
        ),
        ("#PLAYLEVEL ten\n", 0),
        ("", 0),
    ] {
        let facts = attempt_facts(&loaded(text, "tapestry"), false);
        assert_eq!(facts.play_level, Fact::Known(level), "{text:?}");
    }
}

/// The class an attempt reads is the one the play preset's header policy
/// gives: `#RANK 3` is easy under each preset, a `#DEFEXRANK` its band
/// where the policy reads it over a later `#RANK`, and no rank the class
/// of `#RANK 2`. Autoplay is the session's.
#[test]
fn an_attempt_reads_the_class_its_preset_judges_with() {
    let class = |f: &ImportFacts| match f.chart_rank {
        Fact::Known(r) => JudgeClass::of(r),
        other => panic!("{other:?}"),
    };
    for (text, preset, rank, judged) in [
        (
            "#RANK 3\n",
            "tapestry",
            Some(ChartRank::Rank(3)),
            JudgeClass::Easy,
        ),
        (
            "#RANK 3\n",
            "beatoraja-style",
            Some(ChartRank::Rank(3)),
            JudgeClass::Easy,
        ),
        // The tapestry policy keeps the later declaration of the shared
        // slot; the other reads `#DEFEXRANK` over any `#RANK`.
        (
            "#DEFEXRANK 120\n#RANK 1\n",
            "tapestry",
            Some(ChartRank::Rank(1)),
            JudgeClass::Hard,
        ),
        (
            "#DEFEXRANK 120\n#RANK 1\n",
            "beatoraja-style",
            Some(ChartRank::DefExRankMilli(120_000)),
            JudgeClass::VeryEasy,
        ),
        // A `#RANK` the policy refuses is no rank.
        ("#RANK 7\n", "beatoraja-style", None, JudgeClass::Normal),
        ("", "tapestry", None, JudgeClass::Normal),
    ] {
        for autoplay in [false, true] {
            let facts = attempt_facts(&loaded(text, preset), autoplay);
            assert_eq!(
                facts,
                ImportFacts {
                    scene: ScreenKind::Play,
                    autoplay: Fact::Known(autoplay),
                    chart_rank: Fact::Known(rank),
                    play_level: Fact::Known(0),
                },
                "{text:?} {preset}"
            );
            assert_eq!(class(&facts), Some(judged), "{text:?} {preset}");
        }
    }
}
