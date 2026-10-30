//! Preset files: TOML in, the same canonical bytes out.

use game::{PresetError, load_preset_toml, preset_to_toml};
use rules::{GaugeDelta, GaugeValue, RuleError};

#[test]
fn custom_toml_round_trips_to_same_hash() {
    let tapestry = rules::tapestry();
    let text = preset_to_toml(&tapestry);
    let back = load_preset_toml(&text).unwrap();
    assert_eq!(back, tapestry);
    assert_eq!(replay::preset_hash(&back), replay::preset_hash(&tapestry));
    // Whitespace and comments do not split the hash.
    let commented = format!("# tuned by playing\n\n{}", text.replace(" = ", "   =   "));
    assert_eq!(
        replay::preset_hash(&load_preset_toml(&commented).unwrap()),
        replay::preset_hash(&tapestry)
    );
}

#[test]
fn edited_values_read_back_in_raw_units() {
    // The GREAT window widened to 50 ms and the normal gauge's GOOD change
    // made a fixed 0.5 %, written by hand in the file's units.
    let text = preset_to_toml(&rules::tapestry()).replacen(
        "[tables.keys.windows.great]\nearly_us = 45000\nlate_us = 45000",
        "[tables.keys.windows.great]\nearly_us = 50000\nlate_us = 50000",
        1,
    );
    let p = load_preset_toml(&text).unwrap();
    assert_eq!(p.tables.keys.windows.great.early.0, 50_000);
    assert_eq!(p.tables.keys.windows.great.late.0, 50_000);
    let text = preset_to_toml(&rules::tapestry()).replacen(
        "[gauges.change.good.total_share]\nnum = 1\nden = 2",
        "[gauges.change.good]\nfixed = 50000",
        1,
    );
    let p = load_preset_toml(&text).unwrap();
    assert_eq!(
        p.gauges[0].change.good,
        GaugeDelta::Fixed(GaugeValue(50_000))
    );
}

#[test]
fn malformed_unknown_and_invalid_presets_are_refused() {
    let text = preset_to_toml(&rules::tapestry());
    assert!(matches!(
        load_preset_toml("id = ["),
        Err(PresetError::Parse(_))
    ));
    let unknown = format!("speed = 2\n{text}");
    assert!(matches!(
        load_preset_toml(&unknown),
        Err(PresetError::Parse(e)) if e.message().contains("speed")
    ));
    let mut preset = rules::tapestry();
    preset.gauges.clear();
    assert_eq!(
        load_preset_toml(&preset_to_toml(&preset)),
        Err(PresetError::Invalid(RuleError::NoGauges))
    );
}

/// Before the first release an older preset file is refused, not converted:
/// version 1's miss rule and a file without the empty POOR's `behind_us`.
#[test]
fn older_preset_files_are_refused() {
    let text = preset_to_toml(&rules::tapestry());
    let refused = |old: &str, missing: &str| {
        assert_ne!(old, text, "the edit did not apply");
        let e = load_preset_toml(old).unwrap_err();
        assert!(
            matches!(&e, PresetError::Parse(m) if m.message().contains(missing)),
            "{e}"
        );
        assert!(e.to_string().contains("older version"), "{e}");
    };
    refused(
        &text.replacen(
            "miss = \"poor_after_late_bad_or_overtaken\"",
            "miss = \"poor_after_late_bad\"",
            1,
        ),
        "poor_after_late_bad",
    );
    refused(&text.replacen("behind_us = 150000\n", "", 1), "behind_us");
    // An LN break table where the LN-end edges are: a file of the earlier
    // LN rule.
    let ln_end = "[tables.keys.ln_end]\nperfect_early_us = 100000\ngreat_early_us = 150000\ngood_early_us = 200000\n";
    assert!(text.contains(ln_end), "{text}");
    refused(
        &text.replacen(
            ln_end,
            "[tables.keys.ln_break]\nrelease_margin_us = 100000\nbreaks_combo = true\ncounts_bp = true\n",
            1,
        ),
        "ln_break",
    );
}

#[test]
fn ln_end_edges_and_the_implementation_version_read_back_and_validate() {
    let text = preset_to_toml(&rules::tapestry());
    // The GOOD LN-end edge widened to 250 ms, by hand in µs.
    let p = load_preset_toml(&text.replacen("good_early_us = 200000", "good_early_us = 250000", 1))
        .unwrap();
    assert_eq!(p.tables.keys.ln_end.good.0, 250_000);
    // Narrower than the PERFECT window's early edge (20 ms): invalid.
    assert_eq!(
        load_preset_toml(&text.replacen(
            "perfect_early_us = 100000",
            "perfect_early_us = 19999",
            1
        )),
        Err(PresetError::Invalid(RuleError::LnEndNarrowerThanNote {
            class: rules::LaneClass::Keys,
            judgment: rules::Judgment::Perfect
        }))
    );
    // The standard evaluator's version 2, the earlier key sound rule, is
    // refused.
    let v2 = text.replacen(
        "[implementation]\nkind = \"standard\"\nversion = 3",
        "[implementation]\nkind = \"standard\"\nversion = 2",
        1,
    );
    assert_ne!(v2, text, "the edit did not apply");
    assert_eq!(
        load_preset_toml(&v2),
        Err(PresetError::Invalid(RuleError::UnsupportedImplementation {
            name: "standard",
            version: 2
        }))
    );
}
