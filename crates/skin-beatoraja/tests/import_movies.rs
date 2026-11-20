//! Skin movies in an import (design.md "Rendering and Skins", skin
//! movies): a play or selection skin image whose source is a movie, by
//! its name after the file choice, becomes a movie destination of the
//! screen's movie of that file, at most two files a screen; a pattern no
//! file choice covers is refused.

mod common;

use skin_beatoraja::{Config, NoteKind, import, read_header};
use skin_model::ScreenKind;

/// A skin of `kind` (`type` `ty`) whose images `i0`… show `sources`
/// (each a source path), each drawn once, with `extra` header fields,
/// in a folder holding `files` (empty files: the import reads no
/// movie).
fn movies(
    ty: u32,
    kind: ScreenKind,
    sources: &[&str],
    extra: &str,
    files: &[&str],
) -> (tempfile::TempDir, skin_beatoraja::Imported) {
    movies_in(ty, kind, sources, extra, files, |_| {})
}

/// [`movies`] with `prepare` run on the folder before the import.
fn movies_in(
    ty: u32,
    kind: ScreenKind,
    sources: &[&str],
    extra: &str,
    files: &[&str],
    prepare: impl FnOnce(&std::path::Path),
) -> (tempfile::TempDir, skin_beatoraja::Imported) {
    let source: Vec<String> = sources
        .iter()
        .enumerate()
        .map(|(i, p)| format!(r#"{{"id": "s{i}", "path": "{p}"}}"#))
        .collect();
    let image: Vec<String> = (0..sources.len())
        .map(|i| format!(r#"{{"id": "i{i}", "src": "s{i}"}}"#))
        .collect();
    let dst: Vec<String> = (0..sources.len())
        .map(|i| format!(r#"{{"id": "i{i}", "dst": [{{"w": 10, "h": 10}}]}}"#))
        .collect();
    let json = format!(
        r#"{{"type": {ty}, "w": 1920, "h": 1080 {extra},
           "source": [{}], "image": [{}], "destination": [{}]}}"#,
        source.join(", "),
        image.join(", "),
        dst.join(", ")
    );
    let dir = common::skin(&[("s.json", &json)]);
    for f in files {
        common::write(dir.path(), f, b"not read");
    }
    prepare(dir.path());
    let (mut def, h) = read_header(dir.path(), "s.json").unwrap();
    let imported = import(&mut def, &h, &Config::defaults(&h), kind).unwrap();
    (dir, imported)
}

fn names(i: &skin_beatoraja::Imported) -> Vec<String> {
    i.skin.sheet().movies().to_vec()
}

#[test]
fn a_movie_source_is_a_movie_of_its_file_on_play() {
    let (_d, i) = movies(
        0,
        ScreenKind::Play,
        &["clip.mp4", "clip.mp4"],
        "",
        &["clip.mp4"],
    );
    assert_eq!(names(&i), ["clip.mp4"], "one player a file");
    assert_eq!(unsupported(&i), 0, "{:?}", i.report.notes);
}

#[test]
fn a_movie_source_is_a_movie_of_its_file_on_selection() {
    use common::selection::{IMAGES, SET, TEXT_T, TITLE, song_list};
    let list = song_list(
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        &[r#"{"x": 100, "y": 500, "w": 300, "h": 40}"#],
        0,
        TITLE,
    );
    let json = format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}, {{"id": "m", "path": "mv/default.mp4"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{IMAGES}, {{"id": "mv", "src": "m"}}],
            "imageset": [{SET}], "text": [{TEXT_T}], {list},
            "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}},
                            {{"id": "mv", "dst": [{{"w": 10, "h": 10}}]}}]}}"#
    );
    let d = common::selection::folder("");
    common::write(d.path(), "s.json", json.as_bytes());
    common::write(d.path(), "mv/default.mp4", b"not read");
    let i = common::selection::import_selection(d.path()).unwrap();
    assert_eq!(names(&i), ["mv/default.mp4"]);
    assert_eq!(unsupported(&i), 0, "{:?}", i.report.notes);
}

/// The notes refusing a movie or its source.
fn unsupported(i: &skin_beatoraja::Imported) -> usize {
    common::kinds(&i.report)
        .iter()
        .filter(|k| {
            matches!(
                k,
                NoteKind::Unsupported { .. } | NoteKind::UnsupportedField { .. }
            )
        })
        .count()
}

#[test]
fn a_third_movie_file_on_a_screen_is_refused() {
    let files = ["a.mp4", "b.mp4", "c.mp4"];
    let (_d, i) = movies(0, ScreenKind::Play, &files, "", &files);
    assert_eq!(names(&i), ["a.mp4", "b.mp4"]);
    assert!(
        common::kinds(&i.report).contains(&NoteKind::OverCap {
            cap: "two movies a screen"
        }),
        "{:?}",
        i.report.notes
    );
}

#[test]
fn a_movie_pattern_resolves_through_its_file_choice_or_is_refused() {
    let files = ["mv/a.mp4", "mv/b.mp4", "mv/c.mp4"];
    // Covered by a file choice: its default.
    let choice = r#", "filepath": [{"name": "mv", "path": "mv/*.mp4", "def": "b"}]"#;
    let (_d, i) = movies(0, ScreenKind::Play, &["mv/*.mp4"], choice, &files);
    assert_eq!(names(&i), ["mv/b.mp4"]);
    // Covered by none: refused, never a random pick.
    let (_d, i) = movies(0, ScreenKind::Play, &["mv/*.mp4"], "", &files);
    assert!(names(&i).is_empty());
    assert!(
        common::kinds(&i.report).contains(&NoteKind::UnsupportedField {
            field: "source glob"
        }),
        "{:?}",
        i.report.notes
    );
}

/// Two names of one file take one slot: an alias through a junction
/// inside the folder shares the first name's slot, so the next file
/// still plays.
#[test]
fn an_alias_of_a_movie_shares_its_slot() {
    let (_d, i) = movies_in(
        0,
        ScreenKind::Play,
        &["real/a.mp4", "alias/a.mp4", "b.mp4"],
        "",
        &["real/a.mp4", "b.mp4"],
        |d| common::link_dir(&d.join("alias"), &d.join("real")),
    );
    assert_eq!(names(&i), ["real/a.mp4", "b.mp4"]);
    assert!(
        !common::kinds(&i.report)
            .iter()
            .any(|k| matches!(k, NoteKind::OverCap { .. })),
        "{:?}",
        i.report.notes
    );
}

/// A movie file that cannot be opened takes no slot and is noted
/// missing; the valid files after it keep theirs.
#[test]
fn a_missing_movie_takes_no_slot() {
    let (_d, i) = movies(
        0,
        ScreenKind::Play,
        &["gone.mp4", "a.mp4", "b.mp4"],
        "",
        &["a.mp4", "b.mp4"],
    );
    assert_eq!(names(&i), ["a.mp4", "b.mp4"]);
    let kinds = common::kinds(&i.report);
    assert!(
        kinds.contains(&NoteKind::MissingAsset),
        "{:?}",
        i.report.notes
    );
    assert!(
        !kinds.iter().any(|k| matches!(k, NoteKind::OverCap { .. })),
        "{:?}",
        i.report.notes
    );
}
