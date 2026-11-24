//! The planning's asks (E16), one packing per build (E17) and the
//! wrappers against [`Sheet::plan_atlas`] (E22).

use std::cell::{Cell, RefCell};

use proptest::prelude::*;

use super::pack::{
    self, ALLOC_CALLS, PACKINGS_HERE, PLACE_ATTEMPTS, PLACED, PLAN_BATCH, RECT_UNITS, SIDE_STARTS,
    Work,
};
use super::*;
use crate::image::decode::SITE;
use crate::quad::Filter;
use crate::{Root, SheetBuilder, SolidImageDef};

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 2_048,
    max_layers: 64,
    reserved_layers: 0,
};

/// The cells E16 plans: three batches and seven more.
const N: usize = 3 * PLAN_BATCH + 7;

/// A sheet of generated cells of `sizes`, each its own colour.
fn sheet(sizes: &[[u32; 2]]) -> Sheet {
    let dir = tempfile::tempdir().unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    for (i, &size) in sizes.iter().enumerate() {
        let i = u32::try_from(i).unwrap();
        b.add_solid_image(SolidImageDef {
            size,
            color: [
                u8::try_from(i % 251).unwrap(),
                u8::try_from(i / 251 % 251).unwrap(),
                7,
                255,
            ],
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    }
    b.finish()
}

/// Mixed sizes: on the first sides several pages open, so a placement
/// tries more than one page, and at least three sides are packed.
fn mixed(n: usize) -> Vec<[u32; 2]> {
    (0..n)
        .map(|i| {
            let i = u32::try_from(i).unwrap();
            [30 + i * 37 % 170, 30 + i * 53 % 150]
        })
        .collect()
}

/// One ask: its site and the counters then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ask {
    site: Site,
    rects: u64,
    sides: u64,
    attempts: u64,
    allocs: u64,
    placed: u64,
}

fn reset() {
    for c in [
        &RECT_UNITS,
        &SIDE_STARTS,
        &PLACE_ATTEMPTS,
        &ALLOC_CALLS,
        &PLACED,
        &PACKINGS_HERE,
    ] {
        c.with(|c| c.set(0));
    }
}

fn now(site: Site) -> Ask {
    Ask {
        site,
        rects: RECT_UNITS.with(Cell::get),
        sides: SIDE_STARTS.with(Cell::get),
        attempts: PLACE_ATTEMPTS.with(Cell::get),
        allocs: ALLOC_CALLS.with(Cell::get),
        placed: PLACED.with(Cell::get),
    }
}

/// Plans `s`, stopping at the `stop`th ask (1-based; 0 never), and
/// records every ask.
fn plan(s: &Sheet, stop: usize) -> (Result<u64, AtlasError>, Vec<Ask>) {
    reset();
    let asks = RefCell::new(Vec::new());
    let cancel = || {
        let site = SITE.with(Cell::get).unwrap();
        let mut asks = asks.borrow_mut();
        asks.push(now(site));
        asks.len() == stop
    };
    let r = s
        .plan_atlas(Variant::Lo, LIMITS, KeptLayers::NONE, &cancel)
        .map(|p| p.texels());
    (r, asks.into_inner())
}

#[test]
fn planning_asks_at_each_phase_and_every_batch_of_units() {
    let s = sheet(&mixed(N));
    let (r, asks) = plan(&s, 0);
    let texels = r.unwrap();
    let total = now(Site::PlanEnd);
    let units = |a: &Ask| a.rects + a.allocs;

    // bound: never more than a batch of units between two asks.
    let mut last = 0;
    for a in &asks {
        assert!(
            units(a) - last <= PLAN_BATCH as u64,
            "bound: {a:?} after {last}"
        );
        last = units(a);
    }
    assert_eq!(units(&total), last, "bound: no unit after the last ask");

    // rects: one PlanRects before the first unit, then a batch ask per
    // PLAN_BATCH rectangles, and no placement ask among them.
    let first = asks[0];
    assert_eq!((first.site, first.rects), (Site::PlanRects, 0), "rects");
    let sort = asks.iter().position(|a| a.site == Site::PlanSort).unwrap();
    let pass = &asks[..sort];
    assert_eq!(pass.len(), 1 + N / PLAN_BATCH, "rects: {pass:?}");
    assert!(
        pass[1..].iter().all(|a| a.site == Site::PlanRectWork),
        "rects: {pass:?}"
    );

    // order: the sort after the pass's last unit, the scans after the
    // sort with no unit between.
    assert_eq!(asks[sort].rects, N as u64, "order");
    assert_eq!(asks[sort + 1].site, Site::PlanScan, "order");
    assert_eq!(units(&asks[sort + 1]), units(&asks[sort]), "order");

    // sides: one PlanSide per side begun, before its table setup, each
    // followed by its PlanPlace with no unit between; every other ask of
    // the placements is a batch ask.
    let placements = &asks[sort + 2..asks.len() - 1];
    let sides: Vec<usize> = (0..placements.len())
        .filter(|&i| placements[i].site == Site::PlanSide)
        .collect();
    assert_eq!(sides.len() as u64, total.sides, "sides");
    assert!(total.sides >= 3, "sides: {}", total.sides);
    for (k, &i) in sides.iter().enumerate() {
        assert_eq!(placements[i].sides, k as u64 + 1, "sides: {k}");
        let next = placements[i + 1];
        assert_eq!(next.site, Site::PlanPlace, "sides: {k}");
        assert_eq!(units(&next), units(&placements[i]), "sides: {k}");
    }
    assert!(
        placements.iter().all(|a| matches!(
            a.site,
            Site::PlanSide | Site::PlanPlace | Site::PlanPlaceWork
        )),
        "sides: {placements:?}"
    );
    assert_eq!(asks.last().unwrap().site, Site::PlanEnd, "sides");

    // attempts: page tries, not placements, are counted.
    assert!(total.attempts > total.placed, "attempts: {total:?}");
    let work: Vec<&Ask> = placements
        .iter()
        .filter(|a| a.site == Site::PlanPlaceWork)
        .collect();
    assert!(
        work.windows(2)
            .any(|w| w[1].placed - w[0].placed < PLAN_BATCH as u64),
        "attempts"
    );

    // Positive control: the totals.
    assert_eq!(total.rects, N as u64, "control");
    assert_eq!(total.attempts, total.allocs, "control");
    assert_eq!(
        texels,
        s.plan_keeping(Variant::Lo, LIMITS, KeptLayers::NONE)
            .unwrap(),
        "control"
    );

    // A stop at any ask ends the plan cancelled.
    for k in 1..=asks.len() {
        let (r, seen) = plan(&s, k);
        assert!(
            matches!(r, Err(AtlasError::Cancelled)),
            "stop at {k}: {r:?}"
        );
        assert_eq!(seen.len(), k, "stop at {k}");
    }
}

#[test]
fn a_build_packs_its_sheet_once() {
    let s = sheet(&mixed(40));
    reset();
    s.atlas_keeping(Variant::Lo, LIMITS, KeptLayers::NONE, &|| false)
        .unwrap();
    assert_eq!(PACKINGS_HERE.with(Cell::get), 1, "atlas_keeping");
    reset();
    s.plan_atlas(Variant::Lo, LIMITS, KeptLayers::NONE, &|| false)
        .unwrap()
        .build(&|| false)
        .unwrap();
    assert_eq!(PACKINGS_HERE.with(Cell::get), 1, "plan then build");
    // Positive control: a separate plan and build pack twice.
    reset();
    s.plan_keeping(Variant::Lo, LIMITS, KeptLayers::NONE)
        .unwrap();
    s.atlas_keeping(Variant::Lo, LIMITS, KeptLayers::NONE, &|| false)
        .unwrap();
    assert_eq!(PACKINGS_HERE.with(Cell::get), 2, "control");
}

#[test]
fn a_cancel_that_says_stop_is_seen_before_a_refusal() {
    let small = AtlasLimits {
        max_side: 512,
        max_layers: 8,
        reserved_layers: 0,
    };
    let s = sheet(&[[600, 10], [20, 20]]);
    let stop = || true;
    let r = s.plan_atlas(Variant::Lo, small, KeptLayers::NONE, &stop);
    assert!(matches!(r, Err(AtlasError::Cancelled)), "{r:?}");
    let r = s.atlas_keeping(Variant::Lo, small, KeptLayers::NONE, &stop);
    assert!(matches!(r, Err(AtlasError::Cancelled)), "{r:?}");
    let r = s.plan_keeping(Variant::Lo, small, KeptLayers::NONE);
    assert!(matches!(r, Err(AtlasError::CellTooLarge { .. })), "{r:?}");
    // The limits are still decided first.
    let none = AtlasLimits {
        reserved_layers: 8,
        ..small
    };
    let r = s.plan_atlas(Variant::Lo, none, KeptLayers::NONE, &stop);
    assert!(matches!(r, Err(AtlasError::Limits)), "{r:?}");
}

/// What an atlas came to, for comparison.
fn shape(r: &Result<Atlas, AtlasError>) -> Result<(u32, usize, String), String> {
    r.as_ref()
        .map(|a| {
            (
                a.pages.side,
                a.pages.pages.len(),
                format!("{:?}", a.pages.cells),
            )
        })
        .map_err(|e| format!("{e:?}"))
}

fn sizes() -> impl Strategy<Value = Vec<[u32; 2]>> {
    prop::collection::vec((1_u32..400, 1_u32..400), 1..200)
        .prop_map(|v| v.into_iter().map(|(w, h)| [w, h]).collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn the_wrappers_are_the_plan(sizes in sizes(), side in 2_u32..17, layers in 1_u32..10, text: bool) {
        let limits = AtlasLimits { max_side: side * 128, max_layers: layers + 1, reserved_layers: 1 };
        let kept = KeptLayers::text(text);
        let s = sheet(&sizes);
        let asked = Cell::new(0_u32);
        let counting = || { asked.set(asked.get() + 1); false };
        let planned = s.plan_atlas(Variant::Lo, limits, kept, &counting);
        let never = s.packing(Variant::Lo, limits, kept, &mut Work::never());
        match (&planned, &never) {
            (Ok(p), Ok((rects, packing))) => {
                prop_assert_eq!(&p.rects, rects);
                prop_assert_eq!(&p.packing, packing);
                prop_assert!(asked.get() > 0);
            }
            (Err(a), Err(b)) => prop_assert_eq!(format!("{a:?}"), format!("{b:?}")),
            (a, b) => prop_assert!(false, "{a:?} against {b:?}"),
        }
        let texels = planned.as_ref().map(AtlasPlan::texels).map_err(|e| format!("{e:?}"));
        let keeping = s.plan_keeping(Variant::Lo, limits, kept).map_err(|e| format!("{e:?}"));
        prop_assert_eq!(&keeping, &texels);
        let built = shape(&s.atlas_keeping(Variant::Lo, limits, kept, &|| false));
        let reserving = shape(&s.atlas_reserving(Variant::Lo, limits, text, &|| false));
        let from_plan = shape(&planned.and_then(|p| p.build(&|| false)));
        prop_assert_eq!(&built, &from_plan);
        if !kept.chart {
            prop_assert_eq!(&reserving, &from_plan);
        }
    }

    #[test]
    fn an_aligned_side_packing_asks_without_changing(sizes in sizes(), budget in 1_u32..8) {
        let padded: Vec<[u32; 2]> = sizes.iter().map(|&[w, h]| [w + 2, h + 2]).collect();
        let order = pack::order(&padded);
        let never = pack::pack_side_aligned(&padded, &order, 1_024, budget, pack::MAX_CELLS_PER_PAGE, 1, &mut Work::never());
        let asking = pack::pack_side_aligned(&padded, &order, 1_024, budget, pack::MAX_CELLS_PER_PAGE, 1, &mut Work::new(&|| false));
        prop_assert_eq!(never, asking);
    }
}
