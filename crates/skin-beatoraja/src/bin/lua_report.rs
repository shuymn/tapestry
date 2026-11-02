//! The Lua conversion report (docs/stage3.md §3, the conversion report),
//! for the developer's PC: `lua_report <root> <entry> [<root> <entry>]...`
//! imports each Lua skin, `entry` relative to its skin folder `root`,
//! with its default configuration and prints what did not convert as
//! allowlisted counts and outcomes, each skin by its index in the
//! arguments and each file by its index among those it consulted. It
//! prints no source, name, path, string or Lua error.

use std::path::PathBuf;

use skin_beatoraja::Cancel;
use skin_beatoraja::lua::survey::survey;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || !args.len().is_multiple_of(2) {
        eprintln!("usage: lua_report <root> <entry> [<root> <entry>]...");
        std::process::exit(2);
    }
    for (index, pair) in args.chunks(2).enumerate() {
        let [root, entry] = pair else { continue };
        let cancel = Cancel::new();
        let survey = survey(&PathBuf::from(root), entry, &cancel);
        println!("skin {index}: {}", survey.outcome);
        if let Some((module, line, col)) = survey.at {
            println!("  at: module {module}, line {line}, column {col}");
        }
        println!("  modules: {}", survey.modules);
        let u = survey.usage;
        println!(
            "  usage: instructions {} host calls {} native work {} loads {} (require {}, dofile {}) \
             input bytes {} protected calls {} metatables {} peak memory {}",
            u.instructions,
            u.host_calls,
            u.native_work,
            u.loads,
            u.requires,
            u.dofiles,
            u.input_bytes,
            u.protected_calls,
            u.metatables,
            u.peak_memory
        );
        for ((field, kind), n) in &survey.callables {
            println!("  callable {field} {kind}: {n}");
        }
        for (kind, n) in &survey.notes {
            println!("  note {kind}: {n}");
        }
    }
}
