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

/// A skin whose file choices `choices` (name, path, def) are listed over
/// `parts/` holding `a.png` (8×8), `b.png` (16×16) and `c.png` (24×24),
/// and whose sources `sources` are each drawn at its native size at
/// `x` = 200 × its index.
fn tagged(choices: &[(&str, &str, &str)], sources: &[&str]) -> Loaded {
    let choices: Vec<String> = choices
        .iter()
        .map(|(n, p, d)| format!(r#"{{"name": "{n}", "path": "{p}", "def": "{d}"}}"#))
        .collect();
    let source: Vec<String> = sources
        .iter()
        .enumerate()
        .map(|(i, p)| format!(r#"{{"id": "s{i}", "path": "{p}"}}"#))
        .collect();
    let image: Vec<String> = (0..sources.len())
        .map(|i| format!(r#"{{"id": "i{i}", "src": "s{i}", "w": -1, "h": -1}}"#))
        .collect();
    let dst: Vec<String> = (0..sources.len())
        .map(|i| {
            format!(
                r#"{{"id": "i{i}", "stretch": 9, "dst": [{{"x": {}, "w": 100, "h": 100}}]}}"#,
                200 * i
            )
        })
        .collect();
    let json = hd(&format!(
        r#""filepath": [{}], "source": [{}], "image": [{}], "destination": [{}]"#,
        choices.join(","),
        source.join(","),
        image.join(","),
        dst.join(",")
    ));
    let dir = skin(&[("s.json", &json)]);
    for (name, side) in [("a.png", 8), ("b.png", 16), ("c.png", 24)] {
        png(dir.path(), &format!("parts/{name}"), side, side);
    }
    let result = import_with(dir.path(), |_| {});
    Loaded { dir, result }
}

/// The native side drawn in each of `n` sources' 100-wide places, in
/// order; `None` where nothing is drawn.
fn sides(l: Loaded, n: i32) -> Vec<Option<i32>> {
    let rects = l.rects(0);
    (0..n)
        .map(|i| {
            let place = 200 * i..200 * i + 100;
            rects.iter().find(|r| place.contains(&r[0])).map(|r| r[2])
        })
        .collect()
}

#[test]
fn tagged_choices_substitute_independently() {
    let l = tagged(
        &[
            ("Cover", "parts/*.png|cover|", "a"),
            ("Lift", "parts/*.png|lift|", "b"),
            ("Hidden", "parts/*.png|hidden|", "c"),
        ],
        &[
            "parts/*.png|lift|",
            "parts/*.png|hidden|",
            "parts/*.png|cover|",
        ],
    );
    assert_eq!(sides(l, 3), [Some(16), Some(24), Some(8)]);
}

#[test]
fn the_first_declared_prefix_substitutes_and_a_kept_tag_reads_nothing() {
    let sources = ["parts/*.png|lift|", "parts/*.png"];
    // A tagged key declared first takes its own source; the untagged one
    // takes the rest.
    let l = tagged(
        &[
            ("Lift", "parts/*.png|lift|", "b"),
            ("All", "parts/*.png", "a"),
        ],
        &sources,
    );
    assert_eq!(sides(l, 2), [Some(16), Some(8)]);
    // An untagged key declared first also prefixes the tagged source,
    // which keeps its tag: `parts/a.png|lift|`, read in the skin folder
    // like any other name, is no file there and draws nothing.
    let l = tagged(
        &[
            ("All", "parts/*.png", "a"),
            ("Lift", "parts/*.png|lift|", "b"),
        ],
        &sources,
    );
    assert!(has(l.report(), NoteKind::MissingAsset), "{:?}", l.report());
    assert_eq!(sides(l, 2), [None, Some(8)]);
}

#[test]
fn tagged_patterns_fold_case_as_untagged_ones_do() {
    let l = tagged(
        &[("Lift", "parts/*.PNG|t|", "B")],
        &["parts/*.PNG|t|", "parts/*.PNG|T|"],
    );
    // The pattern lists `b.png`, and the default differing in case picks
    // it; the tag and the key compare exactly, so `|T|` is a glob no
    // choice covers.
    assert_eq!(files(&l), [chosen(Some(1), Rule::Default, false)]);
    assert!(has(
        l.report(),
        NoteKind::UnsupportedField {
            field: "source glob"
        }
    ));
    assert_eq!(sides(l, 2), [Some(16), None]);
}
