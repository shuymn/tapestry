//! Each screen kind's committed skin on WARP, and the texels every kind's
//! committed skin is charged against the limit the kinds share (design.md
//! "Memory Budget"): a commit's charge is its array's level-0 texels, as
//! uploaded; another kind's commit leaves play's installation drawing as
//! it was; one candidate of any kind is staged at a time; and a removal
//! drops only its kind's staging. The limit itself, with installations
//! charged an atlas's cap without uploading one, is tested by the unit
//! tests in `src/admission_tests.rs`.
//!
//! "Tiny" is a 16×16 cell on one 256-texel page.

mod common;
mod decl_support;

use std::sync::Arc;

use bms_model::{ChartBuilder, TimeUs};
use common::{GAUGES, attempt, context, input, offscreen, view};
use decl_support::{Look, solid_bundle};
use render::{CommitError, PreparedToken, Renderer, SkinBundle};
use skin_model::ScreenKind;

/// Tiny's atlas array: one 256-texel page beside the reserved layer.
const TINY: u64 = 256 * 256 * 2;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// The left half's centre, where every fixture draws.
const LEFT: (u32, u32) = (480, 540);

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

/// A commit's charge, which no test stated, is its atlas array's level-0
/// texels as uploaded: a 300-texel image's page of 384 beside the reserved
/// layer, as play's (whose array the renderer reports) and as the
/// selection screen's. The unit tests' stated charges stand for this one.
#[test]
fn a_commit_is_charged_its_uploaded_array() {
    let mut r = offscreen(1_920, 1_080);
    let p = solid_bundle(&r, &Look::left(RED), [300, 300]);
    play(&mut r, &p, 1);
    let (side, layers) = r.atlas_size();
    assert_eq!((side, layers), (384, 2));
    let array = u64::from(side) * u64::from(side) * u64::from(layers);
    assert_eq!(p.atlas_texels(), array);
    assert_eq!(r.admission().charge(ScreenKind::Play), array);

    let s = solid_bundle(&r, &Look::left(GREEN), [300, 300]);
    commit(&mut r, ScreenKind::Selection, &s, 2);
    assert_eq!(r.admission().charge(ScreenKind::Selection), array);
    assert_eq!(r.admission().committed_total(), 2 * array);
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

/// Another kind's staging of play's installed bundle shares its CPU
/// description but has its own loading presentation, which the witness
/// counts; play's own staging of it shares both.
#[test]
fn another_kinds_staging_counts_its_own_presentation() {
    let mut r = offscreen(1_920, 1_080);
    let red = tiny(&r, RED);
    commit(&mut r, ScreenKind::Play, &red, 1);
    let installed = r.memory();
    assert!(installed.loading > 0);

    r.install(ScreenKind::Selection, &red, 2).unwrap();
    let staged = r.memory();
    assert_eq!(staged.staged_cpu, 0, "the pages are play's");
    assert_eq!(staged.staged_loading, installed.loading);
    assert!(staged.staged_atlas > 0, "its own atlas array");

    r.install(ScreenKind::Play, &red, 3).unwrap();
    let again = r.memory();
    assert_eq!((again.staged_cpu, again.staged_loading), (0, 0));
}

/// A kind's committed bundle staged again and then removed cannot commit
/// its token: the removal dropped the staging with the slot. Another
/// kind's staging survives a removal and commits.
#[test]
fn a_removal_drops_only_its_kinds_staging() {
    let mut r = offscreen(1_920, 1_080);
    let green = tiny(&r, GREEN);
    commit(&mut r, ScreenKind::Selection, &green, 1);
    let token = r.install(ScreenKind::Selection, &green, 2).unwrap();
    assert_eq!(r.memory().staged_atlas, 0, "the slot's resources reused");
    r.remove(ScreenKind::Selection);
    assert!(r.staged().is_none());
    assert_eq!(r.commit(token), Err(CommitError::NothingStaged));
    assert!(r.installed(ScreenKind::Selection).is_none());

    commit(&mut r, ScreenKind::Selection, &green, 3);
    let red = tiny(&r, RED);
    let token = r.install(ScreenKind::Decision, &red, 4).unwrap();
    r.remove(ScreenKind::Selection);
    assert_eq!(r.staged(), Some(token));
    r.commit(token).unwrap();
    assert!(Arc::ptr_eq(
        r.installed(ScreenKind::Decision).unwrap(),
        &red
    ));
    assert!(r.installed(ScreenKind::Selection).is_none());
}
