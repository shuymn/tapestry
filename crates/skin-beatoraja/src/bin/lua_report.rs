//! The Lua conversion report (docs/stage3.md §3, the conversion report),
//! for the developer's PC:
//! `lua_report [--autoplay on|off]
//! [--class veryhard|hard|normal|easy|veryeasy|none] [--level <n>|pending]
//! [--result dense] [--option <name> <item>]... [--file <name> <candidate>]...
//! [--local] <root> <entry> [<root> <entry>]...`
//! imports each Lua skin (or JSON skin, an `entry` ending in `.json`,
//! which reports its outcome, kind, notes and frames alone), `entry`
//! relative to its skin folder `root`,
//! with its default configuration, but for what `--option` and `--file`
//! name, on the scene of the kind its header names, against import facts
//! with autoplay, the chart's judgment class and its play level as given
//! (`none` for a chart in no class, `pending` for a level the base
//! snapshot has pending; each
//! unavailable when not given) and, with `--result dense`, a result
//! scene's view known: a ready result with a known best, its chart of
//! the judgment class given, dense timelines (512 bins of 2 s, every
//! channel 9 and 19 notes unjudged), a gauge
//! history crossing its border in every column and a deviation in every
//! ms slot, over which a result skin that imports is then drawn (its
//! widget charge, quad capacity, the most quads a frame drew, the quads
//! dropped and the chip quads), as a decision skin that imports is drawn
//! over its declared scene and fade-out with placeholder labels; and
//! prints its kind, what did not convert
//! as allowlisted counts and outcomes, how many script and unread-entry
//! roots its configured table holds, a record per callback consumer
//! (where it is defined, its field, role, disposition and reason, why a
//! callback classified but never consumed was left, and each inert
//! function the source never reads), and the import
//! facts it read and recorded, each skin by its index in the arguments and
//! each file by its index among those it consulted, then the skins per
//! kind (`none` for a header refused). It prints no source, name, path,
//! string or Lua error, except under `--local`: each record is then
//! followed by a `local` line with its file, the destination it was
//! consumed for (its index, the kind its `id` names and that `id`) and,
//! when it lowered, whether its generated expression is a constant, that
//! expression and its guards, and the report adds the configuration as
//! resolved and a `destination` line per destination with what the import
//! made of it and its notes. That output stays on the developer's PC.
//! Each imported gauge widget is listed with its segments and motion.
//!
//! `lua_report --catalogue <root> <entry> [<root> <entry>]...` prints, per
//! skin, its header as the importer parses it, one tab-separated line per
//! named option item (`option`, the option, the item) and per named file
//! choice candidate (`file`, the choice, the candidate): the
//! configurations a sweep reports each of. It names the skin's own
//! options, so it too stays on the developer's PC.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use bms_model::TimeUs;
use skin_beatoraja::Cancel;
use skin_beatoraja::lua::facts::{Fact, ImportFacts, JudgeClass};
use skin_beatoraja::lua::survey::{
    Choice, DestinationEntry, Local, Survey, survey_json, survey_with,
};
use skin_model::{
    BestFigures, BestRate, BestView, DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeHistory,
    GaugeLook, LampKey, ResultChart, ResultFields, ResultNumber, ResultScore, ResultStatus,
    ResultView, ScreenKind, Timelines, TimingStats,
};

const USAGE: &str = "usage: lua_report [--autoplay on|off] \
                     [--class veryhard|hard|normal|easy|veryeasy|none] \
                     [--level <n>|pending] [--result dense] [--option <name> <item>]... \
                     [--file <name> <candidate>]... [--local] <root> <entry> [<root> <entry>]...\n\
                     \x20      lua_report --catalogue <root> <entry> [<root> <entry>]...";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The snapshot the flags at the front of `args` give, its scene each
/// skin's own, the configuration they name, whether `--local` asks for the
/// developer-local detail, and the arguments after them; `None` for a flag
/// it does not read.
fn facts(mut args: &[String]) -> Option<(ImportFacts, Choice, bool, &[String])> {
    let mut facts = ImportFacts::unavailable(ScreenKind::Play);
    let mut choice = Choice::default();
    let mut local = false;
    loop {
        match args {
            [flag, name, item, rest @ ..] if flag == "--option" => {
                choice.options.push((name.clone(), item.clone()));
                args = rest;
            }
            [flag, name, candidate, rest @ ..] if flag == "--file" => {
                choice.files.push((name.clone(), candidate.clone()));
                args = rest;
            }
            [flag, rest @ ..] if flag == "--local" => {
                local = true;
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--result" => {
                if v != "dense" {
                    return None;
                }
                let view = dense_view();
                facts.result = Fact::Known(ResultFields::of(&view));
                choice.result = Some(view);
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--autoplay" => {
                facts.autoplay = Fact::Known(match v.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return None,
                });
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--level" => {
                facts.play_level = match v.as_str() {
                    "pending" => Fact::Pending,
                    n => Fact::Known(n.parse().ok()?),
                };
                args = rest;
            }
            [flag, c, rest @ ..] if flag == "--class" => {
                facts.judge_class = Fact::Known(match c.as_str() {
                    "none" => None,
                    "veryhard" => Some(JudgeClass::VeryHard),
                    "hard" => Some(JudgeClass::Hard),
                    "normal" => Some(JudgeClass::Normal),
                    "easy" => Some(JudgeClass::Easy),
                    "veryeasy" => Some(JudgeClass::VeryEasy),
                    _ => return None,
                });
                args = rest;
            }
            [flag, ..] if flag.starts_with("--") => return None,
            _ => {
                // A dense view shows the class `--class` gives, whatever
                // the flags' order, as the attempt's chart carries it.
                if let (Some(view), Fact::Known(class)) = (&mut choice.result, facts.judge_class) {
                    view.chart.judge_class = class.map(JudgeClass::index);
                    facts.result = Fact::Known(ResultFields::of(view));
                }
                return Some((facts, choice, local, args));
            }
        }
    }
}

/// The view `--result dense` gives: a ready result with every best value
/// known, timing statistics, dense timelines (512 bins of 2 s, each
/// channel 9 and 100 notes a bin, 19 of them unjudged), a gauge history
/// whose every column crosses the border (an even column rising from 70 %
/// to 90 %, an odd one falling back), and a deviation in every ms slot
/// (up to 250) over 7-key judge areas.
fn dense_view() -> ResultView {
    let pct = |p: i64| rules::GaugeValue(p * rules::GaugeValue::PERCENT);
    let column = |i: usize| {
        let (low, high) = (pct(70), pct(90));
        let last = if i.is_multiple_of(2) { high } else { low };
        GaugeColumn { low, high, last }
    };
    let gauge = GaugeHistory {
        columns: std::array::from_fn(column),
        max: pct(100),
        border: pct(80),
        look: GaugeLook::Normal,
        reached: u16::try_from(GRAPH_COLUMNS).unwrap_or(u16::MAX),
        no_input_from: Some(200),
    };
    let deviations: [u32; DEVIATION_SLOTS] =
        std::array::from_fn(|i| 1 + u32::try_from(i % 250).unwrap_or(0));
    let bins = vec![[9; skin_model::TIMELINE_CHANNELS]; skin_model::MAX_TIMELINE_BINS];
    let notes = vec![100; skin_model::MAX_TIMELINE_BINS];
    ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score: ResultScore {
            clear: LampKey::Normal,
            cleared: true,
            ..ResultScore::default()
        },
        best: BestView::Known(BestFigures {
            ex: 900,
            ex_diff: 100,
            bp: ResultNumber::Shown(23),
            bp_diff: ResultNumber::Shown(-3),
            clear: LampKey::Hard,
            ex_updated: true,
            bp_updated: true,
            rate: Some(BestRate {
                rate_bp: 7_500,
                grade: 5,
                updated: true,
            }),
        }),
        timing: Some(TimingStats {
            samples: 500,
            mean_x100: -149,
            sd_x100: 1_234,
        }),
        timelines: Timelines::new(TimeUs(2_000_000), bins, notes, None).ok(),
        gauge: Some(Box::new(gauge)),
        deviations: Some(Box::new(deviations)),
        timing_areas: Some([[-20, 20], [-60, 60], [-160, 160], [-220, 220], [-500, 150]]),
    }
}

/// A record's `local` line: its file, the destination it was consumed
/// for, and what it lowered to.
fn local_line(l: &Local) -> String {
    let mut s = format!("    local file {:?}", l.file);
    match &l.element {
        Some(e) => {
            let kind = e.kind.as_deref().unwrap_or("?");
            let _ = write!(s, " element {} {kind}", e.index);
            if let Some(id) = &e.id {
                let _ = write!(s, " id {id:?}");
            }
        }
        None => s.push_str(" element -"),
    }
    if let Some(l) = &l.lowered {
        let constant = if l.constant { "constant" } else { "expression" };
        let _ = write!(s, " lowered {constant} {:?}", l.src);
        if !l.guards.is_empty() {
            let _ = write!(s, " guards {:?}", l.guards.join(" && "));
        }
    }
    s
}

/// A destination's `destination` line and its `note` lines.
fn destination_lines(e: &DestinationEntry) -> String {
    let id =
        e.id.as_deref()
            .map_or_else(|| "-".to_owned(), |i| format!("{i:?}"));
    let kind = e.kind.as_deref().unwrap_or("-");
    let mut s = format!("  destination {} {id} {kind} {:?}", e.index, e.became);
    if !e.elements.is_empty() {
        let _ = write!(s, " elements {}..{}", e.elements.start, e.elements.end);
    }
    if e.suppressed > 0 {
        let _ = write!(s, " suppressed {}", e.suppressed);
    }
    s.push('\n');
    for n in &e.notes {
        let _ = write!(s, "    note {:?} {}", n.severity, n.kind);
        if let Some((file, line, col)) = &n.at {
            let _ = write!(s, " file {file:?} line {line} column {col}");
        }
        s.push('\n');
    }
    s
}

/// Skin `index`'s report, its `local` lines included when they are there.
fn report(index: usize, survey: &Survey) -> String {
    let mut s = String::new();
    let mut line = |l: &str| {
        s.push_str(l);
        s.push('\n');
    };
    line(&format!("skin {index}: {}", survey.outcome));
    let kind = survey
        .kind
        .map_or_else(|| "none".to_owned(), |k| format!("{k:?}"));
    line(&format!("  kind: {kind}"));
    if let Some((module, line_no, col)) = survey.at {
        line(&format!(
            "  at: module {module}, line {line_no}, column {col}"
        ));
    }
    line(&format!("  modules: {}", survey.modules));
    for a in &survey.atlas {
        line(&format!("  atlas: {a}"));
    }
    if let Some((ops, callbacks)) = survey.frame_ops {
        line(&format!(
            "  frame: operations {ops} of {}, program callbacks {callbacks}",
            skin_model::MAX_SKIN_OPS
        ));
    }
    for (segments, motion) in &survey.gauges {
        line(&format!("  gauge segments {segments} motion {motion}"));
    }
    let u = survey.usage;
    line(&format!(
        "  usage: instructions {} host calls {} native work {} loads {} (require {}, dofile {}) \
         input bytes {} protected calls {} metatables {} peak memory {} \
         ast nodes {} captures {} expanded {}",
        u.instructions,
        u.host_calls,
        u.native_work,
        u.loads,
        u.requires,
        u.dofiles,
        u.input_bytes,
        u.protected_calls,
        u.metatables,
        u.peak_memory,
        u.ast_nodes,
        u.captures,
        u.expanded
    ));
    for ((field, kind), n) in &survey.callables {
        line(&format!("  callable {field} {kind}: {n}"));
    }
    line(&format!("  root script: {}", survey.root_script));
    line(&format!("  root unread: {}", survey.root_unread));
    if !survey.effects.is_empty() {
        let at = survey
            .effects_at
            .map_or_else(String::new, |(module, line, col)| {
                format!(" at module {module}, line {line}, column {col}")
            });
        line(&format!("  effects: {}{at}", survey.effects));
    }
    for (outcome, n) in &survey.outcomes {
        line(&format!("  callback {outcome}: {n}"));
    }
    if let Some(rt) = &survey.runtime {
        let status: Vec<String> = rt.status.iter().map(|(s, n)| format!("{s} {n}")).collect();
        line(&format!(
            "  runtime frame (fields 0): outputs {} status {} notes {} (+{}) \
             instructions {} host calls {} time {} us{}",
            rt.outputs,
            status.join(", "),
            rt.notes.0,
            rt.notes.1,
            rt.instructions,
            rt.host_calls,
            rt.micros,
            rt.stopped
                .as_deref()
                .map_or_else(String::new, |s| format!(" stopped {s}"))
        ));
    }
    for c in &survey.consumers {
        let at = c.at.map_or_else(
            || "module ?".to_owned(),
            |(module, line, _)| format!("module {module}, line {line}"),
        );
        let reason = c
            .reason
            .as_deref()
            .map_or_else(String::new, |r| format!(" ({r})"));
        line(&format!(
            "  consumer {at}: {} {} {}{reason}",
            c.field, c.role, c.disposition
        ));
        if let Some(l) = &c.local {
            line(&local_line(l));
        }
    }
    for (kind, n) in &survey.notes {
        line(&format!("  note {kind}: {n}"));
    }
    if let Some(r) = &survey.resolved {
        for (name, item, op) in &r.options {
            line(&format!("  resolved option {name:?} {item:?} {op:?}"));
        }
        for (name, candidate) in &r.files {
            line(&format!("  resolved file {name:?} {candidate:?}"));
        }
    }
    for e in &survey.destinations {
        for l in destination_lines(e).lines() {
            line(l);
        }
    }
    if let Some(f) = &survey.frames {
        line(&format!(
            "  frames: widget charge {} capacity {} most quads {} dropped {} chips {}",
            f.widget_quads, f.capacity, f.most, f.dropped, f.chips
        ));
    }
    for (label, n) in &survey.facts {
        line(&format!("  fact {label}: {n}"));
    }
    for r in &survey.reads {
        line(&format!("  recorded {:?} = {:?}", r.fact, r.value));
    }
    s
}

/// The survey of `entry` in `root`: a JSON skin (an entry ending in
/// `.json`) takes the JSON import, every other entry the Lua one.
fn survey_entry(
    root: &str,
    entry: &str,
    facts: &ImportFacts,
    cancel: &Cancel,
    local: bool,
    choice: &Choice,
) -> Survey {
    if entry.ends_with(".json") {
        survey_json(&PathBuf::from(root), entry, choice)
    } else {
        survey_with(&PathBuf::from(root), entry, facts, cancel, local, choice)
    }
}

/// The header catalogue of each `<root> <entry>` pair in `args`, as
/// `--catalogue` prints it; a header refused prints its refusal's label.
fn catalogue(args: &[String]) -> String {
    let mut s = String::new();
    for pair in args.chunks(2) {
        let [root, entry] = pair else { continue };
        let _ = writeln!(s, "skin\t{root}\t{entry}");
        let h = match skin_beatoraja::read_lua_header(&PathBuf::from(root), entry, &Cancel::new()) {
            Ok(h) => h,
            Err(e) => {
                let _ = writeln!(s, "refused\t{e}");
                continue;
            }
        };
        let header = h.header();
        for o in &header.options {
            let Some(name) = &o.name else { continue };
            for item in o.items.iter().filter_map(|i| i.name.as_ref()) {
                let _ = writeln!(s, "option\t{name}\t{item}");
            }
        }
        for f in &header.files {
            let Some(name) = &f.name else { continue };
            for c in &f.candidates {
                let _ = writeln!(s, "file\t{name}\t{c}");
            }
        }
    }
    s
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    if let Some((flag, args)) = all.split_first()
        && flag == "--catalogue"
    {
        if args.is_empty() || !args.len().is_multiple_of(2) {
            usage();
        }
        print!("{}", catalogue(args));
        return;
    }
    let Some((facts, choice, local, args)) = facts(&all) else {
        usage()
    };
    if args.is_empty() || !args.len().is_multiple_of(2) {
        usage();
    }
    let mut kinds: BTreeMap<String, u32> = BTreeMap::new();
    for (index, pair) in args.chunks(2).enumerate() {
        let [root, entry] = pair else { continue };
        let cancel = Cancel::new();
        let survey = survey_entry(root, entry, &facts, &cancel, local, &choice);
        print!("{}", report(index, &survey));
        let kind = survey
            .kind
            .map_or_else(|| "none".to_owned(), |k| format!("{k:?}"));
        *kinds.entry(kind).or_default() += 1;
    }
    for (kind, n) in &kinds {
        println!("kind {kind}: {n}");
    }
}

#[cfg(test)]
mod tests {
    use skin_beatoraja::lua::facts::{Fact, JudgeClass};
    use skin_beatoraja::lua::survey::{
        Became, Choice, Consumer, DestinationEntry, DestinationNote, Element, Local, Lowered,
        Resolved, Survey, survey_json,
    };
    use skin_beatoraja::{NoteKind, PropertyKind, Severity};

    use skin_beatoraja::Cancel;
    use skin_beatoraja::lua::facts::ImportFacts;
    use skin_beatoraja::lua::survey::survey_with;
    use skin_model::ScreenKind;

    use super::{facts, report};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_flags_set_the_snapshot_and_leave_the_skins() {
        let a = args("--autoplay off --class easy --level 12 root s.luaskin");
        let (f, _, local, rest) = facts(&a).unwrap();
        assert_eq!(f.autoplay, Fact::Known(false));
        assert_eq!(f.judge_class, Fact::Known(Some(JudgeClass::Easy)));
        assert_eq!(f.play_level, Fact::Known(12));
        assert!(!local);
        assert_eq!(rest, ["root", "s.luaskin"]);
        let a = args("--level pending root s.luaskin");
        let (f, ..) = facts(&a).unwrap();
        assert_eq!(f.play_level, Fact::Pending);
        assert_eq!(f.judge_class, Fact::Unavailable);
        let a = args("--class none root s");
        assert_eq!(facts(&a).unwrap().0.judge_class, Fact::Known(None));
        let a = args("root s.luaskin");
        assert_eq!(facts(&a).unwrap().0.play_level, Fact::Unavailable);
    }

    /// A result skin's callback no expression represents is reported as
    /// the runtime's, and one runtime frame over a zero state is printed.
    #[test]
    fn a_runtime_callback_is_reported_and_run_once() {
        let a = args("root s.luaskin");
        let (f, choice, _, _) = facts(&a).unwrap();
        let d = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255; 4]))
            .save(d.path().join("a.png"))
            .unwrap();
        std::fs::write(
            d.path().join("s.luaskin"),
            "local skin = { type = 7, name = \"t\", w = 1920, h = 1080 }\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             skin.source = { { id = \"a\", path = \"a.png\" } }\n\
             skin.image = { { id = \"dot\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
             skin.value = { { id = \"ex\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, \
                 digit = 4, ref = 71 },\n\
               { id = \"r\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, digit = 4, \
                 value = function() return ms.number(71) / 2 end } }\n\
             skin.destination = {\n\
               { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
               { id = \"r\", dst = { { x = 0, y = 120, w = 4, h = 8 } } },\n\
               { id = \"dot\", op = { 90 }, dst = { { x = 0, y = 300, w = 8, h = 8 } } } }\n\
             return skin\n",
        )
        .unwrap();
        let f = ImportFacts {
            scene: ScreenKind::Result,
            ..f
        };
        // The frame's time budget out of reach: the call's status is not
        // the clock's to decide.
        skin_beatoraja::lua::vm_hooks::set_frame_time(Some(std::time::Duration::from_secs(3_600)));
        let s = survey_with(d.path(), "s.luaskin", &f, &Cancel::new(), false, &choice);
        assert_eq!(s.outcome, "imported");
        assert_eq!(s.outcomes.get("runtime"), Some(&1), "{:?}", s.outcomes);
        let rt = s.runtime.as_ref().unwrap();
        assert_eq!(rt.outputs, 1);
        assert_eq!(rt.status.get("ok"), Some(&1));
        assert!(rt.host_calls >= 1);
        let printed = report(0, &s);
        assert!(
            printed.contains("  runtime frame (fields 0): outputs 1 status ok 1 "),
            "{printed}"
        );
    }

    /// `--result dense` makes a result scene's view known, and a result
    /// skin that imports with it is drawn over it: its judge graph's chips,
    /// every bin's grey and channels, are in the frame, nothing dropped.
    #[test]
    fn a_result_import_with_the_result_flag_draws_its_judge_graph() {
        let a = args("--result dense root s.luaskin");
        let (f, choice, _, rest) = facts(&a).unwrap();
        assert!(matches!(f.result, Fact::Known(_)));
        assert!(choice.result.is_some());
        assert_eq!(rest, ["root", "s.luaskin"]);
        let d = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255; 4]))
            .save(d.path().join("a.png"))
            .unwrap();
        std::fs::write(
            d.path().join("s.luaskin"),
            "local skin = { type = 7, name = \"t\", w = 1920, h = 1080 }\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             local gap = ms.number(178) < 0 and 0 or 10\n\
             skin.source = { { id = \"a\", path = \"a.png\" } }\n\
             skin.image = { { id = \"dot\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
             skin.value = { { id = \"ex\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, \
                 digit = 4, ref = 71 } }\n\
             skin.judgegraph = { { id = \"g\", type = 2 } }\n\
             skin.destination = {\n\
               { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
               { id = \"dot\", op = { 90 }, dst = { { x = gap, y = 300, w = 8, h = 8 } } },\n\
               { id = \"g\", dst = { { x = 0, y = 400, w = 520, h = 82 } } } }\n\
             return skin\n",
        )
        .unwrap();
        let s = survey_with(d.path(), "s.luaskin", &f, &Cancel::new(), false, &choice);
        assert_eq!(s.outcome, "imported");
        let frames = s.frames.unwrap();
        assert_eq!(frames.chips, 512 * 10);
        assert_eq!(frames.dropped, 0);
        assert!(frames.widget_quads >= 5_702);
        let printed = report(0, &s);
        assert!(printed.contains("  frames: widget charge "), "{printed}");
        // Without the flag the best's BP difference is unavailable: the
        // skin reading it while it loads stops the import.
        let plain_args = args("root s.luaskin");
        let plain = facts(&plain_args).unwrap();
        let s = survey_with(
            d.path(),
            "s.luaskin",
            &plain.0,
            &Cancel::new(),
            false,
            &plain.1,
        );
        assert_ne!(s.outcome, "imported");
        assert!(s.frames.is_none());
    }

    /// A JSON result skin takes the JSON import, and with `--result dense`
    /// its gauge graph and distribution draw over the dense view: the
    /// gauge graph's back and every column's line, the distribution's
    /// cut columns, nothing dropped.
    #[test]
    fn a_json_result_skin_draws_its_graphs_over_the_dense_view() {
        let a = args("--result dense root s.json");
        let (_, choice, _, _) = facts(&a).unwrap();
        let d = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(64, 64, image::Rgba([255; 4]))
            .save(d.path().join("a.png"))
            .unwrap();
        std::fs::write(
            d.path().join("s.json"),
            r#"{"type": 7, "w": 1920, "h": 1080,
                "source": [{"id": "a", "path": "a.png"}],
                "image": [{"id": "dot", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8}],
                "value": [{"id": "ex", "src": "a", "x": 0, "y": 0, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}],
                "gaugegraph": [{"id": "gg"}],
                "timingdistributiongraph": [{"id": "td"}],
                "destination": [
                    {"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
                    {"id": "dot", "op": [90], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]},
                    {"id": "gg", "dst": [{"x": 0, "y": 500, "w": 576, "h": 200}]},
                    {"id": "td", "dst": [{"x": 0, "y": 800, "w": 301, "h": 100}]}]}"#,
        )
        .unwrap();
        let (f, ..) = facts(&a).unwrap();
        let root = d.path().to_str().unwrap();
        let s = super::survey_entry(root, "s.json", &f, &Cancel::new(), false, &choice);
        assert_eq!(s.outcome, "imported");
        assert_eq!(s.kind, Some(ScreenKind::Result));
        let frames = s.frames.unwrap();
        assert_eq!(frames.dropped, 0);
        // The two graphs and the four-digit number.
        assert_eq!(frames.widget_quads, 1_443 + 1_516 + 4);
        // Both graphs drew: at least two pieces of each of the gauge graph's
        // 288 line columns and of the distribution's 301 barred
        // columns.
        assert!(frames.most > 288 * 2 + 301 * 2, "{}", frames.most);
        assert!(
            survey_json(d.path(), "s.json", &Choice::default())
                .frames
                .is_none()
        );
    }

    #[test]
    fn local_is_a_flag_of_its_own_among_the_others() {
        for a in ["--local root s", "--autoplay on --local --level 3 root s"] {
            let a = args(a);
            let (_, _, local, rest) = facts(&a).unwrap();
            assert!(local);
            assert_eq!(rest, ["root", "s"]);
        }
    }

    #[test]
    fn options_and_files_are_named_in_pairs() {
        let a = args("--option fc on --local --file frame x --option g y root s");
        let (_, choice, local, rest) = facts(&a).unwrap();
        assert!(local);
        assert_eq!(rest, ["root", "s"]);
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(choice.options, [pair("fc", "on"), pair("g", "y")]);
        assert_eq!(choice.files, [pair("frame", "x")]);
    }

    #[test]
    fn a_bad_flag_or_value_is_refused() {
        for a in [
            "--level x root s",
            "--level 2147483648 root s",
            "--level",
            "--autoplay maybe root s",
            "--class 2 root s",
            "--result ready root s",
            "--rank 2 root s",
            "--other 1 root s",
            "--locals root s",
        ] {
            assert!(facts(&args(a)).is_none(), "{a}");
        }
    }

    fn consumer(disposition: &str, local: Option<Local>) -> Consumer {
        Consumer {
            at: Some((1, 7, 3)),
            field: "draw",
            role: "decorative".to_owned(),
            disposition: disposition.to_owned(),
            reason: None,
            local,
        }
    }

    fn element(index: usize, kind: &str) -> Option<Element> {
        Some(Element {
            index,
            id: Some("x".to_owned()),
            kind: Some(kind.to_owned()),
        })
    }

    fn lowered(src: &str, guards: &[&'static str], constant: bool) -> Option<Lowered> {
        Some(Lowered {
            src: src.to_owned(),
            guards: guards.to_vec(),
            constant,
        })
    }

    #[test]
    fn without_local_detail_the_report_is_unchanged() {
        let s = Survey {
            outcome: "imported".to_owned(),
            consumers: vec![consumer("lowered", None)],
            ..Survey::default()
        };
        assert_eq!(
            report(0, &s),
            "skin 0: imported\n  kind: none\n  modules: 0\n  \
             usage: instructions 0 host calls 0 native work 0 loads 0 (require 0, dofile 0) \
             input bytes 0 protected calls 0 metatables 0 peak memory 0 \
             ast nodes 0 captures 0 expanded 0\n  \
             root script: 0\n  root unread: 0\n  \
             consumer module 1, line 7: draw decorative lowered\n"
        );
    }

    #[test]
    fn a_valid_choice_without_local_detail_prints_nothing_new() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("s.luaskin"),
            "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
               property = { { name = \"fc\", item = { { name = \"off\", op = 905 }, { name = \"on\", op = 906 } } } },\n\
               filepath = { { name = \"bg\", path = \"*.png\", def = \"a\" } } }\n\
             if not skin_config then return skin end\n\
             skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
             return skin\n",
        )
        .unwrap();
        for name in ["a.png", "b.png"] {
            std::fs::write(d.path().join(name), []).unwrap();
        }
        // The command line's own flags, as `main` passes them on.
        let run = |line: &str| {
            let a = args(line);
            let (_, choice, local, _) = facts(&a).unwrap();
            let s = survey_with(
                d.path(),
                "s.luaskin",
                &ImportFacts::unavailable(ScreenKind::Play),
                &Cancel::new(),
                local,
                &choice,
            );
            report(0, &s)
        };
        let plain = run("root s");
        assert!(plain.starts_with("skin 0: imported\n"), "{plain}");
        let named = run("--option fc on --file bg b.png root s");
        assert_eq!(named, plain);
        for private in ["resolved", "destination", "fc", "bg", "b.png"] {
            assert!(!named.contains(private), "{private} in {named}");
        }
    }

    #[test]
    fn local_detail_lists_the_configuration_and_each_destination() {
        let s = Survey {
            resolved: Some(Resolved {
                options: vec![(Some("fc".to_owned()), Some("on".to_owned()), Some(906))],
                files: vec![
                    (Some("frame".to_owned()), Some("x".to_owned())),
                    (None, None),
                ],
            }),
            destinations: vec![
                DestinationEntry {
                    index: 3,
                    id: Some("lv".to_owned()),
                    kind: Some("Value".to_owned()),
                    became: Became::Dropped,
                    elements: 1..1,
                    notes: vec![DestinationNote {
                        at: Some(("play.lua".to_owned(), 0, 601)),
                        severity: Severity::Dropped,
                        kind: NoteKind::UnsupportedProperty {
                            kind: PropertyKind::Integer,
                            id: Some(96),
                        },
                    }],
                    suppressed: 0,
                },
                DestinationEntry {
                    index: 4,
                    id: Some("i".to_owned()),
                    kind: Some("Image".to_owned()),
                    became: Became::Retained,
                    elements: 1..3,
                    notes: Vec::new(),
                    suppressed: 2,
                },
                DestinationEntry {
                    index: 5,
                    id: None,
                    kind: None,
                    became: Became::NotReached,
                    elements: 3..3,
                    notes: Vec::new(),
                    suppressed: 0,
                },
            ],
            ..Survey::default()
        };
        let out = report(0, &s);
        let got: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.starts_with("  resolved")
                    || l.starts_with("  destination")
                    || l.starts_with("    note")
            })
            .collect();
        assert_eq!(
            got,
            [
                "  resolved option Some(\"fc\") Some(\"on\") Some(906)",
                "  resolved file Some(\"frame\") Some(\"x\")",
                "  resolved file None None",
                "  destination 3 \"lv\" Value Dropped",
                "    note Dropped an unsupported Integer property 96 file \"play.lua\" line 0 column 601",
                "  destination 4 \"i\" Image Retained elements 1..3 suppressed 2",
                "  destination 5 - - NotReached",
            ]
        );
    }

    #[test]
    fn local_detail_follows_each_record() {
        let records = [
            (
                "replaced Unmapped main_state.number 425",
                Local {
                    file: "parts/m.lua".to_owned(),
                    element: element(0, "Image"),
                    lowered: None,
                },
                "    local file \"parts/m.lua\" element 0 Image id \"x\"",
            ),
            (
                "replaced Consumer image set value",
                Local {
                    file: "s.luaskin".to_owned(),
                    element: element(4, "ImageSet"),
                    lowered: None,
                },
                "    local file \"s.luaskin\" element 4 ImageSet id \"x\"",
            ),
            (
                "lowered",
                Local {
                    file: "s.luaskin".to_owned(),
                    element: element(1, "Image"),
                    lowered: lowered("false", &[], true),
                },
                "    local file \"s.luaskin\" element 1 Image id \"x\" lowered constant \"false\"",
            ),
            (
                "lowered",
                Local {
                    file: "s.luaskin".to_owned(),
                    element: element(2, "Gauge"),
                    lowered: lowered("(gauge.kind == ex_hard)", &["gauge.valid"], false),
                },
                "    local file \"s.luaskin\" element 2 Gauge id \"x\" \
                 lowered expression \"(gauge.kind == ex_hard)\" guards \"gauge.valid\"",
            ),
            (
                "classified only",
                Local {
                    file: "s.luaskin".to_owned(),
                    element: None,
                    lowered: None,
                },
                "    local file \"s.luaskin\" element -",
            ),
        ];
        let s = Survey {
            consumers: records
                .iter()
                .map(|(d, l, _)| consumer(d, Some(l.clone())))
                .collect(),
            ..Survey::default()
        };
        let out = report(0, &s);
        let got: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("  consumer") || l.starts_with("    local"))
            .collect();
        let mut want = Vec::new();
        for (d, _, line) in &records {
            want.push(format!("  consumer module 1, line 7: draw decorative {d}"));
            want.push((*line).to_owned());
        }
        assert_eq!(got, want);
    }
}
