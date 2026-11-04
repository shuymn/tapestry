//! The Lua conversion report (docs/stage3.md §3, the conversion report),
//! for the developer's PC:
//! `lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none]
//! [--level <n>|pending] <root> <entry> [<root> <entry>]...`
//! imports each Lua skin, `entry` relative to its skin folder `root`,
//! with its default configuration on the scene of the kind its header
//! names, against import facts with autoplay, the rank the preset read
//! and the chart's play level as given (`none` for a chart with no rank
//! header, `pending` for a level the base snapshot has pending; each
//! unavailable when not given), and prints its kind, what did not convert
//! as allowlisted counts and outcomes, a record per callback consumer
//! (where it is defined, its field, role, disposition and reason, and
//! why a callback classified but never consumed was left), and the import
//! facts it read and recorded, each skin by its index in the arguments and
//! each file by its index among those it consulted, then the skins per
//! kind (`none` for a header refused). It prints no source, name, path,
//! string or Lua error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use skin_beatoraja::Cancel;
use skin_beatoraja::lua::facts::{ChartRank, Fact, ImportFacts};
use skin_beatoraja::lua::survey::survey;
use skin_model::ScreenKind;

const USAGE: &str = "usage: lua_report [--autoplay on|off] [--rank <n>|defex <milli>|none] \
                     [--level <n>|pending] <root> <entry> [<root> <entry>]...";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The snapshot the flags at the front of `args` give, its scene each
/// skin's own, and the arguments after them; `None` for a flag it does
/// not read.
fn facts(mut args: &[String]) -> Option<(ImportFacts, &[String])> {
    let mut facts = ImportFacts::unavailable(ScreenKind::Play);
    loop {
        match args {
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
            _ => return Some((facts, args)),
        }
    }
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let Some((facts, args)) = facts(&all) else {
        usage()
    };
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
        for c in &survey.consumers {
            let at = c.at.map_or_else(
                || "module ?".to_owned(),
                |(module, line, _)| format!("module {module}, line {line}"),
            );
            let reason = c
                .reason
                .as_deref()
                .map_or_else(String::new, |r| format!(" ({r})"));
            println!(
                "  consumer {at}: {} {} {}{reason}",
                c.field, c.role, c.disposition
            );
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

#[cfg(test)]
mod tests {
    use skin_beatoraja::lua::facts::{ChartRank, Fact};

    use super::facts;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_flags_set_the_snapshot_and_leave_the_skins() {
        let a = args("--autoplay off --rank 2 --level 12 root s.luaskin");
        let (f, rest) = facts(&a).unwrap();
        assert_eq!(f.autoplay, Fact::Known(false));
        assert_eq!(f.chart_rank, Fact::Known(Some(ChartRank::Rank(2))));
        assert_eq!(f.play_level, Fact::Known(12));
        assert_eq!(rest, ["root", "s.luaskin"]);
        let a = args("--level pending root s.luaskin");
        let (f, _) = facts(&a).unwrap();
        assert_eq!(f.play_level, Fact::Pending);
        assert_eq!(f.chart_rank, Fact::Unavailable);
        let a = args("root s.luaskin");
        assert_eq!(facts(&a).unwrap().0.play_level, Fact::Unavailable);
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
        ] {
            assert!(facts(&args(a)).is_none(), "{a}");
        }
    }
}
