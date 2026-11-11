//! A BPM graph's preparation is bounded by its tempo rows and its stroke
//! width, not by the rows times the columns each stroke covers (design.md
//! "Threat Model" T08; s10-10-design.md C32): a chart of 60 000 tempo
//! changes over a 2 048-column graph at the widest stroke, 16 texels,
//! prepares with its live heap peaking under 24 MiB above where it began.
//! And a stroke past 16 texels is refused at load.

mod play_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use bms_model::{ChartBuilder, ChartProfile, ChartSummary, Tempo, TimeUs};
use skin_model::{
    Anchor, Blend, BpmColors, BpmGraphDef, ElementDef, Filter, Fit, KeyDef, LoadError,
    MAX_BPM_LINE_WIDTH, Place, PlayContext, PlayDefs, PlayScore, Region, RegionDef, Root,
    SheetBuilder, Skin, SkinDef, SolidImageDef, Source, TimerRef, Track, VisualizerError, timer,
};

use play_support::{MS, full, lane};

/// The system allocator, counting the live bytes and their peak.
struct Peak;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the same arguments; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `System::alloc`, with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: as `System::dealloc`, with the caller's pointer and layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Peak = Peak;

/// A play skin of one BPM graph over `[0, 0, 2048, 1000]` with strokes
/// `line_width` texels wide.
fn skin(line_width: u32) -> Result<Skin, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    b.add_solid_image(SolidImageDef {
        size: [8, 8],
        color: [255; 4],
        filter: Filter::Nearest,
        grid: None,
    })
    .unwrap();
    let mut props = [None; 9];
    for (p, v) in props
        .iter_mut()
        .zip([0, 0, 2_048, 1_000, 255, 255, 255, 255, 0])
    {
        *p = Some(v);
    }
    let element = ElementDef {
        source: Source::BpmGraph(0),
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            skin_model::Ease::Linear,
            skin_model::End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place {
            pivot: Anchor::TopLeft,
            ..Place::SCREEN
        },
    };
    Skin::new_play(
        SkinDef {
            fit: Fit::Fixed,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet: b.finish(),
            lanes: None,
            elements: vec![element],
            widgets: skin_model::WidgetDefs::default(),
            bomb_up_to: None,
            close: None,
            loadend: None,
            present: None,
        },
        PlayDefs {
            bpm_graphs: vec![BpmGraphDef {
                delay: TimeUs(0),
                line_width,
                colors: BpmColors::DEFAULT,
                solid: 0,
            }],
            ..PlayDefs::default()
        },
    )
}

/// 60 000 tempo changes 1 ms apart, alternating 300 and 150 BPM, after
/// five notes at 150 (MAIN), and a note at the end.
#[test]
fn a_bpm_graph_prepares_within_its_rows_and_stroke() {
    let mut b = ChartBuilder::new(150_000);
    for k in 1..=5 {
        b = b.tap(lane(1), TimeUs(100 * k * MS));
    }
    for k in 0..60_000_i64 {
        let bpm = if k % 2 == 0 { 300_000 } else { 150_000 };
        b = b.bpm(TimeUs((1_000 + k) * MS), bpm);
    }
    let mut chart = b.tap(lane(2), TimeUs(62_000 * MS)).build().unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let profile = ChartProfile::of(&chart, &[]);
    assert!(profile.speeds().len() > 60_000);
    let skin = skin(MAX_BPM_LINE_WIDTH).unwrap();
    let ctx = PlayContext {
        score: PlayScore {
            profile: Some(&profile),
            main: Some(Tempo::from_units(150 * Tempo::UNITS_PER_BPM).unwrap()),
            ..PlayScore::NONE
        },
        ..full(0)
    };
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let play = skin.prepare(&chart, &ctx).unwrap();
    let peak = PEAK.load(Ordering::SeqCst) - base;
    drop(play);
    assert!(
        peak < 24 << 20,
        "the preparation peaked {peak} bytes above its start"
    );
}

/// C32: the model refuses a BPM graph whose stroke passes 16 texels.
#[test]
fn a_stroke_past_16_texels_is_refused() {
    assert!(skin(MAX_BPM_LINE_WIDTH).is_ok());
    assert!(matches!(
        skin(MAX_BPM_LINE_WIDTH + 1),
        Err(LoadError::BpmGraph {
            graph: 0,
            error: VisualizerError::LineWidth
        })
    ));
}
