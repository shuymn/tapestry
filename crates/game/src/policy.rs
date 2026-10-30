//! Session policy and session events: two extension points that sit
//! outside the evaluator (design.md "Session Policy Lives Outside the
//! Evaluator", "Screen Transition and Session Events").

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use eval::GaugeState;
use replay::EndStatus;

/// Which of the preset's gauges is shown, by its index in the preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeIndex(pub usize);

/// What the session does when a survival gauge fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureAction {
    /// The play goes on to the end; the failure stays in the record.
    KeepPlaying,
    /// The play ends there, as ended midway.
    EndPlay,
}

/// The session policy extension point. It never changes evaluation.
pub trait SessionPolicy {
    /// The id recorded in the input log.
    fn id(&self) -> &str;
    /// The gauge shown, given every gauge's state in the preset's order.
    fn displayed_gauge(&self, gauges: &[GaugeState]) -> GaugeIndex;
    /// What to do when a survival gauge fails.
    fn on_survival_failure(&self) -> FailureAction;
}

/// The stage 1 policy: the gauge chosen in `player.toml` is shown, and the
/// play goes on to the end after a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultPolicy {
    /// The gauge to show; an index the preset does not have shows the
    /// first.
    pub displayed: usize,
}

impl SessionPolicy for DefaultPolicy {
    fn id(&self) -> &str {
        "default"
    }

    fn displayed_gauge(&self, gauges: &[GaugeState]) -> GaugeIndex {
        GaugeIndex(if self.displayed < gauges.len() {
            self.displayed
        } else {
            0
        })
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn on_survival_failure(&self) -> FailureAction {
        FailureAction::KeepPlaying
    }
}

/// A change of screen or play state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// Song selection was entered.
    Select,
    /// The chart was loaded.
    Load,
    /// A play started; `attempt` counts from 1.
    Start {
        /// The play's number in the session.
        attempt: u32,
    },
    /// A retry ended the play `attempt`.
    Retry {
        /// The play that was retried.
        attempt: u32,
    },
    /// The play `attempt` ended.
    End {
        /// The play.
        attempt: u32,
        /// How it ended.
        status: EndStatus,
    },
    /// The result of the play `attempt` is shown.
    Result {
        /// The play.
        attempt: u32,
    },
}

/// The screen transition and session events extension point: receives
/// only. The bounded queue, separate thread and endpoint are later work
/// (design.md "Local Event Endpoint").
pub trait SessionEventSink {
    /// One event.
    fn on_event(&mut self, ev: &SessionEvent);
}

/// A handle to one session event sink, reported to through a shared
/// borrow: a run's selection and each chart it loads hold handles to the
/// same sink, and a play reports its start while its logic step borrows
/// the chart.
#[derive(Clone)]
pub(crate) struct SharedEvents(Rc<RefCell<Box<dyn SessionEventSink>>>);

impl SharedEvents {
    pub(crate) fn new(sink: Box<dyn SessionEventSink>) -> Self {
        Self(Rc::new(RefCell::new(sink)))
    }

    pub(crate) fn on_event(&self, ev: &SessionEvent) {
        self.0.borrow_mut().on_event(ev);
    }
}

impl fmt::Debug for SharedEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedEvents")
    }
}

/// Receives events and does nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSessionEvents;

impl SessionEventSink for NoSessionEvents {
    fn on_event(&mut self, _ev: &SessionEvent) {}
}
