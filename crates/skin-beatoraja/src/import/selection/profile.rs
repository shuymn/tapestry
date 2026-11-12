//! The selection profile (docs/stage3.md §3, Selection): one explicit
//! `match` per id space over every id the surveyed selection skins read,
//! each classified. **N** ids are bridged from the selection schema, the
//! player's settings among them (read-only); **E** ids (actions, and
//! settings with no native equivalent) are unavailable, drawing nothing
//! with a note; **D** ids are a truthful constant or an omission with a
//! note. An id outside the survey
//! follows the existing rules of its space: an option the source's
//! factory knows is unsupported, one neither built in nor declared never
//! draws, and an unknown number, text or image index is unsupported by
//! its role or resolves to nothing as the factory's does.
//!
//! Every reader of an id, a JSON field's `ref` or `op` and a Lua host
//! read alike, goes through these functions, so the two cannot differ.

use skin_model::{RatioSource, SelectionText, TextField, selection_timer};

/// What the profile does with an id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class<T> {
    /// Needed: bridged from the selection schema.
    Bridge(T),
    /// Needed, but with no native equivalent or action yet: it draws
    /// nothing, with a note.
    Unavailable,
    /// Decorative: omitted with a note.
    Omit,
}

impl<T> Class<T> {
    /// The class with its bridge mapped by `f`.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> Class<U> {
        match self {
            Self::Bridge(t) => Class::Bridge(f(t)),
            Self::Unavailable => Class::Unavailable,
            Self::Omit => Class::Omit,
        }
    }
}

/// A selection number as the schema gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelValue {
    Ex,
    /// The EX score less a rival's, a target's or the best's, none of
    /// which selection has: the selected EX, 0 with no score.
    ExDifference,
    MaxEx,
    Notes,
    Bp,
    Plays,
    BpmMax,
    BpmMin,
    Level,
    RatePercent,
    RateHundredths,
    NextRank,
    Charts,
    LengthMin,
    LengthSec,
    /// The settings' display offset, whole ms.
    DisplayOffset,
    /// The master gain's number.
    Volume,
    /// Floating's travel duration and its green number.
    Duration,
    Green,
    /// The application's readouts, 20 to 29 as `id − 20`, play's fields
    /// of the same names.
    App(u8),
}

impl SelValue {
    /// Its expression over the selection schema.
    pub(crate) fn expr(self) -> &'static str {
        match self {
            Self::Ex | Self::ExDifference => "selected.ex",
            Self::MaxEx => "selected.max_ex",
            Self::Notes => "selected.notes",
            Self::Bp => "selected.bp",
            Self::Plays => "selected.plays",
            Self::BpmMax => "selected.bpm_max",
            Self::BpmMin => "selected.bpm_min",
            Self::Level => "selected.level",
            Self::RatePercent => "selected.rate_percent",
            Self::RateHundredths => "selected.rate_hundredths",
            Self::NextRank => "selected.next_rank",
            Self::Charts => "cursor.charts",
            Self::LengthMin => "selected.length_min",
            Self::LengthSec => "selected.length_sec",
            Self::DisplayOffset => "settings.display_offset",
            Self::Volume => "settings.volume",
            Self::Duration => "settings.duration",
            Self::Green => "settings.green",
            Self::App(n) => super::super::property::app_field(n),
        }
    }

    /// The condition it is drawn under: absent, it draws nothing, as the
    /// source's `Integer.MIN_VALUE` (`skin/property/IntegerPropertyFactory.java:133-198`).
    pub(crate) fn availability(self) -> &'static str {
        match self {
            Self::Ex | Self::Bp => "selected.best",
            Self::MaxEx => "selected.max_ex_valid",
            Self::Plays => "selected.played",
            Self::Notes | Self::BpmMax | Self::BpmMin | Self::LengthMin | Self::LengthSec => {
                "selected.summary"
            }
            Self::Level => "cursor.chart",
            Self::RatePercent | Self::RateHundredths | Self::NextRank => "selected.rate_valid",
            Self::Charts => "cursor.folder || cursor.all",
            Self::ExDifference | Self::DisplayOffset | Self::Volume | Self::App(_) => "true",
            // A duration needs the chart's tempo in Classic.
            Self::Duration | Self::Green => "settings.floating",
        }
    }
}

/// The selection number `id` (`main_state.number` and a value's `ref`).
pub(crate) fn number(id: i32) -> Option<Class<SelValue>> {
    use Class::{Bridge, Omit};
    Some(match id {
        // The ids after the first of each arm are the same property, or
        // give the same value on selection (`IntegerPropertyFactory.java`
        // at `0.8.9`: `:155-159`, `:175`, `:183`, `:187-188`, `:200-201`,
        // `:231`, `:237`).
        71 | 101 | 150 | 171 => Bridge(SelValue::Ex),
        // EX less the rival's, the target's or the best's, which are 0
        // here (`:186`, `:192`, `:197-198`, `:232`, `:702-710`).
        108 | 128 | 152 | 153 | 172 => Bridge(SelValue::ExDifference),
        72 => Bridge(SelValue::MaxEx),
        74 | 106 => Bridge(SelValue::Notes),
        76 | 177 => Bridge(SelValue::Bp),
        77 => Bridge(SelValue::Plays),
        90 => Bridge(SelValue::BpmMax),
        91 => Bridge(SelValue::BpmMin),
        45..=49 | 96 => Bridge(SelValue::Level),
        102 | 115 | 155 => Bridge(SelValue::RatePercent),
        103 | 116 | 156 => Bridge(SelValue::RateHundredths),
        154 => Bridge(SelValue::NextRank),
        300 => Bridge(SelValue::Charts),
        1163 => Bridge(SelValue::LengthMin),
        1164 => Bridge(SelValue::LengthSec),
        // The display offset (the source's judge timing, of the same
        // sign), the master gain standing for the source's system volume,
        // and Floating's travel duration and its green number.
        12 => Bridge(SelValue::DisplayOffset),
        57 => Bridge(SelValue::Volume),
        312 => Bridge(SelValue::Duration),
        313 => Bridge(SelValue::Green),
        // The application's readouts: the presents per second, the local
        // date and time and the uptime (`IntegerPropertyFactory.java:98-107`).
        20..=29 => Bridge(SelValue::App(u8::try_from(id - 20).ok()?)),
        // Session and global statistics; the volumes without their own
        // channel; what the bests do not keep; rivals and IR; per-lane-class
        // note counts the summary does not split; density metrics.
        17..=19
        | 30
        | 31
        | 58
        | 59
        | 75
        | 78
        | 92
        | 100
        | 105
        | 174
        | 110..=114
        | 179
        | 180
        | 227
        | 241
        | 271
        | 283
        | 284
        | 333
        | 350..=354
        | 360
        | 362..=365
        | 368
        | 380..=384
        | 410..=426 => Omit,
        _ => return None,
    })
}

/// A selection text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelText {
    /// The cursor's row and chart, as the chart fields read on selection.
    Field(TextField),
    Screen(SelectionText),
}

/// The selection text `id`.
pub(crate) fn text(id: i32) -> Option<Class<SelText>> {
    use Class::{Bridge, Omit};
    Some(match id {
        10 => Bridge(SelText::Field(TextField::Title)),
        11 => Bridge(SelText::Field(TextField::Subtitle)),
        12 => Bridge(SelText::Field(TextField::FullTitle)),
        13 => Bridge(SelText::Field(TextField::Genre)),
        16 => Bridge(SelText::Field(TextField::FullArtist)),
        30 => Bridge(SelText::Screen(SelectionText::Query)),
        1000 => Bridge(SelText::Screen(SelectionText::Place)),
        // Rivals, targets, IR, rankings and courses.
        1 | 3 | 120..=124 | 150..=154 | 201..=218 | 1020 => Omit,
        _ => return None,
    })
}

/// A selection option's bridge: an expression over the selection schema,
/// or a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelOption {
    Expr(&'static str),
    Constant(bool),
}

/// The rank conditions' expressions, by the source's id less 200 (and
/// 300): AAA first, the rank index counting down from 7.
const RANKS: [&str; 8] = [
    "selected.rate_valid && selected.rank == 7",
    "selected.rate_valid && selected.rank == 6",
    "selected.rate_valid && selected.rank == 5",
    "selected.rate_valid && selected.rank == 4",
    "selected.rate_valid && selected.rank == 3",
    "selected.rate_valid && selected.rank == 2",
    "selected.rate_valid && selected.rank == 1",
    "selected.rate_valid && selected.rank == 0",
];

/// The selected row's lamp conditions: the cursor on a chart, its lamp
/// the [`skin_model::LampKey`] index.
const LAMPS: [&str; 8] = [
    "cursor.chart && selected.lamp == 0",
    "cursor.chart && selected.lamp == 1",
    "cursor.chart && selected.lamp == 2",
    "cursor.chart && selected.lamp == 3",
    "cursor.chart && selected.lamp == 4",
    "cursor.chart && selected.lamp == 5",
    "cursor.chart && selected.lamp == 6",
    "cursor.chart && selected.lamp == 7",
];

/// The lamp condition of [`skin_model::LampKey`] index `k`.
fn lamp(k: usize) -> SelOption {
    SelOption::Expr(LAMPS.get(k).copied().unwrap_or("false"))
}

/// The rank condition of the source's rank offset `k` (0 for AAA).
fn rank(k: i32) -> SelOption {
    let k = usize::try_from(k).unwrap_or(usize::MAX);
    SelOption::Expr(RANKS.get(k).copied().unwrap_or("false"))
}

/// The selection option `id` (positive): a built-in boolean property of
/// the source's factory.
pub(crate) fn option(id: i32) -> Option<Class<SelOption>> {
    use Class::Bridge;
    use SelOption::{Constant, Expr};
    Some(Bridge(match id {
        1 => Expr("cursor.folder || cursor.all"),
        // Every listed chart is 7-key (S10-09c D1).
        2 | 5 | 160 => Expr("cursor.chart"),
        21 => Expr("on(panel1)"),
        22 => Expr("on(panel2)"),
        23 => Expr("on(panel3)"),
        // The selected row's lamp (docs/stage3.md S10-09d, D7): 100 no
        // play, 101 failed, 1101 the assist-easy gauge's clear, 102 easy,
        // 103 normal, 104 hard, 1102 ex-hard, 105 a full combo; 1100,
        // 1103 and 1104 are never met.
        100 => lamp(0),
        101 => lamp(1),
        1101 => lamp(2),
        102 => lamp(3),
        103 => lamp(4),
        104 => lamp(5),
        1102 => lamp(6),
        105 => lamp(7),
        1100 | 1103 | 1104 => Constant(false),
        // Other modes are not listed.
        161..=164 | 1160 | 1161 => Constant(false),
        176 => Expr("selected.summary && !selected.bpm_change"),
        177 => Expr("selected.bpm_change"),
        200..=207 => rank(id - 200),
        300..=307 => rank(id - 300),
        // Offline: no IR, no rivals.
        50 | 624 => Constant(true),
        // No grade rows, courses, IR, replays, option-specific clear
        // history, BGA, `#TEXT`, stage file or STOP facts, and no
        // judgment class on selection.
        3
        | 51
        | 60
        | 126..=131
        | 170
        | 171
        | 174
        | 175
        | 180..=184
        | 191
        | 197
        | 352..=354
        | 606
        | 625
        | 1002..=1007
        | 1010..=1017
        | 1128..=1131
        | 1177
        | 1197
        | 1200
        | 1203
        | 1205..=1208 => Constant(false),
        _ => return None,
    }))
}

/// The selection timer `id`: a slot of the selection schema.
pub(crate) fn timer(id: i32) -> Option<u16> {
    Some(match id {
        11 => selection_timer::CHANGE,
        21 => selection_timer::PANEL1,
        22 => selection_timer::PANEL2,
        23 => selection_timer::PANEL3,
        _ => return None,
    })
}

/// The selector of image index 370, the selected row's own clear type as
/// the source's lamp index (docs/stage3.md S10-09d, D7): its
/// [`skin_model::LampKey`] index, past the assist-option clear (2) from
/// assist-easy on; unavailable (−1, drawing nothing) on an unplayed chart
/// and on a row that is not a chart.
pub(crate) const CLEAR_TYPE: &str = "if cursor.chart && selected.played then \
     (if selected.lamp >= 2 then selected.lamp + 1 else selected.lamp) else -1";

/// The selection image index `id`: an image's or an image set's `ref`.
pub(crate) fn image_index(id: i32) -> Option<Class<&'static str>> {
    use Class::{Bridge, Omit, Unavailable};
    Some(match id {
        370 => Bridge(CLEAR_TYPE),
        // The settings: the source's sorter index (none for the
        // modification order), the displayed gauge's look, the fixed
        // hi-speed index and the covers' switches.
        12 => Bridge("settings.order"),
        40 => Bridge("settings.gauge"),
        55 => Bridge("settings.fix_hispeed"),
        330 => Bridge("if settings.sudden then 1 else 0"),
        331 => Bridge("if settings.lift then 1 else 0"),
        332 => Bridge("if settings.hidden then 1 else 0"),
        // Truthful constants, at the source's value for what tapestry has
        // or lacks: only 7-key charts are listed (`BEAT_7K`), no BGA is
        // drawn (`BGA_OFF`), no lane option, display-timing auto
        // adjustment or assist option exists, and long notes are LN.
        11 | 72 => Bridge("2"),
        42 | 43 | 54 | 75 | 301..=308 => Bridge("0"),
        // No image-index property on selection: the source's image draws
        // its entry 0.
        15 | 16 | 19 | 77 | 315..=318 => Bridge("0"),
        // The gauge auto shift, the judge algorithm, the bottom-shiftable
        // gauge and the hi-speed auto adjustment: no native equivalent.
        78 | 340..=342 => Unavailable,
        // The target's clear type.
        371 => Omit,
        _ => return None,
    })
}

/// The selection slider of `type` `ty`.
pub(crate) fn slider(ty: i32) -> Option<Class<RatioSource>> {
    use Class::{Bridge, Omit};
    Some(match ty {
        1 => Bridge(RatioSource::ListPosition),
        // The source's system volume: the master gain.
        17 => Bridge(RatioSource::MasterVolume),
        // The key and background volumes: no channel of their own.
        18 | 19 => Omit,
        _ => return None,
    })
}

/// The selection graph of `type` `ty`.
pub(crate) fn graph(ty: i32) -> Option<Class<RatioSource>> {
    use Class::{Bridge, Omit};
    Some(match ty {
        147 => Bridge(RatioSource::ExRate),
        17 => Bridge(RatioSource::MasterVolume),
        // Folder distributions, the other volumes and the judgment splits
        // the bests do not keep.
        -2 | -1 | 18 | 19 | 140..=144 => Omit,
        _ => return None,
    })
}

/// The selection event `id`: an image's `act` or `click`, a
/// `customEvents` action. None has a hit target before the native
/// actions exist.
pub(crate) fn event(id: i32) -> Option<Class<()>> {
    use Class::{Omit, Unavailable};
    Some(match id {
        // Sort, play, practice and the cover actions.
        12 | 15 | 315 | 330..=332 => Unavailable,
        // No native contract.
        11 | 13 | 14 | 16 | 17 | 19 | 308 | 316..=318 | 340..=342 => Omit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use skin_model::{SELECTION, Scope, Ty, compile};

    use super::*;

    /// The class letters of the profile table (docs/stage3.md §3,
    /// Selection): N, E, D, one row per id.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum C {
        N,
        E,
        D,
        /// A truthful constant (class D with its value).
        K(i32),
    }

    /// An image index's class: a bridge that is a whole number is a
    /// constant.
    fn of_index(c: Option<Class<&'static str>>) -> Option<C> {
        match c {
            Some(Class::Bridge(e)) => Some(e.parse().map_or(C::N, C::K)),
            c => of(c),
        }
    }

    fn of<T>(c: Option<Class<T>>) -> Option<C> {
        c.map(|c| match c {
            Class::Bridge(_) => C::N,
            Class::Unavailable => C::E,
            Class::Omit => C::D,
        })
    }

    /// Ranges of ids with one class.
    fn rows(spans: &[(i32, i32, C)]) -> Vec<(i32, C)> {
        spans
            .iter()
            .flat_map(|&(a, b, c)| (a..=b).map(move |id| (id, c)))
            .collect()
    }

    const NUMERIC: &[(i32, i32, C)] = &[
        (12, 12, C::N),
        (45, 49, C::N),
        (101, 101, C::N),
        (105, 105, C::D),
        (106, 106, C::N),
        (108, 108, C::N),
        (115, 116, C::N),
        (128, 128, C::N),
        (152, 153, C::N),
        (155, 156, C::N),
        (171, 172, C::N),
        (174, 174, C::D),
        (177, 177, C::N),
        (17, 19, C::D),
        (20, 29, C::N),
        (30, 31, C::D),
        (57, 57, C::N),
        (58, 59, C::D),
        (71, 72, C::N),
        (74, 74, C::N),
        (75, 75, C::D),
        (76, 77, C::N),
        (78, 78, C::D),
        (90, 91, C::N),
        (92, 92, C::D),
        (96, 96, C::N),
        (100, 100, C::D),
        (102, 103, C::N),
        (110, 114, C::D),
        (150, 150, C::N),
        (154, 154, C::N),
        (179, 180, C::D),
        (227, 227, C::D),
        (241, 241, C::D),
        (271, 271, C::D),
        (283, 284, C::D),
        (300, 300, C::N),
        (312, 313, C::N),
        (333, 333, C::D),
        (350, 354, C::D),
        (360, 360, C::D),
        (362, 365, C::D),
        (368, 368, C::D),
        (380, 384, C::D),
        (410, 426, C::D),
        (1163, 1164, C::N),
    ];

    const TEXT: &[(i32, i32, C)] = &[
        (1, 1, C::D),
        (3, 3, C::D),
        (10, 13, C::N),
        (16, 16, C::N),
        (30, 30, C::N),
        (120, 124, C::D),
        (150, 154, C::D),
        (201, 218, C::D),
        (1000, 1000, C::N),
        (1020, 1020, C::D),
    ];

    /// The image indices: N, E, D (omitted) or a truthful constant `K`.
    const IMAGE_INDEX: &[(i32, i32, C)] = &[
        (11, 11, C::K(2)),
        (12, 12, C::N),
        (15, 16, C::K(0)),
        (19, 19, C::K(0)),
        (40, 40, C::N),
        (42, 43, C::K(0)),
        (54, 54, C::K(0)),
        (55, 55, C::N),
        (72, 72, C::K(2)),
        (75, 75, C::K(0)),
        (77, 77, C::K(0)),
        (78, 78, C::E),
        (301, 308, C::K(0)),
        (315, 318, C::K(0)),
        (330, 332, C::N),
        (340, 342, C::E),
        (370, 370, C::N),
        (371, 371, C::D),
    ];

    const EVENT: &[(i32, i32, C)] = &[
        (11, 11, C::D),
        (12, 12, C::E),
        (13, 14, C::D),
        (15, 15, C::E),
        (16, 17, C::D),
        (19, 19, C::D),
        (308, 308, C::D),
        (315, 315, C::E),
        (316, 318, C::D),
        (330, 332, C::E),
        (340, 342, C::D),
    ];

    /// The options of the table: the needed ones and the decorative
    /// constants, each with its value where it is one.
    const OPTION_N: &[(i32, i32)] = &[
        (1, 2),
        (5, 5),
        (21, 23),
        (100, 105),
        (160, 164),
        (176, 177),
        (200, 207),
        (300, 307),
        (1100, 1104),
        (1160, 1161),
    ];
    const OPTION_D: &[(i32, i32, bool)] = &[
        (3, 3, false),
        (50, 50, true),
        (51, 51, false),
        (60, 60, false),
        (126, 131, false),
        (170, 171, false),
        (174, 175, false),
        (180, 184, false),
        (191, 191, false),
        (197, 197, false),
        (352, 354, false),
        (606, 606, false),
        (624, 624, true),
        (625, 625, false),
        (1002, 1007, false),
        (1010, 1017, false),
        (1128, 1131, false),
        (1177, 1177, false),
        (1197, 1197, false),
        (1200, 1200, false),
        (1203, 1203, false),
        (1205, 1208, false),
    ];

    /// Every id of the profile table gets its class, and no other id in
    /// a wide range has one: the explicit tables are exactly the table.
    #[test]
    fn every_profile_id_has_its_class() {
        for (name, table, f) in [
            (
                "numeric",
                NUMERIC,
                (|id| of(number(id))) as fn(i32) -> Option<C>,
            ),
            ("text", TEXT, |id| of(text(id))),
            ("image index", IMAGE_INDEX, |id| of_index(image_index(id))),
            ("event", EVENT, |id| of(event(id))),
        ] {
            let want = rows(table);
            for id in -10..=3_000 {
                let expected = want.iter().find(|&&(i, _)| i == id).map(|&(_, c)| c);
                assert_eq!(f(id), expected, "{name} {id}");
            }
        }
        let n = OPTION_N.iter().flat_map(|&(a, b)| a..=b);
        let d = OPTION_D.iter().flat_map(|&(a, b, _)| a..=b);
        assert_eq!(n.clone().count() + d.clone().count(), 95);
        for id in 0..=3_000 {
            let class = option(id);
            if n.clone().any(|i| i == id) {
                assert!(matches!(class, Some(Class::Bridge(_))), "option {id}");
            } else if let Some(&(.., v)) = OPTION_D.iter().find(|&&(a, b, _)| (a..=b).contains(&id))
            {
                assert_eq!(
                    class,
                    Some(Class::Bridge(SelOption::Constant(v))),
                    "option {id}"
                );
            } else {
                assert_eq!(class, None, "option {id}");
            }
        }
        for (id, slot) in [(11, 1), (21, 2), (22, 3), (23, 4)] {
            assert_eq!(timer(id), Some(slot));
        }
        assert_eq!((0..3_000).filter(|&id| timer(id).is_some()).count(), 4);
        assert_eq!(slider(1), Some(Class::Bridge(RatioSource::ListPosition)));
        assert_eq!(slider(17), Some(Class::Bridge(RatioSource::MasterVolume)));
        assert_eq!(
            (slider(18), slider(19)),
            (Some(Class::Omit), Some(Class::Omit))
        );
        assert_eq!(graph(147), Some(Class::Bridge(RatioSource::ExRate)));
        assert_eq!(graph(17), Some(Class::Bridge(RatioSource::MasterVolume)));
        for ty in [-2, -1, 18, 19, 140, 141, 142, 143, 144] {
            assert_eq!(graph(ty), Some(Class::Omit), "{ty}");
        }
    }

    /// Every bridge compiles against the selection schema with its type.
    #[test]
    fn every_bridge_compiles_against_the_selection_schema() {
        let scope = Scope::new(&SELECTION);
        for id in 0..=2_000 {
            if let Some(Class::Bridge(v)) = number(id) {
                compile(v.expr(), &scope, Ty::Int).unwrap();
                compile(v.availability(), &scope, Ty::Bool).unwrap();
            }
            if let Some(Class::Bridge(SelOption::Expr(e))) = option(id) {
                compile(e, &scope, Ty::Bool).unwrap();
                compile(&format!("!({e})"), &scope, Ty::Bool).unwrap();
            }
            if let Some(Class::Bridge(e)) = image_index(id) {
                compile(e, &scope, Ty::Int).unwrap();
            }
        }
        compile(CLEAR_TYPE, &scope, Ty::Int).unwrap();
    }
}
