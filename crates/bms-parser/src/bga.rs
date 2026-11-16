//! The chart's BGA structure ([`ChartBga`], crate doc "Presentation
//! reads"): its `#BMPxx` definitions, base and layer events and poor
//! sequences, which the play chart neither plays nor judges.
//!
//! They are timed by the loader's own timing over the chart's objects and
//! the BGA's together, a timeline of their own apart from the play chart's,
//! as a preview's are ([`crate::preview`]), so the play chart's timeline,
//! rows, length and identity are what they would be without them. That
//! timeline is held to the same limits, the 60-minute duration among them:
//! one past a limit leaves the chart without BGA, and the chart loads.

use std::collections::BTreeMap;

use bms_model::{BgaEvent, BgaImage, BgaImageId, ChartBga, PoorSequence, Speed, TimelineId};

use crate::Limits;
use crate::parse::{ObjectKind, RawObject, Source};
use crate::timing::{self, Tail};

/// The BGA structure of `source` at `speed`; `None` when it has no
/// definition and no event, or when timing its events fails.
pub fn read(source: &Source, limits: &Limits, speed: Speed) -> Option<ChartBga> {
    if source.bmp.is_empty()
        && source.bga_base.is_empty()
        && source.bga_layer.is_empty()
        && source.poor.is_empty()
    {
        return None;
    }
    let mut ids = BTreeMap::new();
    let images = source
        .bmp
        .iter()
        .zip(0_u16..)
        .map(|((&id, name), index)| {
            ids.insert(id, BgaImageId(index));
            BgaImage {
                id,
                name: name.clone(),
            }
        })
        .collect();
    let image = |id: u16| ids.get(&id).copied();

    let first_base = source.objects.len();
    let first_layer = first_base + source.bga_base.len();
    let first_poor = first_layer + source.bga_layer.len();
    let mut objects = Vec::with_capacity(first_poor + source.poor.len());
    objects.extend_from_slice(&source.objects);
    objects.extend_from_slice(&source.bga_base);
    objects.extend_from_slice(&source.bga_layer);
    // A sequence sits at its measure's start; its frames ride by measure.
    let poor: Vec<_> = source.poor.iter().collect();
    objects.extend(poor.iter().map(|&(&measure, _)| RawObject {
        measure,
        index: 0,
        count: 1,
        kind: ObjectKind::Bgm(0),
        line: 0,
    }));
    let both = Source {
        bpm: source.bpm.clone(),
        bpm_defs: source.bpm_defs.clone(),
        stop_defs: source.stop_defs.clone(),
        measure_lengths: source.measure_lengths.clone(),
        objects,
        ..Source::default()
    };
    // Its warnings repeat the play chart's. The BGA objects are BGM-like,
    // so the dropped tail starts after the last of them.
    let timed = timing::compute_with(&both, limits, speed, Tail::Dropped, &mut Vec::new()).ok()?;
    drop(both);

    let (mut base, mut layer, mut sequences) = (Vec::new(), Vec::new(), Vec::new());
    // Each layer's last event and its row: at one position (one row) the
    // later object in the file replaces the earlier.
    let push =
        |events: &mut Vec<(BgaEvent, TimelineId)>, event: BgaEvent, row: TimelineId| match events
            .last_mut()
        {
            Some(last) if last.1 == row => *last = (event, row),
            _ => events.push((event, row)),
        };
    for o in &timed.objects {
        let ObjectKind::Bgm(id) = o.kind else {
            continue;
        };
        if o.seq >= first_poor {
            let Some(&(_, frames)) = poor.get(o.seq - first_poor) else {
                continue;
            };
            sequences.push(PoorSequence {
                time: o.real,
                frames: frames.iter().map(|frame| frame.and_then(image)).collect(),
            });
        } else if o.seq >= first_base {
            let event = BgaEvent {
                time: o.real,
                image: image(id),
            };
            let events = if o.seq >= first_layer {
                &mut layer
            } else {
                &mut base
            };
            push(events, event, o.id);
        }
    }
    let strip = |events: Vec<(BgaEvent, TimelineId)>| events.into_iter().map(|e| e.0).collect();
    // Sorted by construction: a failure is a loader bug, which leaves the
    // chart without BGA rather than failing a load for what is only shown;
    // debug builds, the fuzzer's included, stop on it.
    let bga = ChartBga::new(images, strip(base), strip(layer), sequences);
    debug_assert!(
        bga.is_ok(),
        "the loader built an invalid BGA structure: {bga:?}"
    );
    bga.ok()
}
