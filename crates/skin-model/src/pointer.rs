//! Pointer input on a selection skin (docs/stage3.md S10-09e): what a
//! press at a point asks of the targets the last build drew, and whether a
//! hover-gated element draws under the pointer.
//!
//! The last [`build_selection`](crate::Skin::build_selection) records each
//! pointer target it drew, in draw order, in target pixels: each changeable
//! slider's travel, each action element's region, each search text's input
//! box and each song list element, with the rect of every slot whose bar
//! drew. [`Skin::hit_at`](crate::Skin::hit_at) walks them topmost first, as
//! the source walks its objects (`skin/Skin.java:394-410`); a song list
//! tries its slots in its clickable order (`select/BarRenderer.java:80-101`).
//! Every rect's edges are inside it, as the source's.
//!
//! Every target and hover rect is taken from the element's region, its
//! destination before a stretch fits its image in it or an alignment moves
//! its text: the source computes `region` from the destination in
//! `SkinObject.prepare`, tests the `mouseRect` and a press against it
//! (`skin/SkinObject.java:591-607, 671-700`) and stretches only the rect
//! `draw` draws (`skin/SkinObject.java:623-643`). A search text's input box
//! is that region moved by its alignment (`skin/SkinText.java:162-169`).
//!
//! A hit never changes the skin's state: the screen carries the command out
//! and validates a row by its [`RowRef`], a fixed-size `Copy` identity that
//! never holds a path.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::Cell;

use crate::selection::RowKind;
use crate::track::Track;
use crate::widget::digits::Align;
use crate::widget::ratio_bar::{Hit, SliderCommand};

/// What kind of row a [`RowRef`] names: a heading is never one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RowRefKind {
    /// The "all charts" row.
    All,
    Folder,
    Chart,
}

impl RowRefKind {
    /// The kind of a row of `kind`; `None` for a heading.
    #[must_use]
    pub const fn of(kind: RowKind) -> Option<Self> {
        match kind {
            RowKind::All { .. } => Some(Self::All),
            RowKind::Folder { .. } => Some(Self::Folder),
            RowKind::Chart => Some(Self::Chart),
            RowKind::Heading => None,
        }
    }
}

/// A drawn row's identity, compared by value: the list's revision when the
/// view was made, the row's index among the list's selectable rows and its
/// kind. 16 bytes, `Copy`; it never holds a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowRef {
    /// [`SelectionView::list_revision`](crate::SelectionView::list_revision)
    /// of the view the row was drawn from.
    pub list_revision: u64,
    /// [`RowView::index`](crate::RowView::index).
    pub index: u32,
    pub kind: RowRefKind,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    /// The step a press of this button gives an action of
    /// [`ClickType::Plus`]: +1 for the left, middle and back buttons, −1
    /// for the right and forward ones (`skin/SkinObject.java:671-700`).
    #[must_use]
    pub const fn step(self) -> i8 {
        match self {
            Self::Left | Self::Middle | Self::Back => 1,
            Self::Right | Self::Forward => -1,
        }
    }
}

/// What a press hit on the last build. `Copy`, at most 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerHit {
    /// A changeable slider's travel: the ratio under the point.
    Slider(SliderCommand),
    /// An action element: its event and its argument, ±1.
    Action { event: u16, arg: i8 },
    /// A song list slot whose bar drew, and the button pressed.
    Row { row: RowRef, button: Button },
    /// The input box of drawn search text, whatever the button.
    Search,
}

/// How an action's argument comes from the press (the source's click
/// types 0 to 3, `skin/SkinObject.java:671-700`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClickType {
    /// The button's step ([`Button::step`]).
    Plus,
    /// The button's step negated.
    Minus,
    /// +1 on the right half, the middle included, −1 on the left.
    Halves,
    /// +1 on the top half, the middle included, −1 on the bottom.
    TopBottom,
}

/// An element's action: the event a press on it gives and how its
/// argument is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActionDef {
    pub event: u16,
    pub click: ClickType,
}

/// A hover rect: the element draws only while the pointer is inside it.
/// In reference pixels, scaled as the element's region is, an offset from
/// the element's region's bottom-left corner in y-up space: `x` right, `y`
/// up (`skin/SkinObject.java:603-607`, `skin/Skin.java:183-184`). Each
/// value within ±[`MAX_COORD`](crate::MAX_COORD).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HoverRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// What pointer input an element of a selection skin takes, at most one
/// per element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerDef {
    /// The element's index in [`SkinDef::elements`](crate::SkinDef::elements).
    pub element: u32,
    /// Its action: only an image's or an image choice's.
    pub action: Option<ActionDef>,
    /// Its hover rect.
    pub hover: Option<HoverRect>,
    /// Whether its drawn box takes a press as the search box's: only text
    /// showing the query.
    pub search: bool,
    /// Its region: its destination before a stretch or a text alignment
    /// moved what it draws, keyed as its element's track is (the same key
    /// times, ease and end) and sampled at the same time; only its rects
    /// are read. Its action, search box and hover rect are placed on it
    /// (`skin/SkinObject.java:591-607, 671-700`). `None`: the element's
    /// sampled rect is its region.
    pub region: Option<Track>,
}

/// Why a selection skin's pointer definitions were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PointerError {
    #[error("it names no element")]
    UnknownElement,
    #[error("another entry names the same element")]
    Duplicate,
    #[error("an action on an element that is not an image or an image choice")]
    ActionSource,
    #[error("a search box on an element that is not text showing the query")]
    SearchSource,
    #[error("its hover rect or its region leaves the coordinate range")]
    Coordinates,
    #[error("its region is not keyed as its element's track")]
    RegionTiming,
}

/// What a press hits ([`Skin::hit_at`](crate::Skin::hit_at)): the topmost
/// target, and whether the press lies outside the input box of every search
/// text drawn, whatever it hit. The source commits a focused search text
/// on a press outside its box before it walks the targets
/// (`skin/Skin.java:394-397`, `skin/SkinTextInput.java:77-81`), so a
/// target drawn over the box takes the press with the focus kept. `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressHit {
    pub hit: Option<PointerHit>,
    pub outside_search: bool,
}

impl PressHit {
    /// Nothing hit, outside every search box: a press on a build that drew
    /// no target.
    pub const NONE: Self = Self {
        hit: None,
        outside_search: true,
    };
}

/// A pointer target as the last build drew it, in target pixels, with
/// that build's target size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Target {
    Slider(Hit),
    Action {
        /// Left, top, width, height.
        rect: [f32; 4],
        def: ActionDef,
        target: [u32; 2],
    },
    Search {
        rect: [f32; 4],
        target: [u32; 2],
    },
    /// A song list element: its first slot's cell in the row hits.
    List {
        first: u16,
        target: [u32; 2],
    },
}

impl Target {
    /// The target size of the build that drew it.
    const fn target(&self) -> [u32; 2] {
        match self {
            Self::Slider(h) => h.target,
            Self::Action { target, .. }
            | Self::Search { target, .. }
            | Self::List { target, .. } => *target,
        }
    }
}

/// A song list slot whose bar the last build drew: its rect in target
/// pixels and its row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RowHit {
    pub(crate) rect: [f32; 4],
    pub(crate) row: RowRef,
}

/// Whether `[px, py]` lies in `[x, y, w, h]`, its edges included.
pub(crate) fn contains([x, y, w, h]: [f32; 4], [px, py]: [f32; 2]) -> bool {
    x <= px && px <= x + w && y <= py && py <= y + h
}

/// An action's argument for a press of `button` at `[px, py]` on `rect`.
/// The source tests halves in y-up space (`y ≥ bottom + h/2` is the top);
/// with y down and the pointer's y mirrored exactly, that is `py ≤ top +
/// h/2`, the middle line included in the top half.
fn arg(click: ClickType, [x, y, w, h]: [f32; 4], [px, py]: [f32; 2], button: Button) -> i8 {
    let half = |upper: bool| if upper { 1 } else { -1 };
    match click {
        ClickType::Plus => button.step(),
        ClickType::Minus => -button.step(),
        ClickType::Halves => half(px >= x + w / 2.0),
        ClickType::TopBottom => half(py <= y + h / 2.0),
    }
}

/// What a press of `button` at `point` hits among `targets` drawn at
/// `target`, topmost first; a song list tries its slots in `clickable`
/// order among `rows`. A slider or a song list the point misses lets the
/// walk go on, as the source's do. Whether the point lies outside every
/// search box is read apart from the walk.
pub(crate) fn hit(
    targets: &[Cell<Option<Target>>],
    rows: &[Cell<Option<RowHit>>],
    clickable: &[u8],
    point: [f32; 2],
    target: [u32; 2],
    button: Button,
) -> PressHit {
    let drawn = || {
        targets
            .iter()
            .rev()
            .filter_map(Cell::get)
            .filter(|t| t.target() == target)
    };
    let outside_search =
        !drawn().any(|t| matches!(t, Target::Search { rect, .. } if contains(rect, point)));
    let hit = drawn().find_map(|t| match t {
        Target::Slider(h) => h.at(point).map(PointerHit::Slider),
        Target::Action { rect, def, .. } => contains(rect, point).then(|| PointerHit::Action {
            event: def.event,
            arg: arg(def.click, rect, point, button),
        }),
        Target::Search { rect, .. } => contains(rect, point).then_some(PointerHit::Search),
        Target::List { first, .. } => clickable.iter().find_map(|&slot| {
            let cell = usize::from(first).checked_add(usize::from(slot))?;
            let h = rows.get(cell).and_then(Cell::get)?;
            contains(h.rect, point).then_some(PointerHit::Row { row: h.row, button })
        }),
    });
    PressHit {
        hit,
        outside_search,
    }
}

/// What dragging to `point` asks of the topmost slider among `targets`
/// drawn at `target` whose travel holds it (`skin/Skin.java:412-419`).
pub(crate) fn drag(
    targets: &[Cell<Option<Target>>],
    point: [f32; 2],
    target: [u32; 2],
) -> Option<SliderCommand> {
    targets
        .iter()
        .rev()
        .filter_map(Cell::get)
        .filter(|t| t.target() == target)
        .find_map(|t| match t {
            Target::Slider(h) => h.at(point),
            Target::Action { .. } | Target::Search { .. } | Target::List { .. } => None,
        })
}

/// A coordinate within ±[`MAX_COORD`](crate::MAX_COORD) as `f32`.
#[expect(
    clippy::cast_precision_loss,
    reason = "within ±2^20, checked at load: exact"
)]
const fn coord(v: i32) -> f32 {
    v as f32
}

/// `hover` placed on an element of region `[x, y, w, h]` (target pixels,
/// y down) at `scale` target pixels per reference pixel on each axis: left,
/// top, width, height. The offset runs from the rect's bottom-left corner,
/// `y` upward.
pub(crate) fn hover_rect(hover: HoverRect, [x, y, _, h]: [f32; 4], [sx, sy]: [f32; 2]) -> [f32; 4] {
    let bottom = y + h;
    [
        x + coord(hover.x) * sx,
        bottom - (coord(hover.y) + coord(hover.h)) * sy,
        coord(hover.w) * sx,
        coord(hover.h) * sy,
    ]
}

/// Whether `pointer` is inside `hover` on an element of region `rect` at
/// `scale`: never without a pointer.
pub(crate) fn hovered(
    hover: HoverRect,
    rect: [f32; 4],
    scale: [f32; 2],
    pointer: Option<[f32; 2]>,
) -> bool {
    pointer.is_some_and(|p| contains(hover_rect(hover, rect, scale), p))
}

/// A search text's input box on its region `[x, y, w, h]` (any units, x
/// right): the region moved left by none, half or all of its width as the
/// text aligns left, centred or right (`skin/SkinText.java:162-169`).
pub(crate) fn input_box(align: Align, [x, y, w, h]: [f32; 4]) -> [f32; 4] {
    let left = match align {
        Align::Left => x,
        Align::Center => x - w / 2.0,
        Align::Right => x - w,
    };
    [left, y, w, h]
}

/// Whether every value of `hover` is within ±`max`.
pub(crate) fn hover_in(hover: HoverRect, max: i32) -> bool {
    [hover.x, hover.y, hover.w, hover.h]
        .into_iter()
        .all(|v| v.unsigned_abs() <= max.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::{Button, ClickType, arg};

    /// The halves' boundaries in y-down target pixels, each side: the
    /// middle line and the edges.
    #[test]
    fn halves_split_at_the_middle_line() {
        let r = [10.0, 100.0, 40.0, 20.0];
        let b = Button::Left;
        // Left and right: x ≥ 30 is the right half.
        assert_eq!(arg(ClickType::Halves, r, [30.0, 105.0], b), 1);
        assert_eq!(arg(ClickType::Halves, r, [29.0, 105.0], b), -1);
        assert_eq!(arg(ClickType::Halves, r, [10.0, 105.0], b), -1);
        assert_eq!(arg(ClickType::Halves, r, [50.0, 105.0], b), 1);
        // Top and bottom: y ≤ 110 is the top half, the middle included.
        assert_eq!(arg(ClickType::TopBottom, r, [20.0, 110.0], b), 1);
        assert_eq!(arg(ClickType::TopBottom, r, [20.0, 111.0], b), -1);
        assert_eq!(arg(ClickType::TopBottom, r, [20.0, 100.0], b), 1);
        assert_eq!(arg(ClickType::TopBottom, r, [20.0, 120.0], b), -1);
    }
}
