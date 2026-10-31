//! File choices (owner decision 2) and source files: the chosen
//! candidate substituted into source paths, reported; uncovered globs and
//! names of the wrong form.

mod common;

use common::{Loaded, has, hd, import_with, png, skin};
use skin_beatoraja::{Config, FileChoice, ImportError, ImportLimit, NoteKind, Rule};

/// A skin whose file choice `bg/*.png` has `def`, over `bg/` holding
/// `files` (name, side), with one image of `bg/*.png` drawn at its
/// native size.
fn choice(def: &str, files: &[(&str, u32)], edit: impl FnOnce(&mut Config)) -> Loaded {
    let json = hd(&format!(
        r#""filepath": [{{"name": "Background", "path": "bg/*.png" {def}}}],
           "source": [{{"id": "b", "path": "bg/*.png"}}],
           "image": [{{"id": "i", "src": "b", "w": -1, "h": -1}}],
           "destination": [{{"id": "i", "stretch": 9, "dst": [{{"w": 100, "h": 100}}]}}]"#
    ));
    let dir = skin(&[("s.json", &json)]);
    for &(name, side) in files {
        png(dir.path(), &format!("bg/{name}"), side, side);
    }
    let result = import_with(dir.path(), edit);
    Loaded { dir, result }
}

fn files(l: &Loaded) -> Vec<FileChoice> {
    l.report().files.clone()
}

fn chosen(candidate: Option<u16>, rule: Rule, ambiguous: bool) -> FileChoice {
    FileChoice {
        choice: 0,
        candidate,
        rule,
        ambiguous,
    }
}

/// The native side of the drawn image.
fn side(l: Loaded) -> i32 {
    l.rects(0)[0][2]
}

const BG: [(&str, u32); 2] = [("dark.png", 8), ("light.png", 16)];

#[test]
fn the_default_matches_a_name_or_stem() {
    for def in [r#", "def": "dark""#, r#", "def": "DARK.PNG""#] {
        let l = choice(def, &BG, |_| {});
        assert_eq!(files(&l), [chosen(Some(0), Rule::Default, false)]);
        assert_eq!(side(l), 8);
    }
    let l = choice(r#", "def": "Light""#, &BG, |_| {});
    assert_eq!(files(&l), [chosen(Some(1), Rule::Default, false)]);
    assert_eq!(side(l), 16);
}

#[test]
fn several_default_matches_take_the_first_sorted() {
    let json = hd(r#""filepath": [{"name": "c", "path": "x/*", "def": "dark"}]"#);
    let dir = skin(&[("s.json", &json)]);
    png(dir.path(), "x/dark.png", 1, 1);
    png(dir.path(), "x/dark", 1, 1);
    let l = Loaded {
        result: import_with(dir.path(), |_| {}),
        dir,
    };
    assert_eq!(files(&l), [chosen(Some(0), Rule::Default, true)]);
}

#[test]
fn a_configured_candidate_wins() {
    let set = |v: &str| {
        let v = v.to_owned();
        move |c: &mut Config| {
            c.files.insert("Background".into(), v);
        }
    };
    let l = choice(r#", "def": "dark""#, &BG, set("light.png"));
    assert_eq!(files(&l), [chosen(Some(1), Rule::Configured, false)]);
    assert!(l.report().notes.is_empty());
    assert_eq!(side(l), 16);
    // "Random" and other non-candidates fall back to the default.
    for v in ["Random", "other.png"] {
        let l = choice(r#", "def": "light""#, &BG, set(v));
        assert_eq!(files(&l), [chosen(Some(1), Rule::Default, false)]);
        assert!(has(l.report(), NoteKind::Ignored { field: "files" }));
    }
}

#[test]
fn no_match_takes_the_first_and_no_candidate_is_missing() {
    let l = choice(r#", "def": "dusk""#, &BG, |_| {});
    assert_eq!(files(&l), [chosen(Some(0), Rule::First, false)]);
    assert_eq!(side(l), 8);
    let l = choice("", &[], |_| {});
    assert_eq!(files(&l), [chosen(None, Rule::None, false)]);
    assert!(has(l.report(), NoteKind::MissingAsset));
    assert_eq!(l.report().usage.file_attempts, 0);
}

#[test]
fn an_uncovered_glob_is_unsupported() {
    let json = hd(r#""source": [{"id": "b", "path": "bg/*.png"}],
                     "image": [{"id": "i", "src": "b", "w": 1, "h": 1}],
                     "destination": [{"id": "i", "dst": [{"w": 1, "h": 1}]}]"#);
    let dir = skin(&[("s.json", &json)]);
    png(dir.path(), "bg/a.png", 1, 1);
    let l = Loaded {
        result: import_with(dir.path(), |_| {}),
        dir,
    };
    assert!(has(
        l.report(),
        NoteKind::UnsupportedField {
            field: "source glob"
        }
    ));
}

/// `n` destinations of one image whose file name has the wrong form.
fn bad_names(n: usize) -> Loaded {
    let dsts = vec![r#"{"id": "i", "dst": [{"w": 1, "h": 1}]}"#; n].join(",");
    let json = hd(&format!(
        r#""source": [{{"id": "b", "path": "a:b.png"}}],
           "image": [{{"id": "i", "src": "b", "w": 1, "h": 1}}],
           "destination": [{dsts}]"#
    ));
    let dir = skin(&[("s.json", &json)]);
    let result = import_with(dir.path(), |_| {});
    Loaded { dir, result }
}

#[test]
fn names_of_the_wrong_form_count_every_attempt() {
    let l = bad_names(4_096);
    assert_eq!(l.report().usage.file_attempts, 4_096);
    assert_eq!(l.report().usage.files, 0);
    let e = bad_names(4_097).err();
    assert!(
        matches!(
            e,
            ImportError::Limit {
                limit: ImportLimit::FileAttempts,
                at: Some(_)
            }
        ),
        "{e:?}"
    );
}
