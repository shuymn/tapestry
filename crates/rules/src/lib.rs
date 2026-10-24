//! Judgment and gauge rule presets.
//!
//! Gauges are fixed-point; this crate does not use floating point. The float
//! lints below are partial checks: they catch float arithmetic and
//! comparisons, not every use of a float type.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

/// A named set of judgment and gauge rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    /// Display name of the preset.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::Preset;

    #[test]
    fn preset_keeps_its_name() {
        let preset = Preset {
            name: "standard".to_owned(),
        };
        assert_eq!(preset.name, "standard");
    }
}
