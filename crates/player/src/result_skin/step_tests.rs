//! The atlas step's checkpoints, run on the test's thread through the
//! pack with a check the test answers: a stop in the union branch (E18),
//! the slot unlocked while it plans (E19), one packing per build (E17),
//! the snapshot gone before the release (E23), and the gap tracker's
//! dev-only time, scored interval and cycles (E20, E24).

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use skin_beatoraja::{Cancel, JudgeClass, LuaImported, LuaOutcome};

use super::preload_tests::{CYCLES, CYCLES_ON, Witness, advance, clock_now, held_clock};
use super::tests::{
    AT_RELEASE, IN_PROBE_MISS, LIMITS, cells_skin, pending, skin_reading, staged, staged_at, view,
    view_at,
};
use super::{
    NotPacked, Packed, PendingResult, ResultSkin, ResultStart, Slot, ViewFacts, builds, gaps, lock,
    pack, result_facts,
};
use crate::imports::tests::serial;

/// A skin whose probe (154 = 1) draws one cell of `s.png`, and every other
/// view 900 one-texel cells of `big.png`: a layout's atlas then needs a
/// union with the probe's entry and many page tries to plan.
fn wide() -> (tempfile::TempDir, Arc<PendingResult>) {
    let d = skin_reading(
        "local nr = ms.number(154)",
        "local imgs = { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }\n\
         if nr ~= 1 then\n\
           imgs = {}\n\
           for i = 0, 899 do\n\
             imgs[#imgs + 1] = { id = \"c\" .. i, src = \"b\", x = i % 300, \
                                 y = math.floor(i / 300), w = 1, h = 1 }\n\
           end\n\
         end",
        "imgs",
    );
    let p = pending(d.path(), LIMITS);
    (d, p)
}

/// `p` imported for `v`.
fn import(p: &PendingResult, v: ViewFacts) -> LuaImported {
    let facts = result_facts(v.0.unwrap(), Some(Some(JudgeClass::Normal)));
    match p.header.import_reading(&p.config, &facts, &Cancel::new()) {
        Ok(LuaOutcome::Imported(i)) => i,
        other => panic!("{:?}", other.map(|_| ()).map_err(|e| e.0.to_string())),
    }
}

/// The pack of `p` for `v` on this thread, lent `rs`'s image state, its
/// stop `check`; the slot it was lent.
fn pack_with(
    rs: &mut ResultSkin,
    p: &Arc<PendingResult>,
    v: ViewFacts,
    check: &dyn Fn() -> bool,
) -> (Result<Packed, NotPacked>, Slot) {
    let imported = import(p, v);
    let base = rs.shown_entry();
    let others = rs.counted("the test");
    let slot = rs.slot_for_job();
    let out = pack(p, imported, &slot, base, others, &Cancel::new(), check);
    (out, slot)
}

/// What a pack left, given back to `rs`.
fn give_back(rs: &mut ResultSkin, out: Result<Packed, NotPacked>, slot: &Slot) {
    match out {
        Ok((new, ..)) => rs.restore(new, slot),
        Err(not) => rs.restore(not.1, slot),
    }
}

/// A check that stops at the `n`th ask at `site`: that ask (`after`
/// false) or the next one.
struct StopAt {
    site: u8,
    n: u32,
    after: bool,
    seen: Cell<u32>,
    armed: Cell<bool>,
    sites: RefCell<Vec<u8>>,
    stopped: Cell<Option<u8>>,
}

impl StopAt {
    fn new(site: u8, n: u32, after: bool) -> Self {
        Self {
            site,
            n,
            after,
            seen: Cell::new(0),
            armed: Cell::new(false),
            sites: RefCell::new(Vec::new()),
            stopped: Cell::new(None),
        }
    }

    /// Never stops.
    fn never() -> Self {
        Self::new(u8::MAX, 0, false)
    }

    fn ask(&self) -> bool {
        let s = gaps::site_now();
        self.sites.borrow_mut().push(s);
        if self.armed.get() || (s == self.site && self.count() && !self.after) {
            self.stopped.set(Some(s));
            return true;
        }
        if s == self.site && self.seen.get() == self.n && self.after {
            self.armed.set(true);
        }
        false
    }

    /// Counts a visit of its site: whether it is the `n`th.
    fn count(&self) -> bool {
        self.seen.set(self.seen.get() + 1);
        self.seen.get() == self.n
    }
}

const UNION: u8 = 17;
const UNION_END: u8 = 18;
const TAKE: u8 = 19;
const PLAN_END: u8 = 14;
const PLAN_PLACE_WORK: u8 = 13;
const PLAN_RECTS: u8 = 7;

/// The probe's entry in `rs`, then a layout of 900 cells that needs a
/// union with it.
fn earlier() -> (tempfile::TempDir, Arc<PendingResult>, ResultSkin) {
    let (d, p) = wide();
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    staged(&mut rs);
    assert!(rs.image().is_some(), "the probe's entry");
    (d, p, rs)
}

/// E18 in the union branch: a stop published while the step plans, or
/// unions, or after its plan, is seen at its next ask; the step builds
/// nothing, and the earlier entry and kept cells come back as they were.
#[test]
fn a_stop_in_the_union_branch_keeps_the_earlier_entry() {
    let _s = serial();
    // The control: the sites a full union pack asks at.
    let (_d, p, mut rs) = earlier();
    let before = rs.image().map(Arc::clone).unwrap();
    let control = StopAt::never();
    let b0 = builds();
    let (out, slot) = pack_with(&mut rs, &p, view(2), &|| control.ask());
    assert!(out.is_ok(), "the union packs");
    assert_eq!(builds(), b0 + 1);
    let sites = control.sites.borrow().clone();
    let unions = sites.iter().filter(|&&s| s == UNION).count();
    assert!(unions >= 1, "the union branch ran: {sites:?}");
    assert!(
        sites.iter().filter(|&&s| s == PLAN_PLACE_WORK).count() >= 4,
        "placements asked in batches"
    );
    drop((out, slot, before));

    for (what, site, n, want) in [
        ("a later placement", PLAN_PLACE_WORK, 3, None),
        (
            "the union",
            UNION,
            u32::try_from(unions).unwrap(),
            Some(UNION_END),
        ),
        ("the final checkpoint", PLAN_END, 1, Some(TAKE)),
    ] {
        let (_d, p, mut rs) = earlier();
        let before = rs.image().map(Arc::clone).unwrap();
        let cells = format!("{:?}", rs.cells);
        let stop = StopAt::new(site, n, true);
        let b0 = builds();
        let (out, slot) = pack_with(&mut rs, &p, view(2), &|| stop.ask());
        let seen = stop.stopped.get();
        match want {
            Some(want) => assert_eq!(seen, Some(want), "{what}: seen at"),
            None => assert!(
                seen.is_some_and(|s| (PLAN_RECTS..=PLAN_END).contains(&s)),
                "{what}: seen at a planning ask, not {seen:?}"
            ),
        }
        assert!(out.is_err(), "{what}: stopped");
        assert_eq!(builds(), b0, "{what}: no build");
        give_back(&mut rs, out, &slot);
        let after = rs.image().map(Arc::clone).unwrap();
        assert!(Arc::ptr_eq(&before, &after), "{what}: the same entry");
        assert_eq!(format!("{:?}", rs.cells), cells, "{what}: the kept cells");
    }
}

/// E19 in the union branch: at every union, planning and final ask the
/// job's slot is unlocked.
#[test]
fn the_slot_is_unlocked_while_the_union_is_planned() {
    let _s = serial();
    let (_d, p, mut rs) = earlier();
    let checked = RefCell::new(Vec::new());
    let slot_seen: RefCell<Option<Slot>> = RefCell::new(None);
    let imported = import(&p, view(2));
    let base = rs.shown_entry();
    let others = rs.counted("the test");
    let slot = rs.slot_for_job();
    *slot_seen.borrow_mut() = Some(Arc::clone(&slot));
    let check = || {
        let s = gaps::site_now();
        if matches!(s, 7..=14 | UNION | UNION_END | TAKE) {
            let free = slot_seen
                .borrow()
                .as_ref()
                .is_some_and(|sl| sl.try_lock().is_ok());
            checked.borrow_mut().push((s, free));
        }
        false
    };
    let out = pack(&p, imported, &slot, base, others, &Cancel::new(), &check);
    assert!(out.is_ok());
    let checked = checked.into_inner();
    for site in [
        PLAN_RECTS,
        PLAN_PLACE_WORK,
        PLAN_END,
        UNION,
        UNION_END,
        TAKE,
    ] {
        assert!(
            checked.iter().any(|&(s, _)| s == site),
            "{site} asked: {checked:?}"
        );
    }
    let locked: Vec<_> = checked.iter().filter(|&&(_, free)| !free).collect();
    assert!(locked.is_empty(), "the slot locked at {locked:?}");
}

/// E17 in the player: a union that fits packs once; a refused union and
/// the own sheet that fits pack twice; the build adds none.
#[test]
fn a_union_pack_plans_once_or_twice_and_builds_once() {
    let _s = serial();
    let (_d, p, mut rs) = earlier();
    let k0 = skin_model::dev::packings();
    let b0 = builds();
    let (out, _slot) = pack_with(&mut rs, &p, view(2), &|| false);
    assert!(out.is_ok());
    assert_eq!(skin_model::dev::packings() - k0, 1, "the union fits: once");
    assert_eq!(builds(), b0 + 1);
    drop(out);

    // C shown at level 7 fills the one page; A and B at level 8 do not fit
    // with it, and are packed alone.
    let (_d, p) = cells_skin();
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
    staged_at(&mut rs, 7, JudgeClass::Normal);
    let k0 = skin_model::dev::packings();
    let b0 = builds();
    let (out, _slot) = pack_with(&mut rs, &p, view_at(8, 2), &|| false);
    assert!(out.is_ok());
    assert_eq!(
        skin_model::dev::packings() - k0,
        2,
        "the refused union, then the own sheet"
    );
    assert_eq!(builds(), b0 + 1);
}

/// E23: the earlier entry the release drops has dropped by its return:
/// the step's snapshot of it was gone before.
#[test]
fn the_snapshot_is_gone_before_the_release() {
    let _s = serial();
    let (_d, p, mut rs) = earlier();
    let witness = Witness::on();
    static AT: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);
    fn at_release() {
        *AT.lock().unwrap() = Some(Witness::count());
    }
    *AT_RELEASE.lock().unwrap() = Some(at_release);
    let (out, _slot) = pack_with(&mut rs, &p, view(2), &|| false);
    *AT_RELEASE.lock().unwrap() = None;
    assert!(out.is_ok());
    assert_eq!(
        *AT.lock().unwrap(),
        Some(1),
        "the earlier entry dropped by the release's return"
    );
    drop(witness);
}

/// The marks of `tag` since index `from`: (time, job, value).
fn marks(from: usize, tag: &str) -> Vec<(u64, i64, i64)> {
    crate::dev::loop_probe::tests::marks()
        .into_iter()
        .skip(from)
        .filter(|m| m.1 == tag)
        .map(|(t, _, v)| (t, v >> 40, v & ((1 << 40) - 1)))
        .collect()
}

/// A check that brackets each ask as the pack's does, `before` run first.
fn bracketing(before: &dyn Fn(u8)) -> impl Fn() -> bool {
    move || {
        let s = gaps::site_now();
        before(s);
        gaps::bracket_in(s);
        gaps::bracket_out(s);
        false
    }
}

/// E20's dev-only time and a scored gap's interval: the probe-miss
/// diagnostic's 20 ms is left out of the stretch it falls in and marked as
/// dev-only time; a stretch over the bound is marked with its start and
/// that excluded span.
#[test]
fn dev_only_time_is_left_out_and_a_scored_gap_names_its_interval() {
    let _s = serial();
    let _ = held_clock();
    fn miss() {
        *MISS_AT.lock().unwrap() = Some(clock_now());
        advance(20_000);
    }
    static MISS_AT: std::sync::Mutex<Option<i64>> = std::sync::Mutex::new(None);
    *IN_PROBE_MISS.lock().unwrap() = Some(miss);

    // Dev-only time alone: no gap.
    let (_d, p, mut rs) = earlier();
    let from = crate::dev::loop_probe::tests::marks().len();
    gaps::admit(41);
    let check = bracketing(&|_| {});
    let (out, _slot) = pack_with(&mut rs, &p, view(2), &check);
    gaps::finish();
    assert!(out.is_ok());
    assert!(MISS_AT.lock().unwrap().is_some(), "the diagnostic ran");
    assert_eq!(
        marks(from, "pre:gap"),
        [(marks(from, "pre:gap")[0].0, 41, 0)]
    );
    assert_eq!(marks(from, "pre:dev_us")[0].2, 20_000);
    assert!(marks(from, "pre:gap_over").is_empty());

    // 60 ms of work in the stretch the diagnostic falls in.
    let (_d, p, mut rs) = earlier();
    let from = crate::dev::loop_probe::tests::marks().len();
    let start = Cell::new(None);
    let first_union = Cell::new(true);
    gaps::admit(42);
    let check = || {
        let s = gaps::site_now();
        if s == UNION && first_union.replace(false) {
            advance(60_000);
        }
        gaps::bracket_in(s);
        gaps::bracket_out(s);
        if s == 16 {
            start.set(Some(clock_now()));
        }
        false
    };
    let (out, _slot) = pack_with(&mut rs, &p, view(2), &check);
    gaps::finish();
    assert!(out.is_ok());
    let over = marks(from, "pre:gap_over");
    assert_eq!(over.len(), 1, "{over:?}");
    assert_eq!((over[0].1, over[0].2), (42, 60_000));
    let start = u64::try_from(start.get().unwrap()).unwrap();
    assert_eq!(marks(from, "pre:gap_from"), [(start, 0, 42)]);
    let miss_at = u64::try_from(MISS_AT.lock().unwrap().unwrap()).unwrap();
    assert_eq!(marks(from, "pre:gap_skip"), [(miss_at, 42, 20_000)]);
    let sites = marks(from, "pre:gap_sites");
    assert_eq!(sites[0].2, 16 << 8 | i64::from(UNION));
    *IN_PROBE_MISS.lock().unwrap() = None;
}

/// E24: the cycles marked beside a stretch over the bound are that
/// stretch's, from its start to its end, not the thread's total.
#[test]
fn a_gaps_cycles_are_its_own() {
    let _s = serial();
    let _ = held_clock();
    let from = crate::dev::loop_probe::tests::marks().len();
    *CYCLES.lock().unwrap() = vec![100, 250, 400];
    CYCLES_ON.store(true, SeqCst);
    gaps::admit(7);
    advance(10_000);
    gaps::bracket_in(16);
    gaps::bracket_out(16);
    advance(60_000);
    gaps::bracket_in(19);
    gaps::finish();
    CYCLES_ON.store(false, SeqCst);
    let cpu = marks(from, "cpu:pack");
    assert_eq!(cpu.len(), 1, "{cpu:?}");
    assert_eq!((cpu[0].1, cpu[0].2), (7, 150));
    // Positive control: no stretch over the bound marks none.
    let from = crate::dev::loop_probe::tests::marks().len();
    *CYCLES.lock().unwrap() = vec![100, 250, 400];
    CYCLES_ON.store(true, SeqCst);
    gaps::admit(8);
    advance(10_000);
    gaps::bracket_in(16);
    gaps::bracket_out(16);
    advance(10_000);
    gaps::bracket_in(19);
    gaps::finish();
    CYCLES_ON.store(false, SeqCst);
    assert!(marks(from, "cpu:pack").is_empty());
    let _ = lock;
}
