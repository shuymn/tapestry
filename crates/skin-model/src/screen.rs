//! The screens a skin is made for.

use std::fmt;

/// A screen a skin is made for. The importers map their formats' type
/// numbers onto it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScreenKind {
    /// 7-key play. Other key modes, courses and skin selection are not
    /// kinds.
    Play,
    /// Song selection.
    Selection,
    /// Song decision.
    Decision,
    /// The result of one chart.
    Result,
}

impl fmt::Display for ScreenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Play => "7-key play",
            Self::Selection => "song selection",
            Self::Decision => "song decision",
            Self::Result => "single-chart result",
        })
    }
}
