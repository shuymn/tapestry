//! A cell no atlas page can hold (stage 3's D19): decided at import from
//! the cell the sheet would admit, by the element's role, for every object
//! that adds an image file to the sheet. Covers' crops are tested in
//! `import_covers.rs`, bitmap font pages in `import_fonts.rs` and Lua skins
//! in `lua.rs`.

mod common;

use common::{Loaded, hd, import_body, import_dir, kinds, skin_with};
use skin_beatoraja::{CriticalPart, ImportError, NoteKind, Severity};
use skin_model::{AtlasLimits, Variant};

/// One texel past the largest cell a page holds: 8 192 less two 8-texel
/// gutters is 8 176.
const OVER: NoteKind = NoteKind::CellTooLarge { w: 8_177, h: 2 };

/// Imports a skin of `body` with source `w` reading `w.png`, a `file`
/// PNG.
fn import_wide(file: [u32; 2], body: &str) -> Loaded {
    let json = hd(&format!(
        r#""source": [{{"id": "w", "path": "w.png"}}], {body}"#
    ));
    let dir = skin_with(&json, &[("w.png", file[0], file[1])]);
    let result = import_dir(dir.path());
    Loaded { dir, result }
}

/// One image of `w.png` cut `src`, drawn 1×1.
fn image(file: [u32; 2], src: &str) -> Loaded {
    import_wide(
        file,
        &format!(
            r#""image": [{{"id": "i", "src": "w", {src}}}],
               "destination": [{{"id": "i", "dst": [{{"w": 1, "h": 1}}]}}]"#
        ),
    )
}

/// A critical EX score whose ten glyphs are rows of `w.png`.
fn ex_score(file: [u32; 2]) -> Loaded {
    let [w, h] = file;
    import_wide(
        file,
        &format!(
            r#""value": [{{"id": "v", "src": "w", "w": {w}, "h": {h}, "divy": 10,
                           "digit": 4, "ref": 71}}],
               "destination": [{{"id": "v", "timer": 41, "dst": [{{"w": 12, "h": 20}}]}}]"#
        ),
    )
}

#[test]
fn an_image_is_dropped_only_past_the_page_s_content() {
    let whole = r#""w": -1, "h": -1"#;
    let fits = image([8_176, 2], whole);
    assert!(fits.report().notes.is_empty(), "{:?}", fits.report().notes);
    // The atlas agrees at the same size: the largest page holds it.
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 2,
        reserved_layers: 1,
    };
    let atlas = fits
        .ok()
        .skin
        .atlas(Variant::Lo, limits, &|| false)
        .unwrap();
    assert_eq!(atlas.side(), 8_192);

    // One texel more: dropped with its note, on either axis, and the rest
    // of the skin still builds.
    for (file, why) in [
        ([8_177, 2], OVER),
        ([2, 8_177], NoteKind::CellTooLarge { w: 2, h: 8_177 }),
    ] {
        let over = image(file, whole);
        let notes = &over.report().notes;
        assert_eq!(kinds(over.report()), [why], "{file:?}");
        assert_eq!(notes[0].severity, Severity::Dropped);
        assert!(notes[0].at.is_some(), "{file:?}");
        assert_eq!(
            why.to_string(),
            format!(
                "a frame of {}×{} with its gutter does not fit a page",
                file[0], file[1]
            )
        );
        assert!(
            over.ok()
                .skin
                .atlas(Variant::Lo, common::play::LIMITS, &|| false)
                .is_ok()
        );
        assert!(over.quads(0).is_empty(), "{file:?}");
    }
}

#[test]
fn a_cut_past_its_file_is_charged_at_its_full_size() {
    // Two 8 177-wide frames from a 64-wide file: each runs past the file's
    // edge, padded to the cell the source cuts, which no page holds.
    let over = image([64, 2], r#""x": 0, "y": 0, "w": 16354, "h": 2, "divx": 2"#);
    assert_eq!(kinds(over.report()), [OVER]);
    assert_eq!(over.report().notes[0].severity, Severity::Dropped);
    // One texel narrower, it fits.
    let fits = image([64, 2], r#""x": 0, "y": 0, "w": 16352, "h": 2, "divx": 2"#);
    assert!(fits.report().notes.is_empty(), "{:?}", fits.report().notes);
}

#[test]
fn the_cells_decide_not_the_file() {
    // A whole-file frame of a sheet whose frames fit goes alone: the file
    // and its small frames stay. Its 1×1 destination is not what drops it.
    let l = import_wide(
        [8_200, 2],
        r#""image": [{"id": "whole", "src": "w", "w": -1, "h": -1},
                     {"id": "frames", "src": "w", "w": 8200, "h": 2, "divx": 4}],
           "destination": [{"id": "whole", "dst": [{"w": 1, "h": 1}]},
                           {"id": "frames", "dst": [{"w": 500, "h": 500}]}]"#,
    );
    assert_eq!(
        kinds(l.report()),
        [NoteKind::CellTooLarge { w: 8_200, h: 2 }]
    );
    assert_eq!(l.report().usage.files, 1);
    let got = l.rects(0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][2] - got[0][0], 500);

    // A small destination of a fitting frame stays.
    let l = image([8_200, 2], r#""w": 8200, "h": 2, "divx": 4"#);
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.quads(0).len(), 1);
}

#[test]
fn a_frame_past_the_edge_counts_as_the_edge_line_it_cuts() {
    // Nominally 8 177 wide, wholly past a.png's right edge: the sheet
    // admits its last column.
    let l = import_body(
        r#""image": [{"id": "i", "src": "a", "x": 64, "w": 8177, "h": 2}],
           "destination": [{"id": "i", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.quads(0).len(), 1);
}

#[test]
fn a_critical_number_is_refused_located() {
    // 8 176 fits and is kept; 8 177 refuses the skin, naming the part and
    // the place.
    let fits = ex_score([8_176, 20]);
    assert!(fits.report().notes.is_empty(), "{:?}", fits.report().notes);
    match ex_score([8_177, 20]).err() {
        ImportError::Critical { part, why, at } => {
            assert_eq!(part, CriticalPart::ExScore);
            assert_eq!(why, OVER);
            assert_eq!(at.file, "s.json");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn an_image_group_is_dropped() {
    let group = |w: u32| {
        import_wide(
            [w, 4],
            &format!(
                r#""image": [{{"id": "g", "src": "w", "w": {w}, "h": 4, "divy": 2, "len": 2}}],
                   "destination": [{{"id": "g", "dst": [{{"w": 10, "h": 10}}]}}]"#
            ),
        )
    };
    let fits = group(8_176);
    assert!(fits.report().notes.is_empty(), "{:?}", fits.report().notes);
    assert_eq!(fits.ok().skin.choice_ref_count(), 2);
    let over = group(8_177);
    assert_eq!(kinds(over.report()), [OVER]);
    assert_eq!(over.ok().skin.choice_ref_count(), 0);
}

#[test]
fn an_impossible_entry_drops_its_image_set() {
    // An entry the source draws, as a file only this importer refuses: no
    // hole stands for it.
    let l = import_wide(
        [8_177, 2],
        r#""image": [{"id": "small", "src": "w", "w": 8, "h": 2},
                     {"id": "whole", "src": "w", "w": -1, "h": -1}],
           "imageset": [{"id": "s", "images": ["small", "whole"]}],
           "destination": [{"id": "s", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert_eq!(kinds(l.report()), [OVER]);
    assert_eq!(l.report().notes[0].severity, Severity::Dropped);
    assert_eq!(l.ok().skin.choice_ref_count(), 0);
    // Its fitting entry stays out of the sheet too.
    assert_eq!(l.ok().skin.sheet().image_count(), 0);
    assert!(l.quads(0).is_empty());
}

#[test]
fn a_dropped_image_set_cannot_fail_the_atlas() {
    // Its fitting entry is past `LIMITS`' 4 096 side: kept in the sheet, it
    // would fail the build of a skin that draws nothing of it.
    let l = import_wide(
        [8_177, 2],
        r#""image": [{"id": "big", "src": "w", "w": 8176, "h": 2},
                     {"id": "whole", "src": "w", "w": -1, "h": -1}],
           "imageset": [{"id": "s", "images": ["big", "whole"]}],
           "destination": [{"id": "s", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert_eq!(kinds(l.report()), [OVER]);
    let limits = common::play::LIMITS;
    assert!(l.ok().skin.atlas(Variant::Lo, limits, &|| false).is_ok());
}

#[test]
fn a_dropped_image_set_keeps_images_others_use() {
    // `plain` is drawn before the set and the set shares its image; `after`
    // follows it. Only the set goes, and `plain` and `after` keep their
    // images, in order.
    let l = import_wide(
        [8_177, 4],
        r#""image": [{"id": "plain", "src": "w", "w": 8, "h": 2},
                     {"id": "small", "src": "w", "y": 2, "w": 8, "h": 2},
                     {"id": "whole", "src": "w", "w": -1, "h": -1},
                     {"id": "after", "src": "w", "x": 8, "w": 8, "h": 2}],
           "imageset": [{"id": "s", "images": ["plain", "small", "whole"]},
                        {"id": "kept", "images": ["plain", "after"]}],
           "destination": [{"id": "kept", "dst": [{"w": 20, "h": 20}]},
                           {"id": "s", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert_eq!(
        kinds(l.report()),
        [NoteKind::CellTooLarge { w: 8_177, h: 4 }]
    );
    assert_eq!(l.ok().skin.sheet().image_count(), 2);
    assert_eq!(l.ok().skin.choice_ref_count(), 2);
    assert_eq!(l.quads(0).len(), 1);
}

#[test]
fn a_set_adds_its_entries_once_in_order() {
    // A repeated entry is one image, charged once, and the set still
    // selects each entry's own cell.
    let l = import_wide(
        [64, 2],
        r#""image": [{"id": "x", "src": "w", "w": 8, "h": 2},
                     {"id": "y", "src": "w", "x": 8, "w": 16, "h": 2}],
           "imageset": [{"id": "s", "images": ["x", "y", "x"], "ref": 0}],
           "destination": [{"id": "s", "dst": [{"w": 8, "h": 8}]}]"#,
    );
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.report().usage.images, 2);
    assert_eq!(l.ok().skin.sheet().image_count(), 2);
    assert_eq!(l.ok().skin.choice_ref_count(), 3);
}

#[test]
fn an_image_set_dropped_by_its_stretch_adds_nothing() {
    // Stretch 1 changes branch between the entries' keys for the wide
    // entry: the set goes before its entries reach the sheet.
    let l = import_wide(
        [64, 64],
        r#""image": [{"id": "x", "src": "w", "w": 16, "h": 16},
                     {"id": "y", "src": "w", "w": 64, "h": 8}],
           "imageset": [{"id": "s", "images": ["x", "y"]}],
           "destination": [{"id": "s", "stretch": 1,
                            "dst": [{"time": 0, "w": 32, "h": 32},
                                    {"time": 100, "w": 32, "h": 2}]}]"#,
    );
    assert_eq!(kinds(l.report()), [NoteKind::Stretch(1)]);
    assert_eq!(l.ok().skin.sheet().image_count(), 0);
}
