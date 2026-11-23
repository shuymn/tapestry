//! A screen's movies against the screen instance they belong to
//! (design.md "Rendering and Skins", skin movies; "Memory Budget"; "Scope
//! of No Allocation"): each selection entry and each play preparation opens
//! its own set, and the set, its generation, the renderer's planes and the
//! loading frames' shows go together when that instance ends, by every path;
//! a movie is admitted at the boundary before a frame, never in it; a
//! movie's clock, started by the first presented frame that draws it, runs
//! on through a play screen's loading frames, READY and play frames.

use std::sync::Arc;

use bms_model::TimeUs;
use render::PreparedToken;
use skin_model::ScreenKind;
use skin_model::media::{Geometry, Writer};
use skin_model::movie::MovieShow;

use crate::media_host::session::{Loss, Movie};
use crate::movie_screen::{Holder, MediaHub};
use crate::render_side::Screen as _;

/// A play frame at chart time 0, 1×: no chart video set here.
const CHART0: crate::movie_frame::ChartAt = crate::movie_frame::ChartAt { now: 0, k: 20 };

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
    let stamp = skin_model::media::Stamp {
        at: stamp,
        duration: 33_333,
    };
    writer.publish(words, slot, &frame, seq, 1, stamp).unwrap();
}

/// A live 64 × 32 movie with a frame at each of `stamps` (µs).
pub(crate) fn clip(stamps: &[i64]) -> Arc<Movie> {
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
/// `movies`: opened, admitted, committed and begun.
fn play_screen(
    s: &mut super::MenuScreen,
    hub: &MediaHub,
    bundle: &Arc<render::SkinBundle>,
    request: u64,
    movies: [Option<Arc<Movie>>; 2],
) {
    hub.supply(movies);
    s.open_play_movies(Some(bundle), request, Some(request), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.commit(token(s, request)).unwrap();
    s.begin_play();
    assert!(matches!(s.movies.get(), Some((Holder::Playing, _))));
}

/// The frame the last hook built, presented and drawing slot 0's movie
/// (the fixture skin draws no quad of its own).
fn present(s: &mut super::MenuScreen) {
    s.movies_presented(&render::FrameTiming {
        cpu: std::time::Duration::ZERO,
        sprites: 1,
        dropped: 0,
        outcome: render::FrameOutcome::Shown,
        present_count: None,
        movies: [true, false, false, false],
    });
}

/// Slot 0's clock origin on the frame clock, once started.
fn origin(s: &super::MenuScreen) -> Option<i64> {
    s.movies.get().and_then(|(_, m)| m.movies.origins()[0])
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
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Lost, ..]
    ));
    assert!(s.renderer.video_bytes().0 > 0, "the play's planes");
    s.commit(token(&s, 1)).unwrap();
    s.begin_play();
    // Selection's entry.
    hub.supply([Some(clip(&[0])), None]);
    s.enter_selection_movies(&selection, 7);
    assert_eq!(
        s.renderer.loading_movies(),
        [MovieShow::Nothing; 4],
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
    assert_eq!(s.renderer.loading_movies(), [MovieShow::Nothing; 4]);
    assert_eq!(s.renderer.video_bytes(), (0, 0), "selection's planes left");
    assert_eq!(hub.open_sets(), 0);
}

/// A committed play's movies show only in the loading frames of the
/// lifetime its request was published in: a loading frame of another
/// lifetime (a later Start's, drawn while the committed set is still held)
/// shows none of them (neither its frame nor its loss) and starts no
/// clock of theirs; the later Start's own preparation's movies then show
/// in it.
// Revert: `Movies::unhook` keeping the hook's frame time -> "no clock
// started by the second lifetime".
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
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Lost, ..]
    ));
    s.commit(token(&s, 1)).unwrap();
    assert!(matches!(s.movies.get(), Some((Holder::Committed(1), _))));
    s.loading_movies(&loading_frame(2, 3_000_000));
    present(&mut s);
    assert_eq!(
        s.renderer.loading_movies(),
        [MovieShow::Nothing; 4],
        "the first Start's frame and loss in the second's loading frames"
    );
    assert_eq!(origin(&s), None, "no clock started by the second lifetime");
    // The second Start's preparation: its own movies show.
    hub.supply([Some(clip(&[0])), None]);
    s.open_play_movies(Some(&play), 2, Some(2), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(2, 3_000_000));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Nothing, ..]
    ));
    assert_eq!(hub.open_sets(), 1);
}

/// A practice Start quit after its commit and before READY, back on the
/// practice page: the game's abandonment of its request retires the
/// committed set (its generation left, its planes released, its loading
/// shows gone) before the page draws, and nothing of it waits for a later
/// screen. The controls: an abandonment of another request, and one of a
/// preparation not committed (kept for a local retry), leave the set.
// Revert: `ScreenMovies::abandon` retiring nothing -> "the abandoned set
// retired".
#[test]
fn an_abandoned_committed_start_retires_its_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    // Not committed: a local retry keeps its preparation's movies.
    s.abandon(1);
    assert!(
        matches!(s.movies.get(), Some((Holder::Preparing(1), _))),
        "a preparation kept"
    );
    s.commit(token(&s, 1)).unwrap();
    s.abandon(2);
    assert!(
        matches!(s.movies.get(), Some((Holder::Committed(1), _))),
        "another request's abandonment"
    );
    assert!(s.renderer.video_bytes().0 > 0, "its planes");
    let generation = hub.generation();
    // The quit: the game abandons the request, then the practice page.
    s.abandon(1);
    let _ = s.menu(&super::MenuPage::default(), true);
    assert!(s.movies.get().is_none(), "the abandoned set retired");
    assert_eq!(hub.open_sets(), 0, "the set left");
    assert!(hub.generation() > generation, "its generation left");
    assert_eq!(s.renderer.video_bytes(), (0, 0), "its planes left");
    assert_eq!(s.renderer.loading_movies(), [MovieShow::Nothing; 4]);
}

/// A selection entry whose render loop never finds slack for the
/// boundary (a grant ready at every pass): its live movie is admitted at
/// the admission turn after its second frame, once, and shows from the
/// third frame on, within its preparation deadline, never through the
/// boundary.
// Revert: `MenuScreen::movie_turn` admitting nothing -> "shown by the
// third frame".
#[test]
fn a_selection_movie_without_slack_shows_after_its_admission_turn() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let selection = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    let start = std::time::Instant::now();
    s.enter_selection_movies(&selection, 7);
    let mut shows = Vec::new();
    for k in 0..6_i64 {
        shows.push(s.selection_movies(TimeUs(k * 16_667))[0]);
        s.movie_turn();
    }
    assert!(
        start.elapsed() < crate::media_host::session::preparation_deadline(start) - start,
        "within the deadline"
    );
    assert_eq!(shows[..2], [MovieShow::Nothing; 2], "{shows:?}");
    assert!(
        shows[2..]
            .iter()
            .all(|s| matches!(s, MovieShow::Frame { .. })),
        "shown by the third frame: {shows:?}"
    );
    let (texels, _) = s.renderer.video_bytes();
    assert_eq!(texels, 64 * 32 + 2 * 32 * 16, "its planes");
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
                s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
                s.chart_manifest(0, crate::bga::Manifest::empty());
                s.movie_boundary();
                s.commit(token(&s, 1)).unwrap();
            } else {
                s.enter_selection_movies(&skinned, 3);
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
    s.enter_selection_movies(&selection, 7);
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
    s.enter_selection_movies(&selection, 7);
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
/// released, so the new play's first frame is the new movie's first, never
/// the one held at 20 s, and a second on its own clock shows its frame at
/// 1 s.
#[test]
fn each_play_screen_opens_its_own_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    for between in ["practice page", "practice result", "direct retry"] {
        let hub = MediaHub::with(None);
        let mut s = screen(&hub);
        play_screen(&mut s, &hub, &play, 1, [Some(clip(&[0, 20_000_000])), None]);
        let _ = s.play_movies(1_000, CHART0);
        present(&mut s);
        let _ = s.play_movies(20_001_000, CHART0);
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
        play_screen(&mut s, &hub, &play, 2, [Some(clip(&[0, 1_000_000])), None]);
        let _ = s.play_movies(30_000_000, CHART0);
        present(&mut s);
        assert_eq!(
            shown(&s),
            Some(0),
            "{between}: its own first frame, not the old 20 s one"
        );
        let _ = s.play_movies(31_000_000, CHART0);
        assert_eq!(shown(&s), Some(1_000_000), "{between}: a second on");
        assert_ne!(s.movies.get().unwrap().1.generation(), first, "{between}");
        assert_eq!(hub.open_sets(), 1, "{between}: the old set left");
    }
}

/// A practice Start cancelled after its loading state drew, then another
/// Start: its movie, first drawn by its loading frame at 5 s, keeps that
/// clock through READY into play, so the play at 6 s shows the frame at
/// 1 s; neither the first Start's clock nor READY moves it.
// Revert: `begin_play` starting the movies' clocks again -> "the loading
// frame's clock through READY".
#[test]
fn a_cancelled_start_leaves_one_clock_for_the_next_loading_and_ready() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    hub.supply([Some(clip(&[0])), None]);
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(1, 0));
    present(&mut s);
    s.discard_token(token(&s, 1));
    assert_eq!(hub.open_sets(), 0, "the cancelled Start's movies");
    hub.supply([Some(clip(&[0, 1_000_000, 3_000_000, 6_000_000])), None]);
    s.open_play_movies(Some(&play), 2, Some(2), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    s.loading_movies(&loading_frame(2, 5_000_000));
    present(&mut s);
    assert_eq!(origin(&s), Some(5_000_000), "its own clock");
    s.commit(token(&s, 2)).unwrap();
    s.begin_play();
    let _ = s.play_movies(6_000_000, CHART0);
    assert_eq!(
        shown(&s),
        Some(1_000_000),
        "the loading frame's clock through READY"
    );
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
        s.open_play_movies(Some(&play), 5, Some(5), i64::MIN);
        s.chart_manifest(0, crate::bga::Manifest::empty());
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

/// A visible loading state polls the play's movies while it draws: with
/// the peer still pending (the preparation not settled), the ready clip,
/// drawn from the first loading frame, moves on through 1 s, 1.5 s and 2 s
/// rather than holding a frame.
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
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    s.chart_manifest(0, crate::bga::Manifest::empty());
    s.movie_boundary();
    // The first loading frame draws it: the fixture skin has no quad of
    // it, so a frame presented with its quad stands in for that one.
    s.loading_movies(&loading_frame(1, 0));
    present(&mut s);
    let page = super::MenuPage::default();
    let mut freed = vec![a.freed_total()];
    for (k, at) in [1_000_000, 1_500_000, 2_000_000].into_iter().enumerate() {
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
    s.enter_selection_movies(&selection, 7);
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
        present(&mut s);
    }
    let after = app_allocations();
    assert_eq!(after - before, 0, "the app's allocations in the frames");
    assert_eq!(shown, 4, "each due frame taken and shown");
    assert_eq!(
        s.movies.get().and_then(|(_, m)| m.movies.stamps()[0]),
        Some(66_668),
        "the last due frame"
    );
    // The positive control: an allocation in the app region is counted.
    {
        let _region = crate::alloc_count::AppRegion::enter();
        drop(std::hint::black_box(vec![0_u8; 16]));
    }
    assert!(app_allocations() > after);
}

/// A manifest of `files` chart video files made in `dir`, ids 0 and 1 in
/// slots 2 and 3, each starting at `starts` (µs) on the base and the layer.
fn manifest_with(dir: &std::path::Path, files: usize, starts: &[i64]) -> crate::bga::Manifest {
    use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga};
    let ev = |t: &i64, id| BgaEvent {
        time: TimeUs(*t),
        image: Some(BgaImageId(id)),
    };
    let bga = ChartBga::new(
        (1..=2)
            .map(|id| BgaImage {
                id,
                name: format!("v{id}.mp4"),
            })
            .collect(),
        starts.iter().map(|t| ev(t, 0)).collect(),
        starts.iter().map(|t| ev(t, 1)).collect(),
        vec![],
    )
    .unwrap();
    let slots = [Some(2), Some(3)];
    let open = |k: usize| {
        (k < files).then(|| {
            let path = dir.join(format!("v{k}.mp4"));
            std::fs::write(&path, b"x").unwrap();
            std::fs::File::open(path).unwrap()
        })
    };
    crate::bga::Manifest::of_files(
        [open(0), open(1)],
        skin_model::bga::ChartVideos::of(&bga, &slots[..files.max(1)]),
    )
}

/// The generation of the set held.
fn held_generation(s: &super::MenuScreen) -> u64 {
    s.movies.get().unwrap().1.generation()
}

/// A play's set declares the chart's videos first, in its slots and not
/// looping, then the skin's movie, and only once the manifest is there;
/// with the BGA off, the empty manifest opens the skin's movie alone.
#[test]
fn chart_videos_open_before_the_skin_s_movies() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    assert!(hub.asked().is_empty(), "nothing opened before the manifest");
    s.chart_manifest(0, manifest_with(dir.path(), 2, &[0]));
    let g = held_generation(&s);
    assert_eq!(
        hub.asked(),
        [(g, true, false), (g, true, false), (g, false, true)]
    );
    // The BGA off, a new BGA generation: an empty manifest, the skin's
    // movie alone.
    s.bga_generation(1);
    s.open_play_movies(Some(&play), 2, Some(2), i64::MIN);
    assert_eq!(hub.asked().len(), 3, "waits for its own generation's");
    s.chart_manifest(1, crate::bga::Manifest::empty());
    let g2 = held_generation(&s);
    assert_eq!(hub.asked()[3..], [(g2, false, true)]);
}

/// The manifest and the preparation's set meet in either order, with the
/// same set; a manifest of another BGA generation (a preparation left) is
/// not taken, and the next replaces it, its files closed.
#[test]
fn the_manifest_and_the_holder_meet_in_either_order() {
    use std::sync::atomic::Ordering;
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    // The manifest first.
    s.chart_manifest(0, manifest_with(dir.path(), 2, &[0]));
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    let g = held_generation(&s);
    assert_eq!(
        hub.asked(),
        [(g, true, false), (g, true, false), (g, false, true)]
    );
    // The set first, in BGA generation 5: generation 4's manifest is not
    // taken; 5's is.
    s.bga_generation(5);
    s.open_play_movies(Some(&play), 2, Some(2), i64::MIN);
    let alive = crate::bga::MANIFESTS.load(Ordering::SeqCst);
    s.chart_manifest(4, manifest_with(dir.path(), 2, &[0]));
    assert_eq!(hub.asked().len(), 3, "another generation's is not taken");
    assert_eq!(
        crate::bga::MANIFESTS.load(Ordering::SeqCst),
        alive,
        "the one before closed"
    );
    s.chart_manifest(5, manifest_with(dir.path(), 2, &[0]));
    let g2 = held_generation(&s);
    assert_eq!(
        hub.asked()[3..],
        [(g2, true, false), (g2, true, false), (g2, false, true)]
    );
    assert_eq!(
        crate::bga::MANIFESTS.load(Ordering::SeqCst),
        alive,
        "generation 4's closed"
    );
}

/// A play whose skin has no movie still waits for its chart video: not
/// ready before the manifest, nor before its first frame is staged; ready
/// once it is; and past the preparation deadline ready with the video lost.
#[test]
fn ready_waits_for_the_chart_videos_with_no_skin_movie() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let video = Movie::live_here(1, 64, 32);
    hub.supply_chart([Some(Arc::clone(&video)), None]);
    s.open_play_movies(None, 1, Some(1), i64::MIN);
    let now = std::time::Instant::now();
    assert!(!s.play_movies_settled(1, now), "no manifest");
    s.chart_manifest(0, manifest_with(dir.path(), 1, &[0]));
    s.movie_boundary();
    assert!(!s.play_movies_settled(1, now), "its first frame not staged");
    publish(&video, 1, 40_000, 7);
    assert!(s.play_movies_settled(1, now));
    // Another preparation whose video never stages: past the deadline,
    // ready with it lost.
    let stalled = Movie::live_here(1, 64, 32);
    hub.supply_chart([Some(Arc::clone(&stalled)), None]);
    s.open_play_movies(None, 2, Some(2), i64::MIN);
    s.chart_manifest(0, manifest_with(dir.path(), 1, &[0]));
    s.movie_boundary();
    let late = std::time::Instant::now()
        + crate::media_host::session::PREPARATION
        + std::time::Duration::from_millis(1);
    assert!(s.play_movies_settled(2, late));
    assert_eq!(stalled.loss(), Some(Loss::Preparation));
}

/// A retry and then a practice round each open a set of their own, whose
/// chart video shows its first frame at its first start.
#[test]
fn a_retry_and_a_round_open_their_own_chart_videos() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let mut generations = Vec::new();
    // The attempt, its retry, and a round whose lead-in starts at 1 s.
    for (request, boundary) in [(1, i64::MIN), (2, i64::MIN), (3, 1_000_000)] {
        let video = Movie::live_here(1, 64, 32);
        publish(&video, 1, 40_000, u8::try_from(request).unwrap());
        hub.supply_chart([Some(Arc::clone(&video)), None]);
        s.open_play_movies(None, request, Some(request), boundary);
        s.chart_manifest(0, manifest_with(dir.path(), 1, &[2_000_000]));
        s.movie_boundary();
        assert!(s.play_movies_settled(request, std::time::Instant::now()));
        s.commit(token(&s, request)).unwrap();
        s.begin_play();
        generations.push(held_generation(&s));
        let chart = |now| crate::movie_frame::ChartAt { now, k: 20 };
        let before = s.play_movies(9_000_000, chart(1_500_000));
        assert_eq!(
            before[2],
            MovieShow::Nothing,
            "attempt {request}: before its start"
        );
        let at = s.play_movies(9_500_000, chart(2_000_000));
        assert!(
            matches!(at[2], MovieShow::Frame { .. }),
            "attempt {request}"
        );
        assert_eq!(s.movies.get().unwrap().1.movies.stamps()[2], Some(40_000));
    }
    generations.dedup();
    assert_eq!(generations.len(), 3, "a set of its own each");
}

/// The clip's first frame's stamp in [`bga_pixels`]' video.
const CLIP_FIRST: i64 = 40_000;
/// Its two frames' luma, the chroma neutral: two greys a read-back tells
/// apart, neither black.
pub(crate) const CLIP_LUMA: [u8; 2] = [60, 180];

/// Publishes a frame of run 1 stamped `stamp` µs, its luma `luma` and its
/// chroma neutral, into live `movie`'s ring.
fn publish_grey(movie: &Movie, seq: u64, stamp: i64, luma: u8) {
    let live = movie.live().unwrap();
    let words = live.ring.words();
    let geometry = Geometry::new(live.width, live.height).unwrap();
    let writer = Writer::attach(words).unwrap();
    let slot = writer.claim(words).unwrap();
    let y = usize::try_from(live.width * live.height).unwrap();
    let mut frame = vec![128_u8; geometry.frame_bytes()];
    frame[..y].fill(luma);
    let stamp = skin_model::media::Stamp {
        at: stamp,
        duration: 500_000,
    };
    writer.publish(words, slot, &frame, seq, 1, stamp).unwrap();
}

/// The colour a read-back of `luma` with neutral chroma gives, limited
/// range.
pub(crate) fn grey(luma: u8) -> [u8; 3] {
    let v = ((u32::from(luma) - 16) * 255 + 109) / 219;
    [u8::try_from(v).unwrap(); 3]
}

/// Asserts `got` is within 3 levels of `want` in each channel.
#[track_caller]
pub(crate) fn near(got: [u8; 3], want: [u8; 3], what: &str) {
    let off = got.iter().zip(want).map(|(g, w)| g.abs_diff(w)).max();
    assert!(
        off.is_some_and(|o| o <= 3),
        "{what}: {got:?} against {want:?}"
    );
}

/// What a play's `bga` destination reads back at each of `times` (chart
/// µs), entering through the play's preparation on a WARP screen: the
/// chart `bga` loaded, `entries` published as its BGA table with no array
/// and `manifest` sent by the BGA job, the attempt prepared with the skin in
/// use, or with a skin staged for it when `staged`, its movies settled,
/// committed and begun. The manifest's slot 2 plays a 64 × 32 clip of two
/// frames, stamped [`CLIP_FIRST`] and 500 ms later, of [`CLIP_LUMA`].
pub(crate) fn bga_pixels(
    bga: bms_model::ChartBga,
    entries: Vec<skin_model::bga::BgaEntry>,
    manifest: crate::bga::Manifest,
    staged: bool,
    times: &[i64],
) -> Vec<[u8; 3]> {
    use crate::render_decl::{BGA_RECT, Look, bundle};
    let dir = tempfile::tempdir().unwrap();
    let hub = MediaHub::with(None);
    let r = crate::render_common::offscreen(1_920, 1_080);
    let mut s = super::MenuScreen::new(r, None).with_media(Arc::clone(&hub));
    let look = Look {
        bga: true,
        ..Look::left([255, 255, 255, 255])
    };
    let (installed, _) = bundle(&s.renderer, dir.path(), &look);
    assert_eq!(
        s.install(ScreenKind::Play, &installed, 1).unwrap(),
        game::Preparation::Ready
    );
    s.commit(token(&s, 1)).unwrap();
    let staging = staged.then(|| bundle(&s.renderer, dir.path(), &look).0);
    let mut chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
    chart.bga = Some(bga);
    s.load(&chart, None).unwrap();
    let video = Movie::live_here(1, 64, 32);
    publish_grey(&video, 1, CLIP_FIRST, CLIP_LUMA[0]);
    publish_grey(&video, 2, CLIP_FIRST + 500_000, CLIP_LUMA[1]);
    hub.supply_chart([Some(Arc::clone(&video)), None]);
    // The BGA job's messages: its manifest, then its table.
    s.bga_generation(0);
    s.chart_manifest(0, manifest);
    s.publish_bga(None, entries).unwrap();
    let ctx = crate::render_common::context(&crate::render_common::GAUGES, false);
    let a = crate::render_common::attempt(&s.renderer, &chart, &ctx);
    let request = 2;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let ready = loop {
        if let Some(answer) = s.prepare_attempt(&a, staging.as_ref(), request, Some(request)) {
            break answer.unwrap();
        }
        assert!(std::time::Instant::now() < end, "never ready");
        // A frame boundary: the staging's upload steps, then the movies'
        // admission.
        let _ = s.upload_boundary(&mut || false);
        s.wait_step(std::time::Duration::from_millis(10)).unwrap();
        s.movie_boundary();
    };
    assert_eq!(ready, game::Preparation::Ready);
    s.commit(token(&s, request)).unwrap();
    s.begin_play();
    let v = crate::render_common::view(0, 0);
    let [x, y, w, h] = BGA_RECT.map(|c| u32::try_from(c).unwrap());
    times
        .iter()
        .map(|&now| {
            s.movie_boundary();
            s.movie_frame(now + 9_000_000, crate::movie_frame::ChartAt { now, k: 20 });
            let f = render::FrameInput {
                target: s.target(),
                ..crate::render_common::input(&v, TimeUs(now), TimeUs(0), &[])
            };
            s.build(&f).unwrap();
            s.draw().unwrap();
            let pixels = crate::render_common::read_back(&s.renderer);
            crate::render_common::rgb_at(&pixels, 1_920, (x + w / 2, y + h / 2))
        })
        .collect()
}

/// A chart whose base names id 0, a video, at 1 s, and the manifest the
/// BGA job sends for it: id 0 in slot 2.
fn video_chart(dir: &std::path::Path) -> (bms_model::ChartBga, crate::bga::Manifest) {
    use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga};
    let bga = ChartBga::new(
        vec![BgaImage {
            id: 1,
            name: "v.mp4".to_owned(),
        }],
        vec![BgaEvent {
            time: TimeUs(1_000_000),
            image: Some(BgaImageId(0)),
        }],
        vec![],
        vec![],
    )
    .unwrap();
    let path = dir.join("v.mp4");
    std::fs::write(&path, b"x").unwrap();
    let manifest = crate::bga::Manifest::of_files(
        [Some(std::fs::File::open(path).unwrap()), None],
        skin_model::bga::ChartVideos::of(&bga, &[Some(2)]),
    );
    (bga, manifest)
}

/// The chart's video reaches the pixels of the play's `bga` destination
/// on the production path (design.md "Rendering and Skins", BGA): entered
/// through the play's preparation, with the skin in use and with one
/// staged for the attempt, the destination reads back the base's black
/// placeholder before the video's start, the clip's first frame 100 ms
/// after it and its second frame 600 ms after it, each frame's grey.
#[test]
fn a_chart_video_s_frames_reach_the_bga_destination() {
    let _turn = crate::warp_turn::take();
    for staged in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (bga, manifest) = video_chart(dir.path());
        let got = bga_pixels(
            bga,
            vec![skin_model::bga::BgaEntry::Video],
            manifest,
            staged,
            &[500_000, 1_100_000, 1_600_000],
        );
        assert_eq!(
            got[0], [0; 3],
            "staged {staged}: the placeholder before the start"
        );
        near(
            got[1],
            grey(CLIP_LUMA[0]),
            &format!("staged {staged}: the first frame"),
        );
        near(
            got[2],
            grey(CLIP_LUMA[1]),
            &format!("staged {staged}: the second frame"),
        );
    }
}

/// A play's set waits for its manifest 1.5 s from its preparation's start
/// (design.md "Load Pipeline", movies at READY): a manifest that comes at
/// 1.4 s opens the chart's videos and then the skin's movie; past 1.5 s
/// with none, the skin's movie opens alone, stages its first frame and
/// plays, not lost, and a manifest that comes later opens nothing.
#[test]
fn the_manifest_is_waited_for_half_the_deadline() {
    use std::time::{Duration, Instant};
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    // At 1.4 s: still waiting, and the manifest then taken.
    let before = Instant::now();
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    assert!(!s.play_movies_settled(1, before + Duration::from_millis(1_400)));
    assert!(hub.asked().is_empty(), "nothing opened while it waits");
    s.chart_manifest(0, manifest_with(dir.path(), 1, &[0]));
    let g = held_generation(&s);
    assert_eq!(hub.asked(), [(g, true, false), (g, false, true)]);
    // At 1.6 s with no manifest: the skin's movie alone, which plays.
    s.bga_generation(1);
    let skin = Movie::live_here(1, 64, 32);
    hub.supply([Some(Arc::clone(&skin)), None]);
    s.open_play_movies(Some(&play), 2, Some(2), i64::MIN);
    let past = Instant::now() + Duration::from_millis(1_600);
    assert!(
        !s.play_movies_settled(2, past),
        "its first frame not staged"
    );
    assert_eq!(s.movies.get().unwrap().1.awaits(), None, "no longer waits");
    publish(&skin, 1, 40_000, 7);
    s.movie_boundary();
    assert!(s.play_movies_settled(2, past), "staged within the deadline");
    assert_eq!(skin.loss(), None, "not lost");
    s.commit(token(&s, 2)).unwrap();
    s.begin_play();
    let shows = s.play_movies(9_000_000, CHART0);
    assert!(matches!(shows[0], MovieShow::Frame { .. }), "{shows:?}");
    // A manifest after the wait opens nothing.
    let asked = hub.asked().len();
    s.chart_manifest(1, manifest_with(dir.path(), 1, &[0]));
    assert_eq!(hub.asked().len(), asked);
}

/// A manifest that comes past the 1.5 s wait, before the poll that would
/// find the wait over (design.md "Load Pipeline", movies at READY), opens
/// no chart video: the skin's movie opens alone, as at that poll, and the
/// set no longer waits.
#[test]
fn a_manifest_past_the_wait_before_its_poll_opens_no_chart_video() {
    use std::time::{Duration, Instant};
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let before = Instant::now();
    s.open_play_movies(Some(&play), 1, Some(1), i64::MIN);
    let after = Instant::now();
    assert!(!s.play_movies_settled(1, before + Duration::from_millis(1_490)));
    assert!(hub.asked().is_empty(), "nothing opened while it waits");
    s.chart_manifest_at(
        0,
        manifest_with(dir.path(), 1, &[0]),
        after + Duration::from_millis(1_510),
    );
    let g = held_generation(&s);
    assert_eq!(hub.asked(), [(g, false, true)], "the skin's movie alone");
    assert_eq!(s.movies.get().unwrap().1.awaits(), None, "no longer waits");
}

/// The manifest a screen keeps goes with its play (its files closed):
/// a preparation cancelled before its commit leaves it for the next
/// preparation of the same BGA generation, which reads nothing again and
/// takes it; that play's set retired, by a frame of another screen, closes
/// it; and leaving the chart closes a manifest kept with no set.
#[test]
fn a_retired_play_closes_its_manifest() {
    use std::sync::atomic::Ordering;
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let alive = || crate::bga::MANIFESTS.load(Ordering::SeqCst);
    let before = alive();
    s.chart_manifest(0, manifest_with(dir.path(), 2, &[0]));
    s.open_play_movies(None, 1, Some(1), i64::MIN);
    assert_eq!(hub.asked().len(), 2, "taken");
    s.discard();
    assert!(s.movies.get().is_none());
    assert_eq!(alive(), before + 1, "kept for the next preparation");
    s.open_play_movies(None, 2, Some(2), i64::MIN);
    let g = held_generation(&s);
    assert_eq!(hub.asked()[2..], [(g, true, false), (g, true, false)]);
    s.commit(token(&s, 2)).unwrap();
    s.begin_play();
    assert_eq!(alive(), before + 1, "kept while its play holds it");
    s.loading_movies(&loading_frame(9, 0));
    assert!(s.movies.get().is_none(), "retired");
    assert_eq!(alive(), before, "closed with its play");
    assert!(!s.movies.keeps_manifest());
    // The chart left: a manifest kept with no set is closed.
    s.chart_manifest(0, manifest_with(dir.path(), 2, &[0]));
    assert_eq!(alive(), before + 1);
    s.drop_bga();
    assert_eq!(alive(), before, "closed when the chart is left");
}
