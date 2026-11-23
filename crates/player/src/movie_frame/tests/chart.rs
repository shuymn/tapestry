//! The chart's videos on chart time (design.md "Rendering and Skins",
//! BGA): starts by event time, the frame due at the speed, restarts held,
//! ends at the last frame's end, the cue, the peer loss and the clocks kept
//! apart from the skin movies'. Stamps are not 0: the clip's first frame is
//! stamped [`FIRST`].

use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga, TimeUs};

use super::*;

/// The clip's first frame's stamp.
const FIRST: i64 = 40_000;
/// The clip's frame period.
const P: i64 = 40_000;

/// The chart videos of a chart whose base names id 0 at each of `base`
/// and whose layer names id 1 at each of `layer` (µs), id 0 in slot 2 and
/// id 1 in `layer_slot`.
fn videos(base: &[i64], layer: &[i64], layer_slot: u8) -> Arc<ChartVideos> {
    let image = |id| BgaImage {
        id,
        name: format!("{id}.mp4"),
    };
    let ev = |t: &i64, id| BgaEvent {
        time: TimeUs(*t),
        image: Some(BgaImageId(id)),
    };
    let bga = ChartBga::new(
        vec![image(1), image(2)],
        base.iter().map(|t| ev(t, 0)).collect(),
        layer.iter().map(|t| ev(t, 1)).collect(),
        vec![],
    )
    .unwrap();
    Arc::new(ChartVideos::of(&bga, &[Some(2), Some(layer_slot)]))
}

/// A live chart video in slot 2 (and `peer` in slot 3, `skin` in slot 0),
/// its first frame published and staged by a preparation's poll; slot 2's
/// file starts at `starts`, counted from `boundary`.
struct Play {
    a: Arc<Fake>,
    m: Movies<Fake>,
    planes: Kept,
    start: Instant,
    seq: u64,
}

impl Play {
    fn new(starts: &[i64], boundary: i64) -> Self {
        Self::with(videos(starts, &[], 3), boundary, None, None)
    }

    fn with(
        videos: Arc<ChartVideos>,
        boundary: i64,
        peer: Option<&Arc<Fake>>,
        skin: Option<&Arc<Fake>>,
    ) -> Self {
        let start = Instant::now();
        let a = Fake::new(16, 8);
        a.set(Status::Live);
        a.publish_run(1, 1, FIRST, P, 1);
        let mut m = Movies::new(
            [skin.cloned(), None, Some(Arc::clone(&a)), peer.cloned()],
            start,
        );
        m.set_chart(videos, boundary);
        let mut planes = Kept::default();
        m.admit(&mut planes, start);
        let (_, shows) = m.prepare(&mut planes, start);
        assert_eq!(shows[2], MovieShow::Nothing, "staged, not shown");
        assert!(a.staged.load(Ordering::SeqCst), "its first frame staged");
        Self {
            a,
            m,
            planes,
            start,
            seq: 1,
        }
    }

    /// Publishes a frame of run `run` at `stamp` lasting a period, its
    /// bytes `id`.
    fn publish(&mut self, run: u64, stamp: i64, id: u8) {
        self.seq += 1;
        self.a.publish_run(self.seq, run, stamp, P, id);
    }

    /// A play frame at chart time `now` (µs) and speed `k / 20`, on a
    /// frame clock far from chart time.
    fn at_k(&mut self, now: i64, k: u32) -> MovieShows {
        self.m.admit(&mut self.planes, self.start);
        let shows = self.m.play_frame(
            &mut self.planes,
            now + 7_000_000,
            ChartAt { now, k },
            self.start,
        );
        self.m.presented(true, drawn(shows));
        shows
    }

    fn at(&mut self, now: i64) -> MovieShows {
        self.at_k(now, 20)
    }

    /// The stamp of the frame slot 2's planes hold.
    fn stamp(&self) -> Option<i64> {
        self.m.stamps()[2]
    }
}

fn frame(shows: MovieShows) -> bool {
    matches!(shows[2], MovieShow::Frame { .. })
}

/// At chart time `now` after start `e`, the newest frame at or before
/// `first + (now − e)` is staged: not the frame due at `now − e` alone,
/// not one due later, and a frame between renders stays until due.
#[test]
fn a_chart_video_shows_the_newest_frame_on_chart_time() {
    let e = 2_000_000;
    let mut p = Play::new(&[e], i64::MIN);
    for (k, at) in [600_000, 700_000, 800_000].into_iter().enumerate() {
        p.publish(1, FIRST + at, 2 + u8::try_from(k).unwrap());
    }
    assert!(frame(p.at(e + 700_000)));
    assert_eq!(p.stamp(), Some(FIRST + 700_000));
    assert!(frame(p.at(e + 780_000)));
    assert_eq!(p.stamp(), Some(FIRST + 700_000), "the 800 ms frame not due");
    assert!(frame(p.at(e + 800_000)));
    assert_eq!(p.stamp(), Some(FIRST + 800_000));
}

/// The first start shows the frame staged at the preparation at once, and
/// nothing before the start takes another.
#[test]
fn the_first_start_shows_the_staged_first_frame() {
    let e = 1_000_000;
    let mut p = Play::new(&[e], i64::MIN);
    p.publish(1, FIRST + P, 2);
    assert!(!frame(p.at(e - 1)), "nothing before the start");
    assert_eq!(p.a.freed.load(Ordering::SeqCst), 1, "only the staged frame");
    assert!(frame(p.at(e)));
    assert_eq!(p.stamp(), Some(FIRST));
    assert!(holds(&p.planes, 2, 1));
}

/// A start first rendered late shows the newest frame due then, not the
/// staged first frame.
#[test]
fn a_missed_render_shows_the_newest_due_frame_at_once() {
    let e = 1_000_000;
    let mut p = Play::new(&[e], i64::MIN);
    for k in 1..=3 {
        p.publish(1, FIRST + k * P, u8::try_from(k + 1).unwrap());
    }
    assert!(!frame(p.at(e - 50_000)));
    assert!(frame(p.at(e + 100_000)));
    assert_eq!(p.stamp(), Some(FIRST + 2 * P));
}

/// The media time runs at the speed: at `k = 10`, 2 s after the start is
/// 1 s of the clip; at `k = 30`, 3 s; floored.
#[test]
fn a_chart_video_follows_the_practice_speed() {
    let e = 1_000_000;
    for (k, media) in [(10, 1_000_000), (30, 3_000_000)] {
        let mut p = Play::new(&[e], i64::MIN);
        for (n, at) in [media - P, media, media + P].into_iter().enumerate() {
            p.publish(1, FIRST + at, 2 + u8::try_from(n).unwrap());
        }
        assert!(frame(p.at_k(e + 2_000_000, k)));
        assert_eq!(p.stamp(), Some(FIRST + media), "k = {k}");
    }
    let at = |now, k| chart_due(FIRST, 0, ChartAt { now, k });
    assert_eq!(at(1, 10), FIRST, "half a µs floored");
    assert_eq!(at(3, 30), FIRST + 4, "4.5 µs floored");
    assert_eq!(at(i64::MAX, 30), i64::MAX, "saturated");
}

/// A full play counts a start at chart time 0 on a first frame drawn later.
#[test]
fn a_start_at_zero_counts_on_a_late_first_frame() {
    let mut p = Play::new(&[0], i64::MIN);
    assert!(frame(p.at(33_000)));
    assert_eq!(p.stamp(), Some(FIRST));
}

/// A practice round counts no start before its boundary: the file stays
/// unstarted; a start at the boundary starts it.
#[test]
fn starts_before_the_round_s_boundary_wait() {
    let b = 10_000_000;
    let mut p = Play::new(&[9_000_000], b);
    assert!(!frame(p.at(9_500_000)));
    assert!(!frame(p.at(12_000_000)), "never started");
    assert_eq!(p.m.wakes(), 0);
    let mut p = Play::new(&[9_000_000, b], b);
    assert!(!frame(p.at(9_500_000)));
    assert!(frame(p.at(b)));
    assert_eq!(p.stamp(), Some(FIRST));
}

/// Of two starts crossed between frames, one restart is made, anchored at
/// the later.
// Source: SF-025
#[test]
fn of_starts_crossed_between_frames_the_latest_wins() {
    let mut p = Play::new(&[500_000, 1_000_000, 1_050_000], i64::MIN);
    assert!(frame(p.at(600_000)));
    assert!(frame(p.at(980_000)));
    assert_eq!(p.m.wakes(), 0);
    assert!(frame(p.at(1_100_000)), "the run before held");
    assert_eq!((p.m.wakes(), p.a.want.load(Ordering::SeqCst)), (1, 2));
    for k in 0..3 {
        p.publish(2, FIRST + k * P, 10 + u8::try_from(k).unwrap());
    }
    assert!(frame(p.at(1_120_000)));
    assert_eq!(p.stamp(), Some(FIRST + P), "anchored at 1.05 s");
    assert_eq!(p.m.wakes(), 1, "one restart");
}

/// A video started on the base keeps running on chart time after the base
/// changes to a picture (no start of its own), and a later frame shows the
/// frame due then, with no restart.
// Source: SF-028
#[test]
fn a_video_started_then_replaced_by_a_picture_keeps_running() {
    // The video starts at 1 s; a picture replaces it on the base at 2 s,
    // which is no start of the video's.
    let mut p = Play::new(&[1_000_000], i64::MIN);
    assert!(frame(p.at(1_000_000)));
    p.publish(1, FIRST + 1_000_000, 2);
    p.publish(1, FIRST + 1_500_000, 3);
    p.publish(1, FIRST + 2_000_000, 4);
    p.at(2_100_000);
    assert_eq!(p.stamp(), Some(FIRST + 1_000_000), "taken while not drawn");
    p.at(3_000_000);
    assert_eq!(p.stamp(), Some(FIRST + 2_000_000), "due at 3 s");
    assert_eq!(p.m.wakes(), 0, "no restart");
}

/// Ids on the base and the layer naming one file share its slot: starts at
/// one time on both are one start, and a later start on either restarts
/// the file for both.
// Source: SF-026
#[test]
fn aliases_on_both_channels_share_one_playhead() {
    let both = videos(&[1_000_000], &[1_000_000, 3_000_000], 2);
    assert_eq!(both.starts(2), [TimeUs(1_000_000), TimeUs(3_000_000)]);
    assert_eq!(both.starts(3), [], "no second slot");
    let mut p = Play::with(both, i64::MIN, None, None);
    assert!(frame(p.at(1_000_000)));
    assert_eq!(p.m.wakes(), 0, "one start");
    assert!(frame(p.at(3_000_000)), "held");
    assert_eq!((p.m.wakes(), p.a.want.load(Ordering::SeqCst)), (1, 2));
}

/// A start inside the clip starts a new run, wants it sought and wakes
/// the session; the frame before stays shown until the new run's first
/// frame is taken, and from then only frames of the new run show.
// Source: SF-025
#[test]
fn a_restart_holds_the_last_frame_until_the_new_run() {
    let mut p = Play::new(&[1_000_000, 2_000_000], i64::MIN);
    p.publish(1, FIRST + 400_000, 2);
    assert!(frame(p.at(1_500_000)));
    assert_eq!(p.stamp(), Some(FIRST + 400_000));
    // A frame of the run before, published late, never shows.
    p.publish(1, FIRST + 1_200_000, 9);
    let shows = p.at(2_000_000);
    assert_eq!((p.m.wakes(), p.a.want.load(Ordering::SeqCst)), (1, 2));
    assert!(frame(shows), "held across the restart");
    assert!(holds(&p.planes, 2, 2));
    assert!(frame(p.at(2_050_000)), "still held");
    assert!(holds(&p.planes, 2, 2));
    p.publish(2, FIRST, 20);
    p.publish(2, FIRST + P, 21);
    assert!(frame(p.at(2_030_000)));
    assert!(holds(&p.planes, 2, 20), "the new run's first frame");
    assert!(frame(p.at(2_100_000)));
    assert!(holds(&p.planes, 2, 21));
}

/// An end reported while a frame of the run is still in the ring, stamped
/// after the time due, keeps the run playing until that frame is taken.
#[test]
fn an_end_waits_for_frames_still_in_the_ring() {
    let mut p = Play::new(&[0], i64::MIN);
    p.publish(1, FIRST + P, 2);
    p.publish(1, FIRST + 5 * P, 3);
    p.a.ended.store(1, Ordering::SeqCst);
    assert!(frame(p.at(3 * P)), "past the 40 ms frame's end, one left");
    assert_eq!(p.stamp(), Some(FIRST + P));
    assert!(frame(p.at(5 * P)));
    assert_eq!(p.stamp(), Some(FIRST + 5 * P));
    assert!(!frame(p.at(6 * P)), "its end");
}

/// The run ends at its last frame's stamp plus its duration: shown at one
/// µs before, nothing at it.
// Source: SF-027
#[test]
fn the_end_comes_at_the_last_frame_s_end() {
    let mut p = Play::new(&[0], i64::MIN);
    p.publish(1, FIRST + P, 2);
    p.a.ended.store(1, Ordering::SeqCst);
    assert!(frame(p.at(P)));
    assert!(frame(p.at(2 * P - 1)));
    assert!(!frame(p.at(2 * P)));
    assert_eq!(p.m.wakes(), 0, "no start ahead, no cue");
}

/// A restart whose seek found no frame ends with nothing shown, neither
/// the run before's frame nor a wait for a frame that never comes.
#[test]
fn an_empty_restart_ends_with_no_frame() {
    let mut p = Play::new(&[0, 1_000_000], i64::MIN);
    assert!(frame(p.at(500_000)));
    assert!(frame(p.at(1_000_000)), "held");
    p.a.empty.store(2, Ordering::SeqCst);
    p.a.ended.store(2, Ordering::SeqCst);
    assert!(!frame(p.at(1_010_000)));
    assert!(!frame(p.at(1_500_000)));
}

/// A cued run whose decoder ended before its start still shows its first
/// frame at the start.
#[test]
fn an_end_before_a_cued_start_does_not_end_it() {
    let mut p = Play::new(&[1_000_000], i64::MIN);
    p.a.ended.store(1, Ordering::SeqCst);
    assert!(!frame(p.at(500_000)));
    assert!(frame(p.at(1_000_000)));
    assert_eq!(p.stamp(), Some(FIRST));
    assert!(!frame(p.at(1_000_000 + P)), "ended at its frame's end");
}

/// A run that ended with a start ahead is cued at once: a new run wanted,
/// its first frame staged, and the start shows it with no further seek.
#[test]
fn an_ended_run_is_cued_for_its_next_start() {
    let mut p = Play::new(&[0, 1_000_000], i64::MIN);
    p.a.ended.store(1, Ordering::SeqCst);
    assert!(frame(p.at(0)));
    assert!(!frame(p.at(P)), "ended");
    assert!(!frame(p.at(P + 10_000)));
    assert_eq!(
        (p.m.wakes(), p.a.want.load(Ordering::SeqCst)),
        (1, 2),
        "cued"
    );
    p.publish(2, FIRST, 30);
    p.publish(2, FIRST + P, 31);
    assert!(!frame(p.at(500_000)), "staged, not shown");
    assert!(holds(&p.planes, 2, 30));
    assert!(frame(p.at(1_000_000)));
    assert_eq!(p.stamp(), Some(FIRST));
    assert_eq!(p.m.wakes(), 1, "no seek at the start");
}

/// A skin movie's clock starts at its first presented frame, a chart
/// video's at its start on chart time: the chart video follows chart time
/// whatever the frame clock and presentations do.
#[test]
fn a_skin_movie_and_a_chart_video_keep_their_own_clocks() {
    let skin = Fake::new(16, 8);
    skin.set(Status::Live);
    skin.publish(1, 0, 5);
    let mut p = Play::with(videos(&[1_000_000], &[], 3), i64::MIN, None, Some(&skin));
    p.publish(1, FIRST + 100_000, 2);
    // The frame clock is chart time plus 7 s.
    assert!(frame(p.at(1_100_000)));
    assert_eq!(p.stamp(), Some(FIRST + 100_000));
    assert_eq!(p.m.origins()[0], Some(8_100_000), "the skin movie's origin");
    assert_eq!(
        p.m.origins()[2],
        None,
        "no presentation starts a chart video"
    );
}

/// A chart video whose ring fails loses its peer in the same frame, both
/// shown lost, the peer for its peer's loss; the skin movie keeps its frame.
#[test]
fn a_chart_loss_blacks_the_panel_and_loses_its_peer() {
    let (peer, skin) = (Fake::new(16, 8), Fake::new(16, 8));
    for f in [&peer, &skin] {
        f.set(Status::Live);
        f.publish(1, FIRST, 4);
    }
    let mut p = Play::with(videos(&[0], &[0], 3), i64::MIN, Some(&peer), Some(&skin));
    let shows = p.at(10_000);
    assert!(frame(shows) && matches!(shows[3], MovieShow::Frame { .. }));
    // A frame of a run the game never started: a ring fault.
    p.publish(9, FIRST + P, 2);
    let shows = p.at(2 * P);
    assert_eq!((shows[2], shows[3]), (MovieShow::Lost, MovieShow::Lost));
    assert_eq!(p.a.loss(), Some(Loss::Fault));
    assert_eq!(peer.loss(), Some(Loss::Peer));
    assert!(
        matches!(shows[0], MovieShow::Frame { .. }),
        "the skin movie"
    );
}

/// A chart file refused at the pixel total is absent: the other plays and
/// nothing is lost.
#[test]
fn an_absent_chart_file_starts_no_peer_loss() {
    let peer = Fake::new(16, 8);
    peer.set(Status::Live);
    peer.publish(1, FIRST, 4);
    let mut p = Play::with(videos(&[0], &[0], 3), i64::MIN, Some(&peer), None);
    p.a.lose(Loss::OverTotal);
    let shows = p.at(10_000);
    assert_eq!(shows[2], MovieShow::Nothing);
    assert!(matches!(shows[3], MovieShow::Frame { .. }));
    assert_eq!(peer.loss(), None);
}
