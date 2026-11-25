//! A decided play's early staging on WARP (design.md "Screen Transitions
//! and Loading"): its pages and loading presentation staged in steps with
//! no destruction in any call, the result's release and the pool's
//! reduction retired for the boundaries to destroy, and its adoption by the
//! attempt in place, its array and writes kept.

use std::sync::Arc;
use std::time::Duration;

use bms_model::{ChartBuilder, TimeUs};
use render::{EarlyRoom, RenderError, Renderer, SHARED_TEXELS, SkinBundle};
use skin_model::ScreenKind;
use skin_model::Variant;

use crate::renderer::pool_tests::pool_bundle;
use crate::test_common::{
    GAUGES, attempt, context, input, offscreen, read_back, rgb_at, settle, view,
};
use crate::test_decl::{Look, solid_bundle};

const W: u32 = 1_920;
const H: u32 = 1_080;
/// Tiny's atlas array: one 256-texel page beside the reserved layer.
const TINY: u64 = 256 * 256 * 2;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// Tiny of `rgba`, over the left half.
fn tiny(r: &Renderer, rgba: [u8; 4]) -> Arc<SkinBundle> {
    let b = solid_bundle(r, &Look::left(rgba), [16, 16]);
    assert_eq!(b.atlas_texels(), TINY);
    b
}

/// The fixture of three atlas steps: one 3 500² cell on a 3 584-texel
/// page.
fn big(r: &Renderer) -> Arc<SkinBundle> {
    let b = solid_bundle(r, &Look::left(GREEN), [3_500, 3_500]);
    assert_eq!(b.side(), 3_584);
    b
}

/// Installs `b` as `kind`'s for `request` and commits it.
fn commit(r: &mut Renderer, kind: ScreenKind, b: &Arc<SkinBundle>, request: u64) {
    let token = r.install(kind, b, request).unwrap();
    r.commit(token).unwrap();
}

/// Stages `b` with an attempt as play's for `request` and commits it.
fn play(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64) {
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    let token = r.stage(b, &attempt(r, &chart, &ctx), request).unwrap();
    r.commit(token).unwrap();
}

/// The destructions recorded since `from`.
fn recorded_since(r: &Renderer, from: usize) -> Vec<render::DestroyRecord> {
    let all: Vec<_> = r.destructions().copied().collect();
    let n = r.destructions_recorded() - from;
    all[all.len() - n..].to_vec()
}

/// One boundary with time for a step, its completion waited for.
fn boundary(r: &mut Renderer) -> Option<crate::StagingEnd> {
    let up = r.upload_boundary_within(Duration::from_secs(10), &mut || false, None);
    if r.upload_state() == render::UploadState::InFlight {
        assert!(r.wait_step(Duration::from_secs(10)).unwrap());
    }
    up.staged
}

/// The left half's colour at its top after a play frame.
fn colour(r: &mut Renderer) -> [u8; 3] {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented());
    let pixels = read_back(r);
    rgb_at(&pixels, W, (480, 60))
}

/// B14a: an early staging that fits only once a filled entry is destroyed
/// waits for it (its room `Wait`); taken anyway it is refused with nothing
/// destroyed in its call; once a boundary destroyed the entry it fits, and
/// its staging destroys nothing in its call either.
#[test]
fn the_early_staging_destroys_nothing_in_its_call() {
    let mut r = offscreen(W, H);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 2);
    r.remove(ScreenKind::Decision);
    assert!(r.filled() >= 1, "the decision's array fills an entry");
    let b = tiny(&r, GREEN);
    r.state_candidate_charge(&b, SHARED_TEXELS - TINY / 2);
    assert_eq!(r.early_room(&b), EarlyRoom::Wait);
    let from = r.destructions_recorded();
    let e = r.stage_early(&b, 3).expect_err("taken without room");
    assert!(e.is_refusal(), "{e:?}");
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    settle(&mut r);
    assert_eq!(r.early_room(&b), EarlyRoom::Fits);
    let from = r.destructions_recorded();
    r.stage_early(&b, 4).unwrap();
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    assert_eq!(r.staged().map(|t| t.request), Some(4));
    r.discard();
}

/// B14b: an early staging that fits only once the result's installation
/// is released, beside a pool with optional layers: the release retires
/// the pool's live array (to its firm layer) and the result's into their
/// entries with nothing destroyed in its call, reported as play's release;
/// the staging then waits while the boundaries destroy them, and fits.
#[test]
fn the_early_staging_releases_the_result_at_boundaries() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let res = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &res, 3);
    settle(&mut r);
    assert!(r.pool().unwrap().layers > 1, "optional layers");
    let sel = r.admission().charge(ScreenKind::Selection);
    let b = tiny(&r, BLUE);
    r.state_candidate_charge(&b, SHARED_TEXELS - sel - TINY / 2);
    assert_eq!(r.early_room(&b), EarlyRoom::Release);
    let from = r.destructions_recorded();
    r.release_result_deferred(&b).unwrap();
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    assert!(r.take_result_release(), "reported as play's release");
    assert_eq!(r.admission().charge(ScreenKind::Result), 0);
    assert_eq!(r.pool(), None, "the live pool retired");
    assert_eq!(r.early_room(&b), EarlyRoom::Wait, "its entries charged");
    settle(&mut r);
    assert_eq!(r.early_room(&b), EarlyRoom::Fits);
    let from = r.destructions_recorded();
    r.stage_early(&b, 4).unwrap();
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    r.discard();
}

/// `n` bundles of [`Look::left`] in green on one atlas's pages, each its
/// own layout: one installed, the others stage a layout on its pages.
fn on_one_pages(r: &Renderer, n: usize) -> Vec<Arc<SkinBundle>> {
    let dir = tempfile::tempdir().unwrap();
    let look = Look::left(GREEN);
    let first = crate::test_decl::skin(dir.path(), &look);
    let atlas = first
        .atlas(Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    let mut all = Vec::with_capacity(n);
    for _ in 1..n {
        let s = crate::test_decl::skin(dir.path(), &look);
        let rebound = atlas.rebind(s.sheet(), Variant::Lo, false).unwrap();
        all.push(rebundle(&look, s, rebound));
    }
    all.insert(0, rebundle(&look, first, atlas));
    all
}

/// `s` of `look` on `atlas`, with its clips.
fn rebundle(look: &Look, s: skin_model::Skin, atlas: skin_model::Atlas) -> Arc<SkinBundle> {
    let clips = render::ClipTable::new(look.clip.into_iter().collect()).unwrap();
    Arc::new(SkinBundle::new(Arc::new(s), atlas, clips, skin_model::Side::Left).unwrap())
}

/// B14b2: the result's release waits for the pool's reduction to have
/// its entries free: with the retirement list full of layouts retired
/// with no charge (nothing charged, no entry free), its room waits; once a
/// boundary destroyed them it is the release, which retires the pool and
/// the result with nothing destroyed in its call, and once those are
/// destroyed too the staging fits and is created.
// Revert: the release taken with fewer than two entries free -> the
// room answers the release over a full list.
#[test]
fn the_release_waits_for_the_pool_s_entries() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let layouts = on_one_pages(&r, 16);
    commit(&mut r, ScreenKind::Result, &layouts[0], 3);
    settle(&mut r);
    assert!(r.pool().unwrap().layers > 1, "optional layers");
    // Layouts staged on the result's pages, each superseding the last,
    // until no entry is free.
    r.set_destroy_allowed(false);
    let mut full = false;
    for (b, request) in layouts[1..].iter().zip(4_u64..) {
        match r.install(ScreenKind::Result, b, request) {
            Ok(_) => {}
            Err(e) => {
                assert!(matches!(e, RenderError::ListFull { .. }), "{e:?}");
                full = true;
                break;
            }
        }
    }
    assert!(full, "the list never filled");
    r.discard();
    assert!(r.filled() > 0);
    assert_eq!(r.admission().filled, 0, "nothing charged");
    let sel = r.admission().charge(ScreenKind::Selection);
    let res = r.admission().charge(ScreenKind::Result);
    let b = tiny(&r, BLUE);
    r.state_candidate_charge(&b, SHARED_TEXELS - sel - res / 2);
    assert_eq!(r.early_room(&b), EarlyRoom::Wait, "no entry free");
    settle(&mut r);
    assert_eq!(r.early_room(&b), EarlyRoom::Release);
    let from = r.destructions_recorded();
    r.release_result_deferred(&b).unwrap();
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    assert_eq!(r.pool(), None, "the live pool retired");
    assert_eq!(r.early_room(&b), EarlyRoom::Wait, "its entries charged");
    settle(&mut r);
    assert_eq!(r.early_room(&b), EarlyRoom::Fits);
    r.stage_early(&b, 40).unwrap();
    assert_eq!(r.staged().map(|t| t.request), Some(40), "created");
    r.discard();
}

/// B5b1: the attempt adopts an early staging mid-upload: re-keyed in
/// place, its array (`AtlasId`) and its writes kept, the steps going on
/// from where they were, none begun again, and it commits with every row
/// written.
#[test]
fn the_attempt_adopts_an_early_staging_mid_upload() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    assert_eq!(r.stage_early(&b, 1).unwrap(), None);
    let id = r.retained_atlases(ScreenKind::Play)[1];
    assert_eq!(id, Some(b.atlas_id()));
    assert_eq!(boundary(&mut r), None, "one step, more to come");
    let steps = r.step_submits.get();
    assert!(steps >= 1);
    assert!(r.adopt_staging(1, 2, &attempt(&r, &chart, &ctx)).unwrap());
    assert_eq!(r.staged().map(|t| t.request), Some(2));
    assert_eq!(
        r.retained_atlases(ScreenKind::Play)[1],
        id,
        "the same array"
    );
    let token = loop {
        if let Some(t) = r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 2).unwrap() {
            break t;
        }
        boundary(&mut r);
    };
    assert_eq!(token.request, 2);
    assert_eq!(r.step_submits.get(), 3, "the writes went on, none again");
    r.commit(token).unwrap();
    assert_eq!(colour(&mut r), [0, 255, 0], "every row written");
}

/// B5b2 and B5c: an early staging answered (pages written) stays drawable
/// across its adoption, the same token's staging under the attempt's
/// request, and the attempt reaches READY with it: asked again it answers
/// ready, and commits.
#[test]
fn the_attempt_adopts_an_answered_early_staging() {
    let mut r = offscreen(W, H);
    let b = tiny(&r, GREEN);
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    let early = loop {
        if let Some(t) = r.stage_early(&b, 1).unwrap() {
            break t;
        }
        boundary(&mut r);
    };
    assert_eq!(r.staged_drawable(), Some(early));
    let id = r.retained_atlases(ScreenKind::Play)[1];
    assert!(r.adopt_staging(1, 2, &attempt(&r, &chart, &ctx)).unwrap());
    let drawable = r.staged_drawable().expect("drawable across the adoption");
    assert_eq!(drawable.request, 2);
    assert_eq!(drawable.device, early.device);
    assert_eq!(r.retained_atlases(ScreenKind::Play)[1], id);
    let token = loop {
        if let Some(t) = r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 2).unwrap() {
            break t;
        }
        assert!(r.staged_drawable().is_some(), "never undrawable");
        boundary(&mut r);
    };
    r.commit(token).unwrap();
    assert_eq!(colour(&mut r), [0, 255, 0]);
}

/// An adoption of a staging the renderer does not hold as that early
/// request changes nothing: another request, or a request not newer.
#[test]
fn an_adoption_needs_the_early_staging_held() {
    let mut r = offscreen(W, H);
    let b = tiny(&r, GREEN);
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    let _ = r.stage_early(&b, 3).unwrap();
    assert!(!r.adopt_staging(2, 4, &attempt(&r, &chart, &ctx)).unwrap());
    assert!(!r.adopt_staging(3, 3, &attempt(&r, &chart, &ctx)).unwrap());
    assert_eq!(r.staged().map(|t| t.request), Some(3));
}
