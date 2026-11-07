//! The play level's parse, and an attempt's facts from loaded chart files
//! under either play preset.

use skin_beatoraja::{Fact, ImportFacts, JudgeClass};
use skin_model::ScreenKind;

use super::{attempt_facts, level_of};

/// `text` loaded as `a.bms` under the play preset `preset`, the chart one
/// note after `text`'s headers.
pub(crate) fn loaded(text: &str, preset: &str) -> game::Loaded {
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

/// The judgment class for a chart of these headers: first the reference's,
/// the output of the decoder (jbms-parser) and validation
/// (`BMSPlayerRule.validate`, twice, as the song data takes the chart)
/// banded as options 180 to 184 band them, at beatoraja's `0.8.9` tag, run
/// on these inputs with a 7-key chart; then the one shown here, from the
/// beatoraja-style header policy's reading (`JudgeClass::of_headers`). The
/// validated rank is in each comment, and where the policy reads the
/// headers otherwise than that decoder (a decimal `#DEFEXRANK`, one past
/// `i32`, a `#DEFEXRANK` before a `#RANK`, a non-ASCII digit), what it
/// reads.
pub(crate) const REFERENCE_CLASSES: &[(&str, Option<JudgeClass>, Option<JudgeClass>)] = {
    use JudgeClass::{Easy, Hard, Normal, VeryEasy, VeryHard};
    &[
        ("", Some(Normal), Some(Normal)),                            // 75
        ("#RANK 0\n", Some(VeryHard), Some(VeryHard)),               // 25
        ("#RANK 1\n", Some(Hard), Some(Hard)),                       // 50
        ("#RANK 2\n", Some(Normal), Some(Normal)),                   // 75
        ("#RANK 3\n", Some(Easy), Some(Easy)),                       // 100
        ("#RANK 4\n", Some(VeryEasy), Some(VeryEasy)),               // 125
        ("#RANK 5\n", Some(Normal), Some(Normal)),                   // refused: 75
        ("#RANK -1\n", Some(Normal), Some(Normal)),                  // refused: 75
        ("#RANK 7\n", Some(Normal), Some(Normal)),                   // refused: 75
        ("#RANK 100\n", Some(Normal), Some(Normal)),                 // refused: 75
        ("#RANK x\n", Some(Normal), Some(Normal)),                   // refused: 75
        ("#RANK 2.5\n", Some(Normal), Some(Normal)),                 // refused: 75
        ("#RANK +3\n", Some(Easy), Some(Easy)),                      // 100
        ("#RANK 03\n", Some(Easy), Some(Easy)),                      // 100
        ("#RANK -0\n", Some(VeryHard), Some(VeryHard)),              // 25
        ("#rank 3\n", Some(Easy), Some(Easy)),                       // 100
        ("#RANK 3 4\n", Some(Normal), Some(Normal)),                 // refused: 75
        ("#DEFEXRANK 1\n", Some(Easy), Some(Easy)),                  // 0, then 100
        ("#DEFEXRANK 2\n", Some(Hard), Some(Hard)),                  // 1
        ("#DEFEXRANK 10\n", None, None),                             // 7
        ("#DEFEXRANK 13\n", None, None),                             // 9
        ("#DEFEXRANK 14\n", Some(VeryHard), Some(VeryHard)),         // 10
        ("#DEFEXRANK 34.9\n", Some(Normal), Some(VeryHard)), // refused: 75; here 34 read: 25
        ("#DEFEXRANK 35\n", Some(VeryHard), Some(VeryHard)), // 26
        ("#DEFEXRANK 46\n", Some(VeryHard), Some(VeryHard)), // 34
        ("#DEFEXRANK 47\n", Some(Hard), Some(Hard)),         // 35
        ("#DEFEXRANK 79\n", Some(Hard), Some(Hard)),         // 59
        ("#DEFEXRANK 80\n", Some(Normal), Some(Normal)),     // 60
        ("#DEFEXRANK 100\n", Some(Normal), Some(Normal)),    // 75
        ("#DEFEXRANK 113\n", Some(Normal), Some(Normal)),    // 84
        ("#DEFEXRANK 114\n", Some(Easy), Some(Easy)),        // 85
        ("#DEFEXRANK 120\n", Some(Easy), Some(Easy)),        // 90
        ("#DEFEXRANK 146\n", Some(Easy), Some(Easy)),        // 109
        ("#DEFEXRANK 147\n", Some(VeryEasy), Some(VeryEasy)), // 110
        ("#DEFEXRANK 1000\n", Some(VeryEasy), Some(VeryEasy)), // 750
        ("#DEFEXRANK +120\n", Some(Easy), Some(Easy)),       // 90
        ("#DEFEXRANK 120.0\n", Some(Normal), Some(Easy)),    // refused: 75; here 120 read: 90
        ("#DEFEXRANK 1E2\n", Some(Normal), Some(Normal)),    // refused: 75; here 100 read: 75
        ("#DEFEXRANK 0\n", Some(Normal), Some(Normal)),      // refused: 75
        ("#DEFEXRANK -5\n", Some(Normal), Some(Normal)),     // refused: 75
        ("#DEFEXRANK x\n", Some(Normal), Some(Normal)),      // refused: 75
        ("#DEFEXRANK 120 x\n", Some(Normal), Some(Normal)),  // refused: 75
        ("#DEFEXRANK 3000000000\n", Some(Normal), Some(VeryEasy)), // refused: 75; here read past i32: 2250000000
        ("#DEFEXRANK 2147483647\n", Some(VeryEasy), Some(VeryEasy)), // wraps: 21474835
        ("#DEFEXRANK 28633115\n", Some(VeryEasy), Some(VeryEasy)), // 21474836
        ("#DEFEXRANK 28633116\n", Some(Easy), Some(Easy)),         // wraps: -21474835, then 100
        ("#DEFEXRANK 57266231\n", Some(Easy), Some(Easy)),         // wraps: 0, then 100
        ("#DEFEXRANK 57266232\n", Some(Hard), Some(Hard)),         // wraps: 1
        // A decimal below 1, whose whole part 0 the policy reads, and a
        // non-ASCII digit, which `Integer.parseInt` reads and the policy's
        // decimal grammar does not (`crates/rules/src/beatoraja_style.rs:106`).
        ("#DEFEXRANK 0.5\n", Some(Normal), Some(Easy)), // refused: 75; here 0 read: 0, then 100
        ("#RANK \u{ff13}\n", Some(Easy), Some(Normal)), // 100; here refused: 75
        // The last header accepted counts, whichever it is.
        ("#RANK 3\n#DEFEXRANK 120\n", Some(Easy), Some(Easy)), // 90
        ("#DEFEXRANK 120\n#RANK 3\n", Some(Easy), Some(Easy)), // 100; here #DEFEXRANK read: 90
        ("#RANK 0\n#DEFEXRANK 120\n", Some(Easy), Some(Easy)), // 90
        ("#DEFEXRANK 120\n#RANK 0\n", Some(VeryHard), Some(Easy)), // 25; here #DEFEXRANK read: 90
        ("#RANK 0\n#DEFEXRANK 0\n", Some(VeryHard), Some(VeryHard)), // 25
        ("#DEFEXRANK 0\n#RANK 0\n", Some(VeryHard), Some(VeryHard)), // 25
        ("#RANK 3\n#DEFEXRANK x\n", Some(Easy), Some(Easy)),   // 100
        ("#DEFEXRANK 120\n#RANK x\n", Some(Easy), Some(Easy)), // 90
        ("#RANK 1\n#RANK 3\n", Some(Easy), Some(Easy)),        // 100
        ("#RANK 3\n#RANK 9\n", Some(Easy), Some(Easy)),        // 100
        ("#DEFEXRANK 120\n#DEFEXRANK 0\n", Some(Easy), Some(Easy)), // 90
        // Within the `#RANDOM` branch the load takes.
        // 100
        (
            "#RANDOM 1\n#IF 1\n#RANK 3\n#ENDIF\n#ENDRANDOM\n",
            Some(Easy),
            Some(Easy),
        ),
    ]
};

/// The class an attempt reads is the one shown for the chart's headers
/// ([`REFERENCE_CLASSES`]) under either play preset, whatever rank the
/// preset judges with. Autoplay is the session's.
#[test]
fn an_attempt_reads_the_references_class_whatever_the_preset() {
    for &(text, _, class) in REFERENCE_CLASSES {
        for preset in ["tapestry", "beatoraja-style"] {
            for autoplay in [false, true] {
                let facts = attempt_facts(&loaded(text, preset), autoplay);
                assert_eq!(
                    facts,
                    ImportFacts {
                        scene: ScreenKind::Play,
                        autoplay: Fact::Known(autoplay),
                        judge_class: Fact::Known(class),
                        play_level: Fact::Known(0),
                        result: Fact::Unavailable,
                    },
                    "{text:?} {preset}"
                );
            }
        }
    }
}
