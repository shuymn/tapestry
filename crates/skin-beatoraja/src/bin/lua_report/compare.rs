//! The comparison of a walk with a kept one (docs/stage3.md §3, the
//! conversion report): skins pair by index and compare only when their
//! catalogue digest is equal; their configurations then pair by position.
//! A new failure is a configuration that imported and no longer imports,
//! or a refusal whose reason changed; a changed catalogue is re-baselined;
//! an `inconclusive` or `unstable` configuration is not judged.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::record::{Record, Records};

/// What a comparison found and its exit code.
#[derive(Debug, Default)]
pub struct Verdict {
    pub text: String,
    pub failures: usize,
    pub unjudged: usize,
    pub rebaseline: usize,
    pub changed: usize,
    pub unchanged: usize,
}

impl Verdict {
    /// 1 on a new failure, else 2 when anything was not judged, else 0.
    pub fn code(&self) -> i32 {
        if self.failures > 0 {
            1
        } else if self.unjudged > 0 || self.rebaseline > 0 {
            2
        } else {
            0
        }
    }
}

/// Whether `outcome` is a refusal: an import that did not complete for a
/// reason of the importer's (not `unselectable`, `inconclusive` or
/// `unstable`).
fn refusal(outcome: &str) -> bool {
    !matches!(
        outcome,
        "imported" | "unselectable" | "inconclusive" | "unstable"
    )
}

/// A record's fields by name, as TOML values, for the field-by-field
/// difference.
fn fields(r: &Record) -> BTreeMap<String, toml::Value> {
    let v = toml::Value::try_from(r).unwrap_or(toml::Value::Table(toml::Table::new()));
    let mut out = BTreeMap::new();
    if let toml::Value::Table(t) = v {
        flatten("", &t, &mut out);
    }
    out
}

fn flatten(prefix: &str, t: &toml::Table, out: &mut BTreeMap<String, toml::Value>) {
    for (k, v) in t {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix} {k}")
        };
        match v {
            toml::Value::Table(inner) => flatten(&key, inner, out),
            other => {
                out.insert(key, other.clone());
            }
        }
    }
}

fn show(v: Option<&toml::Value>) -> String {
    match v {
        None => "-".to_owned(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `kept` against `new`; `Err` when the two cannot be compared at all.
///
/// # Errors
///
/// Records under other facts or of another version.
pub fn compare(kept: &Records, new: &Records) -> Result<Verdict, String> {
    if kept.facts != new.facts || kept.result_facts != new.result_facts {
        return Err("kept and new were walked under other import facts".to_owned());
    }
    let mut v = Verdict::default();
    fn by_skin(r: &Records) -> BTreeMap<usize, Vec<&Record>> {
        let mut m: BTreeMap<usize, Vec<&Record>> = BTreeMap::new();
        for rec in &r.record {
            m.entry(rec.skin).or_default().push(rec);
        }
        for list in m.values_mut() {
            list.sort_by_key(|r| r.configuration);
        }
        m
    }
    let (kr, nr) = (by_skin(kept), by_skin(new));
    let skins = kept.skin.len().max(new.skin.len());
    let mut total = 0;
    for i in 0..skins {
        let (Some(ks), Some(ns)) = (kept.skin.get(i), new.skin.get(i)) else {
            let which = if kept.skin.get(i).is_some() {
                "missing from new"
            } else {
                "added in new"
            };
            let _ = writeln!(v.text, "skin {i}: {which}, re-baseline");
            v.rebaseline += 1;
            continue;
        };
        if ks.digest != ns.digest {
            let _ = writeln!(v.text, "skin {i}: catalogue changed, re-baseline");
            v.rebaseline += 1;
            continue;
        }
        let empty = Vec::new();
        let (kl, nl) = (kr.get(&i).unwrap_or(&empty), nr.get(&i).unwrap_or(&empty));
        let same_rows = kl.len() == nl.len()
            && kl
                .iter()
                .zip(nl.iter())
                .all(|(a, b)| a.toggle == b.toggle && a.position == b.position);
        if !same_rows {
            let _ = writeln!(v.text, "skin {i}: configurations differ, re-baseline");
            v.rebaseline += 1;
            continue;
        }
        for (k, n) in kl.iter().zip(nl.iter()) {
            total += 1;
            judge(&mut v, k, n);
        }
    }
    let _ = writeln!(
        v.text,
        "compared {total} configurations over {skins} skins: {} new failures, {} not judged \
         (inconclusive or unstable), {} skins to re-baseline, {} changed, {} unchanged",
        v.failures, v.unjudged, v.rebaseline, v.changed, v.unchanged
    );
    Ok(v)
}

/// One configuration's kept and new records.
fn judge(v: &mut Verdict, k: &Record, n: &Record) {
    let head = format!(
        "skin {} configuration {} ({})",
        n.skin,
        n.configuration,
        n.toggle_label()
    );
    let unjudged = |o: &str| matches!(o, "inconclusive" | "unstable");
    if unjudged(&k.outcome) || unjudged(&n.outcome) {
        let _ = writeln!(
            v.text,
            "{head}: outcome {} -> {}: not judged",
            k.outcome, n.outcome
        );
        v.unjudged += 1;
        return;
    }
    let failure = (k.outcome == "imported" && refusal(&n.outcome))
        || (refusal(&k.outcome) && refusal(&n.outcome) && k.outcome != n.outcome);
    let (kf, nf) = (fields(k), fields(n));
    let mut keys: Vec<&String> = kf.keys().chain(nf.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut changed = false;
    for key in keys {
        let (a, b) = (kf.get(key), nf.get(key));
        if a == b {
            continue;
        }
        changed = true;
        let mark = if failure && key == "outcome" {
            ": new failure"
        } else {
            ""
        };
        let _ = writeln!(v.text, "{head}: {key} {} -> {}{mark}", show(a), show(b));
    }
    if failure {
        v.failures += 1;
    } else if changed {
        v.changed += 1;
    } else {
        v.unchanged += 1;
    }
}
