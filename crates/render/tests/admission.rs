//! Each screen kind's committed skin on WARP, and the texels every kind's
//! committed skin shares with a candidate (design.md "Memory Budget"),
//! through the renderer's own admission: a candidate replaces its kind's
//! charge, a kept VM is charged beside its atlas, a refusal comes before
//! anything is staged, another kind's commit leaves play's installation
//! drawing as it was, one candidate of any kind is staged at a time, and a
//! removed installation gives its charge back.
//!
//! "Big" is one generated cell, no file read, whose atlas array is exactly
//! the shared limit, built once for the whole binary; it is never
//! uploaded: every case with it is refused before allocation or checked
//! through `Renderer::admit`. "Tiny" is a 16×16 cell on one 256-texel
//! page.

mod common;
mod decl_support;

use std::sync::{Arc, OnceLock};

use bms_model::{ChartBuilder, TimeUs};
use common::{GAUGES, attempt, context, input, offscreen, view};
use decl_support::{Look, solid_bundle};
use render::{
    CommitError, PreparedToken, RenderError, Renderer, SHARED_TEXELS, Scissor, SkinBundle,
    VM_TEXELS,
};
use skin_model::{AtlasLimits, ClipId, ScreenKind};

/// Big's cell: past a 4 096-texel page with its gutter, so its one page
/// is 8 192 texels a side, beside the reserved layer.
const BIG: [u32; 2] = [4_096, 3_072];
/// Tiny's atlas array: one 256-texel page beside the reserved layer.
const TINY: u64 = 256 * 256 * 2;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// The left half's centre, where every fixture draws.
const LEFT: (u32, u32) = (480, 540);

/// Big: its atlas array is exactly [`SHARED_TEXELS`]. Built once for
/// every test of this binary, on the first test's device (every test
/// runs on WARP, whose atlas limits are the same), so its CPU pages are
/// held once however many tests run at a time; no test commits it.
fn big(r: &Renderer) -> Arc<SkinBundle> {
    static BIG_BUNDLE: OnceLock<(AtlasLimits, Arc<SkinBundle>)> = OnceLock::new();
    let (limits, b) = BIG_BUNDLE.get_or_init(|| {
        let b = solid_bundle(r, &Look::left([0, 0, 255, 255]), BIG);
        (r.atlas_limits(), b)
    });
    assert_eq!(
        *limits,
        r.atlas_limits(),
        "built for another device's limits"
    );
    assert_eq!(b.atlas_texels(), SHARED_TEXELS);
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

/// A play frame: its sprites and the left half's colour.
fn frame(r: &mut Renderer) -> (usize, [u8; 3]) {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    let pixels = common::read_back(r);
    (timing.sprites, common::rgb_at(&pixels, 1_920, LEFT))
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

/// A candidate replaces its kind's committed installation: its charge is
/// left out, so big fits as the selection screen's beside play's built-in
/// skin (charged nothing) even with a tiny selection skin committed; once
/// play commits a tiny skin, its charge is counted and big is refused.
#[test]
fn a_candidate_replaces_its_kinds_charge() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &b, 1);
    r.admit(ScreenKind::Selection, &big).unwrap();

    let b = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &b, 2);
    let e = r.admit(ScreenKind::Selection, &big).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + TINY);
}

/// A bundle keeping a VM is charged [`VM_TEXELS`] beside its atlas, a VM's
/// memory cap at 4 bytes a texel.
#[test]
fn a_kept_vm_is_charged() {
    assert_eq!(
        VM_TEXELS * 4,
        u64::try_from(skin_beatoraja::lua::limits::MAX_LUA_MEMORY).unwrap()
    );
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let vm = Arc::new(Arc::try_unwrap(tiny(&r, GREEN)).unwrap().keeping_vm());
    assert!(vm.keeps_vm());
    commit(&mut r, ScreenKind::Result, &vm, 1);
    assert_eq!(r.admission().charge(ScreenKind::Result), TINY + VM_TEXELS);
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY + VM_TEXELS);
}

/// Another kind's commit leaves play's installation exactly as it was:
/// its bundle, atlas, resources, sprites and colour.
#[test]
fn a_selection_commit_leaves_play_unchanged() {
    let mut r = offscreen(1_920, 1_080);
    let red = tiny(&r, RED);
    play(&mut r, &red, 1);
    let before = frame(&mut r);
    assert!(before.0 > 0);
    assert_eq!(before.1, [255, 0, 0]);
    let (atlas, generation) = (r.atlas_size(), r.resource_generation());

    let green = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &green, 2);
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Selection).unwrap(),
        &green
    ));
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    assert_eq!(
        (r.atlas_size(), r.resource_generation()),
        (atlas, generation)
    );
    assert_eq!(frame(&mut r), before);
    assert_eq!(
        r.admission().committed,
        [TINY, TINY, 0, 0],
        "play's and the selection's"
    );
}

/// A commit is refused, the staging kept, for a token copied with only
/// its kind changed; the staging's own token installs it.
#[test]
fn a_commit_checks_the_kind() {
    let mut r = offscreen(1_920, 1_080);
    let green = tiny(&r, GREEN);
    let token = r.install(ScreenKind::Selection, &green, 1).unwrap();
    assert_eq!(token.kind, ScreenKind::Selection);
    let other = PreparedToken {
        kind: ScreenKind::Play,
        ..token
    };
    assert_eq!(
        r.commit(other),
        Err(CommitError::Kind {
            staged: ScreenKind::Selection,
            asked: ScreenKind::Play
        })
    );
    assert_eq!(r.staged(), Some(token));
    assert!(r.installed(ScreenKind::Play).is_none());
    r.commit(token).unwrap();
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Selection).unwrap(),
        &green
    ));
    assert!(r.installed(ScreenKind::Play).is_none());
}

/// One candidate of any kind is staged at a time: a staging of another
/// kind drops the one before, whose token no longer commits, and only the
/// newer's atlas is held.
#[test]
fn one_candidate_of_any_kind_is_staged() {
    let mut r = offscreen(1_920, 1_080);
    let small = tiny(&r, GREEN);
    let larger = solid_bundle(&r, &Look::left(RED), [300, 300]);
    let alone = r.install(ScreenKind::Decision, &larger, 1).unwrap();
    let held = r.memory().staged_atlas;
    assert!(held > 0);

    let older = r.install(ScreenKind::Selection, &small, 2).unwrap();
    assert!(r.memory().staged_atlas < held);
    let newer = r.install(ScreenKind::Decision, &larger, 3).unwrap();
    assert_eq!(r.staged(), Some(newer));
    assert_eq!(r.memory().staged_atlas, held, "the newer's alone");
    assert_eq!(
        r.admission().candidate,
        Some((ScreenKind::Decision, larger.atlas_texels()))
    );
    for token in [alone, older] {
        assert!(matches!(
            r.commit(token),
            Err(CommitError::Request { staged: 3, .. })
        ));
    }
    r.commit(newer).unwrap();
    assert!(r.installed(ScreenKind::Selection).is_none());
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Decision).unwrap(),
        &larger
    ));
}

/// A staging refused for admission drops the staging before it, whose
/// token no longer commits; every committed installation stays.
#[test]
fn a_refused_admission_drops_the_staging_before() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    let green = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Result, &green, 2);
    let staged = r
        .install(ScreenKind::Decision, &tiny(&r, GREEN), 3)
        .unwrap();

    let e = r.install(ScreenKind::Selection, &big, 4).unwrap_err();
    refused(&e, ScreenKind::Selection, SHARED_TEXELS + 2 * TINY);
    assert!(r.staged().is_none());
    assert_eq!(r.commit(staged), Err(CommitError::NothingStaged));
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Result).unwrap(),
        &green
    ));
    assert!(r.installed(ScreenKind::Decision).is_none());
    assert!(r.installed(ScreenKind::Selection).is_none());
}

/// A removed installation gives its charge back: big, refused as play's
/// beside a tiny selection skin, then fits. Play's installation is never
/// removed this way.
#[test]
fn a_removed_installation_gives_its_charge_back() {
    let mut r = offscreen(1_920, 1_080);
    let big = big(&r);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    let b = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &b, 2);
    r.remove(ScreenKind::Play);
    assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &red));
    // Big as play's replaces red; the selection's tiny skin is charged.
    let e = r.admit(ScreenKind::Play, &big).unwrap_err();
    refused(&e, ScreenKind::Play, SHARED_TEXELS + TINY);

    r.remove(ScreenKind::Selection);
    assert!(r.installed(ScreenKind::Selection).is_none());
    assert_eq!(r.admission().charge(ScreenKind::Selection), 0);
    r.admit(ScreenKind::Play, &big).unwrap();
}

/// Another kind's staging is projected at its commit onto the target as
/// it is then: staged at 1 920×1 080 and committed after a resize to
/// 960×540, its slot's viewport and clip scissor are the new size's.
#[test]
fn a_slot_is_projected_at_its_commit() {
    let mut r = offscreen(1_920, 1_080);
    let green = tiny(&r, GREEN);
    let token = r.install(ScreenKind::Selection, &green, 1).unwrap();
    r.resize(960, 540);
    r.commit(token).unwrap();
    let staging = r.slot_staging(ScreenKind::Selection).unwrap();
    assert_eq!(staging.projection().target(), [960, 540]);
    assert_eq!(staging.projection().viewport(), [0.0, 0.0, 960.0, 540.0]);
    assert_eq!(
        staging.scissor(Some(ClipId(0))),
        Scissor::Rect([0, 0, 480, 540])
    );
    // A later resize projects it too.
    r.resize(1_920, 1_080);
    let staging = r.slot_staging(ScreenKind::Selection).unwrap();
    assert_eq!(
        staging.scissor(Some(ClipId(0))),
        Scissor::Rect([0, 0, 960, 1_080])
    );
    assert!(r.slot_staging(ScreenKind::Play).is_none());
}
