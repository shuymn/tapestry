//! Regions, their anchors and the player's layout: one integer
//! adjustment per adjustable region, the lane width and the side inset.
//!
//! A [`Layout`] is skin-independent and valid by construction: its fields
//! are private and every setter checks the static ranges. What it does
//! to a skin is decided per frame by [`Skin::placement`](crate::Skin::placement).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::ops::RangeInclusive;

/// A 9-point anchor: a point of a box at fractions `{0, ½, 1}` of its
/// width and height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Anchor {
    /// Every anchor, in reading order.
    pub const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Left,
        Self::Center,
        Self::Right,
        Self::BottomLeft,
        Self::Bottom,
        Self::BottomRight,
    ];

    /// The horizontal and vertical fractions, in halves (0, 1 or 2).
    #[must_use]
    pub const fn halves(self) -> [u8; 2] {
        match self {
            Self::TopLeft => [0, 0],
            Self::Top => [1, 0],
            Self::TopRight => [2, 0],
            Self::Left => [0, 1],
            Self::Center => [1, 1],
            Self::Right => [2, 1],
            Self::BottomLeft => [0, 2],
            Self::Bottom => [1, 2],
            Self::BottomRight => [2, 2],
        }
    }

    /// The fractions `(fx, fy)`, each 0, 0.5 or 1.
    pub(crate) fn fractions(self) -> [f32; 2] {
        self.halves().map(|h| f32::from(h) / 2.0)
    }
}

/// The fixed regions every skin's elements belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// The lanes and everything bound to them.
    Playfield,
    /// The judgment and combo, placed relative to the playfield.
    Judge,
    Left,
    Right,
    Top,
    Bottom,
    /// Everything untagged and the backgrounds; the player adjusts nothing
    /// in it.
    Screen,
}

impl Region {
    /// The number of regions.
    pub const COUNT: usize = 7;
    /// Every region, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Playfield,
        Self::Judge,
        Self::Left,
        Self::Right,
        Self::Top,
        Self::Bottom,
        Self::Screen,
    ];

    /// The index in [`ALL`](Self::ALL).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// A region as a skin authors it: its anchor, and whether the player may
/// hide it (HUD regions only; `judge` may always be hidden).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionDef {
    pub anchor: Anchor,
    pub optional: bool,
}

/// One region's adjustment: a translation in reference pixels, a scale in
/// percent and whether it is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adjust {
    pub x: i32,
    pub y: i32,
    pub scale: u16,
    pub visible: bool,
}

impl Adjust {
    /// The author's placement: not moved, at 100 %, shown.
    pub const IDENTITY: Self = Self {
        x: 0,
        y: 0,
        scale: 100,
        visible: true,
    };
}

/// The largest translation on either axis: the widest canvas.
pub const MAX_OFFSET: i32 = 4_320;
/// The region scales a layout accepts, in percent.
pub const SCALE_PCT: RangeInclusive<u16> = 50..=200;
/// The lane widths a layout accepts, in percent.
pub const LANE_WIDTH_PCT: RangeInclusive<u16> = 50..=200;
/// The largest side inset: the widest canvas's margin past 16:9,
/// `(4320 − 1920) / 2`.
pub const MAX_INSET: u16 = 1_200;

/// Why a layout value was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("{region:?}'s offset is beyond ±{MAX_OFFSET}")]
    Offset { region: Region },
    #[error("{region:?}'s scale is outside 50-200 %")]
    Scale { region: Region },
    #[error("the lane width is outside 50-200 %")]
    LaneWidth,
    #[error("the side inset is beyond {MAX_INSET}")]
    Inset,
    #[error("the screen region has no adjustment")]
    Screen,
    #[error("{region:?} is adjusted twice")]
    Duplicate { region: Region },
}

/// The player's layout for one skin and aspect class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Per region but `Screen`, by index.
    adjust: [Adjust; 6],
    lane_width: u16,
    inset: u16,
}

impl Layout {
    /// The author's layout: every region unmoved at 100 % and shown, lane
    /// width 100 %, no inset.
    pub const AUTHOR: Self = Self {
        adjust: [Adjust::IDENTITY; 6],
        lane_width: 100,
        inset: 0,
    };

    /// A layout of one adjustment per region but `Screen`, in any order.
    ///
    /// # Errors
    ///
    /// The first value out of range, an adjustment of `Screen`, or a
    /// region given twice.
    pub fn new(
        adjust: [(Region, Adjust); 6],
        lane_width: u16,
        inset: u16,
    ) -> Result<Self, LayoutError> {
        let mut layout = Self::AUTHOR;
        let mut seen = [false; 6];
        for (region, a) in adjust {
            let slot = seen.get_mut(region.index()).ok_or(LayoutError::Screen)?;
            if *slot {
                return Err(LayoutError::Duplicate { region });
            }
            *slot = true;
            layout.set(region, a)?;
        }
        layout.set_lane_width(lane_width)?;
        layout.set_inset(inset)?;
        Ok(layout)
    }

    /// Sets `region`'s adjustment.
    ///
    /// # Errors
    ///
    /// `Screen`, an offset beyond ±[`MAX_OFFSET`], or a scale outside
    /// [`SCALE_PCT`].
    pub fn set(&mut self, region: Region, a: Adjust) -> Result<(), LayoutError> {
        let slot = self
            .adjust
            .get_mut(region.index())
            .ok_or(LayoutError::Screen)?;
        if a.x.unsigned_abs() > MAX_OFFSET.unsigned_abs()
            || a.y.unsigned_abs() > MAX_OFFSET.unsigned_abs()
        {
            return Err(LayoutError::Offset { region });
        }
        if !SCALE_PCT.contains(&a.scale) {
            return Err(LayoutError::Scale { region });
        }
        *slot = a;
        Ok(())
    }

    /// Sets the lane width in percent.
    ///
    /// # Errors
    ///
    /// A width outside [`LANE_WIDTH_PCT`].
    pub fn set_lane_width(&mut self, pct: u16) -> Result<(), LayoutError> {
        if !LANE_WIDTH_PCT.contains(&pct) {
            return Err(LayoutError::LaneWidth);
        }
        self.lane_width = pct;
        Ok(())
    }

    /// Sets the side inset in reference pixels.
    ///
    /// # Errors
    ///
    /// An inset beyond [`MAX_INSET`].
    pub fn set_inset(&mut self, px: u16) -> Result<(), LayoutError> {
        if px > MAX_INSET {
            return Err(LayoutError::Inset);
        }
        self.inset = px;
        Ok(())
    }

    /// Returns `region` to the author's layout; the playfield's lane width
    /// with it. `Screen` has nothing to reset.
    pub fn reset(&mut self, region: Region) {
        if let Some(a) = self.adjust.get_mut(region.index()) {
            *a = Adjust::IDENTITY;
        }
        if region == Region::Playfield {
            self.lane_width = 100;
        }
    }

    /// `region`'s adjustment; `Screen`'s is the identity.
    #[must_use]
    pub fn get(&self, region: Region) -> Adjust {
        self.adjust
            .get(region.index())
            .copied()
            .unwrap_or(Adjust::IDENTITY)
    }

    /// The lane width in percent.
    #[must_use]
    pub fn lane_width(&self) -> u16 {
        self.lane_width
    }

    /// The side inset in reference pixels.
    #[must_use]
    pub fn inset(&self) -> u16 {
        self.inset
    }
}

/// The controls a skin offers for one region (the layout ignores the
/// others).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    pub position: bool,
    pub scale: bool,
    pub lane_width: bool,
    pub hide: bool,
}
