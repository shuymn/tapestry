//! The owner's review of one OBS run (design §4.3): `<run>/owner-review.toml`
//! holds `game_visible` and `audio_present` for that run only. Its `run`
//! line names the run by an id derived from the run's private origin, so
//! answers given for one run can never confirm another: a review file
//! copied from another run is refused, and the shared `owner-notes.toml`
//! holds no OBS answers at all. Unanswered fields stay pending.

use crate::rundata::ORIGIN;
use sha2::{Digest, Sha256};
use spike_core::run::OwnerPublic;
use std::path::{Path, PathBuf};

pub const REVIEW: &str = "owner-review.toml";
/// The answers a review holds (no longer read from `owner-notes.toml`).
pub const ANSWERS: [&str; 2] = ["game_visible", "audio_present"];

/// The run's id: a hash of its private origin (QPC origin and wall clock),
/// which no other run shares.
pub fn run_id(dir: &Path) -> Result<String, String> {
    let p = dir.join(ORIGIN);
    let b = std::fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
    Ok(Sha256::digest(&b)
        .iter()
        .take(8)
        .map(|x| format!("{x:02x}"))
        .collect())
}

/// The run's review file, written unanswered if it does not exist yet.
pub fn ensure(dir: &Path) -> Result<PathBuf, String> {
    let p = dir.join(REVIEW);
    if !p.exists() {
        let text = format!(
            "# Owner review of this OBS run (design 4.3). These answers belong to\n\
             # this run only: the run line names it, and report refuses this file\n\
             # in any other run. Watch the recording in private\\ingest\\ (also for\n\
             # hitches, tearing, a stuck BGA or crackles), answer true or false,\n\
             # then rerun the report for this run.\n\
             run = \"{}\"\n\
             # The game was visible in the recording.\n\
             game_visible =\n\
             # The game's audio was present in the recording.\n\
             audio_present =\n",
            run_id(dir)?
        );
        std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
    }
    Ok(p)
}

/// The run's answers; none without a review file.
pub fn read(dir: &Path) -> Result<OwnerPublic, String> {
    let p = dir.join(REVIEW);
    match std::fs::read_to_string(&p) {
        Ok(text) => parse(&text, &run_id(dir)?).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(OwnerPublic::default()),
        Err(e) => Err(format!("read {}: {e}", p.display())),
    }
}

/// A review's answers, if it names run `id`. Values are `true`, `false` or
/// empty (unanswered); anything else is refused.
pub fn parse(text: &str, id: &str) -> Result<OwnerPublic, String> {
    let mut run = None;
    let mut o = OwnerPublic::default();
    for l in text.lines() {
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        let (k, v) = l
            .split_once('=')
            .ok_or_else(|| format!("not a key = value line: {l}"))?;
        let (k, v) = (k.trim(), v.trim());
        let answer = || match v {
            "" => Ok(None),
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(format!("{k} must be true, false or empty")),
        };
        match k {
            "run" => run = Some(v.trim_matches('"').to_owned()),
            "game_visible" => o.game_visible = answer()?,
            "audio_present" => o.audio_present = answer()?,
            _ => return Err(format!("unknown key {k}")),
        }
    }
    match run {
        Some(r) if r == id => Ok(o),
        Some(_) => {
            Err("this review belongs to another run; answer in this run's own review".into())
        }
        None => Err("the review names no run".into()),
    }
}

/// `owner-notes.toml` still answers an OBS confirmation (ignored now).
pub fn stray_answers(notes: &Path) -> bool {
    std::fs::read_to_string(notes).is_ok_and(|t| {
        t.lines().any(|l| {
            let l = l.split('#').next().unwrap_or("");
            l.split_once('=')
                .is_some_and(|(k, v)| ANSWERS.contains(&k.trim()) && !v.trim().is_empty())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_dir(tag: &str, origin: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spike-review-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::write(dir.join(ORIGIN), origin).unwrap();
        dir
    }

    #[test]
    fn answers_for_one_run_never_confirm_another() {
        let a = run_dir("a", r#"{"qpc_origin": 1}"#);
        let b = run_dir("b", r#"{"qpc_origin": 2}"#);
        // A fresh review is unanswered.
        let pa = ensure(&a).unwrap();
        assert_eq!(read(&a).unwrap(), OwnerPublic::default());
        // The owner answers run A.
        let text = std::fs::read_to_string(&pa)
            .unwrap()
            .replace("game_visible =", "game_visible = true")
            .replace("audio_present =", "audio_present = true");
        std::fs::write(&pa, &text).unwrap();
        let yes = OwnerPublic {
            game_visible: Some(true),
            audio_present: Some(true),
        };
        assert_eq!(read(&a).unwrap(), yes);
        // Run B has its own, unanswered review; A's answers are not B's.
        ensure(&b).unwrap();
        assert_eq!(read(&b).unwrap(), OwnerPublic::default());
        // A's review copied into B is refused.
        std::fs::write(b.join(REVIEW), &text).unwrap();
        let e = read(&b).unwrap_err();
        assert!(e.contains("another run"), "{e}");
        // ensure never overwrites an answered review.
        ensure(&a).unwrap();
        assert_eq!(read(&a).unwrap(), yes);
        for d in [a, b] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn reviews_are_parsed_strictly() {
        let p = |t: &str| parse(t, "abc");
        assert_eq!(
            p("run = \"abc\"\ngame_visible = true # seen\naudio_present =\n").unwrap(),
            OwnerPublic {
                game_visible: Some(true),
                audio_present: None,
            }
        );
        assert!(p("game_visible = true\n").is_err());
        assert!(p("run = \"abc\"\ngame_visible = yes\n").is_err());
        assert!(p("run = \"abc\"\nvrr = false\n").is_err());
    }

    #[test]
    fn owner_notes_answers_are_detected() {
        let dir = run_dir("notes", "{}");
        let n = dir.join("owner-notes.toml");
        std::fs::write(&n, "vrr = false\n# game_visible = true\ngame_visible =\n").unwrap();
        assert!(!stray_answers(&n));
        std::fs::write(&n, "vrr = false\naudio_present = true\n").unwrap();
        assert!(stray_answers(&n));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
