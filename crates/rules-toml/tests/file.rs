//! The preset file against `rules`' canonical encoding.

use rules_toml::{PresetError, load_preset_toml, preset_to_toml};

#[test]
fn preset_file_round_trips_to_equal_canonical_bytes() {
    let custom = rules::custom();
    let text = preset_to_toml(&custom);
    let back = load_preset_toml(&text).unwrap();
    assert_eq!(back, custom);
    assert_eq!(back.canonical_bytes(), custom.canonical_bytes());
    // Whitespace and comments do not change the bytes.
    let commented = format!("# tuned by playing\n\n{}", text.replace(" = ", "   =   "));
    assert_eq!(
        load_preset_toml(&commented).unwrap().canonical_bytes(),
        custom.canonical_bytes()
    );
}

#[test]
fn older_preset_file_is_refused_with_the_hint() {
    let text = preset_to_toml(&rules::custom());
    let old = text.replacen("behind_us = 150000\n", "", 1);
    assert_ne!(old, text, "the edit did not apply");
    let e = load_preset_toml(&old).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("behind_us")),
        "{e}"
    );
    assert!(e.to_string().contains("older version"), "{e}");
}
