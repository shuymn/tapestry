//! The shared limit on WARP through the renderer's own admission
//! (design.md "Memory Budget"), with committed installations charged as
//! [`Renderer::state_charge`] states them, so a committed skin at an
//! atlas's cap is charged without its array being uploaded: two skins at
//! the cap fit together, a candidate replaces its kind's charge, a kept VM
//! and a candidate's text layer are charged, a refusal comes before
//! anything is staged, and a removed installation gives its charge back.
//! `tests/admission.rs` checks that a commit's own charge, unstated, is
//! its array's texels.
//!
//! "Big" is two generated cells, no file read, whose atlas array is
//! exactly an atlas's cap, built once for the whole binary and never
//! uploaded. "Tiny" is a 16×16 cell on one 256-texel page. The shared
//! limit is more than two caps, so a committed installation is stated
//! the charge that puts a sum at the boundary a test names, derived from
//! [`SHARED_TEXELS`] and [`CAP`]. A sum past the limit is shown past it by
//! the smallest atlas, tiny's, or by a kept VM: charges are whole layers,
//! so no candidate is one texel past.

use std::sync::{Arc, OnceLock};

use bms_model::ChartBuilder;
use render::{RenderError, Renderer, SHARED_TEXELS, SkinBundle, VM_TEXELS};
use skin_model::{AtlasLimits, MAX_ATLAS_TEXELS, ScreenKind};

use crate::renderer::pool_tests::{LAYER, pool_bundle};
use crate::test_common::{GAUGES, attempt, context, offscreen, settle};
use crate::test_decl::{Look, atlas_cap_bundle, solid_bundle};

/// An atlas's cap: at most half the shared limit.
const CAP: u64 = MAX_ATLAS_TEXELS;
/// Tiny's atlas array: one 256-texel page beside the reserved layer.
const TINY: u64 = 256 * 256 * 2;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// Big: its atlas array is exactly [`CAP`]. Built once for every test of
/// this binary, on the first test's device (every test runs on WARP,
/// whose atlas limits are the same); no test commits it.
fn big(r: &Renderer) -> Arc<SkinBundle> {
    static BIG_BUNDLE: OnceLock<(AtlasLimits, Arc<SkinBundle>)> = OnceLock::new();
    let (limits, b) = BIG_BUNDLE.get_or_init(|| (r.atlas_limits(), atlas_cap_bundle(r, BLUE)));
    assert_eq!(
        *limits,
        r.atlas_limits(),
        "built for another device's limits"
    );
    assert_eq!(b.atlas_texels(), CAP);
    Arc::clone(b)
}

/// Tiny of `rgba`, over the left half.
fn tiny(r: &Renderer, rgba: [u8; 4]) -> Arc<SkinBundle> {
    let b = solid_bundle(r, &Look::left(rgba), [16, 16]);
    assert_eq!(b.atlas_texels(), TINY);
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

/// The refusal `admit` or a staging answers for `needed` texels of `kind`.
#[track_caller]
fn refused(e: &RenderError, kind: ScreenKind, needed: u64) {
    assert!(
        matches!(
            *e,
            RenderError::Admission { kind: k, needed: n, limit }
                if k == kind && n == needed && limit == SHARED_TEXELS
        ),
        "{e:?}"
    );
    assert!(e.is_refusal());
}

/// The shared limit is 896 Mi, at least two atlases at the cap: big as
/// the selection screen's candidate fits beside play's skin charged the
/// cap; charged what the limit leaves beside big, play's skin puts the sum
/// exactly at the limit, and one texel more past it; once a tiny result
/// skin is committed too, the sum is past the limit by tiny's atlas, and
/// big is refused.
#[test]
fn two_skins_at_the_atlas_cap_fit_together() {
    assert_eq!(SHARED_TEXELS, 896 << 20);
    const { assert!(SHARED_TEXELS >= 2 * CAP) };
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    assert_eq!(r.admission().charge(ScreenKind::Play), TINY);
    r.state_charge(&p, CAP);
    assert_eq!(r.admission().charge(ScreenKind::Play), CAP);
    r.admit(ScreenKind::Selection, &big).unwrap();
    // Exactly at the limit, then one texel more is past it.
    let rest = SHARED_TEXELS - CAP;
    r.state_charge(&p, rest);
    r.admit(ScreenKind::Selection, &big).unwrap();
    r.state_charge(&p, rest + 1);
    let e = r.admit(ScreenKind::Selection, &big).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + 1);
    r.state_charge(&p, rest);

    let result = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &result, 2);
    let e = r.admit(ScreenKind::Selection, &big).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
    let e = r.install(ScreenKind::Selection, &big, 3).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
    assert!(r.staged().is_none());
}

/// The largest supported skin of each kind fits with the others: play,
/// selection and decision committed at the largest surveyed charges, a
/// decision skin of seven 4 224-texel image pages charged with its reserved
/// layer and no text layer, play and selection keeping a VM, beside a
/// result candidate at an atlas's cap keeping one too, which is more than
/// the largest result skin surveyed. That set is past the former 576 Mi.
/// A VM charge after S7-07 (4 Mi) is smaller than [`VM_TEXELS`], so the
/// same set fits under it too. At a replacement's peak the admitted
/// texels and the replaced atlas are within 1 152 Mi.
#[test]
fn the_largest_skin_of_each_kind_fits_together() {
    const M: u64 = 1_000_000;
    const DECISION: u64 = 8 * 4_224 * 4_224;
    assert_eq!(DECISION, 142_737_408);
    const { assert!(4 << 20 <= VM_TEXELS) };
    assert_eq!(SHARED_TEXELS + CAP, 1_152 << 20);
    let mut r = offscreen(1_920, 1_080);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    r.state_charge(&p, 111 * M + VM_TEXELS);
    let s = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &s, 2);
    r.state_charge(&s, 196_300_000 + VM_TEXELS);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 3);
    r.state_charge(&d, DECISION);
    let result = Arc::new(
        Arc::try_unwrap(atlas_cap_bundle(&r, GREEN))
            .unwrap()
            .keeping_vm(),
    );
    let needed: u64 = r.admission().committed.iter().sum::<u64>() + CAP + VM_TEXELS;
    const { assert!(231_900_000 < CAP) };
    assert!(needed > 576 << 20, "{needed}");
    assert!(needed <= SHARED_TEXELS, "{needed}");
    r.admit(ScreenKind::Result, &result).unwrap();
}

/// The largest selection skin's charge as measured (201 719 808 texels,
/// 38 layers of 2 304 with its chart layer): its atlas without the chart
/// layer and its chart-image pool's firm layer, one layer of the same side
/// (design.md "Memory Budget"). Its optional layers are a reclaimable term
/// outside the set.
const SELECTION_ATLAS: u64 = 37 * 2_304 * 2_304;
const SELECTION_FIRM: u64 = 2_304 * 2_304;

/// The owner's largest set with a play skin's chart layer (S10-12), as
/// measured over the imported skins at the device's limits: the play skin
/// with its chart layer and its title card's fonts (178 421 760 texels), the
/// largest selection skin (its atlas and its pool's firm layer, 201 719 808)
/// and decision skin (142 737 408), each keeping a VM, and the largest
/// result skin (234 881 024) keeping one: 808 091 648 texels, past the
/// former 768 Mi and within the limit. Committed at those charges, a result
/// candidate at an atlas's cap, more than the measured result skin, is
/// admitted beside them.
#[test]
fn the_measured_set_with_a_chart_layer_fits() {
    const PLAY: u64 = 178_421_760;
    const SELECTION: u64 = SELECTION_ATLAS + SELECTION_FIRM;
    assert_eq!(SELECTION, 201_719_808);
    const DECISION: u64 = 142_737_408;
    const RESULT: u64 = 234_881_024;
    const SET: u64 = PLAY + SELECTION + DECISION + RESULT + 3 * VM_TEXELS;
    assert_eq!(SET, 808_091_648);
    const { assert!(SET > 768 << 20) };
    const { assert!(SET <= SHARED_TEXELS) };
    let mut r = offscreen(1_920, 1_080);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    r.state_charge(&p, PLAY);
    let sel = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &sel, 2);
    r.state_charge(&sel, SELECTION + VM_TEXELS);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 3);
    r.state_charge(&d, DECISION + VM_TEXELS);
    let result = Arc::new(
        Arc::try_unwrap(atlas_cap_bundle(&r, GREEN))
            .unwrap()
            .keeping_vm(),
    );
    const { assert!(RESULT < CAP) };
    r.admit(ScreenKind::Result, &result).unwrap();
}

/// The measured set once the play skin's title card is charged beside its
/// large closing sheet: the play skin with its chart layer, its title
/// card's fonts and a 6400×7941 sheet chosen (249 790 464 texels), the
/// largest selection (its atlas and its pool's firm layer, 201 719 808) and
/// decision (142 737 408) skins, each keeping a VM, and the largest result
/// skin (234 881 024) keeping one: 879 460 352 texels, past the former
/// 832 Mi and within the limit. Committed at those charges, a result
/// candidate at an atlas's cap, more than the measured result skin, is
/// admitted beside them, as the result skin's installation is again after
/// play's commit.
#[test]
fn the_measured_set_with_a_large_closing_sheet_fits() {
    const PLAY: u64 = 249_790_464;
    const SELECTION: u64 = SELECTION_ATLAS + SELECTION_FIRM;
    const DECISION: u64 = 142_737_408;
    const RESULT: u64 = 234_881_024;
    const SET: u64 = PLAY + SELECTION + DECISION + RESULT + 3 * VM_TEXELS;
    assert_eq!(SET, 879_460_352);
    const { assert!(SET > 832 << 20) };
    const { assert!(RESULT < CAP) };
    let mut r = offscreen(1_920, 1_080);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    r.state_charge(&p, PLAY);
    let sel = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &sel, 2);
    r.state_charge(&sel, SELECTION + VM_TEXELS);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 3);
    r.state_charge(&d, DECISION + VM_TEXELS);
    let result = Arc::new(
        Arc::try_unwrap(atlas_cap_bundle(&r, GREEN))
            .unwrap()
            .keeping_vm(),
    );
    let needed: u64 = r.admission().committed.iter().sum::<u64>() + CAP + VM_TEXELS;
    assert_eq!(needed, 913_014_784);
    if let Err(e) = r.admit(ScreenKind::Result, &result) {
        panic!("the measured set is refused: {e:?}");
    }
}

/// A candidate replaces its kind's committed installation: its charge is
/// left out, so big fits as the selection screen's beside a committed
/// selection skin charged the whole limit, but not as the decision
/// screen's.
#[test]
fn a_candidate_replaces_its_kinds_charge() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &b, 1);
    r.state_charge(&b, SHARED_TEXELS);
    r.admit(ScreenKind::Selection, &big).unwrap();
    let e = r.admit(ScreenKind::Decision, &big).unwrap_err();
    refused(&e, ScreenKind::Decision, SHARED_TEXELS + CAP);
}

/// A bundle keeping a VM is charged [`VM_TEXELS`] beside its atlas, a VM's
/// memory cap at 4 bytes a texel: with a selection skin charged what the
/// limit leaves beside big and tiny's atlas, big as play's would be
/// exactly at the limit beside a tiny result skin, and the result skin's
/// VM puts it past.
#[test]
fn a_kept_vm_is_charged() {
    assert_eq!(
        VM_TEXELS * 4,
        u64::try_from(skin_beatoraja::lua::limits::MAX_LUA_MEMORY).unwrap()
    );
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let s = tiny(&r, RED);
    commit(&mut r, ScreenKind::Selection, &s, 1);
    r.state_charge(&s, SHARED_TEXELS - CAP - TINY);
    let vm = Arc::new(Arc::try_unwrap(tiny(&r, GREEN)).unwrap().keeping_vm());
    assert!(vm.keeps_vm());
    commit(&mut r, ScreenKind::Result, &vm, 2);
    assert_eq!(r.admission().charge(ScreenKind::Result), TINY + VM_TEXELS);
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + VM_TEXELS);
}

/// A staging refused for admission drops the staging before it, whose
/// token no longer commits; every committed installation stays.
#[test]
fn a_refused_admission_drops_the_staging_before() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    r.state_charge(&red, SHARED_TEXELS - CAP);
    let green = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &green, 2);
    let staged = r
        .install(ScreenKind::Decision, &tiny(&r, GREEN), 3)
        .unwrap();

    let e = r.install(ScreenKind::Selection, &big, 4).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
    assert!(r.staged().is_none());
    assert_eq!(r.commit(staged), Err(render::CommitError::NothingStaged));
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Result).unwrap(),
        &green
    ));
    assert!(r.installed(ScreenKind::Decision).is_none());
    assert!(r.installed(ScreenKind::Selection).is_none());
}

/// A removed installation gives its charge back once its retirement
/// entry is destroyed: big, refused as play's beside a selection skin
/// charged what the limit leaves beside big and tiny's atlas more, is
/// still refused while the removed installation's entry stands (`admit`
/// counts it as it stands), then fits.
/// Play's installation is never removed this way.
#[test]
fn a_removed_installation_gives_its_charge_back() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &b, 2);
    r.state_charge(&b, SHARED_TEXELS - CAP + TINY);
    r.remove(ScreenKind::Play);
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    // Big as play's replaces red; the selection's skin is charged.
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);

    r.remove(ScreenKind::Selection);
    assert!(r.installed(ScreenKind::Selection).is_none());
    let a = r.admission();
    assert_eq!(a.charge(ScreenKind::Selection), 0);
    assert_eq!(a.filled, SHARED_TEXELS - CAP + TINY, "its entry charged");
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);
    settle(&mut r);
    assert_eq!(r.admission().filled, 0);
    r.admit(ScreenKind::Play, &big).unwrap();
}

/// A candidate's text layer is charged with its pages: two 8 192-texel
/// pages, the reserved layer and the text layer are an atlas's cap. With
/// two other kinds each keeping a VM committed, and the candidate keeping
/// one too, 48 Mi texels and two tiny atlases are charged beside it, and
/// play's skin is charged what leaves the candidate exactly one layer
/// short: with its text layer it passes the limit and is refused, before
/// anything is staged; without the layer it would fit, exactly at the
/// limit.
#[test]
fn a_candidate_s_text_layer_is_charged() {
    const PAGE: u64 = 8_192 * 8_192;
    let mut r = offscreen(1_920, 1_080);
    let t = tiny(&r, RED);
    play(&mut r, &t, 1);
    let others = 2 * (TINY + VM_TEXELS);
    let charge = CAP + VM_TEXELS;
    r.state_charge(&t, SHARED_TEXELS - others - (charge - PAGE));
    let vm = |r: &Renderer| Arc::new(Arc::try_unwrap(tiny(r, GREEN)).unwrap().keeping_vm());
    let (d, re) = (vm(&r), vm(&r));
    commit(&mut r, ScreenKind::Decision, &d, 2);
    commit(&mut r, ScreenKind::Result, &re, 3);
    let candidate = atlas_cap_bundle(&r, RED);
    assert_eq!((candidate.side(), candidate.text_layer()), (8_192, Some(3)));
    let candidate = Arc::new(Arc::try_unwrap(candidate).unwrap().keeping_vm());
    let needed = SHARED_TEXELS + PAGE;
    let e = r.admit(ScreenKind::Selection, &candidate).unwrap_err();
    refused(&e, ScreenKind::Selection, needed);
    let e = r.install(ScreenKind::Selection, &candidate, 4).unwrap_err();
    refused(&e, ScreenKind::Selection, needed);
    assert!(r.staged().is_none());
    assert_eq!(candidate.atlas_texels(), CAP);
    assert_eq!(CAP, 4 * PAGE);
}

/// The chart-image pool's optional layers yield to later candidates in
/// startup's order (play, selection, decision, result; AC-S9): with the
/// selection installed and its pool whole, a decision candidate that fits
/// only with the pool reduced is admitted, the live array retired at once
/// and the layers left owed, made as a new generation once the retired
/// one is destroyed, then a result candidate likewise, the pool down to
/// its firm layer. No candidate is refused and no result released for the
/// pool, and the shared total holds.
#[test]
fn the_pool_yields_to_later_candidates_in_startup_order() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let whole = r.pool().unwrap();
    assert_eq!(whole.layers, 4);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 3 * LAYER));
    let sel = a.charge(ScreenKind::Selection);
    assert_eq!(sel, s.atlas_texels() + LAYER);
    // Play's skin charged what leaves one optional layer beside a tiny
    // decision skin.
    r.state_charge(&p, SHARED_TEXELS - sel - TINY - LAYER);

    let b = tiny(&r, BLUE);

    commit(&mut r, ScreenKind::Decision, &b, 3);
    assert_eq!(r.pool(), None, "retired at the reduction");
    assert!(r.pool_owed());
    assert_eq!(r.admission().total(), SHARED_TEXELS);
    settle(&mut r);
    assert!(r.make_owed_pool());
    let reduced = r.pool().unwrap();
    assert_eq!(reduced.layers, 2);
    assert!(reduced.generation > whole.generation);
    assert_eq!(r.admission().total(), SHARED_TEXELS);

    let b = tiny(&r, GREEN);

    commit(&mut r, ScreenKind::Result, &b, 4);
    assert_eq!(r.pool(), None);
    settle(&mut r);
    assert!(r.make_owed_pool());
    let firm = r.pool().unwrap();
    assert_eq!(firm.layers, 1);
    assert!(firm.generation > reduced.generation);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 0));
    assert_eq!(a.total(), SHARED_TEXELS - LAYER + TINY);
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
    // The firm layer is the selection's own: a candidate past what the
    // optional layers gave is refused, counting it.
    let big = big(&r);
    let e = r.admit(ScreenKind::Decision, &big).unwrap_err();
    refused(&e, ScreenKind::Decision, a.committed_total() - TINY + CAP);
}

/// A larger play specialization staged in Preparing reduces the pool
/// first, its smaller array owed and made as a new generation once the
/// retired one is destroyed, and keeps the result installed (AC-S9); only
/// one that does not fit with the pool at its firm layer releases the
/// result, the pool reduced before. Each commit's replaced play skin is
/// charged until its entry's destruction.
#[test]
fn a_larger_play_specialization_reduces_the_pool_before_the_result() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let whole = r.pool().unwrap();
    let result = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &result, 3);
    // The selection charged what leaves one optional layer beside a tiny
    // play candidate and the result.
    r.state_charge(&s, SHARED_TEXELS - LAYER - 2 * TINY - LAYER);
    let b = tiny(&r, BLUE);
    play(&mut r, &b, 4);
    assert_eq!(r.pool(), None);
    assert!(!r.make_owed_pool(), "the retired pool not destroyed yet");
    // Red's replaced skin, charged until its entry is destroyed.
    assert_eq!(r.admission().total(), SHARED_TEXELS + TINY);
    settle(&mut r);
    assert!(r.make_owed_pool());
    let reduced = r.pool().unwrap();
    assert_eq!(reduced.layers, 2);
    assert!(reduced.generation > whole.generation);
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
    assert_eq!(r.admission().total(), SHARED_TEXELS);

    // One past even the firm layer alone: the pool goes to its firm layer,
    // then the result is released.
    r.state_charge(&s, SHARED_TEXELS - LAYER - TINY);
    let b = tiny(&r, RED);
    play(&mut r, &b, 5);
    assert_eq!(r.pool(), None);
    settle(&mut r);
    assert!(r.make_owed_pool());
    let firm = r.pool().unwrap();
    assert_eq!(firm.layers, 1);
    assert!(firm.generation > reduced.generation);
    assert!(r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_none());
    assert_eq!(r.admission().total(), SHARED_TEXELS);
}

/// A renderer built again restores the kinds in their order and the pool
/// last (AC-S9): the selection's restore makes the pool's firm layer, a
/// new generation, the later kinds are restored beside it, and the
/// regrowth after them sizes the pool to what they leave.
#[test]
fn a_recovery_restores_the_kinds_first_and_the_pool_last() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let b = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &b, 3);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &b, 4);
    let old = r.pool().unwrap();
    assert_eq!(old.layers, 4);
    let installations: Vec<_> = [
        ScreenKind::Play,
        ScreenKind::Selection,
        ScreenKind::Decision,
        ScreenKind::Result,
    ]
    .map(|k| (k, r.installation(k).unwrap()))
    .into();

    let mut again = offscreen(1_920, 1_080);
    for (kind, installation) in &installations {
        again.restore(*kind, installation).unwrap();
        if *kind == ScreenKind::Play {
            // Play's skin charged what leaves one optional layer beside
            // the other kinds.
            let others = s.atlas_texels() + LAYER + 2 * TINY;
            again.state_charge(installation.bundle(), SHARED_TEXELS - others - LAYER);
        }
        if *kind == ScreenKind::Selection {
            let firm = again.pool().unwrap();
            assert_eq!(firm.layers, 1);
            assert!(firm.generation > old.generation);
        }
    }
    let firm = again.pool().unwrap();
    assert_eq!(firm.layers, 1, "the kinds restored beside the firm layer");
    let grown = again.regrow_pool().unwrap();
    assert_eq!(grown.layers, 2);
    assert!(grown.generation > firm.generation);
    assert_eq!(again.admission().total(), SHARED_TEXELS);
}

/// The destructions recorded since `from` of them were.
fn recorded_since(r: &Renderer, from: usize) -> Vec<render::DestroyRecord> {
    let all: Vec<_> = r.destructions().copied().collect();
    let n = r.destructions_recorded() - from;
    all[all.len() - n..].to_vec()
}

/// Stages `b` with an attempt as play's for `request`.
fn stage_play(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    request: u64,
) -> Result<render::PreparedToken, RenderError> {
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    r.stage(b, &attempt(r, &chart, &ctx), request)
}

/// V7 at the renderer: a staging superseded after its upload's submission
/// stays charged in its retirement entry, beside the newer candidate,
/// until the entry's destruction, its completion seen or not; a staging
/// of the committed bundle has nothing of its own and is counted once.
// Revert: `Retirements::charged` leaving out the filled entries -> the
// superseded candidate's entry is not counted: "the superseded candidate".
#[test]
fn a_superseded_staging_stays_charged_until_its_destruction() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let s = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &s, 1);
    r.state_charge(&s, SHARED_TEXELS - CAP);
    r.admit(ScreenKind::Play, &big).unwrap();
    let d1 = tiny(&r, RED);
    let _ = r.install(ScreenKind::Decision, &d1, 2).unwrap();
    let d2 = tiny(&r, BLUE);
    let _ = r.install(ScreenKind::Decision, &d2, 3).unwrap();
    let a = r.admission();
    assert_eq!(a.candidate, Some((ScreenKind::Decision, TINY)));
    assert_eq!(a.filled, TINY, "the superseded candidate");
    // The superseded candidate's entry and the staged decision, as its
    // commit would leave it (H4), both counted.
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + 2 * TINY);
    // Its completion seen: still charged until it is destroyed.
    let began = std::time::Instant::now();
    while r.pending_completion() {
        assert!(began.elapsed().as_secs() < 10, "no completion seen");
        r.poll_completion().unwrap();
    }
    assert_eq!(r.admission().filled, TINY);
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + 2 * TINY);
    let d = r.destroy_due(true).heavy.unwrap();
    assert_eq!(d.id, Some(d1.atlas_id()));
    assert_eq!(r.admission().filled, 0);
    // Destroyed: the staged decision alone is past the limit.
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);
    // The committed bundle staged again: no entry of its own, nothing
    // filled after its discard, its charge counted once.
    r.discard();
    settle(&mut r);
    commit(&mut r, ScreenKind::Decision, &d1, 4);
    let committed = r.admission();
    let _ = r.install(ScreenKind::Decision, &d1, 5).unwrap();
    let a = r.admission();
    assert_eq!(a.committed, committed.committed);
    assert_eq!((a.filled, a.total()), (0, committed.total()));
    r.discard();
    assert_eq!(r.filled(), 0);
}

/// V7 for play: a play staging that fits only once a filled entry is
/// destroyed destroys it in its own call, where destruction is allowed,
/// and keeps the result; where it is not, it is refused counting the
/// entry, the result kept; one that does not fit even without the result
/// is refused, the result kept.
// Revert: `reclaim` without its `Destroy` step -> the play staging that
// fits once the removed decision's entry is destroyed releases the result
// instead.
#[test]
fn play_fits_once_a_filled_entry_is_destroyed_in_its_call() {
    let mut r = offscreen(1_920, 1_080);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let res = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &res, 2);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 3);
    settle(&mut r);
    // The decision removed, its entry charged what puts a tiny play
    // candidate one atlas past the limit.
    r.state_charge(&d, SHARED_TEXELS - TINY);
    r.remove(ScreenKind::Decision);
    assert_eq!(r.admission().filled, SHARED_TEXELS - TINY);
    let b = tiny(&r, BLUE);
    r.set_destroy_allowed(false);
    let e = stage_play(&mut r, &b, 4).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);
    assert_eq!(r.filled(), 1, "destroyed where it is not allowed");
    assert!(r.installed(ScreenKind::Result).is_some());
    r.set_destroy_allowed(true);
    let from = r.destructions_recorded();
    play(&mut r, &b, 5);
    let got: Vec<_> = recorded_since(&r, from)
        .iter()
        .map(|x| (x.kind, x.id, x.in_call, x.last_use <= x.seen))
        .collect();
    assert_eq!(
        got,
        [(ScreenKind::Decision, Some(d.atlas_id()), true, true)]
    );
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
    // One past the limit even without the result: refused, the result
    // kept.
    settle(&mut r);
    let s = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &s, 6);
    r.state_charge(&s, SHARED_TEXELS);
    let c = tiny(&r, RED);
    let e = stage_play(&mut r, &c, 7).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + 2 * TINY);
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
}

/// V7b's order at the renderer: a candidate takes the filled entries'
/// charges first, destroyed in its call, the pool kept whole; then the
/// pool's optional layers, its array owed; and the result's release only
/// for play, the pool down to its firm layer first.
// Revert: `reclaim` reducing the pool before destroying filled entries
// -> the decision candidate's call reduces the pool instead, and an
// entry stays filled: "the entry destroyed".
#[test]
fn filled_entries_go_before_the_pool_and_the_pool_before_the_result() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let res = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &res, 3);
    settle(&mut r);
    let sel = r.admission().charge(ScreenKind::Selection);
    // The result removed, its entry charged two layers; play charged so
    // that a tiny decision candidate fits with the whole pool beside it,
    // but not beside the entry.
    r.state_charge(&res, 2 * LAYER);
    r.remove(ScreenKind::Result);
    r.state_charge(&p, SHARED_TEXELS - sel - TINY - 3 * LAYER);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 4);
    assert_eq!(r.filled(), 0, "the entry destroyed");
    assert_eq!(r.pool().unwrap().layers, 4, "the pool reduced");
    assert!(!r.pool_owed());
    // A result candidate beside them: the pool's optional layers yield.
    let res2 = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &res2, 5);
    assert_eq!(r.pool(), None);
    assert!(r.pool_owed());
    assert_eq!(r.admission().pool, 2 * LAYER, "three layers owed");
    assert!(!r.take_result_release());
    // Play past even the firm layer alone: the pool to its firm layer,
    // then the result released.
    r.state_charge(&s, SHARED_TEXELS - 2 * TINY - LAYER);
    let b = tiny(&r, BLUE);
    play(&mut r, &b, 6);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 0));
    assert!(r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_none());
}

/// A full list: twelve entries, play's standing one, ten candidates
/// superseded and the newest staging's own. The next staging destroys the
/// oldest filled entry in its own call where destruction is allowed, and
/// is refused where it is not, its request still open and the newest
/// staging kept.
// Revert: `reserve_room` refusing whenever no entry is free -> the
// twelfth staging is refused with destruction allowed.
#[test]
fn a_full_list_destroys_its_oldest_entry_in_the_staging_call() {
    let mut r = offscreen(1_920, 1_080);
    let mut newest = None;
    for k in 1..=11_u8 {
        let b = tiny(&r, [k * 20, 0, 0, 255]);
        newest = Some(r.install(ScreenKind::Decision, &b, u64::from(k)).unwrap());
    }
    assert_eq!(r.filled(), 10);
    let b = tiny(&r, GREEN);
    r.set_destroy_allowed(false);
    let e = r.install(ScreenKind::Decision, &b, 12).unwrap_err();
    assert!(
        matches!(
            e,
            RenderError::ListFull {
                kind: ScreenKind::Decision
            }
        ),
        "{e:?}"
    );
    assert!(e.is_refusal());
    assert_eq!(r.staged(), newest, "the newest staging dropped");
    assert_eq!(r.filled(), 10);
    r.set_destroy_allowed(true);
    let from = r.destructions_recorded();
    let token = r.install(ScreenKind::Decision, &b, 12).unwrap();
    let got: Vec<_> = recorded_since(&r, from)
        .iter()
        .map(|x| (x.in_call, x.last_use <= x.seen))
        .collect();
    assert_eq!(got, [(true, true)]);
    assert_eq!(r.filled(), 10, "the oldest destroyed, the newest filled");
    assert_eq!(r.staged(), Some(token));
}

/// V18: a removal always fills its installation's standing entry, with
/// every other entry filled or reserved and no destruction allowed: the
/// removed array is never dropped inline, and stays named and charged
/// until a boundary destroys it.
// Revert: `retire_standing` dropping inline (`retire_into(None, f)`) ->
// "the removal dropped inline".
#[test]
fn a_removal_fills_its_standing_entry_with_the_list_full() {
    let mut r = offscreen(1_920, 1_080);
    let sel = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &sel, 1);
    r.set_destroy_allowed(false);
    let mut request = 2_u64;
    loop {
        let b = tiny(&r, [u8::try_from(request * 9).unwrap(), 0, 0, 255]);
        match r.install(ScreenKind::Decision, &b, request) {
            Ok(_) => request += 1,
            Err(e) => {
                assert!(matches!(e, RenderError::ListFull { .. }), "{e:?}");
                break;
            }
        }
        assert!(request < 20, "the list never filled");
    }
    let filled = r.filled();
    let from = r.destructions_recorded();
    r.remove(ScreenKind::Selection);
    assert_eq!(r.filled(), filled + 1, "the removal dropped inline");
    assert_eq!(
        r.destructions_recorded(),
        from,
        "the removal dropped inline"
    );
    assert!(
        r.retained_atlases(ScreenKind::Selection)
            .contains(&Some(sel.atlas_id())),
        "the removed array not named"
    );
    r.set_destroy_allowed(true);
    settle(&mut r);
    assert!(
        !r.retained_atlases(ScreenKind::Selection)
            .contains(&Some(sel.atlas_id()))
    );
}

/// A result view's staging in steps destroys nothing in its call: with a
/// live pool beside the smaller one a regrowth retired, a result candidate
/// that fits only once the live pool is reduced needs the retired one
/// destroyed first, so it has no room, and taken anyway it is refused with
/// nothing destroyed; once a boundary has destroyed the retired pool it has
/// room, and its staging retires the live pool, destroying nothing in its
/// call either.
// Revert: `install_stepped` letting a result staging destroy in its call
// -> the staging taken without room destroys the retired pool inline:
// "taken without room".
#[test]
fn a_result_view_staging_destroys_nothing_in_its_call() {
    let mut r = offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let res = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &res, 3);
    settle(&mut r);
    assert_eq!(r.pool().unwrap().layers, 4);
    let sel = r.admission().charge(ScreenKind::Selection);
    // A decision candidate reduces the pool to two layers, made once the
    // retired one is destroyed.
    r.state_charge(&p, SHARED_TEXELS - sel - 2 * TINY - LAYER);
    let d = tiny(&r, BLUE);
    commit(&mut r, ScreenKind::Decision, &d, 4);
    settle(&mut r);
    assert!(r.make_owed_pool());
    assert_eq!(r.pool().unwrap().layers, 2);
    // Room for the whole pool again: the regrowth's live pool of four
    // layers beside the two-layer one it retired, left standing.
    r.state_charge(&p, SHARED_TEXELS - sel - 2 * TINY - 3 * LAYER);
    assert_eq!(r.regrow_pool().unwrap().layers, 4);
    assert_eq!(r.admission().retired_pool, 2 * LAYER);
    // A result candidate that fits once the live pool keeps three layers.
    let b = tiny(&r, GREEN);
    r.state_candidate_charge(&b, TINY + LAYER);
    assert!(
        !r.install_room(ScreenKind::Result, &b),
        "room while the retired pool stands"
    );
    let from = r.destructions_recorded();
    let e = r
        .install_stepped(ScreenKind::Result, &b, 5)
        .expect_err("taken without room");
    assert!(matches!(e, RenderError::ListFull { .. }), "{e:?}");
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    assert_eq!(r.admission().retired_pool, 2 * LAYER);
    assert_eq!(r.pool().unwrap().layers, 4);
    // A boundary destroys the retired pool: room, and the staging retires
    // the live pool with nothing destroyed in its call.
    settle(&mut r);
    assert!(r.install_room(ScreenKind::Result, &b));
    let from = r.destructions_recorded();
    let _ = r.install_stepped(ScreenKind::Result, &b, 6).unwrap();
    assert_eq!(
        recorded_since(&r, from),
        Vec::new(),
        "destroyed in its call"
    );
    assert_eq!(r.pool(), None, "the live pool retired");
    assert!(r.pool_owed());
    assert_eq!(r.admission().retired_pool, 4 * LAYER);
    r.discard();
}
