//! The presenter's revisions, lifetimes, barrier and fallback scopes, and
//! the strip's lines and the built-in page's composition (the S10-09i
//! design's C3, C4, C12 and C17e).

use std::sync::Arc;

use bms_model::TimeUs;
use skin_model::{ResultChart, ResultLabels};

use super::{
    OverlayFacts, Presenter, Publish, ResultPresentation, ResultReport, ResultSlot, STRIP_LINES,
    Scope, Snapshot, ViewKey, compose_page,
};
use crate::render_side::EndPage;
use crate::result_skin::Published;

/// A link that keeps what it is sent.
#[derive(Default)]
struct Sent {
    lifetimes: u64,
    ended: u32,
    requests: u64,
    shown: Vec<ResultPresentation>,
    presented: Option<ViewKey>,
    pinned: Option<ViewKey>,
    /// The presented view's timing, as the render thread would give it.
    timing: Option<skin_model::ResultTiming>,
    /// The lifetime's origin, as the render thread would give it.
    origin: Option<TimeUs>,
}

impl Publish for Sent {
    fn begin(&mut self) -> u64 {
        self.lifetimes += 1;
        self.lifetimes
    }
    fn end(&mut self) {
        self.ended += 1;
    }
    fn request(&mut self) -> u64 {
        self.requests += 1;
        self.requests
    }
    fn show(&mut self, p: ResultPresentation) {
        self.presented = Some(p.payload.key);
        self.shown.push(p);
    }
    fn pin(
        &mut self,
        at: TimeUs,
        gate: Option<TimeUs>,
    ) -> Option<(Option<ViewKey>, Option<skin_model::ResultTiming>)> {
        if game::gated(self.timing.map(crate::play::flow_timing), at, gate) == game::Outro::Gated {
            return None;
        }
        self.pinned = self.presented;
        Some((self.pinned, self.timing))
    }
    fn timing(&self) -> Option<skin_model::ResultTiming> {
        self.timing
    }
    fn origin(&self) -> Option<TimeUs> {
        self.origin
    }
}

fn snapshot(line: &str) -> Snapshot {
    Snapshot {
        view: crate::result_skin::probe_view(ResultChart::default()),
        labels: ResultLabels::default(),
        lines: vec![line.to_owned()],
        page: EndPage::default(),
    }
}

fn bundle() -> Arc<render::SkinBundle> {
    crate::render_side::tests::bundle()
}

/// A presentation of an ended lifetime is not taken, and one offered for
/// another lifetime is refused; the slot keeps the newest of its lifetime
/// with one signal outstanding (C4).
#[test]
fn the_slot_keeps_only_the_newest_of_its_lifetime() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    p.begin(&mut sent);
    p.want(snapshot("a"));
    p.land(&mut sent, Some(&Published::BuiltIn), None);
    p.lines(&mut sent, vec!["n".to_owned()]);
    let mut shown = sent.shown.into_iter();
    let (first, second) = (shown.next().unwrap(), shown.next().unwrap());
    let mut slot = ResultSlot::default();
    // No lifetime: nothing is taken.
    assert!(!slot.offer(ResultPresentation { ..clone(&first) }));
    slot.enter(1);
    assert!(slot.offer(clone(&first)), "the first signals");
    assert!(!slot.offer(clone(&second)), "one signal waits");
    assert_eq!(slot.take().map(|p| p.revision), Some(2));
    assert!(slot.take().is_none());
    // A new lifetime drops what waited of the old one.
    assert!(slot.offer(clone(&first)));
    slot.enter(2);
    assert!(slot.take().is_none());
    assert!(!slot.offer(clone(&first)), "of an ended lifetime");
}

fn clone(p: &ResultPresentation) -> ResultPresentation {
    ResultPresentation {
        lifetime: p.lifetime,
        revision: p.revision,
        request: p.request,
        payload: Arc::clone(&p.payload),
        overlay: p.overlay.clone(),
    }
}

/// A switch under the same bundle (a skin that reads no viewed fact)
/// publishes a payload of its own under a newer revision, each with a
/// request newer than the last (C3).
#[test]
fn a_switch_under_the_same_bundle_publishes_a_new_revision() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    let b = bundle();
    p.begin(&mut sent);
    let k1 = p.want(snapshot("v1"));
    p.land(
        &mut sent,
        Some(&Published::Skin(Arc::clone(&b), None, None)),
        None,
    );
    // The same layout landing again publishes nothing.
    p.land(
        &mut sent,
        Some(&Published::Skin(Arc::clone(&b), None, None)),
        None,
    );
    let k2 = p.want(snapshot("v2"));
    p.land(
        &mut sent,
        Some(&Published::Skin(Arc::clone(&b), None, None)),
        None,
    );
    let got: Vec<(u64, ViewKey, u64)> = sent
        .shown
        .iter()
        .map(|s| (s.revision, s.payload.key, s.request))
        .collect();
    assert_eq!(got, [(1, k1, 1), (2, k2, 2)]);
    assert!(sent.shown.iter().all(|s| s.lifetime == 1));
    assert!(
        sent.shown
            .iter()
            .all(|s| s.payload.skin.as_ref().is_some_and(|s| Arc::ptr_eq(s, &b)))
    );
    assert_eq!(sent.shown[1].payload.lines, ["v2"]);
}

/// The result's origin is the render thread's, given only while a
/// lifetime is current (design.md "Rendering and Skins", Timers).
#[test]
fn the_origin_is_the_render_threads_within_a_lifetime() {
    let mut sent = Sent {
        origin: Some(TimeUs(7)),
        ..Sent::default()
    };
    let mut p = Presenter::default();
    assert_eq!(p.origin(&sent), None, "no lifetime");
    p.begin(&mut sent);
    assert_eq!(p.origin(&sent), Some(TimeUs(7)));
    p.end(&mut sent);
    assert_eq!(p.origin(&sent), None, "the lifetime ended");
}

/// A view refused by the render thread shows the built-in result for that
/// view only; a refusal of a superseded view changes nothing (C17e), and
/// the next view lays its skin out again (C17a's presenter half).
#[test]
fn a_view_fallback_applies_to_its_own_view_only() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    let b = bundle();
    let skin = Published::Skin(Arc::clone(&b), None, None);
    p.begin(&mut sent);
    let k1 = p.want(snapshot("v1"));
    p.land(&mut sent, Some(&skin), None);
    let k2 = p.want(snapshot("v2"));
    // A stale refusal (of v1, no longer desired): nothing changes.
    let stale = ResultReport {
        refused: Some((k1, "old".to_owned())),
        ..ResultReport::default()
    };
    assert!(!p.status(&stale));
    p.land(&mut sent, Some(&skin), None);
    assert!(sent.shown.last().unwrap().payload.skin.is_some());
    // v2's own refusal: the built-in result for it.
    let own = ResultReport {
        refused: Some((k2, "staging refused".to_owned())),
        ..ResultReport::default()
    };
    assert!(p.status(&own));
    p.land(&mut sent, Some(&skin), None);
    let last = sent.shown.last().unwrap();
    assert_eq!(last.payload.key, k2);
    assert!(last.payload.skin.is_none());
    assert_eq!(
        last.payload.why,
        Some((Scope::View, "staging refused".to_owned()))
    );
    // Another view shows the skin again.
    let k3 = p.want(snapshot("v3"));
    p.land(&mut sent, Some(&skin), None);
    let last = sent.shown.last().unwrap();
    assert_eq!(last.payload.key, k3);
    assert!(last.payload.skin.is_some());
}

/// The attempt's fallback shows the built-in result for every view until
/// the next attempt; the run's register for good (C17b, C17c's presenter
/// halves).
#[test]
fn attempt_and_run_fallbacks_show_the_built_in_result() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    let skin = Published::Skin(bundle(), None, None);
    p.attempt_fallback("refused upload".to_owned());
    p.begin(&mut sent);
    p.want(snapshot("v1"));
    p.land(&mut sent, Some(&skin), None);
    p.want(snapshot("v2"));
    p.land(&mut sent, Some(&skin), None);
    assert!(sent.shown.iter().all(|s| s.payload.skin.is_none()
        && s.payload.why == Some((Scope::Attempt, "refused upload".to_owned()))));
    p.next_attempt();
    p.end(&mut sent);
    p.begin(&mut sent);
    p.want(snapshot("v3"));
    p.land(&mut sent, Some(&skin), None);
    assert!(sent.shown.last().unwrap().payload.skin.is_some());
    let disabled = ResultReport {
        disabled: Some("frame refused".to_owned()),
        ..ResultReport::default()
    };
    assert!(p.status(&disabled));
    p.land(&mut sent, Some(&skin), None);
    let last = sent.shown.last().unwrap();
    assert!(last.payload.skin.is_none());
    assert_eq!(last.payload.why.as_ref().map(|w| w.0), Some(Scope::Run));
    assert_eq!(sent.ended, 1);
}

/// Once the barrier is set, no view payload is published: the overlay
/// with the fade-out's start and new lines go out with the payload pinned
/// (C5a's and C5c's presenter halves).
#[test]
fn the_barrier_keeps_the_view_and_merges_the_overlay() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    let skin = Published::Skin(bundle(), None, None);
    p.begin(&mut sent);
    let k1 = p.want(snapshot("v1"));
    p.land(&mut sent, Some(&skin), None);
    assert_eq!(p.pin(&mut sent, TimeUs(40), None).unwrap().0, Some(k1));
    p.want(snapshot("v2"));
    p.land(&mut sent, Some(&skin), None);
    p.lines(&mut sent, vec!["NOTICE".to_owned()]);
    let after: Vec<_> = sent.shown.iter().skip(1).collect();
    assert_eq!(after.len(), 2, "the pin's overlay and the notice");
    assert!(after.iter().all(|s| s.payload.key == k1));
    assert!(after.iter().all(|s| s.overlay.fadeout == Some(TimeUs(40))));
    assert_eq!(after[1].overlay.lines, ["NOTICE"]);
}

/// The built-in page takes its view rows and graphs from the view's page
/// and every other row from the newest page, with the reason in a free
/// row (D1, D7).
#[test]
fn the_built_in_page_is_the_views_rows_over_the_newest() {
    let page = |tag: &str| EndPage {
        lines: (0..27).map(|i| format!("{tag}{i}")).collect(),
        graphs: None,
        #[cfg(feature = "dev-tools")]
        id: None,
    };
    let view = page("v");
    let mut latest = page("n");
    latest.lines[21].clear();
    let p = compose_page(&view, &latest, Some("why"));
    for (i, l) in p.lines.iter().enumerate() {
        let want = match i {
            1 | 2 | 3 | 4 | 8 | 9 | 10 | 11 | 13 | 14 | 15 => format!("v{i}"),
            21 => "RESULT SKIN NOT SHOWN: WHY".to_owned(),
            _ => format!("n{i}"),
        };
        assert_eq!(*l, want, "row {i}");
    }
}

/// The lines that do not depend on the view keep D5's order, the offset
/// guidance only while asked for, and every line is cut to the strip's
/// width; with every fact applying the strip holds its 13 lines (C12).
#[test]
fn the_overlay_lines_keep_their_order() {
    let facts = OverlayFacts {
        before: vec![
            "CANCELLED".to_owned(),
            "NOT SAVED".to_owned(),
            "DIFFERS".to_owned(),
        ],
        median: Some(Some(-4_000)),
        after: vec!["N1".to_owned(), "N2".to_owned(), "RELOAD".to_owned()],
    };
    assert_eq!(
        facts.lines(None, None),
        ["CANCELLED", "NOT SAVED", "DIFFERS", "N1", "N2", "RELOAD"]
    );
    let lines = facts.lines(Some(2_000), Some("INPUT: lost".to_owned()));
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[3], "INPUT OFFSET +2.0 MS, RECOMMENDED -2.0 MS");
    assert_eq!(lines[7], "INPUT: lost");
    // Five view lines at most beside them: 13 in all.
    assert_eq!(5 + lines.len(), STRIP_LINES);
    let long = OverlayFacts {
        before: vec!["x".repeat(200)],
        ..OverlayFacts::default()
    };
    assert_eq!(long.lines(None, None)[0].chars().count(), 76);
}

/// A play whose best cannot be set shows why on the strip, whether it was
/// counted without setting one or not counted at all; an eligible play
/// shows no such line (C12's projection of each best fact, through the
/// real report and result screen).
#[test]
fn either_ineligible_best_has_its_strip_line() {
    use crate::end_screen::tests::{report, screen};
    let dir = tempfile::tempdir().unwrap();
    let mut played = report(rules::tapestry());
    let best = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .filter(|l| l.starts_with("BEST NOT UPDATED"))
            .cloned()
            .collect()
    };
    let shown = screen(dir.path(), &played);
    assert!(best(&super::view_lines(&played, &shown)).is_empty());
    played.best = Some(game::BestView::NotCounted("failed: disk full".to_owned()));
    assert_eq!(
        best(&super::view_lines(&played, &shown)),
        ["BEST NOT UPDATED: failed: disk full"]
    );
    played.best = Some(game::BestView::Counted {
        play: (11, 24),
        previous: None,
        current: game::BestScore::default(),
        ineligible: Some("autoplay".to_owned()),
    });
    assert_eq!(
        best(&super::view_lines(&played, &shown)),
        ["BEST NOT UPDATED: autoplay"]
    );
}

/// The timing is the publisher's, the render thread's presented view's,
/// only while a lifetime is current; the pin gives the pinned view's,
/// read with it (the S10-09i design's D6; the state table's T7).
#[test]
fn the_timing_is_the_presented_views_while_a_lifetime_is() {
    let t = skin_model::ResultTiming {
        outro: TimeUs(400_000),
        input: TimeUs(0),
        scene: None,
    };
    let mut sent = Sent {
        timing: Some(t),
        ..Sent::default()
    };
    let mut p = Presenter::default();
    assert_eq!(p.timing(&sent), None, "no lifetime");
    assert_eq!(
        p.pin(&mut sent, TimeUs(1), None),
        Some((None, None)),
        "no lifetime"
    );
    p.begin(&mut sent);
    assert_eq!(p.timing(&sent), Some(t));
    // A payload published last has no say: built-in here.
    p.want(snapshot("v1"));
    p.land(&mut sent, Some(&Published::BuiltIn), None);
    assert_eq!(p.timing(&sent), Some(t));
    // A press its input time gates pins nothing and publishes nothing.
    sent.timing = Some(skin_model::ResultTiming {
        input: TimeUs(1_000),
        ..t
    });
    let shown = sent.shown.len();
    assert_eq!(p.pin(&mut sent, TimeUs(5), Some(TimeUs(0))), None);
    assert_eq!((sent.shown.len(), sent.pinned), (shown, None));
    p.want(snapshot("v2"));
    p.land(&mut sent, Some(&Published::BuiltIn), None);
    assert_eq!(sent.shown.len(), shown + 1, "not pinned: views publish");
    sent.timing = Some(t);
    assert_eq!(p.pin(&mut sent, TimeUs(5), None).unwrap().1, Some(t));
    p.end(&mut sent);
    assert_eq!(p.timing(&sent), None, "the lifetime ended");
}

/// The same layout landing with other chart images (a fixed skin
/// published in an earlier attempt, a later attempt's images) is a new
/// payload carrying them; landing again with the same images is not; the
/// built-in result carries none.
#[test]
fn a_payload_carries_its_attempts_chart_images() {
    let mut sent = Sent::default();
    let mut p = Presenter::default();
    let b = bundle();
    let skin = Published::Skin(Arc::clone(&b), None, None);
    let a = Arc::new(crate::chart_images::ChartImages::absent(
        &skin_model::chart_image::Absent::NoHeader,
    ));
    let c = Arc::new(crate::chart_images::ChartImages::absent(
        &skin_model::chart_image::Absent::NoHeader,
    ));
    p.begin(&mut sent);
    p.want(snapshot("v1"));
    p.land(&mut sent, Some(&skin), Some(Arc::clone(&a)));
    p.land(&mut sent, Some(&skin), Some(Arc::clone(&a)));
    p.land(&mut sent, Some(&skin), Some(Arc::clone(&c)));
    let ids: Vec<Option<u64>> = sent
        .shown
        .iter()
        .map(|s| s.payload.chart.as_ref().map(|c| c.id()))
        .collect();
    assert_eq!(ids, [Some(a.id()), Some(c.id())]);
    p.land(&mut sent, Some(&Published::BuiltIn), Some(Arc::clone(&a)));
    assert!(sent.shown.last().unwrap().payload.chart.is_none());
}
