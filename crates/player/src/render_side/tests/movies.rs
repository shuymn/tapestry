//! The skin movies' holder through the render thread (design.md
//! "Rendering and Skins", skin movies): the movies a play preparation
//! opens are its own until its commit, so a practice Start cancelled
//! drops them, and a committed one keeps them until its play's screen
//! ends.

use super::*;
use crate::movie_screen::MediaHub;
use crate::render_side::AttemptSkin;

/// A play skin bundle naming one movie file, `a.mp4`.
fn movie_bundle() -> Arc<render::SkinBundle> {
    movie_bundle_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// [`movie_bundle`] in skin folder `dir`.
pub(crate) fn movie_bundle_in(dir: &std::path::Path) -> Arc<render::SkinBundle> {
    use skin_model::{Anchor, AtlasLimits, Fit, Region, RegionDef, Sheet, SkinDef, Variant};
    let sheet = Sheet::new(dir, Vec::new(), Vec::new())
        .unwrap()
        .with_movies(["a.mp4".to_owned()]);
    let skin = skin_model::Skin::new(SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet,
        lanes: None,
        elements: Vec::new(),
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    })
    .unwrap();
    let limits = AtlasLimits {
        max_side: 256,
        max_layers: 4,
        reserved_layers: 1,
    };
    let atlas = skin.atlas(Variant::Lo, limits, &|| false).unwrap();
    let clips = render::ClipTable::new(Vec::new()).unwrap();
    Arc::new(
        render::SkinBundle::new(Arc::new(skin), atlas, clips, skin_model::Side::Right).unwrap(),
    )
}

/// A render thread on a WARP menu screen playing movies through `hub`,
/// `chart` loaded and the movie bundle chosen for play.
fn on_menu(hub: &Arc<MediaHub>, chart: &Chart) -> super::super::RenderLink {
    let hub = Arc::clone(hub);
    let make: MakeScreen = Box::new(move || {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        let screen = crate::menu_screen::MenuScreen::new(r, None).with_media(Arc::clone(&hub));
        Ok(Box::new(screen) as Box<dyn Screen>)
    });
    let link = spawn_empty(make);
    link.load(Arc::new(chart.clone()), None).unwrap();
    assert_eq!(
        link.select(ScreenKind::Play, Some(movie_bundle()))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link
}

/// A practice Start answered ready and then cancelled, back to its page:
/// the movies it opened are dropped (their screen is left). The control,
/// the same Start committed, keeps them.
#[test]
fn a_cancelled_start_drops_its_movies_and_a_committed_one_keeps_them() {
    let _turn = crate::warp_turn::take();
    let chart = ChartBuilder::new(120_000).build().unwrap();
    for cancel in [true, false] {
        let hub = MediaHub::with(None);
        let link = on_menu(&hub, &chart);
        let mut pending = link.begin_attempt(titled(&chart), AttemptSkin::InUse);
        let answer = loop {
            if let Some(a) = pending.wait_until(None, || false) {
                break a;
            }
        };
        assert_eq!(answer.unwrap().preparation, Preparation::Ready);
        assert_eq!(hub.open_sets(), 1, "opened by the Start");
        if cancel {
            link.discard_attempt();
            link.send(RenderMsg::Menu(Box::default()));
            assert!(wait_until(|| hub.open_sets() == 0), "dropped");
        } else {
            link.commit_attempt().unwrap();
            link.send(RenderMsg::Menu(Box::default()));
            // A discard after the commit changes nothing.
            link.discard_attempt();
            let menus = link.shared.stamps[0].load(Ordering::SeqCst);
            assert!(wait_until(
                || link.shared.stamps[0].load(Ordering::SeqCst) > menus + 2
            ));
            assert_eq!(hub.open_sets(), 1, "kept once committed");
        }
        link.exit();
    }
}

/// A practice Start committed and then quit before READY: the game's
/// abandonment of its request, through the render thread, retires the
/// committed movies before the practice page draws, so the page holds
/// none of them; the abandonment of a later request than the one
/// committed leaves them.
// Revert: the render thread's `RenderMsg::Abandon` arm doing nothing ->
// "retired by the abandonment".
#[test]
fn an_abandoned_committed_start_drops_its_movies() {
    let _turn = crate::warp_turn::take();
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let hub = MediaHub::with(None);
    let link = on_menu(&hub, &chart);
    let mut pending = link.begin_attempt(titled(&chart), AttemptSkin::InUse);
    let answer = loop {
        if let Some(a) = pending.wait_until(None, || false) {
            break a;
        }
    };
    assert_eq!(answer.unwrap().preparation, Preparation::Ready);
    link.commit_attempt().unwrap();
    let committed = link.requests.load(Ordering::Relaxed);
    link.send(RenderMsg::Abandon(committed + 1));
    assert!(link.fence(Duration::from_secs(20)));
    assert_eq!(hub.open_sets(), 1, "another request's abandonment");
    link.abandon_attempt(committed);
    link.send(RenderMsg::PracticePage(Box::default()));
    assert!(
        wait_until(|| hub.open_sets() == 0),
        "retired by the abandonment"
    );
    link.exit();
}
