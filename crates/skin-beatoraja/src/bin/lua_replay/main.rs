//! The offline replay harness (design.md "beatoraja Lua Skins", the
//! reference executor; docs/stage3.md S7-06e), for the developer's PC:
//! `lua_replay [--autoplay on|off] [--class veryhard|hard|normal|easy|veryeasy|none]
//! [--level <n>] [--option <name> <item>]... [--file <name> <candidate>]...
//! [--script full|mixed|rounds|rounds-unwitnessed|closing] [--perturb <id>=<delta>]... [--isolation <file>] [--local]
//! <root> <entry> [<root> <entry>]...`
//!
//! Each play skin is imported twice. The production import's frames are
//! built over a self-made chart that the evaluator plays by the script,
//! the program's trace recorded ([`production`]); the reference import
//! keeps its configured pass's VM, where each installed object of a
//! modelled kind is prepared every frame by the source's own path, the
//! host answering from the recorded frame inputs ([`reference`]). The
//! comparator matches every source invocation with an agreeing production
//! invocation or the record production made of that occurrence, and every
//! production invocation with a source invocation ([`production::compare`]).
//!
//! It prints, per skin, the counts, what was attributed and listed, the
//! roots the reference could not run to their end, and every finding,
//! located by destination index, field and line; file names only under
//! `--local`. The exit status is 1 when any skin disagrees, 2 on a usage
//! error or when the two imports differ.

mod hints;
mod inputs;
mod isolation;
#[cfg(test)]
mod lowered_tests;
mod observed;
mod production;
mod reference;
mod report;
#[cfg(test)]
mod rounds_tests;
mod script;
#[cfg(test)]
mod tests;

use std::path::Path;

use skin_beatoraja::lua::facts::{Fact, ImportFacts, JudgeClass};
use skin_beatoraja::{Cancel, Config, LuaOutcome, read_lua_header};
use skin_model::ScreenKind;

use crate::report::Report;
use crate::script::Script;

const USAGE: &str = "usage: lua_replay [--autoplay on|off] \
     [--class veryhard|hard|normal|easy|veryeasy|none] [--level <n>] \
     [--option <name> <item>]... [--file <name> <candidate>]... [--script full|mixed|rounds|rounds-unwitnessed|closing] \
     [--perturb <id>=<delta>]... [--isolation <file>] [--local] [--all] <root> <entry> [<root> <entry>]...";

/// What the flags ask for.
#[derive(Debug)]
pub struct Options {
    pub facts: ImportFacts,
    pub options: Vec<(String, String)>,
    pub files: Vec<(String, String)>,
    pub script: Script,
    pub perturb: Vec<(i32, i32)>,
    pub isolation: Vec<isolation::Entry>,
    pub local: bool,
    /// Every finding printed, not only the first [`report::SHOWN`].
    pub all: bool,
    /// A test's override of one practice page's point (its ordinal, the
    /// point).
    #[cfg(test)]
    pub page_point: Option<(usize, i32)>,
    /// `number` ids a test makes unanswered on every frame.
    #[cfg(test)]
    pub deny: Vec<i32>,
}

/// The options the flags at the front of `args` give, and the arguments
/// after them; `None` for a flag it does not read.
fn parse(mut args: &[String]) -> Option<(Options, &[String])> {
    let mut o = Options {
        facts: ImportFacts::unavailable(ScreenKind::Play),
        options: Vec::new(),
        files: Vec::new(),
        script: Script::full(),
        perturb: Vec::new(),
        isolation: Vec::new(),
        local: false,
        all: false,
        #[cfg(test)]
        page_point: None,
        #[cfg(test)]
        deny: Vec::new(),
    };
    loop {
        match args {
            [flag, name, item, rest @ ..] if flag == "--option" => {
                o.options.push((name.clone(), item.clone()));
                args = rest;
            }
            [flag, name, candidate, rest @ ..] if flag == "--file" => {
                o.files.push((name.clone(), candidate.clone()));
                args = rest;
            }
            [flag, rest @ ..] if flag == "--local" => {
                o.local = true;
                args = rest;
            }
            [flag, rest @ ..] if flag == "--all" => {
                o.all = true;
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--script" => {
                o.script = Script::named(v)?;
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--perturb" => {
                let (id, d) = v.split_once('=')?;
                o.perturb.push((id.parse().ok()?, d.parse().ok()?));
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--isolation" => {
                let text = std::fs::read_to_string(v).ok()?;
                o.isolation = isolation::parse(&text).ok()?;
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--autoplay" => {
                o.facts.autoplay = Fact::Known(match v.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return None,
                });
                args = rest;
            }
            [flag, v, rest @ ..] if flag == "--level" => {
                o.facts.play_level = Fact::Known(v.parse().ok()?);
                args = rest;
            }
            [flag, c, rest @ ..] if flag == "--class" => {
                o.facts.judge_class = Fact::Known(match c.as_str() {
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
            rest => return Some((o, rest)),
        }
    }
}

/// Why a replay did not compare.
#[derive(Debug)]
pub enum Stop {
    /// The skin did not import, or its frames did not build.
    Failed(String),
    /// The reference import differs from production's.
    Differs,
}

/// Replays the play skin at `entry` under `root` with `o`.
///
/// # Errors
///
/// [`Stop`].
pub fn replay(root: &Path, entry: &str, o: &Options) -> Result<Report, Stop> {
    let fail = |e: String| Stop::Failed(e);
    let cancel = Cancel::new();
    let h = read_lua_header(root, entry, &cancel).map_err(|e| fail(format!("header: {e}")))?;
    let config = Config::named(
        h.header(),
        o.options.iter().map(|(a, b)| (a.as_str(), b.as_str())),
        o.files.iter().map(|(a, b)| (a.as_str(), b.as_str())),
    )
    .map_err(|e| fail(format!("configuration: {e:?}")))?;
    let (out, recorded) = h.import_recorded(&config, &o.facts, &cancel);
    let skin = match out.map_err(|e| fail(format!("import: {e}")))? {
        LuaOutcome::Imported(i) => i.imported.skin,
        LuaOutcome::Pending(p) => return Err(fail(format!("pending {:?}", p.pending))),
    };
    let level = match o.facts.play_level {
        Fact::Known(n) => Some(n),
        _ => None,
    };
    let (prod, inputs) = production::record(&skin, recorded, &o.script, level).map_err(fail)?;
    let (out, rec, vm) = h.import_reference(&config, &o.facts, &cancel);
    let structure = match out.map_err(|e| fail(format!("reference import: {e}")))? {
        LuaOutcome::Imported(i) => i.imported.skin.program_structure(),
        LuaOutcome::Pending(p) => return Err(fail(format!("pending {:?}", p.pending))),
    };
    let vm = vm.ok_or_else(|| fail("the reference kept no VM".into()))?;
    if !prod.same_import(&rec, &structure) {
        return Err(Stop::Differs);
    }
    for &(id, d) in &o.perturb {
        vm.perturb(id, d);
    }
    let reference = reference::Reference::new(&vm, &rec.items, &rec.sources);
    #[cfg(test)]
    let reference = {
        for &id in &o.deny {
            vm.deny(id);
        }
        match o.page_point {
            Some((page, point)) => reference.page_point(page, point),
            None => reference,
        }
    };
    let mut observed = reference.run(&inputs);
    observed.root = root.to_path_buf();
    Ok(production::compare::compare(&prod, &observed, &o.isolation))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((o, rest)) = parse(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    if rest.is_empty() || rest.len() % 2 != 0 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let mut status = 0;
    for (i, pair) in rest.chunks(2).enumerate() {
        let [root, entry] = pair else { continue };
        match replay(Path::new(root), entry, &o) {
            Ok(r) => {
                print!("{}", r.text(i, o.local, o.all));
                if !r.passes() {
                    status = status.max(1);
                }
            }
            Err(Stop::Failed(e)) => {
                println!("skin {i}\nfailed: {}", if o.local { e } else { "-".into() });
                status = 2;
            }
            Err(Stop::Differs) => {
                println!("skin {i}\nthe reference import differs from production's");
                status = 2;
            }
        }
    }
    std::process::exit(status);
}
