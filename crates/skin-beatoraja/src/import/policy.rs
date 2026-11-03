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
//! 7. Elements without a timer run on the `screen` timer, which restarts
//!    at READY; a critical one only with its placement and its colour
//!    other than alpha the same at every key, so only its presentation
//!    phase (alpha, its first key, its end, an image cycle without a
//!    timer) depends on the start, noted (D3, [`untimed_ok`]); an image
//!    cycle without a timer runs on `screen` under a timed destination
//!    too, noted on a critical one ([`start_dependent`]).
//! 8. Play-state display differences on decorative elements are accepted
//!    (D4, [`super::ops`], [`super::property`]).
//! 9. Text is always drawn with smooth sampling and normal blending (D5).
//! 10. Text content and layout fallbacks: literals cut at 256 bytes,
//!     control characters removed, overflow 2 shrinks, outline, shadow and
//!     wrapping removed (D6, [`super::widget::text`]).
//! 11. The scratch side of an imported skin: the side the skin draws it
//!     on, given to the caller as `Imported::scratch` (a setting comes
//!     with S11).
//! 12. The lane approximations of the notes and LIFT (D9 items
//!     a–l, [`super::report::LaneApprox`]), each accepted by the owner:
//!     a notes and long-note bodies stop at the top of the lanes; b an
//!     unhit tap keeps falling below the line until it is judged a miss
//!     or leaves the lanes; c a tap hit early disappears at once; d a hit
//!     mine disappears when hit (an unhit one leaves at its time, as the
//!     source does); e `dst2` is ignored;
//!     f `expansionrate` is ignored; g a long note's end cap is drawn and a
//!     skin without one is refused; h a long note's body runs between the
//!     caps' centres; j bar lines are drawn across the lanes at note
//!     centres, white and opaque, until they leave the lanes' bottom, also
//!     before the chart starts and through a STOP; l the LIFT image is
//!     stretched over the covered area. SUDDEN+'s paint, LIFT and HIDDEN+
//!     are drawn in their destinations' order, as the source draws them.
//! 13. A bar line outside the accepted mapping is dropped with a note
//!     instead of refusing the skin ([`super::widget::lanes`]).
//! 14. Images, numbers, text, gauges and judgment grades that list LIFT
//!     (offset 3) or SUDDEN+ (offset 4) follow them by this frame's lane
//!     geometry (D9 item p); a combo drawn relative to its grade takes the
//!     grade's motion and none of its own. A configured x, width, height,
//!     angle or alpha on either id, or on an id SUDDEN+, LIFT or HIDDEN+
//!     consumes (listed or added by the source), is refused as the
//!     element's role says, its configured y ignored; offset 5 outside
//!     HIDDEN+ and offsets 1 and 2 stay refused.
//! 15. HIDDEN+ is optional for the player, so it is decorative: outside
//!     the mapped subset it is dropped with a note and the skin still
//!     imports ([`super::widget::lanes`]). Its differences from the source
//!     are noted as lane approximations (D9): n cut at its trim line
//!     after its stretch, o never drawn outside the lanes.
//! 16. The failure presentation is play-critical: on play, a destination
//!     that can still draw once the configured options are folded and
//!     whose timer resolves to `failed` (3, directly or through a custom
//!     timer chain) is reproduced or refuses the skin, never dropped. Its
//!     role is fixed before any of its fields is mapped; a part with a
//!     role of its own (EX score, max combo, the covers, the play
//!     objects) keeps it ([`Role::or_failure`], `Cx::failure`).

use std::fmt;

use skin_model::End;

use super::report::{NoteKind, Notes, Severity, Span};
use super::track::Accepted;
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
    /// What the play presents when a survival gauge fails: a destination
    /// timed on `failed` (owner decision 16).
    Failure,
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
            Self::Failure => "failure presentation",
        })
    }
}

/// An element's role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Critical(CriticalPart),
    Decorative,
}

impl Role {
    /// The role of an element that is failure presentation when `failure`
    /// (owner decision 16): a decorative one becomes play-critical, and a
    /// content part keeps its own.
    pub(crate) fn or_failure(self, failure: bool) -> Self {
        match self {
            Self::Decorative if failure => Self::Critical(CriticalPart::Failure),
            r => r,
        }
    }
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

/// Whether a critical element without a timer, running on `screen`
/// (owner decision 7, D3), is admitted: its placement (rect and angle)
/// and its colour other than alpha the same at every key. Its alpha, its
/// first key's time, its end and an image cycle without a timer may make
/// what it shows depend on when the state started. Provisional until a
/// visual comparison of a real skin against the source's timing.
pub(crate) fn untimed_ok(track: &Accepted) -> bool {
    track.fixed_but_alpha()
}

/// Whether what a critical element shows depends on when the state
/// started (D3): it keeps an image cycling without a timer, `untimed_cycle`,
/// whatever its own timer; or it is `untimed` itself and its keys differ,
/// its first is after 0 or it hides after its last.
pub(crate) fn start_dependent(track: &Accepted, untimed: bool, untimed_cycle: bool) -> bool {
    untimed_cycle
        || (untimed
            && (!track.identical()
                || track.keys.first().is_some_and(|k| k.at != 0)
                || matches!(track.end, End::Hide)))
}
