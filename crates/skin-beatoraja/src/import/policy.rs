//! Roles and the owner decisions (design.md, the S6b decision table).
//!
//! A role comes from what an element shows, never from its id string.
//! A play-critical element must be reproduced exactly in value,
//! visibility and timing, or the import is refused; anything else the
//! importer cannot reproduce is dropped with a note. The owner's
//! decisions, each implemented where named:
//!
//! 1. Geometry: exact arithmetic, each edge rounded half up; the source's
//!    `f32`, its 0.01 px nudge and its whole-degree angles are not copied;
//!    nothing is refused for geometry (D1, [`super::geom`]).
//! 2. File choice: configured, then the default by name or stem ignoring
//!    ASCII case (first sorted when ambiguous), then the first sorted
//!    candidate; reported (D8, [`super::asset`]).
//! 3. Bitmap `.fnt` fonts: drawn as bitmap fonts with their fallbacks,
//!    the size rounded to a whole pixel (D12); distance-field ones with
//!    the default font at the source's size, top kept (D13); a size of 0
//!    or less ignored; reported ([`super::widget::text`]).
//! 4. The five 16:9 skin sizes ([`crate::SIZES`]).
//! 5. Play-critical numbers: EX score and max combo, by what they show
//!    ([`super::property::Value::critical`]).
//! 6. A decorative element whose image or font fails: only it is dropped;
//!    aggregate limits still refuse ([`unsupported`]).
//! 7. Elements without a timer start at `ready`; a critical one only with
//!    output independent of the start time (D3, [`untimed_ok`]).
//! 8. Play-state display differences on decorative elements are accepted
//!    (D4, [`super::ops`], [`super::property`]).
//! 9. Text is always drawn with smooth sampling and normal blending (D5).
//! 10. Text content and layout fallbacks: literals cut at 256 bytes,
//!     control characters removed, overflow 2 shrinks, outline, shadow and
//!     wrapping removed (D6, [`super::widget::text`]).
//! 11. The scratch side of an imported skin: the side the skin draws it
//!     on, given to the caller as `Imported::scratch` (a setting comes
//!     with S11).
//! 12. The lane approximations of the notes, SUDDEN+ and LIFT (D9 items
//!     a–l, [`super::report::LaneApprox`]), each accepted by the owner:
//!     a notes and long-note bodies stop at the top of the lanes; b an
//!     unhit tap keeps falling below the line until it is judged a miss
//!     or leaves the lanes; c a tap hit early disappears at once; d a hit
//!     mine disappears when hit (an unhit one leaves at its time, as the
//!     source does); e `dst2` is ignored;
//!     f `expansionrate` is ignored; g a long note's end cap is drawn and a
//!     skin without one is refused; h a long note's body runs between the
//!     caps' centres; i the part of SUDDEN+ above the lanes is never
//!     drawn; j bar lines are drawn across the lanes at note centres, white
//!     and opaque, until they leave the lanes' bottom, also before the
//!     chart starts and through a STOP; k what the skin draws between the
//!     notes and SUDDEN+ or LIFT is drawn above them; l the LIFT image is
//!     stretched over the covered area.
//! 13. A bar line outside the accepted mapping is dropped with a note
//!     instead of refusing the skin ([`super::widget::lanes`]).
//! 14. Judgment and gauge parts that move with LIFT (offset 3) are
//!     refused, as every run-time offset on them is.
//! 15. HIDDEN+ is optional for the player, so it is decorative: outside
//!     the mapped subset it is dropped with a note and the skin still
//!     imports ([`super::widget::lanes`]). Its differences from the source
//!     are noted as lane approximations (D9): m drawn under SUDDEN+ and
//!     LIFT, n cut at its trim line after its stretch, o never drawn
//!     outside the lanes.

use std::fmt;

use skin_model::End;

use super::report::{NoteKind, Notes, Severity, Span};
use crate::error::ImportError;
use crate::tree::Arena;

/// The play-critical parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CriticalPart {
    ExScore,
    MaxCombo,
    /// The note lanes.
    Lanes,
    /// The judgment display.
    Judge,
    /// The gauge.
    Gauge,
    /// The SUDDEN+ cover.
    Cover,
    /// The LIFT cover.
    Lift,
}

impl fmt::Display for CriticalPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ExScore => "EX score number",
            Self::MaxCombo => "max combo number",
            Self::Lanes => "note lanes",
            Self::Judge => "judgment display",
            Self::Gauge => "gauge",
            Self::Cover => "SUDDEN+ cover",
            Self::Lift => "LIFT cover",
        })
    }
}

/// An element's role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Critical(CriticalPart),
    Decorative,
}

/// Something `role`'s element at `at` needs that the importer cannot
/// reproduce: a critical element refuses the import; a decorative one is
/// dropped with a note (owner decision 6).
pub(crate) fn unsupported(
    arena: &Arena,
    role: Role,
    at: Span,
    why: NoteKind,
    notes: &mut Notes,
) -> Result<(), ImportError> {
    match role {
        Role::Critical(part) => Err(ImportError::Critical {
            at: arena.loc_at(at.file, at.offset),
            part,
            why,
        }),
        Role::Decorative => {
            notes.push(Some(at), Severity::Dropped, why);
            Ok(())
        }
    }
}

/// Whether an untimed critical element's output is independent of when
/// the state started (owner decision 7, D3): every key identical, the
/// first at 0, a perpetual end, and an image cycle absent or on a timer.
pub(crate) fn untimed_ok(identical: bool, first_at_zero: bool, end: End, cycle_ok: bool) -> bool {
    identical && first_at_zero && matches!(end, End::Hold | End::Loop { .. }) && cycle_ok
}
