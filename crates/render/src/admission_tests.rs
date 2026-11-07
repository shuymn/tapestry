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
//! "Big" is one generated cell, no file read, whose atlas array is exactly
//! an atlas's cap, built once for the whole binary and never uploaded.
//! "Tiny" is a 16×16 cell on one 256-texel page. A sum past the limit is
//! shown past it by the smallest atlas, tiny's, or by a kept VM: charges
//! are whole layers, so no candidate is one texel past.

use std::sync::{Arc, OnceLock};

use bms_model::ChartBuilder;
use render::{RenderError, Renderer, SHARED_TEXELS, SkinBundle, VM_TEXELS};
use skin_model::{AtlasLimits, MAX_ATLAS_TEXELS, ScreenKind};

use crate::test_common::{GAUGES, attempt, context, offscreen};
use crate::test_decl::{Look, atlas_cap_bundle, solid_bundle};

/// An atlas's cap: half the shared limit.
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

/// The shared limit is 384 Mi, two atlases at the cap: big as the
/// selection screen's candidate fits beside play's skin charged the cap,
/// exactly at the limit; once a tiny result skin is committed too, the
/// sum is past the limit by tiny's atlas, and big is refused.
#[test]
fn two_skins_at_the_atlas_cap_fit_together() {
    assert_eq!(SHARED_TEXELS, 384 << 20);
    assert_eq!(SHARED_TEXELS, 2 * CAP);
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let p = tiny(&r, RED);
    play(&mut r, &p, 1);
    assert_eq!(r.admission().charge(ScreenKind::Play), TINY);
    r.state_charge(&p, CAP);
    assert_eq!(r.admission().charge(ScreenKind::Play), CAP);
    r.admit(ScreenKind::Selection, &big).unwrap();
    // One texel more is past the limit.
    r.state_charge(&p, CAP + 1);
    let e = r.admit(ScreenKind::Selection, &big).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + 1);
    r.state_charge(&p, CAP);

    let result = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &result, 2);
    let e = r.admit(ScreenKind::Selection, &big).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
    let e = r.install(ScreenKind::Selection, &big, 3).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
    assert!(r.staged().is_none());
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
/// memory cap at 4 bytes a texel: with a selection skin charged the cap
/// less tiny's atlas, big as play's would be exactly at the limit beside a
/// tiny result skin, and the result skin's VM puts it past.
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
    r.state_charge(&s, CAP - TINY);
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
    r.state_charge(&red, CAP);
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

/// A removed installation gives its charge back: big, refused as play's
/// beside a selection skin charged the cap and tiny's atlas, then fits.
/// Play's installation is never removed this way.
#[test]
fn a_removed_installation_gives_its_charge_back() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &b, 2);
    r.state_charge(&b, CAP + TINY);
    r.remove(ScreenKind::Play);
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    // Big as play's replaces red; the selection's skin is charged.
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);

    r.remove(ScreenKind::Selection);
    assert!(r.installed(ScreenKind::Selection).is_none());
    assert_eq!(r.admission().charge(ScreenKind::Selection), 0);
    r.admit(ScreenKind::Play, &big).unwrap();
}

/// A candidate's text layer is charged with its pages: one 8 192-texel
/// page, the reserved layer and the text layer are an atlas's cap. With
/// play's skin charged the cap and two other kinds each keeping a VM
/// committed, and the candidate keeping one too, 48 Mi texels and two tiny
/// atlases are charged beside them: with its text layer it passes the
/// limit and is refused, before anything is staged; without the layer
/// it would fit.
#[test]
fn a_candidate_s_text_layer_is_charged() {
    const PAGE: u64 = 8_192 * 8_192;
    let mut r = offscreen(1_920, 1_080);
    let t = tiny(&r, RED);
    play(&mut r, &t, 1);
    r.state_charge(&t, CAP);
    let vm = |r: &Renderer| Arc::new(Arc::try_unwrap(tiny(r, GREEN)).unwrap().keeping_vm());
    let (d, re) = (vm(&r), vm(&r));
    commit(&mut r, ScreenKind::Decision, &d, 2);
    commit(&mut r, ScreenKind::Result, &re, 3);
    let candidate = atlas_cap_bundle(&r, RED);
    assert_eq!((candidate.side(), candidate.text_layer()), (8_192, Some(2)));
    let candidate = Arc::new(Arc::try_unwrap(candidate).unwrap().keeping_vm());
    let committed = CAP + 2 * (TINY + VM_TEXELS);
    let needed = committed + 3 * PAGE + VM_TEXELS;
    assert!(needed > SHARED_TEXELS && needed - PAGE <= SHARED_TEXELS);
    let e = r.admit(ScreenKind::Selection, &candidate).unwrap_err();
    refused(&e, ScreenKind::Selection, needed);
    let e = r.install(ScreenKind::Selection, &candidate, 4).unwrap_err();
    refused(&e, ScreenKind::Selection, needed);
    assert!(r.staged().is_none());
    assert_eq!(candidate.atlas_texels(), 3 * PAGE);
}
