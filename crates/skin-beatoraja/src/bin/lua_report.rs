//! The Lua conversion report (docs/stage3.md §3, the conversion report),
//! for the developer's PC:
//! `lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none]
//! [--level <n>|pending] [--local] <root> <entry> [<root> <entry>]...`
//! imports each Lua skin, `entry` relative to its skin folder `root`,
//! with its default configuration on the scene of the kind its header
//! names, against import facts with autoplay, the rank the preset read
//! and the chart's play level as given (`none` for a chart with no rank
//! header, `pending` for a level the base snapshot has pending; each
//! unavailable when not given), and prints its kind, what did not convert
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
//! expression and its guards. That output stays on the developer's PC.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use skin_beatoraja::Cancel;
use skin_beatoraja::lua::facts::{ChartRank, Fact, ImportFacts};
use skin_beatoraja::lua::survey::{Local, Survey, survey};
use skin_model::ScreenKind;

const USAGE: &str = "usage: lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none] \
                     [--level <n>|pending] [--local] <root> <entry> [<root> <entry>]...";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The snapshot the flags at the front of `args` give, its scene each
/// skin's own, whether `--local` asks for the developer-local detail, and
/// the arguments after them; `None` for a flag it does not read.
fn facts(mut args: &[String]) -> Option<(ImportFacts, bool, &[String])> {
    let mut facts = ImportFacts::unavailable(ScreenKind::Play);
    let mut local = false;
    loop {
        match args {
            [flag, rest @ ..] if flag == "--local" => {
                local = true;
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
            [flag, defex, m, rest @ ..] if flag == "--rank" && defex == "defex" => {
                let m = m.parse().ok()?;
                facts.chart_rank = Fact::Known(Some(ChartRank::DefExRankMilli(m)));
                args = rest;
            }
            [flag, none, rest @ ..] if flag == "--rank" && none == "none" => {
                facts.chart_rank = Fact::Known(None);
                args = rest;
            }
            [flag, r, rest @ ..] if flag == "--rank" => {
                let r = r.parse().ok()?;
                facts.chart_rank = Fact::Known(Some(ChartRank::Rank(r)));
                args = rest;
            }
            [flag, ..] if flag.starts_with("--") => return None,
            _ => return Some((facts, local, args)),
        }
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
    for (label, n) in &survey.facts {
        line(&format!("  fact {label}: {n}"));
    }
    for r in &survey.reads {
        line(&format!("  recorded {:?} = {:?}", r.fact, r.value));
    }
    s
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let Some((facts, local, args)) = facts(&all) else {
        usage()
    };
    if args.is_empty() || !args.len().is_multiple_of(2) {
        usage();
    }
    let mut kinds: BTreeMap<String, u32> = BTreeMap::new();
    for (index, pair) in args.chunks(2).enumerate() {
        let [root, entry] = pair else { continue };
        let cancel = Cancel::new();
        let survey = survey(&PathBuf::from(root), entry, &facts, &cancel, local);
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
    use skin_beatoraja::lua::facts::{ChartRank, Fact};
    use skin_beatoraja::lua::survey::{Consumer, Element, Local, Lowered, Survey};

    use super::{facts, report};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_flags_set_the_snapshot_and_leave_the_skins() {
        let a = args("--autoplay off --rank 2 --level 12 root s.luaskin");
        let (f, local, rest) = facts(&a).unwrap();
        assert_eq!(f.autoplay, Fact::Known(false));
        assert_eq!(f.chart_rank, Fact::Known(Some(ChartRank::Rank(2))));
        assert_eq!(f.play_level, Fact::Known(12));
        assert!(!local);
        assert_eq!(rest, ["root", "s.luaskin"]);
        let a = args("--level pending root s.luaskin");
        let (f, _, _) = facts(&a).unwrap();
        assert_eq!(f.play_level, Fact::Pending);
        assert_eq!(f.chart_rank, Fact::Unavailable);
        let a = args("root s.luaskin");
        assert_eq!(facts(&a).unwrap().0.play_level, Fact::Unavailable);
    }

    #[test]
    fn local_is_a_flag_of_its_own_among_the_others() {
        for a in ["--local root s", "--autoplay on --local --level 3 root s"] {
            let a = args(a);
            let (_, local, rest) = facts(&a).unwrap();
            assert!(local);
            assert_eq!(rest, ["root", "s"]);
        }
    }

    #[test]
    fn a_bad_flag_or_value_is_refused() {
        for a in [
            "--level x root s",
            "--level 2147483648 root s",
            "--level",
            "--autoplay maybe root s",
            "--rank x root s",
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
