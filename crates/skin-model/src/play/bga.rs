//! The BGA a play frame shows (design.md "Rendering and Skins", BGA): the
//! base and layer events and the poor sequence in effect at the frame's
//! chart time, and the miss window. A pure function of chart time and the
//! latest miss, kept by cursors that a rearm, or any step back in time,
//! places again; nothing is allocated.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{BgaEvent, BgaImageId, ChartBga, PoorSequence, TimeUs};

use crate::bga::{BgaEntry, BgaStretch, BgaTable, NullMedia, SOLID};
use crate::quad::{ClipRect, Filter, QuadBatch};
use crate::widget::Brush;

/// How long the poor pictures replace the base and the layer after a
/// miss: the source's default miss layer duration (500 ms).
pub const MISS_US: i64 = 500_000;

/// What a frame's BGA destinations show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Show {
    /// The black placeholder: before chart time 0, for a chart with no
    /// BGA, or while no table for the live array is published.
    Placeholder,
    /// The miss window: the poor sequence's frame, `None` for one that
    /// names no picture (nothing drawn).
    Miss(Option<BgaImageId>),
    /// The base and the layer in effect, `None` for none (the base then
    /// shows the placeholder, the layer nothing).
    Layers {
        base: Option<BgaImageId>,
        layer: Option<BgaImageId>,
    },
}

/// The count of entries at or before `now`, kept from the last frame:
/// advanced while `now` moves forward, placed again by search when it
/// moves back.
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    passed: usize,
}

impl Cursor {
    fn at<T>(&mut self, items: &[T], time: impl Fn(&T) -> TimeUs, now: TimeUs, back: bool) {
        if back {
            self.passed = items.partition_point(|i| time(i) <= now);
            return;
        }
        while items.get(self.passed).is_some_and(|i| time(i) <= now) {
            self.passed += 1;
        }
    }

    /// The last entry at or before `now`.
    fn last<'a, T>(&self, items: &'a [T]) -> Option<&'a T> {
        self.passed.checked_sub(1).and_then(|i| items.get(i))
    }
}

/// One attempt's BGA clock over the chart's BGA.
#[derive(Clone, Debug)]
pub(crate) struct BgaClock {
    bga: Option<ChartBga>,
    base: Cursor,
    layer: Cursor,
    poor: Cursor,
    /// The last frame's time, `None` before the first and after a clear.
    last: Option<TimeUs>,
}

impl BgaClock {
    pub(crate) fn new(bga: Option<ChartBga>) -> Self {
        Self {
            bga,
            base: Cursor::default(),
            layer: Cursor::default(),
            poor: Cursor::default(),
            last: None,
        }
    }

    /// Forgets the cursors, for a rearm: the next frame places them again.
    pub(crate) fn clear(&mut self) {
        self.base = Cursor::default();
        self.layer = Cursor::default();
        self.poor = Cursor::default();
        self.last = None;
    }

    /// What the frame at chart time `now` shows, the latest miss at `miss`.
    pub(crate) fn show(&mut self, now: TimeUs, miss: Option<TimeUs>) -> Show {
        let Some(bga) = &self.bga else {
            return Show::Placeholder;
        };
        if now.0 < 0 {
            return Show::Placeholder;
        }
        let back = self.last.is_none_or(|l| now < l);
        self.last = Some(now);
        let event = |e: &BgaEvent| e.time;
        self.base.at(bga.base(), event, now, back);
        self.layer.at(bga.layer(), event, now, back);
        self.poor
            .at(bga.poor(), |p: &PoorSequence| p.time, now, back);
        if let (Some(seq), Some(at)) = (self.poor.last(bga.poor()), miss)
            && at <= now
            && now.0 - at.0 < MISS_US
        {
            let frames = seq.frames.len();
            let elapsed = u64::try_from(now.0 - at.0).unwrap_or(0);
            let n = u64::try_from(frames).unwrap_or(0);
            let i = usize::try_from(n * elapsed / MISS_US.unsigned_abs()).unwrap_or(0);
            return Show::Miss(seq.frames.get(i).copied().flatten());
        }
        Show::Layers {
            base: self.base.last(bga.base()).and_then(|e| e.image),
            layer: self.layer.last(bga.layer()).and_then(|e| e.image),
        }
    }
}

/// What one BGA element draws from at a frame.
pub(crate) struct Parts<'a> {
    pub(crate) show: Show,
    pub(crate) table: &'a BgaTable,
    pub(crate) media: NullMedia,
    pub(crate) stretch: BgaStretch,
    /// Target pixels per canvas pixel.
    pub(crate) scale: f32,
}

impl Parts<'_> {
    /// Pushes the element's quads over its placed `rect` with `brush`'s
    /// key, transform and colour (see [`Show`]): the placeholder in black
    /// at the brush's alpha over the whole rect, each picture met by the
    /// stretch against its drawn size, the base and a poor picture at
    /// their linear samples, the layer keyed.
    pub(crate) fn draw(
        &self,
        out: &mut QuadBatch,
        brush: &Brush,
        rect: [f32; 4],
        trim: Option<ClipRect>,
    ) {
        #[cfg(feature = "dev-tools")]
        if crate::bga::ALLOCATE_IN_DRAW.load(std::sync::atomic::Ordering::Relaxed) {
            std::hint::black_box(vec![0_u8; 1]);
        }
        let placeholder = |out: &mut QuadBatch| {
            let [.., a] = brush.color;
            let b = Brush {
                native: None,
                color: [0.0, 0.0, 0.0, a],
                ..*brush
            };
            b.push_clipped(out, rect, SOLID, Filter::Linear, trim);
        };
        let picture = |out: &mut QuadBatch, id: BgaImageId, filter: Filter| {
            let cell = match self.table.get(id) {
                BgaEntry::Picture(c) => Some(c),
                BgaEntry::Video => self.media.frame(id),
                BgaEntry::Absent => None,
            };
            let Some(c) = cell else {
                return false;
            };
            let (r, uv, native) = self.stretch.place(rect, c.native, c.uv, self.scale);
            let b = Brush { native, ..*brush };
            b.push_clipped(out, r, uv, filter, trim);
            true
        };
        match self.show {
            Show::Placeholder => placeholder(out),
            Show::Miss(frame) => {
                if let Some(id) = frame {
                    picture(out, id, Filter::Bga);
                }
            }
            Show::Layers { base, layer } => {
                if !base.is_some_and(|id| picture(out, id, Filter::Bga)) {
                    placeholder(out);
                }
                if let Some(id) = layer {
                    picture(out, id, Filter::BgaLayer);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bms_model::BgaImage;

    fn bga() -> ChartBga {
        let image = |id| BgaImage {
            id,
            name: format!("{id}.png"),
        };
        let ev = |t, i: Option<u16>| BgaEvent {
            time: TimeUs(t),
            image: i.map(BgaImageId),
        };
        ChartBga::new(
            vec![image(1), image(2), image(3), image(4)],
            vec![ev(0, Some(0)), ev(1_000_000, Some(1)), ev(2_000_000, None)],
            vec![ev(500_000, Some(2))],
            vec![PoorSequence {
                time: TimeUs(3_000_000),
                frames: vec![Some(BgaImageId(3)), None].into_boxed_slice(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn events_follow_chart_time_forward_and_back() {
        let mut c = BgaClock::new(Some(bga()));
        assert_eq!(c.show(TimeUs(-1), None), Show::Placeholder);
        let at = |c: &mut BgaClock, t| c.show(TimeUs(t), None);
        assert_eq!(
            at(&mut c, 600_000),
            Show::Layers {
                base: Some(BgaImageId(0)),
                layer: Some(BgaImageId(2))
            }
        );
        assert_eq!(
            at(&mut c, 2_000_000),
            Show::Layers {
                base: None,
                layer: Some(BgaImageId(2))
            }
        );
        assert_eq!(
            at(&mut c, 1_000_000),
            Show::Layers {
                base: Some(BgaImageId(1)),
                layer: Some(BgaImageId(2))
            }
        );
    }

    #[test]
    fn a_miss_shows_the_poor_frames_only_once_a_sequence_has_passed() {
        let mut c = BgaClock::new(Some(bga()));
        let before = c.show(TimeUs(2_500_000), Some(TimeUs(2_400_000)));
        assert!(matches!(before, Show::Layers { .. }));
        let miss = Some(TimeUs(3_100_000));
        assert_eq!(
            c.show(TimeUs(3_100_000), miss),
            Show::Miss(Some(BgaImageId(3)))
        );
        assert_eq!(
            c.show(TimeUs(3_349_999), miss),
            Show::Miss(Some(BgaImageId(3)))
        );
        assert_eq!(c.show(TimeUs(3_350_000), miss), Show::Miss(None));
        assert!(matches!(
            c.show(TimeUs(3_600_000), miss),
            Show::Layers { .. }
        ));
    }

    #[test]
    fn no_bga_is_the_placeholder() {
        let mut c = BgaClock::new(None);
        assert_eq!(c.show(TimeUs(5), None), Show::Placeholder);
    }
}
