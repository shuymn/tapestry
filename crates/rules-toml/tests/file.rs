//! The preset file against `rules`' canonical encoding.

use rules::RulePreset;
use rules_toml::{
    MAX_PRESET_FILE_BYTES, NameError, PresetError, check_user_preset_name, load_preset_toml,
    preset_to_toml,
};

/// The text read as the user preset `mine`.
fn load(text: &str) -> Result<RulePreset, PresetError> {
    load_preset_toml("mine", text)
}

/// `p` named `mine`, as a file of it reads back under that name.
fn mine(mut p: RulePreset) -> RulePreset {
    p.id = "mine".to_owned();
    p
}

/// The file names nothing: its stem names it, so the same text reads back
/// under each name with that ID.
#[test]
fn the_name_comes_from_outside_the_file() {
    let text = preset_to_toml(&rules::tapestry());
    assert!(!text.contains("tapestry"), "{text}");
    for name in ["a", "my-rules", "x_1"] {
        let p = load_preset_toml(name, &text).unwrap();
        assert_eq!(p.id, name);
        assert_eq!(
            p,
            RulePreset {
                id: name.to_owned(),
                ..rules::tapestry()
            }
        );
    }
}

#[test]
fn user_preset_names_are_checked() {
    let longest = "a".repeat(64);
    for name in ["a", "my-rules", "x_1", "0", longest.as_str()] {
        assert_eq!(check_user_preset_name(name), Ok(()), "{name}");
    }
    let over = "a".repeat(65);
    for name in ["", "My", "a.b", "a b", "é", "custom.toml", over.as_str()] {
        assert_eq!(
            check_user_preset_name(name),
            Err(NameError::NotAName(name.to_owned())),
            "{name}"
        );
    }
    let text = preset_to_toml(&rules::tapestry());
    for name in rules::BUILT_IN_PRESETS {
        let e = load_preset_toml(name, &text).unwrap_err();
        assert_eq!(e, PresetError::Name(NameError::BuiltIn(name.to_owned())));
        assert_eq!(
            e.to_string(),
            format!("`{name}` is the name of a built-in preset")
        );
    }
}

#[test]
fn preset_file_round_trips_to_equal_canonical_bytes() {
    let tapestry = mine(rules::tapestry());
    let text = preset_to_toml(&tapestry);
    let back = load(&text).unwrap();
    assert_eq!(back, tapestry);
    assert_eq!(back.canonical_bytes(), tapestry.canonical_bytes());
    // Whitespace and comments do not change the bytes.
    let commented = format!("# tuned by playing\n\n{}", text.replace(" = ", "   =   "));
    assert_eq!(
        load(&commented).unwrap().canonical_bytes(),
        tapestry.canonical_bytes()
    );
}

#[test]
fn beatoraja_style_round_trips_to_equal_canonical_bytes() {
    let preset = mine(rules::beatoraja_style());
    let text = preset_to_toml(&preset);
    let back = load(&text).unwrap();
    assert_eq!(back, preset);
    assert_eq!(back.canonical_bytes(), preset.canonical_bytes());
    assert!(text.len() < 8 << 10, "{}", text.len());
}

/// LR2-style's interpolated window policy is a variant of the existing key:
/// `kind = "interpolated"` with the same fields as `scaled`, and its tied
/// LN-end edges read back.
#[test]
fn lr2_style_round_trips_to_equal_canonical_bytes() {
    let preset = mine(rules::lr2_style());
    let text = preset_to_toml(&preset);
    let back = load(&text).unwrap();
    assert_eq!(back, preset);
    assert_eq!(back.canonical_bytes(), preset.canonical_bytes());
    assert!(text.contains("kind = \"interpolated\""), "{text}");
    let scaled = load(&text.replacen("kind = \"interpolated\"", "kind = \"scaled\"", 1)).unwrap();
    assert!(matches!(
        scaled.window_policy,
        rules::WindowPolicy::Scaled(_)
    ));
    assert_ne!(scaled.canonical_bytes(), preset.canonical_bytes());
}

/// A file from before user presets states its name in `id`: it is refused
/// as a file from an older version, with the command that writes a new one.
#[test]
fn older_preset_file_is_refused_with_the_hint() {
    let text = preset_to_toml(&rules::tapestry());
    let e = load(&format!("id = \"mine\"\n{text}")).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("id")),
        "{e}"
    );
    let shown = e.to_string();
    assert!(shown.contains("older version"), "{shown}");
    assert!(shown.contains("eval-cli preset export"), "{shown}");
}

#[test]
fn preset_file_without_lamp_is_refused_with_the_hint() {
    let text = preset_to_toml(&rules::tapestry());
    let old = text.replacen("lamp = 1\n", "", 1);
    assert_ne!(old, text, "the edit did not apply");
    let e = load(&old).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("lamp")),
        "{e}"
    );
    let shown = e.to_string();
    assert!(shown.contains("older version"), "{shown}");
    assert!(shown.contains("eval-cli preset export"), "{shown}");
}

#[test]
fn file_without_the_empty_poor_behind_edge_is_refused() {
    let text = preset_to_toml(&rules::tapestry());
    let old = text.replacen("behind_us = 150000\n", "", 1);
    assert_ne!(old, text, "the edit did not apply");
    let e = load(&old).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("behind_us")),
        "{e}"
    );
    assert!(e.to_string().contains("older version"), "{e}");
}

/// A file from before the lane-class tables: one `windows` table and a
/// `rank_scale` beside the header policy.
#[test]
fn file_without_lane_class_tables_is_refused() {
    let text = preset_to_toml(&rules::tapestry());
    for (key, table) in [
        ("tables", "[tables.keys.windows.perfect]"),
        ("window_policy", "[window_policy]"),
    ] {
        assert!(text.contains(table), "{text}");
        let old = text.replace(&format!("[{key}"), "[old");
        let e = load(&old).unwrap_err();
        assert!(
            matches!(&e, PresetError::Parse(m) if m.message().contains("old")
                || m.message().contains(key)),
            "{e}"
        );
        assert!(e.to_string().contains("older version"), "{e}");
    }
    let style = preset_to_toml(&rules::beatoraja_style());
    assert!(style.contains("kind = \"scaled\""), "{style}");
    assert!(text.contains("kind = \"thousandths\""), "{text}");
}

#[test]
fn edited_scratch_and_policy_values_read_back() {
    let text = preset_to_toml(&rules::beatoraja_style());
    let p = load(&text.replacen("defexrank_percent = 75", "defexrank_percent = 80", 1)).unwrap();
    let rules::WindowPolicy::Scaled(s) = p.window_policy else {
        panic!("beatoraja-style scales by whole percentages");
    };
    assert_eq!(s.defexrank_percent, 80);
    // The scratch's table comes after the keys'.
    let behind = "behind_us = 160000";
    assert_eq!(text.matches(behind).count(), 1, "{text}");
    let p = load(&text.replacen(behind, "behind_us = 170000", 1)).unwrap();
    assert_eq!(p.tables.scratch.empty_poor.behind.0, 170_000);
    assert_eq!(p.tables.keys.empty_poor.behind.0, 150_000);
}

#[test]
fn file_without_a_header_policy_is_refused() {
    // Each standard preset names its own policy.
    let tapestry = preset_to_toml(&rules::tapestry());
    let style = preset_to_toml(&rules::beatoraja_style());
    assert!(tapestry.contains("headers = \"decimals\"\n"), "{tapestry}");
    assert!(
        style.contains("headers = \"def_ex_rank_first\"\n"),
        "{style}"
    );
    // A file from before the judgment-header model has no `headers` key.
    let old = tapestry.replacen("headers = \"decimals\"\n", "", 1);
    let e = load(&old).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("headers")),
        "{e}"
    );
    assert!(e.to_string().contains("older version"), "{e}");
}

/// A file from before the gauge policies: a `default_total_milli` number,
/// no correction, and a survival gauge without its threshold.
#[test]
fn file_before_the_gauge_policies_is_refused() {
    let text = preset_to_toml(&rules::beatoraja_style());
    for (current, old, key) in [
        (
            "[default_total]\nkind = \"notes_curve\"\n",
            "default_total_milli = 260000\n",
            "default_total",
        ),
        ("correction = \"none\"\n", "", "correction"),
        (
            "kind = \"survival\"\nfails_below = 0\n",
            "kind = \"survival\"\n",
            "fails_below",
        ),
    ] {
        assert!(text.contains(current), "{text}");
        let e = load(&text.replacen(current, old, 1)).unwrap_err();
        assert!(
            matches!(&e, PresetError::Parse(m) if m.message().contains(key)),
            "{key}: {e}"
        );
        assert!(e.to_string().contains("older version"), "{e}");
    }
}

#[test]
fn every_gauge_policy_round_trips() {
    let mut p = mine(rules::beatoraja_style());
    p.default_total = rules::DefaultTotal::NotesLinear;
    let hard = &mut p.gauges[3];
    hard.kind = rules::GaugeKind::Survival {
        fails_below: rules::GaugeValue(2 * rules::GaugeValue::PERCENT),
    };
    hard.correction = rules::TotalCorrection::ScaleDamage;
    for preset in [p, mine(rules::tapestry())] {
        let text = preset_to_toml(&preset);
        let back = load(&text).unwrap();
        assert_eq!(back.canonical_bytes(), preset.canonical_bytes());
        assert_eq!(back, preset);
    }
    let mut fixed = rules::tapestry();
    fixed.default_total = rules::DefaultTotal::Fixed { milli: 0 };
    assert_eq!(
        load(&preset_to_toml(&fixed)),
        Err(PresetError::Invalid(rules::RuleError::OutOfRange {
            field: "default_total.milli"
        }))
    );
}

/// A file with an LN head eligibility, which presets no longer have, is a
/// file from an older version.
#[test]
fn head_eligibility_is_refused() {
    let text = preset_to_toml(&rules::tapestry());
    let late_head = "late_head = \"completes_at_press\"\n";
    assert_eq!(text.matches(late_head).count(), 1, "{text}");
    assert!(!text.contains("head_eligibility"), "{text}");
    let older = text.replacen(
        late_head,
        &format!("{late_head}head_eligibility = \"inside_bad\"\n"),
        1,
    );
    let e = load(&older).unwrap_err();
    assert!(
        matches!(&e, PresetError::Parse(m) if m.message().contains("head_eligibility")),
        "{e}"
    );
    assert!(e.to_string().contains("older version"), "{e}");
}

#[test]
fn the_largest_generated_presets_fit_the_file_limit() {
    let mut p = mine(rules::tapestry());
    // Gauge ids of 136 characters that TOML escapes, the longest the property
    // tests generate, on their largest gauge count, with five-digit lamp
    // ranks.
    let long = "\u{1}".repeat(136);
    let gauge = p.gauges[1].clone();
    p.gauges = (0..131)
        .map(|i| {
            let mut g = gauge.clone();
            g.id = format!("{long}{i}");
            g.lamp = u16::MAX - i;
            g
        })
        .collect();
    let text = preset_to_toml(&p);
    assert_eq!(load(&text), Ok(p));
    assert!(text.len() < MAX_PRESET_FILE_BYTES / 2, "{}", text.len());
    let tapestry = preset_to_toml(&rules::tapestry()).len();
    assert!(tapestry < 4 << 10, "{tapestry}");
}
