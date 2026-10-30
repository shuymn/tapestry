//! The preset file against `rules`' canonical encoding.

use rules_toml::{MAX_PRESET_FILE_BYTES, PresetError, load_preset_toml, preset_to_toml};

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
fn beatoraja_style_round_trips_to_equal_canonical_bytes() {
    let preset = rules::beatoraja_style();
    let text = preset_to_toml(&preset);
    let back = load_preset_toml(&text).unwrap();
    assert_eq!(back, preset);
    assert_eq!(back.canonical_bytes(), preset.canonical_bytes());
    assert!(text.len() < 8 << 10, "{}", text.len());
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

#[test]
fn file_without_a_header_policy_is_refused() {
    // Each standard preset names its own policy.
    let custom = preset_to_toml(&rules::custom());
    let style = preset_to_toml(&rules::beatoraja_style());
    assert!(custom.contains("headers = \"decimals\"\n"), "{custom}");
    assert!(style.contains("headers = \"integer_ranks\"\n"), "{style}");
    // A file from before the judgment-header model has no `headers` key.
    let old = custom.replacen("headers = \"decimals\"\n", "", 1);
    let e = load_preset_toml(&old).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("headers")),
        "{e}"
    );
    assert!(e.to_string().contains("older version"), "{e}");
}

#[test]
fn the_largest_generated_presets_fit_the_file_limit() {
    let mut p = rules::custom();
    // Ids of 136 characters that TOML escapes, the longest the property
    // tests generate, on their largest gauge count.
    let long = "\u{1}".repeat(136);
    p.id.clone_from(&long);
    let gauge = p.gauges[1].clone();
    p.gauges = (0..131)
        .map(|i| {
            let mut g = gauge.clone();
            g.id = format!("{long}{i}");
            g
        })
        .collect();
    let text = preset_to_toml(&p);
    assert_eq!(load_preset_toml(&text), Ok(p));
    assert!(text.len() < MAX_PRESET_FILE_BYTES / 2, "{}", text.len());
    let custom = preset_to_toml(&rules::custom()).len();
    assert!(custom < 4 << 10, "{custom}");
}
