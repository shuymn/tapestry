//! The result profile (docs/stage3.md §3, Result): one explicit `match`
//! per id space over every id the surveyed result skins read, each
//! classified. **N** ids are bridged from the result schema
//! ([`skin_model::RESULT`]): the viewed result, its timing statistics, the
//! best before this play and the chart's facts; **D** ids are a truthful
//! constant for a feature tapestry lacks (IR, replay slots, courses,
//! difficulty tables, lane options) or an omission with a note. An id
//! outside the survey follows the existing rules of its space, as on
//! selection.
//!
//! Every value is shown, hidden or unavailable. A bridge is an expression
//! and the validity it is drawn under: the importer joins the validity
//! into the visibility of each element that reads it, so an unavailable
//! value draws nothing; a hidden value reads `i32::MIN`, which only a
//! number does not draw. Every reader of an id goes through these
//! functions: a JSON field's `ref` or `op`, a Lua callback's read, and a
//! Lua read while the skin loads ([`crate::lua::facts`]), so they cannot
//! differ.

use skin_model::{RESULT, ResultFields, Scope, Ty, compile, result_timer};

use super::super::policy::EssentialPart;
use super::super::selection::profile::Class;
use super::super::timer::Timer;

/// The largest magnitude a shown value has.
const M: i64 = skin_model::RESULT_VALUE_MAX as i64;
/// What a hidden value reads.
const HIDDEN: i64 = skin_model::HIDDEN as i64;

/// A result number as the schema gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResValue {
    /// Its expression over the result schema.
    pub(crate) expr: &'static str,
    /// The validity it is drawn under.
    pub(crate) guard: &'static str,
    /// The least and greatest value of [`Self::expr`].
    pub(crate) range: (i64, i64),
    /// The essential part a number showing it is.
    pub(crate) part: Option<EssentialPart>,
}

const fn v(expr: &'static str, guard: &'static str, range: (i64, i64)) -> ResValue {
    ResValue {
        expr,
        guard,
        range,
        part: None,
    }
}

/// A count of the viewed result.
const fn count(expr: &'static str) -> ResValue {
    v(expr, VALID, (0, M))
}

const VALID: &str = "result.valid";
const BEST: &str = "best.known";

/// The mean timing deviation in whole ms and its hundredths, each
/// truncated toward 0 with the mean's sign, as the source's are
/// (`skin/property/IntegerPropertyFactory.java:422-433`): the expressions
/// floor, so a negative mean is divided by its magnitude.
const MEAN_WHOLE: &str = "if timing.mean_x100 < 0 then -(abs(timing.mean_x100) / 100) \
     else timing.mean_x100 / 100";
const MEAN_HUNDREDTHS: &str = "if timing.mean_x100 < 0 then -(abs(timing.mean_x100) % 100) \
     else timing.mean_x100 % 100";

/// The result number `id` (`main_state.number` and a value's `ref`).
pub(crate) fn number(id: i32) -> Option<Class<ResValue>> {
    use Class::{Bridge, Omit};
    let early_late = |id: i32| {
        // 410 to 419: PERFECT to POOR, early then late.
        const PAIRS: [&str; 10] = [
            "count.early.perfect",
            "count.late.perfect",
            "count.early.great",
            "count.late.great",
            "count.early.good",
            "count.late.good",
            "count.early.bad",
            "count.late.bad",
            "count.early.poor",
            "count.late.poor",
        ];
        usize::try_from(id - 410)
            .ok()
            .and_then(|i| PAIRS.get(i))
            .map(|e| count(e))
    };
    Some(Bridge(match id {
        71 | 171 => ResValue {
            part: Some(EssentialPart::ResultScore),
            ..count("ex_score")
        },
        // The chart's, shown without a result too
        // (`IntegerPropertyFactory.java:562-576`).
        74 => v("chart.notes", "chart.notes_valid", (0, M)),
        75 => count("max_combo"),
        76 => count("result.bp"),
        90 => v("chart.bpm_max_x100 / 100", "chart.bpm_valid", (0, M / 100)),
        91 => v("chart.bpm_min_x100 / 100", "chart.bpm_valid", (0, M / 100)),
        96 => v("chart.level", "chart.level_valid", (-M, M)),
        102 => v("rate.current_bp / 100", "result.rate_valid", (0, M / 100)),
        103 => v("rate.current_bp % 100", "result.rate_valid", (0, 99)),
        107 => v("gauge / 10", VALID, (0, 100)),
        407 => v("gauge % 10", VALID, (0, 9)),
        110 => count("count.perfect"),
        111 => count("count.great"),
        112 => count("count.good"),
        113 => count("count.bad"),
        114 => count("count.poor"),
        150 | 170 => v("best.ex", BEST, (0, M)),
        154 => count("result.next_rank"),
        172 => v("best.ex_diff", BEST, (-M, M)),
        // Hidden with no previous BP, as the source's `MIN_VALUE`
        // (`IntegerPropertyFactory.java:712-732`).
        176 => v("best.bp", BEST, (HIDDEN, M)),
        178 => v("best.bp_diff", BEST, (HIDDEN, M)),
        183 => v("best.rate_bp / 100", "best.rate_valid", (0, M / 100)),
        184 => v("best.rate_bp % 100", "best.rate_valid", (0, 99)),
        368 => v("chart.total", "chart.total_valid", (0, M)),
        370 => ResValue {
            part: Some(EssentialPart::ResultStatus),
            ..v("result.clear", VALID, (0, i64::from(skin_model::CLEAR_MAX)))
        },
        371 => v("best.clear", BEST, (0, i64::from(skin_model::CLEAR_MAX))),
        374 => v(MEAN_WHOLE, "timing.valid", (-M / 100, M / 100)),
        375 => v(MEAN_HUNDREDTHS, "timing.valid", (-99, 99)),
        410..=419 => early_late(id)?,
        420 => count("count.empty_poor"),
        421 => count("count.early.empty_poor"),
        422 => count("count.late.empty_poor"),
        423 => count("count.fast"),
        424 => count("count.slow"),
        // Each count bounded to half the range so the sum stays an `i32`:
        // exact for any chart, whose objects the parser caps far below.
        425 => v(
            "clamp(count.bad, 0, 1073741823) + clamp(count.poor, 0, 1073741823)",
            VALID,
            (0, M),
        ),
        1163 => v(
            "chart.length_ms / 60000 % 60",
            "chart.length_valid",
            (0, 59),
        ),
        1164 => v("chart.length_ms / 1000 % 60", "chart.length_valid", (0, 59)),
        // The clock is no result fact; no score target; no IR.
        21..=25 | 151 | 153 | 157 | 158 | 179 | 180 | 182 | 380..=399 => return Some(Omit),
        _ => return None,
    }))
}

/// The result text `id`: the chart's labels, as on play
/// ([`super::super::property::str_by_id`] reads them); the rest omitted.
pub(crate) fn text(id: i32) -> Option<Class<()>> {
    use Class::{Bridge, Omit};
    Some(match id {
        10 | 12 | 14 | 16 => Bridge(()),
        // The target player, IR rankings and name; the table's full
        // name, the source's empty string without difficulty tables.
        3 | 120..=129 | 1003 | 1020 => Omit,
        _ => return None,
    })
}

/// A result option's bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResOption {
    /// An expression over the result schema.
    Expr(&'static str),
    /// A truthful constant.
    Constant(bool),
    /// The judgment class (180 to 184): an import fact a Lua skin reads
    /// while it loads; the result schema has no field for it, so a frame
    /// condition on it is unsupported.
    JudgeClass,
}

/// The grade conditions by the source's id less 300 (and 320): AAA
/// first, the grade counting down from 7. Each is read under its group's
/// validity ([`option_guard`]).
const GRADES: [&str; 8] = [
    "result.grade == 7",
    "result.grade == 6",
    "result.grade == 5",
    "result.grade == 4",
    "result.grade == 3",
    "result.grade == 2",
    "result.grade == 1",
    "result.grade == 0",
];
const BEST_GRADES: [&str; 8] = [
    "best.grade == 7",
    "best.grade == 6",
    "best.grade == 5",
    "best.grade == 4",
    "best.grade == 3",
    "best.grade == 2",
    "best.grade == 1",
    "best.grade == 0",
];
const DIFFICULTIES: [&str; 6] = [
    "chart.difficulty == 0",
    "chart.difficulty == 1",
    "chart.difficulty == 2",
    "chart.difficulty == 3",
    "chart.difficulty == 4",
    "chart.difficulty == 5",
];

fn from(table: &'static [&'static str], k: i32) -> ResOption {
    let k = usize::try_from(k).unwrap_or(usize::MAX);
    ResOption::Expr(table.get(k).copied().unwrap_or("false"))
}

/// The result option `id` (positive): a built-in boolean property of the
/// source's factory. An expression holds only under its group's validity
/// ([`option_guard`]), which every reader joins outside it, so a negated
/// option on a view without the value holds neither (D7).
pub(crate) fn option(id: i32) -> Option<Class<ResOption>> {
    use ResOption::{Constant, Expr};
    Some(Class::Bridge(match id {
        // The emphasized gauge cleared, or not (`BooleanPropertyFactory.java:
        // 648-657`); neither while the view has no result.
        90 => Expr("result.cleared"),
        91 => Expr("!result.cleared"),
        150..=155 => from(&DIFFICULTIES, id - 150),
        // Every chart is 7-key.
        160 => Constant(true),
        161..=164 => Constant(false),
        177 => Expr("chart.bpm_changes"),
        180..=184 => ResOption::JudgeClass,
        300..=307 => from(&GRADES, id - 300),
        320..=327 => from(&BEST_GRADES, id - 320),
        330 => Expr("best.ex_updated"),
        332 => Expr("best.bp_updated"),
        335 => Expr("best.rate_updated"),
        2244 => Expr("count.bad > 0"),
        2245 => Expr("count.poor > 0"),
        // Offline: no IR.
        50 => Constant(true),
        51 => Constant(false),
        // No replay slots: each slot absent.
        196 | 1196 | 1199 | 1202 => Constant(true),
        197 | 198 | 1197 | 1198 | 1200 | 1201 | 1203 | 1204 => Constant(false),
        // No stage file shown, no courses, no difficulty tables.
        191 | 280..=283 | 289 | 290 | 1008 => Constant(false),
        _ => return None,
    }))
}

/// The result timer `id` (built in): the fade-out, the IR's never set.
pub(crate) fn timer(id: i32) -> Timer {
    match id {
        2 => Timer::Builtin(result_timer::FADEOUT),
        // Nothing sets timer 0; the IR's success and failure, with no IR.
        0 | 173 | 174 => Timer::Unset,
        _ => Timer::Unsupported,
    }
}

/// The new clear type's selector, drawing nothing while the view has no
/// result.
pub(crate) const CLEAR_TYPE: &str = "if result.valid then result.clear else -1";

/// The result image index `id`: an image's or an image set's `ref`.
pub(crate) fn image_index(id: i32) -> Option<Class<&'static str>> {
    use Class::Bridge;
    Some(match id {
        370 => Bridge(CLEAR_TYPE),
        371 => Bridge("if best.known then best.clear else -1"),
        // No lane option or shuffle: the source answers 0
        // (`IntegerPropertyFactory.java:1136-1143`, `:1436-1482`).
        42 | 450..=457 => Bridge("0"),
        _ => return None,
    })
}

/// Whether image index `id` shows the clear status, an essential part.
pub(crate) fn status_index(id: i32) -> bool {
    id == 370
}

/// The result event `id`: no action has a hit target on the result, so
/// an image's `act` is ignored with a note whatever it names, as on play;
/// this table records the surveyed one's class.
#[cfg(test)]
pub(crate) fn event(id: i32) -> Option<Class<()>> {
    // The lane option's cycle is the select screen's
    // (`EventFactory.java:280-286`): a no-op here.
    (id == 42).then_some(Class::Omit)
}

/// A value a skin reads while it loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// Shown, or hidden as `i32::MIN`.
    Value(i32),
    /// Unavailable for the view: the read refuses.
    Unavailable,
}

/// Number `value` read from `fields`, as a frame reads it: its validity
/// first, then its expression.
pub(crate) fn read_number(value: &ResValue, fields: &ResultFields) -> Read {
    read(value.guard, value.expr, Ty::Int, fields)
}

/// The validity option `id` (positive) holds under: its group's, `true`
/// for an option of no group. A frame joins it outside the option's
/// expression and any negation or callback around it, so it draws nothing
/// without it; a read while the skin loads refuses without it instead, as
/// a number's does.
pub(crate) fn option_guard(id: i32) -> &'static str {
    match id {
        90 | 91 | 2244 | 2245 => VALID,
        300..=307 => "result.rate_valid",
        320..=327 | 335 => "best.rate_valid",
        330 | 332 => BEST,
        177 => "chart.bpm_valid",
        _ => "true",
    }
}

/// Option `id` (positive) read from `fields` while a skin loads: its
/// expression as a frame reads it, 0 or 1, under its group's validity
/// ([`option_guard`]); unavailable for an id with no expression.
pub(crate) fn read_option(id: i32, fields: &ResultFields) -> Read {
    match option(id) {
        Some(Class::Bridge(ResOption::Expr(src))) => read(option_guard(id), src, Ty::Bool, fields),
        _ => Read::Unavailable,
    }
}

fn read(guard: &str, src: &str, ty: Ty, fields: &ResultFields) -> Read {
    let scope = Scope::new(&RESULT);
    let state = fields.state();
    // The guard first: an unavailable value compiles nothing more.
    let Ok(g) = compile(guard, &scope, Ty::Bool) else {
        return Read::Unavailable;
    };
    if !g.eval_bool(&state) {
        return Read::Unavailable;
    }
    let Ok(p) = compile(src, &scope, ty) else {
        return Read::Unavailable;
    };
    match ty {
        Ty::Bool => Read::Value(i32::from(p.eval_bool(&state))),
        _ => Read::Value(p.eval(&state)),
    }
}

#[cfg(test)]
mod tests;
