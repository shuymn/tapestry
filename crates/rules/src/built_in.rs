//! The built-in presets, chosen by name (design.md "Standard Presets"). The
//! list lives here so every crate that chooses a preset by name shares it.

use crate::{RulePreset, beatoraja_style, lr2_style, tapestry};

/// The built-in presets' names, each also its preset ID, in the order they
/// are listed.
pub const BUILT_IN_PRESETS: [&str; 3] = ["tapestry", "beatoraja-style", "lr2-style"];

/// The built-in preset named `name`; `None` for any other name.
#[must_use]
pub fn built_in(name: &str) -> Option<RulePreset> {
    match name {
        "tapestry" => Some(tapestry()),
        "beatoraja-style" => Some(beatoraja_style()),
        "lr2-style" => Some(lr2_style()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILT_IN_PRESETS, built_in};
    use crate::RulePreset;

    #[test]
    fn each_built_in_is_named_by_its_id() {
        for name in BUILT_IN_PRESETS {
            assert_eq!(built_in(name).map(|p| p.id), Some(name.to_owned()));
        }
        assert_eq!(built_in("custom"), None);
        assert_eq!(built_in("TAPESTRY"), None);
    }

    /// The gauge ids, easiest clear first.
    fn by_lamp(preset: &RulePreset) -> Vec<&str> {
        let mut gauges: Vec<_> = preset.gauges.iter().collect();
        gauges.sort_by_key(|g| g.lamp);
        gauges.iter().map(|g| g.id.as_str()).collect()
    }

    #[test]
    fn built_in_lamp_ranks() {
        let style = ["assist_easy", "easy", "normal", "hard", "ex_hard", "hazard"];
        assert_eq!(by_lamp(&crate::beatoraja_style()), style);
        assert_eq!(by_lamp(&crate::lr2_style()), style);
        assert_eq!(by_lamp(&crate::tapestry()), ["normal", "survival"]);
        // The display order is unchanged: NORMAL stays first.
        for name in BUILT_IN_PRESETS {
            let preset = built_in(name).expect("a built-in");
            assert_eq!(preset.gauges.first().map(|g| g.id.as_str()), Some("normal"));
            assert_eq!(preset.validate(), Ok(()));
        }
    }
}
