//! The selection skin on a real screen, on WARP through the render thread
//! (dev-tools: the screen reports its selection frames and text).

use std::sync::{Arc, Mutex};

use game::Preparation;
use input::QpcClock;
use skin_model::{RowKey, RowKind, RowScore, RowView, ScreenKind, SelectionPlace, SelectionView};

use super::super::{MakeScreen, measure, view, wait_until};
use super::update_of;
use crate::menu_screen::MenuScreen;
use crate::render_side::{FrameSink, RenderLink, Screen, spawn};

/// What a real screen's frames show: the selection frame's quads and the
/// lines the skin's text asked for.
#[derive(Clone, Default)]
struct Frames(Arc<Mutex<Vec<Frame>>>);

/// One frame: its selection quads, and the lines asked for.
type Frame = (Option<usize>, Vec<String>);

impl FrameSink for Frames {
    fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
        let quads = screen.selection_quads().map(|q| q.len());
        let (lines, _) = screen.selection_requests();
        self.0.lock().unwrap().push((quads, lines));
    }
}

/// A render thread on a WARP menu screen whose renderer refuses its first
/// selection frame, or its first selection text with `text` (`None`:
/// neither), and its frames.
fn warp_link(fault: Option<bool>) -> (RenderLink, Frames) {
    let make: MakeScreen = Box::new(move || {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        let mut s = MenuScreen::new(r, Some(render::text::Fonts::runtime()?));
        if let Some(text) = fault {
            s.renderer_mut().refuse_next_selection(text);
        }
        Ok(Box::new(s) as Box<dyn Screen>)
    });
    let frames = Frames::default();
    let clock = QpcClock::start();
    let link = spawn(
        make,
        None,
        None,
        clock,
        view(),
        measure(clock),
        Box::new(frames.clone()),
    )
    .unwrap();
    (link, frames)
}

fn row(key: RowKey, kind: RowKind, title: &str) -> RowView {
    RowView {
        key,
        kind,
        title: title.to_owned(),
        subtitle: String::new(),
        artist: String::new(),
        level: String::new(),
        level_value: 0,
        difficulty: None,
        score: RowScore::Unplayed,
        features: None,
        random: false,
    }
}

/// A view of the all-charts row, a folder and a chart at `place` with
/// `query`, the cursor on the first.
fn listed(place: SelectionPlace, query: &str) -> SelectionView {
    let folder = skin_model::FolderId::new(std::path::PathBuf::from("曲集"));
    SelectionView {
        rows: vec![
            row(RowKey::All, RowKind::All { charts: 2 }, ""),
            row(
                RowKey::Folder(folder),
                RowKind::Folder { charts: 1 },
                "曲集",
            ),
            row(
                RowKey::Chart(std::path::PathBuf::from("a.bms")),
                RowKind::Chart,
                "譜面の名前",
            ),
        ],
        cursor: Some(0),
        position: None,
        place,
        query: query.to_owned(),
        selected: None,
    }
}

/// The fixture skin built for a WARP target.
fn skin() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let dir = crate::selection_skin::fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080);
    let skin = crate::selection_skin::fixture_bundle(&limits, dir.path());
    (dir, skin)
}

/// The selection skin installed on a real screen draws each update with
/// the player's labels, folder names and the query as they are: the
/// all-charts row's label, the place's text at All, in the results and in
/// a folder, and the search's text; without the skin the same updates
/// draw the page, and no selection frame.
#[test]
fn the_installed_skin_draws_each_place_with_the_players_labels() {
    let (_dir, skin) = skin();
    let (link, frames) = warp_link(None);
    assert_eq!(
        link.select(ScreenKind::Selection, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.enter_selection(1);
    let asked = |lines: &[&str]| {
        wait_until(|| {
            frames.0.lock().unwrap().iter().any(|(quads, asked)| {
                quads.is_some_and(|n| n > 0) && lines.iter().all(|l| asked.iter().any(|a| a == l))
            })
        })
    };
    let places = [
        (
            SelectionPlace::All,
            "",
            vec!["全譜面", "全譜面 > ", "曲集", "譜面の名前"],
        ),
        (SelectionPlace::Results, "abc", vec!["検索結果 > ", "abc"]),
        (
            SelectionPlace::Folder(vec!["曲集".to_owned(), "下の階".to_owned()]),
            "",
            vec!["曲集 > 下の階 > "],
        ),
    ];
    for (revision, (place, query, lines)) in (1..).zip(places) {
        link.show_selection(update_of(1, revision, listed(place, query)));
        assert!(
            asked(&lines),
            "{lines:?}: {:?}",
            frames.0.lock().unwrap().last()
        );
    }
    assert_eq!(link.take_selection_refusal(), None);
    link.exit();

    // No skin installed: the page, and no selection frame.
    let (link, frames) = warp_link(None);
    link.enter_selection(1);
    link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
    assert!(wait_until(|| frames.0.lock().unwrap().len() > 3));
    assert!(frames.0.lock().unwrap().iter().all(|(q, _)| q.is_none()));
    link.exit();
}

/// A selection frame or its text refused on a healthy device draws the
/// page, removes the skin from the screen and from what a screen built
/// again restores, and says why once.
#[test]
fn a_refused_selection_frame_or_text_falls_back_for_the_run() {
    for text in [false, true] {
        let (_dir, skin) = skin();
        let (link, frames) = warp_link(Some(text));
        assert_eq!(
            link.select(ScreenKind::Selection, Some(skin))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        link.enter_selection(1);
        link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
        let why = Mutex::new(None);
        assert!(wait_until(|| {
            let mut why = why.lock().unwrap();
            if why.is_none() {
                *why = link.take_selection_refusal();
            }
            why.is_some()
        }));
        let why = why.into_inner().unwrap().unwrap();
        assert_eq!(why.contains("development fault"), text, "{why}");
        // From then on, and on a screen built again, the page.
        let drawn = frames.0.lock().unwrap().len();
        assert!(wait_until(|| frames.0.lock().unwrap().len() > drawn + 2));
        assert!(
            frames.0.lock().unwrap()[drawn..]
                .iter()
                .all(|(q, _)| q.is_none())
        );
        link.prepare().unwrap();
        link.show_selection(update_of(1, 2, listed(SelectionPlace::All, "")));
        let drawn = frames.0.lock().unwrap().len();
        assert!(wait_until(|| frames.0.lock().unwrap().len() > drawn + 2));
        assert!(
            frames.0.lock().unwrap()[drawn..]
                .iter()
                .all(|(q, _)| q.is_none())
        );
        assert_eq!(link.take_selection_refusal(), None);
        link.exit();
    }
}
