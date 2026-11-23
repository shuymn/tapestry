//! Dev-tools: the preload pack's work between its checkpoints (design.md
//! "beatoraja Lua Skins", the preload's cooperative stop; the development
//! measurement of pause latency, M12), and the hold a measurement or a
//! test puts a pack in at a chosen checkpoint.
//!
//! The tracker runs on the pack's thread from its admission. A stretch of
//! the job's own work ends at each checkpoint's bracket, which holds the
//! whole check (the test gate, a hold, the coordinator's checkpoint and
//! the marks), and at the job's end or the checkpoint that saw its stop;
//! dev-only work ([`dev_only`]) is left out. Each stretch above
//! [`BOUND_US`] is marked when it ends, with its start, its excluded
//! spans, the sites around it and the unchecked kinds of work in it. The
//! marks allocate nothing; the tracker's state is the thread's own.
//!
//! A compound mark packs its job id in bits 40–62 and its value in bits
//! 0–39 ([`pair`]).

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use bms_model::TimeUs;

use crate::dev::loop_probe;

/// The stretch a measurement flags: the pack's 50 ms target between
/// checkpoints on the development PC.
pub const BOUND_US: i64 = 50_000;

/// The job's admission: its first stretch's start.
pub const ADMIT: u8 = 20;
/// The job's end: its last stretch's end.
pub const END: u8 = 21;

/// The site names of the marks and of `hold-preload`, by code.
pub const SITES: [(&str, u8); 23] = [
    ("Read", 0),
    ("Band", 1),
    ("Trailing", 2),
    ("Mip", 3),
    ("Page", 4),
    ("Decoded", 5),
    ("Fitted", 6),
    ("PlanRects", 7),
    ("PlanSort", 8),
    ("PlanScan", 9),
    ("PlanSide", 10),
    ("PlanPlace", 11),
    ("PlanRectWork", 12),
    ("PlanPlaceWork", 13),
    ("PlanEnd", 14),
    ("UnionWork", 15),
    ("StepStart", 16),
    ("Union", 17),
    ("UnionEnd", 18),
    ("Take", 19),
    ("Admit", 20),
    ("End", 21),
    ("None", 31),
];

/// The code of site `name`.
#[must_use]
pub fn site_code(name: &str) -> Option<u8> {
    SITES.iter().find(|(n, _)| *n == name).map(|&(_, c)| c)
}

/// The bits of `pre:visits`' value: sites in 0–27, kinds from 28.
pub const KIND_SHIFT: u32 = 28;

/// The value of a `pre:gap_skip` whose stretch had more excluded spans
/// than the tracker keeps: the stretch cannot be scored.
pub const SKIP_OVERFLOW: i64 = (1 << 40) - 1;

/// How many excluded spans a stretch keeps.
const SKIPS: usize = 4;

/// Job `id` and value `v` in one mark value.
#[must_use]
pub fn pair(id: u64, v: i64) -> i64 {
    let id = i64::try_from(id & ((1 << 23) - 1)).unwrap_or(0);
    (id << 40) | (v & ((1 << 40) - 1))
}

thread_local! {
    /// The atlas step's site being asked, read by the pack's check.
    static STEP: Cell<Option<u8>> = const { Cell::new(None) };
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

/// Asks `check` at the atlas step's `site`: the pack's check names it.
pub fn ask(check: &dyn Fn() -> bool, site: super::StepSite) -> bool {
    STEP.with(|s| s.set(Some(site as u8)));
    let stop = check();
    STEP.with(|s| s.set(None));
    stop
}

/// The site of the checkpoint being asked on this thread: the atlas
/// step's, else skin-model's last.
#[must_use]
pub fn site_now() -> u8 {
    STEP.with(Cell::get)
        .or_else(skin_model::dev::last_site)
        .unwrap_or(31)
}

/// One stretch's state.
struct Tracker {
    id: u64,
    /// The open stretch's start, none inside a bracket or after the stop.
    start: Option<i64>,
    from: u8,
    skips: [(i64, i64); SKIPS],
    skipped: usize,
    /// Dev-only µs in the open stretch.
    dev: i64,
    /// Unchecked kinds in the open stretch.
    kinds: u32,
    /// The open stretch's cycles at its start.
    cycles: u64,
    /// Sites passed and kinds reached by the job.
    visits: u64,
    longest: i64,
    dev_total: i64,
    /// When its stop was seen.
    seen: Option<i64>,
}

fn now() -> Option<i64> {
    loop_probe::stamp().map(|t| t.0)
}

fn with(f: impl FnOnce(&mut Tracker)) {
    TRACKER.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            f(t);
        }
    });
}

/// The thread's cycles (`QueryThreadCycleTime`), a raw diagnostic count;
/// a test's source in its place.
fn cycles() -> u64 {
    #[cfg(test)]
    if let Some(c) = super::preload::tests::cycle_source() {
        return c;
    }
    let mut n = 0_u64;
    // SAFETY: the pseudo-handle of the current thread and a valid out
    // pointer.
    let ok = unsafe {
        windows::Win32::System::WindowsProgramming::QueryThreadCycleTime(
            windows::Win32::System::Threading::GetCurrentThread(),
            &raw mut n,
        )
    };
    if ok.is_ok() { n } else { 0 }
}

/// Job `id` is admitted: its first stretch begins (the probe on).
pub fn admit(id: u64) {
    let Some(t) = now() else {
        return;
    };
    let _ = skin_model::dev::take_kinds();
    let cycles = cycles();
    TRACKER.with(|tr| {
        *tr.borrow_mut() = Some(Tracker {
            id,
            start: Some(t),
            from: ADMIT,
            skips: [(0, 0); SKIPS],
            skipped: 0,
            dev: 0,
            kinds: 0,
            cycles,
            visits: 0,
            longest: 0,
            dev_total: 0,
            seen: None,
        });
    });
}

impl Tracker {
    /// The open stretch ends at `at`, at site `to`.
    fn close(&mut self, at: i64, to: u8) {
        let kinds = skin_model::dev::take_kinds();
        self.kinds |= kinds;
        self.visits |= u64::from(kinds) << KIND_SHIFT;
        let Some(start) = self.start.take() else {
            return;
        };
        let len = at - start - self.dev;
        self.longest = self.longest.max(len);
        if len > BOUND_US {
            let id = self.id;
            let mark_at = |tag, t: i64, v| loop_probe::mark_at(tag, TimeUs(t), v);
            mark_at("pre:gap_from", start, i64::try_from(id).unwrap_or(-1));
            for &(s, l) in self.skips.iter().take(self.skipped.min(SKIPS)) {
                mark_at("pre:gap_skip", s, pair(id, l));
            }
            if self.skipped > SKIPS {
                mark_at("pre:gap_skip", at, pair(id, SKIP_OVERFLOW));
            }
            mark_at("pre:gap_over", at, pair(id, len));
            let sites = i64::from(self.from) << 8 | i64::from(to);
            mark_at("pre:gap_sites", at, pair(id, sites));
            mark_at("pre:gap_kinds", at, pair(id, i64::from(self.kinds)));
            let spent = cycles().saturating_sub(self.cycles);
            mark_at(
                "cpu:pack",
                at,
                pair(id, i64::try_from(spent).unwrap_or(i64::MAX)),
            );
        }
        self.skipped = 0;
        self.dev = 0;
        self.kinds = 0;
    }
}

/// A checkpoint's bracket opens at `site`: the open stretch ends.
pub fn bracket_in(site: u8) {
    let Some(at) = now() else {
        return;
    };
    with(|t| {
        t.visits |= 1 << site;
        t.close(at, site);
    });
}

/// A checkpoint's bracket closes, the job going on: a stretch begins.
pub fn bracket_out(site: u8) {
    let Some(at) = now() else {
        return;
    };
    let cycles = cycles();
    with(|t| {
        t.start = Some(at);
        t.from = site;
        t.cycles = cycles;
    });
}

/// The checkpoint at `site` saw the job's stop: `pre:seen`, and no
/// stretch follows. Returns the job's id when tracked.
pub fn stopped_at(site: u8) {
    let Some(at) = now() else {
        return;
    };
    with(|t| {
        t.seen = Some(at);
        loop_probe::mark_at("pre:seen", TimeUs(at), pair(t.id, i64::from(site)));
    });
}

/// Runs dev-only `f` (measurement work no production build does), its
/// time left out of the stretch it falls in.
pub fn dev_only<R>(f: impl FnOnce() -> R) -> R {
    let Some(began) = now() else {
        return f();
    };
    let out = f();
    let Some(ended) = now() else {
        return out;
    };
    let len = ended - began;
    with(|t| {
        if t.start.is_none() {
            return;
        }
        if let Some(s) = t.skips.get_mut(t.skipped) {
            *s = (began, len);
        }
        t.skipped += 1;
        t.dev += len;
        t.dev_total += len;
    });
    out
}

/// The job's build begins (`pre:build`), once.
pub fn build() {
    with(|t| loop_probe::mark("pre:build", i64::try_from(t.id).unwrap_or(-1)));
}

/// The job ends: its last stretch closes, and `pre:gap`, `pre:dev_us`,
/// `pre:visits` and, after a stop, `pre:dispose` are marked.
pub fn finish() {
    let Some(at) = now() else {
        return;
    };
    let Some(mut t) = TRACKER.with(|t| t.borrow_mut().take()) else {
        return;
    };
    t.close(at, END);
    let id = t.id;
    loop_probe::mark("pre:gap", pair(id, t.longest));
    loop_probe::mark("pre:dev_us", pair(id, t.dev_total));
    loop_probe::mark(
        "pre:visits",
        pair(id, i64::try_from(t.visits).unwrap_or(-1)),
    );
    if let Some(seen) = t.seen {
        loop_probe::mark("pre:dispose", pair(id, at - seen));
    }
}

/// A `hold-preload` request: the pack holds at the `n`th checkpoint of
/// `site`, before it answers or after it answered "go on".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldAt {
    pub site: u8,
    pub n: u32,
    pub after: bool,
}

impl HoldAt {
    /// `<site>:<n>[:before|:after]` (before when not given).
    ///
    /// # Errors
    ///
    /// A site of no name, or a malformed count or timing.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = s.split(':');
        let site = parts
            .next()
            .and_then(site_code)
            .ok_or_else(|| format!("{s}: no such checkpoint site"))?;
        let n = parts
            .next()
            .and_then(|n| n.parse().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| format!("{s}: the count is 1 or more"))?;
        let after = match parts.next() {
            None | Some("before") => false,
            Some("after") => true,
            Some(other) => return Err(format!("{s}: {other} is not before or after")),
        };
        if parts.next().is_some() {
            return Err(format!("{s}: one site, count and timing"));
        }
        Ok(Self { site, n, after })
    }
}

/// The hold armed, and the visits of its site so far.
static HOLD: Mutex<Option<(HoldAt, u32)>> = Mutex::new(None);
/// The held job, while one holds.
static HELD: AtomicU64 = AtomicU64::new(0);
/// The hold is let go.
static RELEASED: AtomicBool = AtomicBool::new(false);
/// The hold is let go once the game thread adopts the held job.
static ON_ADOPT: AtomicBool = AtomicBool::new(false);

/// How long a hold waits at most: a measurement's run fails past it.
const HOLD_BOUND: Duration = Duration::from_secs(30);

/// Arms a hold (`hold-preload`).
pub fn hold(at: HoldAt) {
    RELEASED.store(false, Ordering::SeqCst);
    ON_ADOPT.store(false, Ordering::SeqCst);
    *HOLD.lock().unwrap_or_else(PoisonError::into_inner) = Some((at, 0));
}

/// The held job, if one holds.
#[must_use]
pub fn held() -> Option<u64> {
    let id = HELD.load(Ordering::SeqCst);
    (id != 0).then_some(id)
}

/// Lets the hold go (`release-preload`).
pub fn release() {
    RELEASED.store(true, Ordering::SeqCst);
}

/// Lets the hold go once the held job is adopted
/// (`release-preload=adopted`).
pub fn release_on_adopt() {
    ON_ADOPT.store(true, Ordering::SeqCst);
}

/// The game thread adopted job `id`: a hold waiting for it goes.
pub fn adopted(id: u64) {
    if ON_ADOPT.load(Ordering::SeqCst) && HELD.load(Ordering::SeqCst) == id {
        RELEASED.store(true, Ordering::SeqCst);
    }
}

/// Inside job `id`'s checkpoint bracket at `site`, before its answer
/// (`after` false) or after a "go on": holds while an armed hold names
/// this visit, marking `pre:hold` as it engages and `pre:hold_released`
/// as it ends (`pre:hold_timeout` past [`HOLD_BOUND`]).
pub fn engage(id: u64, site: u8, after: bool) {
    let due = {
        let mut hold = HOLD.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((at, seen)) = hold.as_mut() else {
            return;
        };
        if at.site != site {
            return;
        }
        if !after {
            *seen += 1;
        }
        let due = *seen == at.n && at.after == after;
        if due {
            *hold = None;
        }
        due
    };
    if !due {
        return;
    }
    HELD.store(id, Ordering::SeqCst);
    loop_probe::mark("pre:hold", pair(id, i64::from(site)));
    if loop_probe::on() {
        println!("dev: pre:hold {id} {site}");
    }
    let began = Instant::now();
    while !RELEASED.load(Ordering::SeqCst) {
        if began.elapsed() > HOLD_BOUND {
            loop_probe::mark("pre:hold_timeout", i64::try_from(id).unwrap_or(-1));
            break;
        }
        std::thread::park_timeout(Duration::from_millis(1));
    }
    HELD.store(0, Ordering::SeqCst);
    loop_probe::mark("pre:hold_released", i64::try_from(id).unwrap_or(-1));
}

#[cfg(test)]
mod tests {
    use super::{HoldAt, pair};
    use crate::dev::SelectScript;

    /// The measurement's select steps parse, and a malformed hold is
    /// refused with its reason.
    #[test]
    fn the_preload_steps_parse() {
        assert_eq!(
            HoldAt::parse("PlanPlaceWork:1:after"),
            Ok(HoldAt {
                site: 13,
                n: 1,
                after: true
            })
        );
        assert_eq!(
            HoldAt::parse("PlanEnd:2"),
            Ok(HoldAt {
                site: 14,
                n: 2,
                after: false
            })
        );
        assert_eq!(
            HoldAt::parse("Take:3:before"),
            Ok(HoldAt {
                site: 19,
                n: 3,
                after: false
            })
        );
        for bad in [
            "Nowhere:1",
            "PlanEnd:0",
            "PlanEnd",
            "PlanEnd:1:later",
            "PlanEnd:1:after:x",
        ] {
            assert!(HoldAt::parse(bad).is_err(), "{bad}");
        }
        for ok in [
            "hold-preload=PlanEnd:1:after,release-preload=adopted,pick=a.bme,enter",
            "hold-preload=PlanPlaceWork:1:after,preload-held,practice,release-preload",
        ] {
            assert!(SelectScript::parse(ok).is_ok(), "{ok}");
        }
        assert!(SelectScript::parse("hold-preload=Nowhere:1").is_err());
        assert!(SelectScript::parse("release-preload=later").is_err());
        // A paired mark keeps its job and its value apart.
        let v = pair(5, 123_456);
        assert_eq!((v >> 40, v & ((1 << 40) - 1)), (5, 123_456));
    }
}
