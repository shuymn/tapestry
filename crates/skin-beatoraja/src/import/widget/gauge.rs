//! The gauge (`JsonSkinObjectLoader.java:509-556`, `play/SkinGauge.java`
//! at `0.8.9`): its nodes mapped to the model's looks and parts, its
//! motion, and its segments.

use bms_model::TimeUs;
use skin_model::{Filter, GaugeDef, GaugeParts, MAX_KEY_US, MAX_SEGMENTS, Motion, Source};

use super::super::policy::{CriticalPart, Role};
use super::super::report::{DeviationId, NoteKind, Severity};
use super::super::{Cx, Dst, Opts};
use crate::error::ImportError;

/// The source's 36 node indices: `look·6 + part + below`, part 0 front,
/// 2 back, 4 tip (`SkinGauge.java:179, 190-191`).
const NODES: usize = 36;

/// For 4, 8 and 12 nodes, the indices each node fills
/// (`JsonSkinObjectLoader.java:510-519`); 36 nodes fill their own.
fn index_map(nodes: usize) -> Option<&'static [&'static [usize]]> {
    const FOUR: [&[usize]; 4] = [
        &[0, 4, 6, 10, 12, 16, 18, 22, 24, 28, 30, 34],
        &[1, 5, 7, 11, 13, 17, 19, 23, 25, 29, 31, 35],
        &[2, 8, 14, 20, 26, 32],
        &[3, 9, 15, 21, 27, 33],
    ];
    const EIGHT: [&[usize]; 8] = [
        &[12, 16, 18, 22],
        &[13, 17, 19, 23],
        &[14, 20],
        &[15, 21],
        &[0, 4, 6, 10, 24, 28, 30, 34],
        &[1, 5, 7, 11, 25, 29, 31, 35],
        &[2, 8, 26, 32],
        &[3, 9, 27, 33],
    ];
    const TWELVE: [&[usize]; 12] = [
        &[12, 18],
        &[13, 19],
        &[14, 20],
        &[15, 21],
        &[0, 6, 24, 30],
        &[1, 7, 25, 31],
        &[2, 8, 26, 32],
        &[3, 9, 27, 33],
        &[16, 22],
        &[17, 23],
        &[4, 10, 28, 34],
        &[5, 11, 29, 35],
    ];
    match nodes {
        4 => Some(&FOUR),
        8 => Some(&EIGHT),
        12 => Some(&TWELVE),
        _ => None,
    }
}

/// The node filling each of the 36 indices, for `nodes` nodes; `None` for
/// a count the source does not index.
pub(super) fn node_of(nodes: usize) -> Option<[usize; NODES]> {
    if nodes == NODES {
        return Some(std::array::from_fn(|i| i));
    }
    let map = index_map(nodes)?;
    let mut out = [usize::MAX; NODES];
    for (node, indices) in map.iter().enumerate() {
        for &i in *indices {
            *out.get_mut(i)? = node;
        }
    }
    out.iter().all(|&n| n < nodes).then_some(out)
}

/// The motion of `kind` with `range` and `cycle` ms
/// (`SkinGauge.java:110-132`), or why it is refused; `Ok(None)` for a
/// kind the source draws nothing for (`:185-213`).
fn motion(kind: i32, range: i32, cycle: i32, parts: i32) -> Result<Option<Motion>, &'static str> {
    let period = || {
        let us = i64::from(cycle) * 1000;
        if cycle <= 0 || us > MAX_KEY_US {
            Err("cycle")
        } else {
            Ok(TimeUs(us))
        }
    };
    let moving = |make: fn(u16, TimeUs) -> Motion| {
        if range > parts {
            return Err("range");
        }
        let r = u16::try_from(range).map_err(|_| "range")?;
        Ok(Some(make(r, period()?)))
    };
    match (kind, range) {
        // `(int) (random · (range + 1))` is 0 for −2 ≤ range ≤ 0, and
        // `% (range + 1)` is 0 for range −2 (`% −1`); range −1 divides
        // Rise and Fall by zero.
        (0, -2..=0) | (1 | 2, -2 | 0) => Ok(Some(Motion::Still)),
        (1 | 2, -1) => Err("range"),
        // A negative phase the model's range cannot hold.
        (0..=2, ..=-3) => Err("range"),
        (0, _) => moving(|range, step| Motion::Random { range, step }),
        (1, _) => moving(|range, step| Motion::Rise { range, step }),
        (2, _) => moving(|range, step| Motion::Fall { range, step }),
        (3, _) => Ok(Some(Motion::Flicker { period: period()? })),
        _ => Ok(None),
    }
}

impl Cx<'_> {
    /// A gauge destination.
    pub(in super::super) fn gauge(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let part = CriticalPart::Gauge;
        let role = Role::Critical(part);
        let refuse = |cx: &Self, field: &'static str| {
            Err(ImportError::Critical {
                at: cx.loc(at),
                part,
                why: NoteKind::UnsupportedField { field },
            })
        };
        let [nodes, parts, kind, range, cycle, start, end] = self.def.fields(
            rec,
            [
                "nodes",
                "parts",
                "type",
                "range",
                "cycle",
                "starttime",
                "endtime",
            ],
        );
        if nodes.is_none() {
            return refuse(self, "nodes");
        }
        let nodes = self.strings(nodes, "nodes")?;
        let Some(node_of) = node_of(nodes.len()) else {
            return refuse(self, "nodes");
        };
        let parts = self.int_or(parts, "parts", 50)?;
        let kind = self.int_or(kind, "type", 0)?;
        let range = self.int_or(range, "range", 3)?;
        let cycle = self.int_or(cycle, "cycle", 33)?;
        if start.is_some() || end.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "starttime" });
        }
        if parts <= 0 {
            self.ignored(at, NoteKind::Ignored { field: "parts" });
            return Ok(());
        }
        if parts > i32::from(MAX_SEGMENTS) {
            return refuse(self, "parts");
        }
        let motion = match motion(kind, range, cycle, parts) {
            Ok(Some(m)) => m,
            Ok(None) => {
                self.ignored(at, NoteKind::Ignored { field: "type" });
                return Ok(());
            }
            Err(field) => return refuse(self, field),
        };
        // The gauge draws unrotated (`SkinGauge.java:189-192`) and without
        // stretch or filter of its own.
        let o = Opts {
            unrotated: true,
            ..Opts::default()
        };
        let Some(mut c) = self.common_with(role, &dst, o)? else {
            return Ok(());
        };
        if dst.stretch != -1 {
            self.ignored(at, NoteKind::Ignored { field: "stretch" });
        }
        if dst.filter != 0 {
            self.ignored(at, NoteKind::Ignored { field: "filter" });
        }
        // Frame 0 drawn nearest (`SkinGauge.java:183`).
        c.filter = Filter::Nearest;

        // The nodes: frame 0 of each (`SkinSourceImageSet.java:71-73`),
        // every one at least as many frames as the last.
        let mut images = Vec::with_capacity(nodes.len());
        let mut frames = Vec::with_capacity(nodes.len());
        for name in &nodes {
            let Some(img) = self.image_rec(name)? else {
                return refuse(self, "nodes");
            };
            let s = self.src_image(img)?;
            frames.push(self.grid_frames(at, s.div)?);
            images.push((img, s));
        }
        let last = frames.last().copied().unwrap_or(0);
        if frames.iter().any(|&f| f < last) {
            return refuse(self, "nodes");
        }
        let mut admitted = Vec::with_capacity(images.len());
        for (img, s) in &images {
            let Some((image, _)) = self.add_src_image(&c, *img, s, [1, 1])? else {
                return refuse(self, "nodes");
            };
            admitted.push(image);
        }
        let at_index = |i: usize| {
            node_of
                .get(i)
                .and_then(|&n| admitted.get(n))
                .copied()
                .unwrap_or(0)
        };
        let looks = std::array::from_fn(|look| {
            let base = look * 6;
            let pair = |p: usize| [at_index(base + p), at_index(base + p + 1)];
            GaugeParts {
                front: pair(0),
                back: pair(2),
                tip: pair(4),
            }
        });
        let segments = u16::try_from(parts).map_err(|_| self.range(at, "parts"))?;
        let def = GaugeDef {
            segments,
            looks,
            motion,
        };
        let tip = u32::from(matches!(motion, Motion::Flicker { .. }));
        self.budget
            .widget_quads(u32::from(segments) + tip)
            .map_err(|l| self.limit(at, l))?;
        let mut rects = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            rects.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
        }
        let t = self.model_track(&c, &rects)?;
        self.note(
            Some(at),
            Severity::Deviation,
            NoteKind::Deviation(DeviationId::D10),
        );
        let index = u32::try_from(self.widgets.gauges.len()).unwrap_or(u32::MAX);
        self.widgets.gauges.push(def);
        self.push(c, Source::Gauge(index), t);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables, pinned by inverting them: each index's node, as the
    /// source lists them.
    #[test]
    fn node_tables_invert() {
        let four = node_of(4).unwrap();
        // Front and tip from nodes 0 (above) and 1 (below), back from 2
        // and 3, for every look.
        for look in 0..6 {
            let b = look * 6;
            assert_eq!(&four[b..b + 6], [0, 1, 2, 3, 0, 1]);
        }
        let eight = node_of(8).unwrap();
        assert_eq!(&eight[..6], [4, 5, 6, 7, 4, 5]);
        assert_eq!(&eight[12..18], [0, 1, 2, 3, 0, 1]);
        assert_eq!(&eight[24..30], [4, 5, 6, 7, 4, 5]);
        let twelve = node_of(12).unwrap();
        assert_eq!(&twelve[..6], [4, 5, 6, 7, 10, 11]);
        assert_eq!(&twelve[12..18], [0, 1, 2, 3, 8, 9]);
        assert_eq!(node_of(36).unwrap()[35], 35);
        assert_eq!(node_of(5), None);
    }

    #[test]
    fn motions_follow_the_source() {
        let us = |ms: i64| TimeUs(ms * 1000);
        assert_eq!(
            motion(0, 3, 33, 50),
            Ok(Some(Motion::Random {
                range: 3,
                step: us(33)
            }))
        );
        assert_eq!(
            motion(1, 3, 33, 50),
            Ok(Some(Motion::Rise {
                range: 3,
                step: us(33)
            }))
        );
        assert_eq!(
            motion(2, 3, 33, 50),
            Ok(Some(Motion::Fall {
                range: 3,
                step: us(33)
            }))
        );
        assert_eq!(
            motion(3, -5, 40, 50),
            Ok(Some(Motion::Flicker { period: us(40) }))
        );
        for kind in 0..=2 {
            assert_eq!(motion(kind, 0, 0, 50), Ok(Some(Motion::Still)), "{kind}");
            assert_eq!(motion(kind, -2, 33, 50), Ok(Some(Motion::Still)), "{kind}");
            assert_eq!(motion(kind, -3, 33, 50), Err("range"), "{kind}");
        }
        assert_eq!(motion(0, -1, 33, 50), Ok(Some(Motion::Still)));
        assert_eq!(motion(1, -1, 33, 50), Err("range"));
        assert_eq!(motion(2, -1, 33, 50), Err("range"));
        assert_eq!(motion(4, 3, 33, 50), Ok(None));
        assert_eq!(motion(0, 51, 33, 50), Err("range"));
        assert_eq!(
            motion(0, 50, 33, 50),
            Ok(Some(Motion::Random {
                range: 50,
                step: us(33)
            }))
        );
        assert_eq!(motion(3, 3, 0, 50), Err("cycle"));
        assert_eq!(motion(1, 2, 0, 50), Err("cycle"));
        let max = i32::try_from(MAX_KEY_US / 1000).unwrap();
        assert!(motion(3, 3, max, 50).is_ok());
        assert_eq!(motion(3, 3, max + 1, 50), Err("cycle"));
    }

    /// Java's `%` keeps the dividend's sign: the phases for range −2 are
    /// all 0, and range −3 reaches −1 (Random, Rise) or 1 (Fall).
    #[test]
    fn negative_ranges_follow_java_arithmetic() {
        let rise = |range: i32| {
            let mut a = 0_i32;
            (0..6)
                .map(|_| {
                    a = (a + range) % (range + 1);
                    a
                })
                .collect::<Vec<_>>()
        };
        let fall = |range: i32| {
            let mut a = 0_i32;
            (0..6)
                .map(|_| {
                    a = (a + 1) % (range + 1);
                    a
                })
                .collect::<Vec<_>>()
        };
        assert!(rise(-2).iter().all(|&a| a == 0));
        assert!(fall(-2).iter().all(|&a| a == 0));
        assert!(rise(-3).contains(&-1));
        assert!(fall(-3).contains(&1));
        // Random truncates toward zero: (−1, 0] for range −2, (−2, 0] for
        // range −3.
        #[expect(clippy::cast_possible_truncation, reason = "Java's (int)")]
        let random = |range: i32, x: f64| (x * f64::from(range + 1)) as i32;
        assert_eq!(random(-2, 0.999), 0);
        assert_eq!(random(-3, 0.999), -1);
    }
}
