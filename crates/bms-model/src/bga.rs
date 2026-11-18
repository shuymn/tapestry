//! A chart's BGA, read for presentation alone (design.md "Chart Model",
//! "What the Loader Reads"): its `#BMPxx` definitions, the base and layer
//! events and the poor sequences, in playback µs. Nothing judged reads it.

use std::fmt;
use std::sync::Arc;

use crate::TimeUs;

/// A `#BMPxx` definition: its object id and the name it gives, as written
/// (trimmed); resolved only by its reader, never here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BgaImage {
    /// The object id, two base-36 characters (`00` to `ZZ`, 0 to 1295).
    pub id: u16,
    /// The file name as written.
    pub name: String,
}

/// An index into [`ChartBga::images`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BgaImageId(pub u16);

/// A base (channel 04) or layer (07) event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BgaEvent {
    /// Its time in playback µs at the loaded speed.
    pub time: TimeUs,
    /// The definition it shows, or `None` for an id no `#BMPxx` defines,
    /// which clears its layer.
    pub image: Option<BgaImageId>,
}

/// A channel 06 line: the frames a miss shows in order, each an equal
/// share of the miss window, from its measure's start on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoorSequence {
    /// Its measure's start in playback µs at the loaded speed.
    pub time: TimeUs,
    /// At least one frame; `None` shows nothing (a `00` pair, or an id no
    /// `#BMPxx` defines).
    pub frames: Box<[Option<BgaImageId>]>,
}

/// Which part of a [`ChartBga`] a [`BgaError`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgaPart {
    /// [`ChartBga::images`].
    Images,
    /// [`ChartBga::base`].
    Base,
    /// [`ChartBga::layer`].
    Layer,
    /// [`ChartBga::poor`].
    Poor,
}

/// Why [`ChartBga::new`] refused its parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgaError {
    /// Definitions not in strictly increasing id order, or an id past
    /// `ZZ`; or events or sequences out of time order.
    Unsorted {
        /// The part.
        part: BgaPart,
        /// The first entry out of order.
        index: usize,
    },
    /// An event or frame naming no definition.
    UnknownImage {
        /// The part.
        part: BgaPart,
        /// The event or sequence.
        index: usize,
    },
    /// A poor sequence with no frame.
    EmptySequence {
        /// The sequence.
        index: usize,
    },
}

impl fmt::Display for BgaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsorted { part, index } => write!(f, "BGA {part:?} entry {index} out of order"),
            Self::UnknownImage { part, index } => {
                write!(f, "BGA {part:?} entry {index} names no definition")
            }
            Self::EmptySequence { index } => write!(f, "BGA poor sequence {index} has no frame"),
        }
    }
}

impl std::error::Error for BgaError {}

/// The highest object id, `ZZ`.
const MAX_ID: u16 = 36 * 36 - 1;

/// A chart's BGA, immutable and shared by its clones (design.md "Chart
/// Model"). The evaluation subset omits it and the chart's hashes do not
/// cover it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartBga(Arc<BgaData>);

#[derive(Debug, PartialEq, Eq)]
struct BgaData {
    images: Vec<BgaImage>,
    base: Vec<BgaEvent>,
    layer: Vec<BgaEvent>,
    poor: Vec<PoorSequence>,
}

impl ChartBga {
    /// The BGA of these parts: definitions in increasing id order, events
    /// and sequences in time order, every image index naming a definition
    /// and every sequence holding a frame.
    ///
    /// # Errors
    ///
    /// The first part that breaks one of those rules.
    pub fn new(
        images: Vec<BgaImage>,
        base: Vec<BgaEvent>,
        layer: Vec<BgaEvent>,
        poor: Vec<PoorSequence>,
    ) -> Result<Self, BgaError> {
        if let Some(index) = images
            .iter()
            .enumerate()
            .position(|(i, image)| image.id > MAX_ID || i > 0 && images[i - 1].id >= image.id)
        {
            return Err(BgaError::Unsorted {
                part: BgaPart::Images,
                index,
            });
        }
        let known =
            |image: Option<BgaImageId>| image.is_none_or(|i| usize::from(i.0) < images.len());
        for (part, events) in [(BgaPart::Base, &base), (BgaPart::Layer, &layer)] {
            for (index, event) in events.iter().enumerate() {
                if index > 0 && event.time < events[index - 1].time {
                    return Err(BgaError::Unsorted { part, index });
                }
                if !known(event.image) {
                    return Err(BgaError::UnknownImage { part, index });
                }
            }
        }
        for (index, sequence) in poor.iter().enumerate() {
            if index > 0 && sequence.time < poor[index - 1].time {
                return Err(BgaError::Unsorted {
                    part: BgaPart::Poor,
                    index,
                });
            }
            if sequence.frames.is_empty() {
                return Err(BgaError::EmptySequence { index });
            }
            if !sequence.frames.iter().all(|&f| known(f)) {
                return Err(BgaError::UnknownImage {
                    part: BgaPart::Poor,
                    index,
                });
            }
        }
        Ok(Self(Arc::new(BgaData {
            images,
            base,
            layer,
            poor,
        })))
    }

    /// The `#BMPxx` definitions in increasing id order, the last
    /// definition of an id kept.
    #[must_use]
    pub fn images(&self) -> &[BgaImage] {
        &self.0.images
    }

    /// The definition `id` names.
    #[must_use]
    pub fn image(&self, id: BgaImageId) -> Option<&BgaImage> {
        self.0.images.get(usize::from(id.0))
    }

    /// The base (channel 04) events in time order.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn base(&self) -> &[BgaEvent] {
        &self.0.base
    }

    /// The layer (channel 07) events in time order.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn layer(&self) -> &[BgaEvent] {
        &self.0.layer
    }

    /// The poor sequences (channel 06) in time order, one a measure at
    /// most.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn poor(&self) -> &[PoorSequence] {
        &self.0.poor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: u16) -> BgaImage {
        BgaImage {
            id,
            name: format!("{id}.bmp"),
        }
    }

    fn event(time: i64, image: Option<u16>) -> BgaEvent {
        BgaEvent {
            time: TimeUs(time),
            image: image.map(BgaImageId),
        }
    }

    #[test]
    fn new_refuses_what_consumers_cannot_walk() {
        let ok = ChartBga::new(
            vec![image(1), image(2)],
            vec![event(0, Some(0)), event(0, None), event(5, Some(1))],
            vec![],
            vec![PoorSequence {
                time: TimeUs(3),
                frames: vec![None, Some(BgaImageId(1))].into(),
            }],
        )
        .unwrap();
        assert_eq!(ok.image(BgaImageId(1)).map(|i| i.id), Some(2));
        assert_eq!(ok.base().len(), 3);
        assert_eq!(
            ChartBga::new(vec![image(2), image(2)], vec![], vec![], vec![]),
            Err(BgaError::Unsorted {
                part: BgaPart::Images,
                index: 1
            })
        );
        assert_eq!(
            ChartBga::new(vec![image(MAX_ID + 1)], vec![], vec![], vec![]),
            Err(BgaError::Unsorted {
                part: BgaPart::Images,
                index: 0
            })
        );
        assert_eq!(
            ChartBga::new(vec![], vec![], vec![event(2, None), event(1, None)], vec![]),
            Err(BgaError::Unsorted {
                part: BgaPart::Layer,
                index: 1
            })
        );
        assert_eq!(
            ChartBga::new(vec![image(1)], vec![event(0, Some(1))], vec![], vec![]),
            Err(BgaError::UnknownImage {
                part: BgaPart::Base,
                index: 0
            })
        );
        let empty = PoorSequence {
            time: TimeUs(0),
            frames: Box::new([]),
        };
        assert_eq!(
            ChartBga::new(vec![], vec![], vec![], vec![empty]),
            Err(BgaError::EmptySequence { index: 0 })
        );
    }
}
