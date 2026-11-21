//! A screen's movies against their owner (design.md "Rendering and
//! Skins", skin movies; "Memory Budget"; "Scope of No Allocation"): the
//! set, its generation, the renderer's planes and the loading frames'
//! shows live as long as their owner, and go through every exit; a
//! selection's movie is admitted at its boundary, never in its frame; a
//! visible loading state moves the play's movies on its own clock.

use std::sync::Arc;
use std::time::Instant;

use bms_model::TimeUs;
use render::PreparedToken;
use skin_model::ScreenKind;
use skin_model::media::{Geometry, Writer};
use skin_model::movie::MovieShow;

use crate::media_host::session::Movie;
use crate::movie_screen::{MediaHub, MovieSet};
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

/// Holds `set` as the screen's movies, the way an opening does.
fn hold(s: &mut super::MenuScreen, set: MovieSet) {
    s.movies.replace(&mut s.renderer, || Some(set));
}

/// A loading frame of `lifetime` at `now` on the loading clock, before
/// the lifetime published a state.
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

/// Play, then selection, then the next play's loading state: nothing of
/// a screen's movies outlives it. The play's loading frames show its
/// movie; selection's movies replace the play's, so the loading shows are
/// cleared and the play's planes released before selection reserves its
/// own; the next loading state leaves selection's movies, so their planes
/// go and it shows no movie of theirs and no stale loss.
#[test]
fn a_screen_s_movies_never_outlive_it() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let (play, selection) = (bundle(&dir), bundle(&dir));
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    let start = Instant::now();
    let a = Movie::live_here(1, 64, 32);
    let lost = Movie::new(1);
    let _ = lost.lose(crate::media_host::session::Loss::Failed);
    publish(&a, 1, 0, 9);
    hold(
        &mut s,
        MovieSet::of(
            &hub,
            ScreenKind::Play,
            &play,
            1,
            [Some(Arc::clone(&a)), Some(lost)],
            start,
        ),
    );
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.renderer.loading_movies(),
        [MovieShow::Frame { .. }, MovieShow::Lost]
    ));
    let play_planes = s.renderer.video_bytes();
    assert!(play_planes.0 > 0, "the play's planes");
    // Selection's movies, opened as its entry opens them.
    let b = Movie::live_here(2, 32, 16);
    publish(&b, 1, 0, 200);
    hold(
        &mut s,
        MovieSet::of(
            &hub,
            ScreenKind::Selection,
            &selection,
            7,
            [Some(Arc::clone(&b)), None],
            start,
        ),
    );
    assert_eq!(
        s.renderer.loading_movies(),
        [MovieShow::Nothing; 2],
        "the play's shows left with it"
    );
    assert_eq!(s.renderer.video_bytes(), (0, 0), "the play's planes left");
    s.admit_selection_movies();
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

/// A device lost after a play's preparation answered and before its
/// commit: the set, still its request's, is dropped with the screen (its
/// generation left), not parked, so neither a later cancel nor the screen
/// built again can keep it. The control, a committed set, is parked and
/// taken back.
#[test]
fn a_set_still_preparing_is_dropped_with_its_screen() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    for committed in [false, true] {
        let hub = MediaHub::with(None);
        let mut s = screen(&hub);
        s.play_state(3);
        s.open_play_movies(Some(&play), 5);
        assert_eq!(hub.open_sets(), 1);
        if committed {
            let token = PreparedToken {
                request: 5,
                device: s.renderer.device_generation(),
                kind: ScreenKind::Play,
            };
            s.commit(token).unwrap();
        }
        let generation = hub.generation();
        drop(s);
        let again = screen(&hub);
        if committed {
            assert!(again.movies.get().is_some(), "taken back");
            assert_eq!(hub.open_sets(), 1);
            assert_eq!(hub.generation(), generation);
        } else {
            assert!(!hub.parked());
            assert!(again.movies.get().is_none(), "not taken back");
            assert_eq!(hub.open_sets(), 0, "dropped");
            assert!(hub.generation() > generation, "its generation left");
        }
    }
}

/// A direct play's retry starts a new play state: its movies open again,
/// so the frame the last play held at 20 s is gone and READY's first
/// frame at 1 s on the new screen clock never shows it. A practice round,
/// the same play state, keeps its movies and their frame.
#[test]
fn a_retry_opens_its_movies_afresh_and_a_practice_round_keeps_them() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    s.play_state(1);
    let a = Movie::live_here(1, 64, 32);
    publish(&a, 1, 20_000_000, 7);
    hold(
        &mut s,
        MovieSet::of(
            &hub,
            ScreenKind::Play,
            &play,
            1,
            [Some(Arc::clone(&a)), None],
            Instant::now(),
        ),
    );
    let first = s.movies.get().unwrap().generation();
    s.loading_movies(&loading_frame(1, 0));
    assert!(matches!(
        s.play_movies(20_000_000)[0],
        MovieShow::Frame { .. }
    ));
    // A practice round: the same play state.
    s.open_play_movies(Some(&play), 2);
    assert_eq!(s.movies.get().unwrap().generation(), first, "kept");
    assert!(matches!(
        s.play_movies(21_000_000)[0],
        MovieShow::Frame { .. }
    ));
    // A retry: a new play state, READY at 1 s on its screen clock.
    s.play_state(2);
    s.open_play_movies(Some(&play), 3);
    assert_ne!(s.movies.get().unwrap().generation(), first, "opened again");
    assert_eq!(hub.open_sets(), 1);
    assert_eq!(s.renderer.video_bytes(), (0, 0), "the old frame's planes");
    assert!(
        !matches!(s.play_movies(1_000_000)[0], MovieShow::Frame { .. }),
        "no frame of the last play"
    );
}

/// A visible loading state draws the play's movies from its own screen
/// clock and polls them while it draws: with the peer still pending (the
/// preparation not settled), the ready clip moves on through 1 s, 1.5 s
/// and 2 s rather than holding a frame.
#[test]
fn loading_with_a_pending_peer_keeps_the_ready_clip_moving() {
    let _turn = crate::warp_turn::take();
    let dir = tempfile::tempdir().unwrap();
    let play = bundle(&dir);
    let hub = MediaHub::with(None);
    let mut s = screen(&hub);
    s.play_state(1);
    let a = Movie::live_here(1, 64, 32);
    let pending = Movie::new(1);
    for (seq, stamp, id) in [
        (1, 0, 1),
        (2, 1_000_000, 2),
        (3, 1_500_000, 3),
        (4, 2_000_000, 4),
    ] {
        publish(&a, seq, stamp, id);
    }
    let mut set = MovieSet::of(
        &hub,
        ScreenKind::Play,
        &play,
        1,
        [Some(Arc::clone(&a)), Some(Arc::clone(&pending))],
        Instant::now(),
    );
    set.preparing = Some(1);
    hold(&mut s, set);
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
    let a = Movie::live_here(1, 64, 32);
    for seq in 1..=4_u64 {
        let stamp = i64::try_from(seq).unwrap() * 16_667;
        publish(&a, seq, stamp, u8::try_from(seq).unwrap());
    }
    hold(
        &mut s,
        MovieSet::of(
            &hub,
            ScreenKind::Selection,
            &selection,
            7,
            [Some(Arc::clone(&a)), None],
            Instant::now(),
        ),
    );
    // Live, not admitted yet: the frame neither admits nor reserves.
    let before = this_thread();
    let shows = s.selection_movies(TimeUs(16_667));
    let after = this_thread();
    assert_eq!(after - before, 0, "allocations of a frame before admission");
    assert_eq!(shows[0], MovieShow::Nothing);
    assert_eq!(s.renderer.video_bytes(), (0, 0));
    // The boundary: admitted, its planes reserved by the renderer.
    s.admit_selection_movies();
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
