//! The timeline's construction charge measured (design.md "Chart Model",
//! "Timeline"): the rows, bar rows and mapped times a load allocates are
//! the counted sizes, and the load's peak and retained bytes are reported
//! by the ignored `measure_` test (`cargo test --release -p bms-parser
//! --test memory -- --ignored --nocapture`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use bms_model::{Speed, TimelineId, TimelinePoint};
use bms_parser::{LoadOptions, Loaded, load_bms};

/// Tracks live and peak bytes of every allocation.
struct Peak;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delegates to `System`; only adds counters, which never allocate.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size
                    - layout.size();
                // The old and new blocks may both be live while copying.
                PEAK.fetch_max(live + layout.size(), Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Peak = Peak;

/// A chart of `stops` positions each holding a STOP, and a tap with
/// `taps`, 600 to a measure from measure 1, then a last tap: two rows per
/// position.
fn stop_heavy(stops: usize, taps: bool) -> String {
    // 1000 BPM keeps the near-cap chart within the duration limit.
    let mut text = String::from("#BPM 1000\n#WAV01 a.wav\n#STOP01 1\n");
    let per = 600;
    let measures = stops.div_ceil(per);
    for m in 1..=measures {
        let n = per.min(stops - (m - 1) * per);
        let pairs = "01".repeat(n);
        text.push_str(&format!("#{m:03}09:{pairs}\n"));
        if taps {
            text.push_str(&format!("#{m:03}11:{pairs}\n"));
        }
    }
    text.push_str(&format!("#{:03}12:01\n", measures + 1));
    text
}

/// Loads `text` at `speed`, returning the chart with the load's peak and
/// retained bytes above what was live before it.
fn measured(text: &str, speed: Speed) -> (Loaded, usize, usize) {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        speed,
        ..LoadOptions::default()
    };
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let loaded = load_bms(text.as_bytes(), dir.path(), &opts).unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    let retained = LIVE.load(Ordering::Relaxed) - base;
    (loaded, peak, retained)
}

#[test]
fn the_timeline_is_held_at_its_counted_size() {
    let text = stop_heavy(2_000, true);
    for speed in [Speed::NORMAL, Speed::SLOWEST] {
        let (loaded, ..) = measured(&text, speed);
        let t = &loaded.chart.timeline;
        // Measure 0's bar line, then per measure its bar line with the
        // first position, two rows a position, and the last tap's measure.
        // 4 002 rows of 48 bytes and 6 bar rows of 4: the charge's sizes.
        assert_eq!(t.points().len(), 1 + 2 * 2_000 + 1);
        assert_eq!(size_of_val(t.points()), 192_096);
        assert_eq!(size_of_val(t.bar_ids()), 24);
    }
}

#[test]
#[ignore = "a measurement: run in release with --ignored --nocapture"]
fn measure_load_peaks() {
    for (name, stops, taps) in [
        ("50,000 STOPs and taps", 50_000, true),
        ("499,999 STOPs, near the row cap", 499_999, false),
    ] {
        let text = stop_heavy(stops, taps);
        for speed in [Speed::NORMAL, Speed::SLOWEST] {
            let (loaded, peak, retained) = measured(&text, speed);
            let t = &loaded.chart.timeline;
            let rows = t.points().len();
            let bars = t.bar_ids().len();
            let notes: usize = loaded.chart.lanes.iter().map(Vec::len).sum();
            println!(
                "{name} at {}/20: {rows} rows, {bars} bars, {notes} notes; \
                 rows {} B, bar rows {} B, mapped scratch {} B; \
                 load peak {peak} B, retained {retained} B",
                speed.twentieths(),
                size_of_val(t.points()),
                size_of_val(t.bar_ids()),
                if speed == Speed::NORMAL { 0 } else { rows * 8 },
            );
        }
    }
    println!(
        "sizes: TimelinePoint {} B, TimelineId {} B, Note {} B, Chart {} B",
        size_of::<TimelinePoint>(),
        size_of::<TimelineId>(),
        size_of::<bms_model::Note>(),
        size_of::<bms_model::Chart>()
    );
}
