//! Target names in text (docs/stage3.md S10-44; design.md "Personal
//! Bests", Target): a selection skin shows the chosen target's name and
//! its neighbours', a result skin the chosen one's alone, through the
//! outline requests and the bitmap lines alike; a neighbour beyond ten,
//! or a target on a screen that shows none, is refused at load.

mod result_support;
mod selection_support;

use result_support::{RFx, labels, ready, relement};
use selection_support::{Fx, chart, folder, labels_of, view};
use skin_model::{
    BestView, FontRef, LoadError, Overflow, ResultDefs, ResultLabels, ScreenKind, SelectionDefs,
    SelectionLabels, Skin, Source, TargetName, TargetNames, TextDef, TextError, TextValue,
    WidgetDefs,
};

fn text(name: TargetName) -> TextDef {
    TextDef {
        value: TextValue::Target(name),
        font: FontRef::Default,
        align: skin_model::Align::Left,
        overflow: Overflow::Spill,
    }
}

fn bitmap(name: TargetName) -> TextDef {
    TextDef {
        font: FontRef::Bitmap(0),
        ..text(name)
    }
}

/// Names the player would not give: `A`, `B` and `AB` runs whose lengths
/// tell each name apart in glyph quads, the chosen `AAAAAAAAAAA` (11),
/// `k` before it `k` `A`s, `k` after it `k` `B`s.
fn names() -> TargetNames {
    TargetNames {
        chosen: "A".repeat(11),
        before: std::array::from_fn(|i| "A".repeat(i + 1)),
        after: std::array::from_fn(|i| "B".repeat(i + 1)),
    }
}

fn selection(texts: Vec<TextDef>, sheet: Option<skin_model::Sheet>) -> Result<Skin, LoadError> {
    let elements = (0_u32..)
        .zip(&texts)
        .map(|(i, _)| relement(Source::Text(i), [0, 20 * i as i32, 300, 10], None))
        .collect();
    let mut def = selection_support::skin_def(
        elements,
        WidgetDefs {
            texts,
            ..WidgetDefs::default()
        },
    );
    if let Some(sheet) = sheet {
        def.sheet = sheet;
    }
    Skin::new_selection(def, SelectionDefs::default())
}

fn result(texts: Vec<TextDef>, sheet: Option<skin_model::Sheet>) -> Result<Skin, LoadError> {
    let elements = (0_u32..)
        .zip(&texts)
        .map(|(i, _)| relement(Source::Text(i), [0, 20 * i as i32, 300, 10], None))
        .collect();
    let mut def = selection_support::skin_def(
        elements,
        WidgetDefs {
            texts,
            ..WidgetDefs::default()
        },
    );
    if let Some(sheet) = sheet {
        def.sheet = sheet;
    }
    Skin::new_result(def, ResultDefs::default())
}

/// The selection skin's names: the chosen one, the tenth and first before
/// it, the first and tenth after it, through the outline requests on a
/// chart row, a folder row and an empty list, and in a bitmap font the
/// same names' glyphs.
#[test]
fn selection_texts_show_the_labels_target_names() {
    use TargetName::{After, Before, Chosen};
    let shown = [Chosen, Before(10), Before(1), After(1), After(10)];
    let mut fx = Fx::new(selection(shown.iter().map(|&n| text(n)).collect(), None).unwrap());
    let labels = SelectionLabels {
        targets: names(),
        ..labels_of()
    };
    let n = names();
    let want = [
        n.chosen.as_str(),
        &n.before[9],
        &n.before[0],
        &n.after[0],
        &n.after[9],
    ];
    for v in [
        view(vec![chart("a"), folder("F", 1)], Some(0)),
        view(vec![chart("a"), folder("F", 1)], Some(1)),
        view(Vec::new(), None),
    ] {
        fx.skin.update_selection(&mut fx.sel, &v, &labels).unwrap();
        let req = fx
            .skin
            .selection_text_requests(&fx.sel, fx.target, &fx.layout);
        let got: Vec<&str> = req.texts.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(got, want, "{:?}", v.cursor);
    }
    // Bitmap lines: one glyph quad per character of each name, 11 + 10 +
    // 1 + 1 + 10.
    let (_dir, sheet) = selection_support::bitmap_sheet();
    let mut fx =
        Fx::new(selection(shown.iter().map(|&n| bitmap(n)).collect(), Some(sheet)).unwrap());
    fx.skin
        .update_selection(&mut fx.sel, &view(Vec::new(), None), &labels)
        .unwrap();
    fx.skin.attach_selection_bitmap_text(&mut fx.sel);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 11 + 10 + 1 + 1 + 10);
}

/// The result skin's text 3: the labels' name in the outline requests and
/// the bitmap line, and none once a practice round's labels, which name
/// none, land: its bitmap line goes with it.
#[test]
fn result_text_shows_the_plays_target_name_and_a_practice_round_none() {
    let named = ResultLabels {
        target: "ABAB".to_owned(),
        ..labels()
    };
    let practice = ResultLabels {
        target: String::new(),
        ..labels()
    };
    let view = ready(1, BestView::Unavailable, 1);
    let mut fx = RFx::new(result(vec![text(TargetName::Chosen)], None).unwrap());
    fx.skin.update_result(&mut fx.res, &view, &named).unwrap();
    let req = fx.skin.result_text_requests(&fx.res, fx.target, &fx.layout);
    let got: Vec<&str> = req.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(got, ["ABAB"]);
    fx.skin
        .update_result(&mut fx.res, &view, &practice)
        .unwrap();
    assert!(
        fx.skin
            .result_text_requests(&fx.res, fx.target, &fx.layout)
            .is_empty()
    );
    let (_dir, sheet) = selection_support::bitmap_sheet();
    let mut fx = RFx::new(result(vec![bitmap(TargetName::Chosen)], Some(sheet)).unwrap());
    fx.skin.update_result(&mut fx.res, &view, &named).unwrap();
    fx.skin.attach_result_bitmap_text(&mut fx.res);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 4);
    fx.skin
        .update_result(&mut fx.res, &view, &practice)
        .unwrap();
    fx.skin.attach_result_bitmap_text(&mut fx.res);
    fx.frame(0);
    assert_eq!(fx.batch.quads().len(), 0);
}

/// A neighbour beyond ten, or none, is refused at load; a result skin
/// shows the chosen target alone, a play or decision skin none.
#[test]
fn target_texts_out_of_range_or_off_their_screens_are_refused() {
    use TargetName::{After, Before, Chosen};
    for bad in [Before(0), Before(11), After(0), After(11)] {
        assert!(
            matches!(
                selection(vec![text(bad)], None),
                Err(LoadError::Text {
                    element: 0,
                    error: TextError::TargetOffset
                })
            ),
            "{bad:?}"
        );
    }
    assert!(selection(vec![text(Before(10)), text(After(10))], None).is_ok());
    assert!(result(vec![text(Chosen)], None).is_ok());
    for n in [Before(1), After(1)] {
        assert!(
            matches!(
                result(vec![text(n)], None),
                Err(LoadError::SourceKind {
                    kind: ScreenKind::Result,
                    ..
                })
            ),
            "{n:?}"
        );
    }
    let def = || {
        selection_support::skin_def(
            vec![relement(Source::Text(0), [0, 0, 300, 10], None)],
            WidgetDefs {
                texts: vec![text(Chosen)],
                ..WidgetDefs::default()
            },
        )
    };
    assert!(matches!(
        Skin::new_decision(def(), skin_model::DecisionDefs::default()),
        Err(LoadError::SourceKind {
            kind: ScreenKind::Decision,
            ..
        })
    ));
    assert!(matches!(
        Skin::new(def()),
        Err(LoadError::SourceKind {
            kind: ScreenKind::Play,
            ..
        })
    ));
}
