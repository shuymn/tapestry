//! The decision screen's injected faults (dev-tools): what a run's
//! environment asks, and what the render thread counts against it at each
//! decision skin build and submission, in play.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// The environment variable of the decision screen's injected faults (the
/// allocation count's controls, design.md "Scope of No Allocation"): a
/// comma list of `alloc<N>`, the N-th decision skin build allocating
/// inside the skin's build; `allocrefuse<N>`, the same, then refused;
/// `submitalloc<N>`, the N-th decision skin frame allocating before its
/// submission, outside the counted build; and `lost<N>`, the N-th decision
/// skin frame drawn lost as a device loss. Each is counted from 1 over the
/// render thread's run.
pub const DECISION_FAULTS_ENV: &str = "TAPESTRY_DEV_DECISION_FAULTS";

/// The most faults of one kind a list gives.
const DECISION_FAULTS: usize = 8;

/// The decision screen's faults ([`DECISION_FAULTS_ENV`]), read once
/// before any frame, and what the render thread has counted against
/// them: a frame reads and counts only fixed fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionFaults {
    alloc: [u64; DECISION_FAULTS],
    refuse: [u64; DECISION_FAULTS],
    submit: [u64; DECISION_FAULTS],
    lost: [u64; DECISION_FAULTS],
    /// The decision skin builds asked so far.
    builds: u64,
    /// The decision skin frames submitted so far.
    submits: u64,
    /// The faults that fired so far.
    pub fired: u64,
}

/// The decision build fault due for a build: allocate, then refuse when
/// `true`.
pub type BuildFault = Option<bool>;

impl DecisionFaults {
    /// The faults a comma list gives; unknown items are ignored.
    #[must_use]
    pub fn parse(list: &str) -> Self {
        let mut f = Self::default();
        for item in list.split(',').map(str::trim) {
            let (slots, n) = if let Some(n) = item.strip_prefix("allocrefuse") {
                (&mut f.refuse, n)
            } else if let Some(n) = item.strip_prefix("submitalloc") {
                (&mut f.submit, n)
            } else if let Some(n) = item.strip_prefix("alloc") {
                (&mut f.alloc, n)
            } else if let Some(n) = item.strip_prefix("lost") {
                (&mut f.lost, n)
            } else {
                continue;
            };
            if let (Ok(n), Some(slot)) = (n.parse::<u64>(), slots.iter_mut().find(|s| **s == 0))
                && n > 0
            {
                *slot = n;
            }
        }
        f
    }

    /// The faults [`DECISION_FAULTS_ENV`] gives, read now.
    #[must_use]
    pub fn read() -> Self {
        Self::parse(&std::env::var(DECISION_FAULTS_ENV).unwrap_or_default())
    }

    /// A decision skin build is asked: its fault, if one is due.
    pub fn build(&mut self) -> BuildFault {
        self.builds = self.builds.saturating_add(1);
        let n = self.builds;
        let fault = if self.refuse.contains(&n) {
            Some(true)
        } else if self.alloc.contains(&n) {
            Some(false)
        } else {
            None
        };
        if fault.is_some() {
            self.fired = self.fired.saturating_add(1);
        }
        fault
    }

    /// A decision skin frame is to be submitted: whether it allocates
    /// first, and whether it is drawn lost, each when due.
    pub fn submit(&mut self) -> (bool, bool) {
        self.submits = self.submits.saturating_add(1);
        let n = self.submits;
        let (alloc, lost) = (self.submit.contains(&n), self.lost.contains(&n));
        self.fired = self
            .fired
            .saturating_add(u64::from(alloc))
            .saturating_add(u64::from(lost));
        (alloc, lost)
    }
}
