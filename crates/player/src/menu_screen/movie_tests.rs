//! A screen's movies against the screen instance they belong to
//! (design.md "Rendering and Skins", skin movies; "Memory Budget"; "Scope
//! of No Allocation"): each selection entry and each play preparation opens
//! its own set, and the set, its generation, the renderer's planes and the
//! loading frames' shows go together when that instance ends, by every path;
//! a movie is admitted at the boundary before a frame, never in it; one play
//! screen's loading frames and play frames share one movie clock.

use std::sync::Arc;

use bms_model::TimeUs;
use render::PreparedToken;
use skin_model::ScreenKind;
use skin_model::media::{Geometry, Writer};
use skin_model::movie::MovieShow;

use crate::media_host::session::{Loss, Movie};
use crate::movie_screen::{Holder, MediaHub};
use crate::render_side::Screen as _;

/// A WARP screen playing movies through `hub`.
fn screen(hub: &Arc<MediaHub>) -> super::MenuScreen {
    let r = render::Renderer::new(
        render::Target::Offscreen {
            width: 64,
            height: 64,
        },
        render::AdapterChoice::Warp,
    )
    .unwrap();
    super::MenuScreen::new(r, None).with_media(Arc::clone(hub))
}

/// A skin bundle naming one movie file in a folder of its own.
fn bundle(dir: &tempfile::TempDir) -> Arc<render::SkinBundle> {
    std::fs::write(dir.path().join("a.mp4"), b"x").unwrap();
    crate::render_side::tests::movies::movie_bundle_in(dir.path())
}

/// Publishes a frame of run 1, stamped `stamp` µs, whose every byte is
/// `id`, into live `movie`'s ring.
fn publish(movie: &Movie, seq: u64, stamp: i64, id: u8) {
    let live = movie.live().unwrap();
    let words = live.ring.words();
    let geometry = Geometry::new(live.width, live.height).unwrap();
    let writer = Writer::attach(words).unwrap();
    let slot = writer.claim(words).unwrap();
    let frame = vec![id; geometry.frame_bytes()];
    writer.publish(words, slot, &frame, seq, 1, stamp).unwrap();
}

/// A live 64 × 32 movie with a frame at each of `stamps` (µs).
fn clip(stamps: &[i64]) -> Arc<Movie> {
    let m = Movie::live_here(1, 64, 32);
    for (k, &stamp) in stamps.iter().enumerate() {
        let k = u64::try_from(k).unwrap();
        publish(&m, k + 1, stamp, u8::try_from(k + 1).unwrap());
    }
    m
}

/// A loading frame of `lifetime` at `now` on the frame clock, before the
/// lifetime published a state (its origin is `now`).
fn loading_frame(lifetime: u64, now: i64) -> render::LoadingFrame<'static> {
    crate::render_side::loading_frame_of(
        lifetime,
        TimeUs(now),
        None,
        None,
        0,
        skin_model::AppReadouts::LAUNCH,
    )
}

/// Play preparation `request`'s token on `s`'s device.
fn token(s: &super::MenuScreen, request: u64) -> PreparedToken {
    PreparedToken {
        request,
        device: s.renderer.device_generation(),
        kind: ScreenKind::Play,
    }
}

/// A play screen of preparation `request` with `bundle`, its movies
/// `movies`: opened, admitted, committed and begun with its READY anchor
/// at `anchor` µs.
fn play_screen(
    s: &mut super::MenuScreen,
    hub: &MediaHub,
    bundle: &Arc<render::SkinBundle>,
    request: u64,
    movies: [Option<Arc<Movie>>; 2],
    anchor: i64,
) {
    hub.supply(movies);
    s.open_play_movies(Some(bundle), request, Some(request));
    s.movie_boundary();
    s.commit(token(s, request)).unwrap();
    s.begin_play(anchor);
    assert!(matches!(s.movies.get(), Some((Holder::Playing, _))));
}

/// The stamp of the frame slot 0's planes hold.
fn shown(s: &super::MenuScreen) -> Option<i64> {
    s.movies.get().and_then(|(_, m)| m.movies.stamps()[0])
}

/// Play, then selection, then the next play's loading state: nothing of a
/// screen's movies outlives it. The play's loading frames show its movie;
/// selection's entry retires the play's set, its loading shows and its
/// planes before it reserves its own; the next loading state leaves
/// selection's, so their planes go and it shows no movie of theirs.
#[test]
fn a_screen_s_movies_never_outlive_it() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let (play, selection) = (bundle(&dir), bundle(&dir));
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let lost = Movie::new(1);
    let _ = lost.lose(Loss::Failed);
    hub.supply([Some(clip(&[0])), Some(lost)]);
    s.open_play_movies(Some(&play), 1, Some(1));
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Lost]
    ));
    assert!(s.renderer.video_bytes().0 > 0, "the play's planes");
    s.commit(token(&s, 1)).unwrap();
    s.begin_play(0);
    // Selection's entry.
    hub.supply([Some(clip(&[0])), None]);
    s.enter_selection_movies(&selection, 7, TimeUs(0));
    assert_eq!(
        s.renderer.loading_movies(),
        [MovieShow::Nothing; 2],
        "the play's shows left with it"
    );
    assert_eq!(s.renderer.video_bytes(), (0, 0), "the play's planes left");
    assert_eq!(hub.open_sets(), 1);
    s.movie_boundary();
    assert!(matches!(
        s.selection_movies(TimeUs(0))[0],
        MovieShow::Frame { .. }
    ));
    assert!(s.renderer.video_bytes().0 > 0, "selection's planes");
    // The next play's loading state, before its preparation opens its
    // movies.
    s.loading_movies(&loading_frame(2, 0));
    assert!(s.movies.get().is_none(), "selection's movies left");
    assert_eq!(s.renderer.loading_movies(), [MovieShow::Nothing; 2]);
    assert_eq!(s.renderer.video_bytes(), (0, 0), "selection's planes left");
    assert_eq!(hub.open_sets(), 0);
}

/// A practice Start quit after its commit and before READY, then another
/// Start: the second's loading lifetime publishes before its preparation
/// opens its movies, and its loading frames show none of the first
/// Start's (neither its frame nor its loss) and take no clock from them;
/// its own preparation's movies then show in them.
#[test]
fn a_later_start_s_loading_shows_nothing_of_a_committed_one() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let lost = Movie::new(1);
    let _ = lost.lose(Loss::Failed);
    hub.supply([Some(clip(&[0])), Some(lost)]);
    s.open_play_movies(Some(&play), 1, Some(1));
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Lost]
    ));
    s.commit(token(&s, 1)).unwrap();
    // Quit before READY: the practice page, then the next Start's loading
    // state, its preparation not begun.
    let _ = s.menu(&super::MenuPage::default(), true);
    assert!(matches!(s.movies.get(), Some((Holder::Committed(1), _))));
    s.loading_movies(&loading_frame(2, 3_000_000));
    assert_eq!(
        s.renderer.loading_movies(),
        [MovieShow::Nothing; 2],
        "the first Start's frame and loss in the second's loading frames"
    );
    assert_eq!(
        s.movies.get().and_then(|(_, m)| m.origin),
        Some(0),
        "no clock taken from the second lifetime"
    );
    // The second Start's preparation: its own movies show.
    hub.supply([Some(clip(&[0])), None]);
    s.open_play_movies(Some(&play), 2, Some(2));
    s.movie_boundary();
    s.loading_movies(&loading_frame(2, 3_000_000));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Nothing]
    ));
    assert_eq!(hub.open_sets(), 1);
}

/// A WARP screen playing movies through `hub`, with the fixture
/// selection skin installed when `installed` (kept alive by the folder
/// returned).
fn selection_screen(
    hub: &Arc<MediaHub>,
    installed: bool,
) -> (Option<tempfile::TempDir>, super::MenuScreen) {
    let mut r = render::Renderer::new(
        render::Target::Offscreen {
            width: 64,
            height: 64,
        },
        render::AdapterChoice::Warp,
    )
    .unwrap();
    let fixture = installed.then(|| {
        let dir = crate::selection_skin::fixture();
        let b = crate::selection_skin::fixture_bundle(&r, dir.path(), "s.json");
        let t = r.install(ScreenKind::Selection, &b, 1).unwrap();
        r.commit(t).unwrap();
        assert!(r.selection_bundle().is_some());
        dir
    });
    (
        fixture,
        super::MenuScreen::new(r, None).with_media(Arc::clone(hub)),
    )
}

/// Selection entered on its built-in page, with no selection skin
/// installed and with one installed that no state is for: it ends a
/// committed play not begun (quit before READY) and a skinned entry's set
/// alike. Each set's generation is left and its planes released before
/// the page draws.
#[test]
fn the_built_in_selection_page_retires_a_committed_play_and_a_skinned_entry() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let (play, skinned) = (bundle(&dir), bundle(&dir));
    for installed in [false, true] {
        for held in ["committed play", "skinned entry"] {
            let hub = MediaHub::with(None);
            let (_fixture, mut s) = selection_screen(&hub, installed);
            hub.supply([Some(clip(&[0])), None]);
            if held == "committed play" {
                s.open_play_movies(Some(&play), 1, Some(1));
                s.movie_boundary();
                s.commit(token(&s, 1)).unwrap();
            } else {
                s.enter_selection_movies(&skinned, 3, TimeUs(0));
                s.movie_boundary();
            }
            assert!(s.renderer.video_bytes().0 > 0, "{held}: its planes");
            let generation = hub.generation();
            let _ = s.selection(
                &crate::render_side::tests::selection_update(4, 1),
                None,
                true,
                TimeUs(0),
                skin_model::AppReadouts::LAUNCH,
            );
            let case = format!("installed {installed}, {held}");
            assert!(s.movies.get().is_none(), "{case}: the set retired");
            assert_eq!(hub.open_sets(), 0, "{case}: the set left");
            assert!(hub.generation() > generation, "{case}: its generation left");
            assert_eq!(s.renderer.video_bytes(), (0, 0), "{case}: its planes left");
        }
    }
}

/// Selection's entry ends with a page drawn in its place, the decision
/// screen's built-in presentation among them: the entry's movies, their
/// generation and planes go before that page draws.
#[test]
fn a_decision_by_the_built_in_page_retires_selection_s_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let selection = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    s.enter_selection_movies(&selection, 7, TimeUs(0));
    s.movie_boundary();
    assert!(s.renderer.video_bytes().0 > 0);
    let generation = hub.generation();
    let _ = s.menu(&super::MenuPage::default(), true);
    assert!(s.movies.get().is_none());
    assert_eq!(hub.open_sets(), 0, "the set left");
    assert!(hub.generation() > generation, "its generation left");
    assert_eq!(s.renderer.video_bytes(), (0, 0), "its planes left");
}

/// A selection skin refused at a frame is removed from the screen: its
/// entry's movies go with it, before the installation.
#[test]
fn a_selection_refusal_retires_its_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let selection = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    s.enter_selection_movies(&selection, 7, TimeUs(0));
    s.movie_boundary();
    s.remove(ScreenKind::Selection);
    assert!(s.movies.get().is_none());
    assert_eq!(hub.open_sets(), 0);
    assert_eq!(s.renderer.video_bytes(), (0, 0));
}

/// Each play screen opens its own movies: after a play whose movie shows
/// its frame at 20 s, a practice round (its page drawn between), a retry
/// from a practice result (its result drawn between) and a direct retry
/// (straight from play) each open a new set, the old frame's planes
/// released, so the new READY at 1 s on its own movie clock shows the new
/// movie's frame at 1 s, never the one held at 20 s.
#[test]
fn each_play_screen_opens_its_own_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    for between in ["practice page", "practice result", "direct retry"] {
        let hub = MediaHub::with(None);
        let mut s = screen(&hub);
        play_screen(
            &mut s,
            &hub,
            &play,
            1,
            [Some(clip(&[0, 20_000_000])), None],
            0,
        );
        let _ = s.play_movies(20_000_000);
        assert_eq!(shown(&s), Some(20_000_000), "{between}: the old frame");
        let first = s.movies.get().unwrap().1.generation();
        match between {
            "practice page" => {
                let _ = s.menu(&super::MenuPage::default(), true);
            }
            "practice result" => {
                let _ = s.submit_result();
            }
            _ => {}
        }
        play_screen(
            &mut s,
            &hub,
            &play,
            2,
            [Some(clip(&[0, 1_000_000])), None],
            10_000_000,
        );
        let _ = s.play_movies(11_000_000);
        assert_eq!(
            shown(&s),
            Some(1_000_000),
            "{between}: READY at 1 s shows its own frame, not the old 20 s one"
        );
        assert_ne!(s.movies.get().unwrap().1.generation(), first, "{between}");
        assert_eq!(hub.open_sets(), 1, "{between}: the old set left");
    }
}

/// A practice Start cancelled after its loading state drew, then another
/// Start: the second's loading frames and its play frames share the
/// second's movie clock (its own loading lifetime's origin, 5 s), so the
/// play at 6 s shows the frame at 1 s; neither the first Start's origin nor
/// the READY anchor moves it.
#[test]
fn a_cancelled_start_leaves_one_clock_for_the_next_loading_and_ready() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    s.open_play_movies(Some(&play), 1, Some(1));
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    s.discard_token(token(&s, 1));
    assert_eq!(hub.open_sets(), 0, "the cancelled Start's movies");
    hub.supply([Some(clip(&[0, 1_000_000, 3_000_000, 6_000_000])), None]);
    s.open_play_movies(Some(&play), 2, Some(2));
    s.movie_boundary();
    s.loading_movies(&loading_frame(2, 5_000_000));
    s.commit(token(&s, 2)).unwrap();
    s.begin_play(5_200_000);
    let _ = s.play_movies(6_000_000);
    assert_eq!(shown(&s), Some(1_000_000));
}

/// A device lost before a play's preparation commits and after: the
/// screen dropped drops its set and leaves its generation; nothing is kept
/// for the screen built again.
#[test]
fn a_device_loss_drops_the_set_before_and_after_commit() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    for committed in [false, true] {
        let hub = MediaHub::with(None);
        let mut s = screen(&hub);
        s.open_play_movies(Some(&play), 5, Some(5));
        assert_eq!(hub.open_sets(), 1);
        if committed {
            s.commit(token(&s, 5)).unwrap();
        }
        let generation = hub.generation();
        drop(s);
        assert_eq!(hub.open_sets(), 0, "committed {committed}");
        assert!(hub.generation() > generation, "committed {committed}");
        let again = screen(&hub);
        assert!(again.movies.get().is_none());
    }
}

/// A visible loading state draws the play's movies on its own clock and
/// polls them while it draws: with the peer still pending (the preparation
/// not settled), the ready clip moves on through 1 s, 1.5 s and 2 s rather
/// than holding a frame.
#[test]
fn loading_with_a_pending_peer_keeps_the_ready_clip_moving() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let a = clip(&[0, 1_000_000, 1_500_000, 2_000_000]);
    let pending = Movie::new(1);
    hub.supply([Some(Arc::clone(&a)), Some(Arc::clone(&pending))]);
    s.open_play_movies(Some(&play), 1, Some(1));
    s.movie_boundary();
    let page = super::MenuPage::default();
    let mut freed = Vec::new();
    for (k, at) in [0, 1_000_000, 1_500_000, 2_000_000].into_iter().enumerate() {
        let drawn = s
            .loading(&page, None, k == 0, &loading_frame(1, at))
            .unwrap();
        assert!(drawn.is_ok());
        freed.push(a.freed_total());
    }
    assert!(!pending.settled(), "the peer still pending");
    assert!(
        freed.windows(2).all(|w| w[1] > w[0]),
        "each loading frame took the clip's next due frame: {freed:?}"
    );
}

/// A selection movie gone live is admitted at the boundary before its
/// frame, through the renderer's own reservation, and never in the frame:
/// a frame before the boundary allocates nothing at all, wgpu included,
/// and shows nothing; after it, frames that take the movie's due frames
/// allocate nothing of the app's (design.md "Scope of No Allocation").
/// With a positive control.
#[test]
fn selection_movie_admission_alloc() {
    use crate::alloc_count::{app_allocations, this_thread};
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let selection = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[16_667, 33_334, 50_001, 66_668])), None]);
    s.enter_selection_movies(&selection, 7, TimeUs(0));
    // Live, not admitted yet: the frame neither admits nor reserves.
    let before = this_thread();
    let shows = s.selection_movies(TimeUs(16_667));
    let after = this_thread();
    assert_eq!(after - before, 0, "allocations of a frame before admission");
    assert_eq!(shows[0], MovieShow::Nothing);
    assert_eq!(s.renderer.video_bytes(), (0, 0));
    // The boundary: admitted, its planes reserved by the renderer.
    s.movie_boundary();
    let (texels, _) = s.renderer.video_bytes();
    assert_eq!(texels, 64 * 32 + 2 * 32 * 16, "the movie's planes");
    let before = app_allocations();
    let mut shown = 0;
    for k in 1..=4_i64 {
        if matches!(
            s.selection_movies(TimeUs(k * 16_667))[0],
            MovieShow::Frame { .. }
        ) {
            shown += 1;
        }
    }
    let after = app_allocations();
    assert_eq!(after - before, 0, "the app's allocations in the frames");
    assert_eq!(shown, 4, "each due frame taken and shown");
    // The positive control: an allocation in the app region is counted.
    {
        let _region = crate::alloc_count::AppRegion::enter();
        drop(std::hint::black_box(vec![0_u8; 16]));
    }
    assert!(app_allocations() > after);
}
