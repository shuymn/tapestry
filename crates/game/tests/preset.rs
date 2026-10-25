//! Preset files: TOML in, the same canonical bytes out.

use game::{PresetError, load_preset_toml, preset_to_toml};
use rules::{GaugeDelta, GaugeValue, RuleError};

#[test]
fn custom_toml_round_trips_to_same_hash() {
    let custom = rules::custom();
    let text = preset_to_toml(&custom);
    let back = load_preset_toml(&text).unwrap();
    assert_eq!(back, custom);
    assert_eq!(replay::preset_hash(&back), replay::preset_hash(&custom));
    // Whitespace and comments do not split the hash.
    let commented = format!("# tuned by playing\n\n{}", text.replace(" = ", "   =   "));
    assert_eq!(
        replay::preset_hash(&load_preset_toml(&commented).unwrap()),
        replay::preset_hash(&custom)
    );
}

#[test]
fn edited_values_read_back_in_raw_units() {
    // The GREAT window widened to 50 ms and the normal gauge's GOOD change
    // made a fixed 0.5 %, written by hand in the file's units.
    let text = preset_to_toml(&rules::custom()).replacen(
        "[windows.great]\nearly_us = 45000\nlate_us = 45000",
        "[windows.great]\nearly_us = 50000\nlate_us = 50000",
        1,
    );
    let p = load_preset_toml(&text).unwrap();
    assert_eq!(p.windows.great.early.0, 50_000);
    assert_eq!(p.windows.great.late.0, 50_000);
    let text = preset_to_toml(&rules::custom()).replacen(
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
    let text = preset_to_toml(&rules::custom());
    assert!(matches!(
        load_preset_toml("id = ["),
        Err(PresetError::Parse(_))
    ));
    let unknown = format!("speed = 2\n{text}");
    assert!(matches!(
        load_preset_toml(&unknown),
        Err(PresetError::Parse(e)) if e.contains("speed")
    ));
    let mut preset = rules::custom();
    preset.gauges.clear();
    assert_eq!(
        load_preset_toml(&preset_to_toml(&preset)),
        Err(PresetError::Invalid(RuleError::NoGauges))
    );
}
