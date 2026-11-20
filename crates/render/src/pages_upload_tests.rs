//! The result pages upload on WARP (design.md "Memory Budget", the
//! uploader): its begin releases the result kind's installation and
//! creates the array with nothing written; each boundary writes a step;
//! the boundary that sees the last one complete reports it written, and
//! only its owner's publication installs it; a withdrawal retires the
//! array, written or partial; the play staging's begin, commit and
//! discard leave it, and the result kind's own operations withdraw it.

use std::sync::Arc;
use std::time::Duration;

use skin_model::PackedPages;

use super::tests::refuse;
use super::{PagesBegun, PagesStep, Renderer, ScreenKind};
use crate::decl::pages_texels;

/// A renderer and two bundles of different pages, the folders their
/// images live in kept.
fn fixture() -> (
    Renderer,
    [Arc<crate::SkinBundle>; 2],
    [tempfile::TempDir; 2],
) {
    let r = crate::test_common::offscreen(1_920, 1_080);
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let look = |rgba| crate::test_decl::Look {
        side: 600,
        ..crate::test_decl::Look::left(rgba)
    };
    let (a, _) = crate::test_decl::bundle(&r, da.path(), &look([255, 0, 0, 255]));
    let (b, _) = crate::test_decl::bundle(&r, db.path(), &look([0, 0, 255, 255]));
    (r, [a, b], [da, db])
}

/// One boundary of a zero slice, each step's completion seen at once, so
/// one write: how the upload ended there, if it did.
fn boundary(r: &mut Renderer) -> Option<PagesStep> {
    r.step_seen.set(Some(true));
    let out = r.upload_boundary_within(Duration::ZERO, &mut || false, None);
    r.step_seen.set(None);
    out.pages
}

/// Boundaries until the upload ends: the steps written and its end.
fn until_end(r: &mut Renderer) -> (usize, PagesStep) {
    let before = r.step_submits.get();
    for _ in 0..10_000 {
        if let Some(end) = boundary(r) {
            return (r.step_submits.get() - before, end);
        }
    }
    panic!("the upload never ended");
}

/// The result kind's committed pages, by id.
fn installed(r: &Renderer) -> Option<skin_model::AtlasId> {
    r.installed_pages(ScreenKind::Result).map(|p| p.id())
}

fn pages(b: &crate::SkinBundle) -> &Arc<PackedPages> {
    b.atlas_pages()
}

/// V15: begun, the pages are charged and their array created with
/// nothing written; each boundary writes one step; the boundary that sees
/// the last complete reports it written, and nothing is installed until
/// the owner publishes it, which makes it the pages-only installation.
#[test]
fn an_upload_steps_and_installs_only_at_its_publication() {
    let (mut r, [a, _], _dirs) = fixture();
    let p = pages(&a);
    assert_eq!(r.begin_pages(p, 1).unwrap(), PagesBegun::Begun);
    assert_eq!(r.pages_in_hand(), Some(1));
    assert_eq!(r.admission().charge(ScreenKind::Result), pages_texels(p));
    assert_eq!(installed(&r), None);
    let (steps, end) = until_end(&mut r);
    assert!(steps > 1, "{steps} steps");
    assert!(matches!(end, PagesStep::Written(1)), "{end:?}");
    assert_eq!(installed(&r), None, "written, not published");
    assert_eq!(r.publish_pages(), Some(1));
    assert_eq!(r.pages_in_hand(), None);
    assert_eq!(installed(&r), Some(p.id()));
    assert_eq!(r.admission().charge(ScreenKind::Result), pages_texels(p));
    assert!(boundary(&mut r).is_none(), "nothing left to report");
}

/// V10g's guard: a begin of the committed pages takes nothing: no
/// release, no array, nothing in hand.
#[test]
fn a_begin_of_the_installed_pages_takes_nothing() {
    let (mut r, [a, _], _dirs) = fixture();
    let p = pages(&a);
    let token = r.install_pages(ScreenKind::Result, p, 1).unwrap();
    r.commit(token).unwrap();
    let uploads = r.atlas_uploads();
    assert_eq!(r.begin_pages(p, 1).unwrap(), PagesBegun::Installed);
    assert_eq!(r.atlas_uploads(), uploads);
    assert_eq!(r.pages_in_hand(), None);
    assert_eq!(installed(&r), Some(p.id()));
}

/// V16a: the begin releases the earlier installation, which a withdrawal
/// does not restore; the partial array retires beside it.
#[test]
fn a_withdrawal_after_the_begin_keeps_the_release_and_retires_the_array() {
    let (mut r, [a, b], _dirs) = fixture();
    let token = r.install_pages(ScreenKind::Result, pages(&a), 1).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.begin_pages(pages(&b), 1).unwrap(), PagesBegun::Begun);
    assert_eq!(installed(&r), None, "released at the begin");
    assert!(boundary(&mut r).is_none());
    assert_eq!(r.withdraw_pages(), Some(1));
    assert_eq!(installed(&r), None);
    assert_eq!(r.pages_in_hand(), None);
    let kept = r.retained_atlases(ScreenKind::Result);
    for id in [pages(&a).id(), pages(&b).id()] {
        assert!(kept.contains(&Some(id)), "{kept:?}");
    }
    assert!(
        boundary(&mut r).is_none(),
        "the owner's withdrawal is not reported"
    );
}

/// §5.3: the play staging's begin, commit and discard never touch the
/// upload in hand.
#[test]
fn a_play_staging_leaves_the_upload() {
    let (mut r, [a, b], _dirs) = fixture();
    assert_eq!(r.begin_pages(pages(&b), 1).unwrap(), PagesBegun::Begun);
    crate::test_common::install(&mut r, &a, 1);
    assert_eq!(r.pages_in_hand(), Some(1));
    let token = r.install(ScreenKind::Play, &a, 2).unwrap();
    r.discard();
    let _ = token;
    assert_eq!(r.pages_in_hand(), Some(1));
    let (_, end) = until_end(&mut r);
    assert!(matches!(end, PagesStep::Written(1)), "{end:?}");
    assert_eq!(r.publish_pages(), Some(1));
    assert_eq!(installed(&r), Some(pages(&b).id()));
}

/// §5.3: the result kind's own operations withdraw the upload first,
/// reported at the next boundary.
#[test]
fn the_result_kinds_own_operations_withdraw_it() {
    let (mut r, [a, b], _dirs) = fixture();
    assert_eq!(r.begin_pages(pages(&b), 1).unwrap(), PagesBegun::Begun);
    r.remove(ScreenKind::Result);
    assert_eq!(r.pages_in_hand(), None);
    assert!(matches!(boundary(&mut r), Some(PagesStep::Withdrawn(1))));
    assert_eq!(r.begin_pages(pages(&b), 2).unwrap(), PagesBegun::Begun);
    r.restore_pages(ScreenKind::Result, pages(&a)).unwrap();
    assert!(matches!(boundary(&mut r), Some(PagesStep::Withdrawn(2))));
    assert_eq!(r.begin_pages(pages(&b), 3).unwrap(), PagesBegun::Begun);
    r.disable_result();
    assert!(matches!(boundary(&mut r), Some(PagesStep::Withdrawn(3))));
    assert_eq!(installed(&r), None);
}

/// Failure: a refused step retires the upload and is reported refused;
/// nothing is installed.
#[test]
fn a_refused_step_ends_the_upload() {
    let (mut r, [_, b], _dirs) = fixture();
    assert_eq!(r.begin_pages(pages(&b), 1).unwrap(), PagesBegun::Begun);
    r.step_write_fault = Some(refuse);
    let end = boundary(&mut r);
    r.step_write_fault = None;
    assert!(
        matches!(end, None | Some(PagesStep::Refused(1, _))),
        "{end:?}"
    );
    let end = end.or_else(|| boundary(&mut r));
    assert!(matches!(end, Some(PagesStep::Refused(1, _))), "{end:?}");
    assert_eq!(r.pages_in_hand(), None);
    assert_eq!(installed(&r), None);
    assert_eq!(r.publish_pages(), None);
}
