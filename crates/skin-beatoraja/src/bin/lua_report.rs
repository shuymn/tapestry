//! The Lua conversion report (docs/stage3.md §3, the conversion report),
//! for the developer's PC:
//! `lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none]
//! <root> <entry> [<root> <entry>]...`
//! imports each Lua skin, `entry` relative to its skin folder `root`,
//! with its default configuration on the scene of the kind its header
//! names, against import facts with autoplay and the rank the preset
//! read as given (`none` for a chart with no rank header; each
//! unavailable when not given), and prints its kind, what did not convert
//! as allowlisted counts and outcomes and the import facts it read and
//! recorded, each skin by its index in the arguments and each file by its
//! index among those it consulted, then the skins per kind (`none` for a
//! header refused). It prints no source, name, path, string or Lua error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use skin_beatoraja::Cancel;
use skin_beatoraja::lua::facts::{ChartRank, Fact, ImportFacts};
use skin_beatoraja::lua::survey::survey;
use skin_model::ScreenKind;

const USAGE: &str = "usage: lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none] \
                     <root> <entry> [<root> <entry>]...";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The snapshot the flags at the front of `args` give, its scene each
/// skin's own, and the arguments after them.
fn facts(mut args: &[String]) -> (ImportFacts, &[String]) {
    let mut facts = ImportFacts::unavailable(ScreenKind::Play);
    loop {
        match args {
            [flag, v, rest @ ..] if flag == "--autoplay" => {
                facts.autoplay = Fact::Known(match v.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => usage(),
                });
                args = rest;
            }
            [flag, defex, m, rest @ ..] if flag == "--rank" && defex == "defex" => {
                let m = m.parse().unwrap_or_else(|_| usage());
                facts.chart_rank = Fact::Known(Some(ChartRank::DefExRankMilli(m)));
                args = rest;
            }
            [flag, none, rest @ ..] if flag == "--rank" && none == "none" => {
                facts.chart_rank = Fact::Known(None);
                args = rest;
            }
            [flag, r, rest @ ..] if flag == "--rank" => {
                let r = r.parse().unwrap_or_else(|_| usage());
                facts.chart_rank = Fact::Known(Some(ChartRank::Rank(r)));
                args = rest;
            }
            _ => return (facts, args),
        }
    }
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let (facts, args) = facts(&all);
    if args.is_empty() || !args.len().is_multiple_of(2) {
        usage();
    }
    let mut kinds: BTreeMap<String, u32> = BTreeMap::new();
    for (index, pair) in args.chunks(2).enumerate() {
        let [root, entry] = pair else { continue };
        let cancel = Cancel::new();
        let survey = survey(&PathBuf::from(root), entry, &facts, &cancel);
        println!("skin {index}: {}", survey.outcome);
        let kind = survey
            .kind
            .map_or_else(|| "none".to_owned(), |k| format!("{k:?}"));
        println!("  kind: {kind}");
        *kinds.entry(kind).or_default() += 1;
        if let Some((module, line, col)) = survey.at {
            println!("  at: module {module}, line {line}, column {col}");
        }
        println!("  modules: {}", survey.modules);
        let u = survey.usage;
        println!(
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
        );
        for ((field, kind), n) in &survey.callables {
            println!("  callable {field} {kind}: {n}");
        }
        for (outcome, n) in &survey.outcomes {
            println!("  callback {outcome}: {n}");
        }
        for (kind, n) in &survey.notes {
            println!("  note {kind}: {n}");
        }
        for (label, n) in &survey.facts {
            println!("  fact {label}: {n}");
        }
        for r in &survey.reads {
            println!("  recorded {:?} = {:?}", r.fact, r.value);
        }
    }
    for (kind, n) in &kinds {
        println!("kind {kind}: {n}");
    }
}
