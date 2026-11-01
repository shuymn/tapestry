//! The import's memory, measured (design.md "Memory Budget"): a counting
//! allocator keeps the live bytes and their peak, a reallocation counted
//! as a new buffer, a copy and a free so both buffers are live at once.
//! Each fixture reaches the caps it names, asserted through
//! `Report::usage` and what the skin keeps, and its peak over
//! `read_header` and `import`, with the `Header` and `Definition` kept,
//! stays within the recorded bound.
//!
//! Only the thread that opts in is counted, and this binary runs its
//! fixtures in one test, one after another.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use common::play::{Drawn, judged};
use common::{font, join, png, write};
use rules::{GaugeValue, Judgment};
use skin_beatoraja::limits::{
    MAX_FILE_CHOICES, MAX_INPUT_BYTES, MAX_LIST_ENTRIES, MAX_NODES, MAX_NOTES, MAX_OFFSETS,
    MAX_OPTION_ITEMS, MAX_OPTIONS,
};
use skin_beatoraja::{
    Config, Header, ImportError, Imported, InputLimit, Note, Report, import, read_header,
};
use skin_model::{
    ElementDef, ImageChoiceDef, ImageChoicePart, ImageDef, KeyDef, MAX_ELEMENTS, MAX_FILES,
    MAX_FNT_BYTES, MAX_FNT_PAGES, MAX_FONTS, MAX_IMAGE_CHOICE_REFS, MAX_SHEET_GLYPHS,
    MAX_SHEET_KERNINGS, MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS, MAX_TEXT_ELEMENTS,
    MAX_WIDGET_QUADS, Program,
};

/// The recorded bound: the largest measured peak over the fixtures plus
/// 25 %.
const BOUND: i64 = 153_286_620;

struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn tracked() -> bool {
    TRACK.try_with(Cell::get).unwrap_or(false)
}

fn grow(n: usize) {
    let live = LIVE.fetch_add(i64::try_from(n).unwrap_or(i64::MAX), Ordering::Relaxed)
        + i64::try_from(n).unwrap_or(i64::MAX);
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn shrink(n: usize) {
    LIVE.fetch_sub(i64::try_from(n).unwrap_or(i64::MAX), Ordering::Relaxed);
}

// SAFETY: delegates to `System`; only counts sizes, which never
// allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if tracked() {
            grow(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if tracked() {
            shrink(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if tracked() {
            // The new buffer, then the old freed: both live at the peak.
            grow(new_size);
            shrink(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// `f`'s peak live bytes on this thread above where it started.
fn peak<T>(f: impl FnOnce() -> T) -> (T, i64) {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    TRACK.with(|t| t.set(false));
    (out, PEAK.load(Ordering::Relaxed))
}

/// Reads and imports `s.json` of `root` within the window, the header
/// and definition kept with the import, then checks them with `check`
/// outside it.
fn measure(root: &Path, check: impl FnOnce(&Header, Imported)) -> i64 {
    let ((def, header, imported), p) = peak(|| {
        let (mut def, header) = read_header(root, "s.json").unwrap();
        let imported = import(&mut def, &header, &Config::defaults(&header)).unwrap();
        (def, header, imported)
    });
    check(&header, imported);
    drop(def);
    p
}

/// What importing `s.json` of `root` stops at.
fn refused(root: &Path) -> ImportError {
    let (mut def, header) = read_header(root, "s.json").unwrap();
    import(&mut def, &header, &Config::defaults(&header)).unwrap_err()
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap()
}

/// The ops one destination with `ops` costs.
fn ops_of(ops: &str) -> u32 {
    let d = common::skin(&[(
        "s.json",
        &common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}],
               "image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
               "destination": [{{"id": "i", "op": [{ops}], "dst": [{{"w": 1, "h": 1}}]}}]"#
        )),
    )]);
    png(d.path(), "a.png", 1, 1);
    common::import_dir(d.path()).unwrap().report.usage.ops
}

/// F1: 16,384 elements of 4 keys, each its own four-frame image of 16
/// files, 65,536 ops, a note each, in 1,024 included files.
fn f1(root: &Path) {
    // Two condition lists around 4 ops: x elements of the cheaper and the
    // rest of the dearer reach 65,536 exactly. Every supported condition
    // with a judge region's 241 aside (no element is left for a judge)
    // costs more than 4, so the cheaper is none.
    let lists = [
        "",
        "232",
        "-233",
        "232,-233",
        "-231,232",
        "232,-233,-234",
        "-231,232,-233",
    ];
    let costs: Vec<(String, u32)> = lists.iter().map(|l| ((*l).to_owned(), ops_of(l))).collect();
    let n = as_u32(MAX_ELEMENTS);
    let total = as_u32(MAX_SKIN_OPS);
    let (lo, hi, dear) = costs
        .iter()
        .flat_map(|a| costs.iter().map(move |b| (a, b)))
        .find_map(|(a, b)| {
            let (ka, kb) = (a.1, b.1);
            (ka < kb && ka * n <= total && (total - ka * n).is_multiple_of(kb - ka))
                .then(|| (a.0.clone(), b.0.clone(), (total - ka * n) / (kb - ka)))
                .filter(|&(.., d)| d <= n)
        })
        .unwrap_or_else(|| panic!("no mix of {costs:?}"));
    for i in 0..16 {
        png(root, &format!("s{i}.png"), 64, 64);
    }
    let sources = join((0..16).map(|i| format!(r#"{{"id": "s{i}", "path": "s{i}.png"}}"#)));
    let images = join((0..MAX_ELEMENTS).map(|i| {
        format!(
            r#"{{"id": "{i}", "src": "s{}", "x": {}, "w": 32, "h": 32, "divx": 2, "divy": 2}}"#,
            i % 16,
            (i / 16) % 32
        )
    }));
    let per = MAX_ELEMENTS / 1_024;
    for f in 0..1_024 {
        let dsts = join((0..per).map(|k| {
            let i = f * per + k;
            let ops = if u32::try_from(i).unwrap() < dear {
                &hi
            } else {
                &lo
            };
            format!(
                r#"{{"id": "{i}", "op": [{ops}], "loop": 3, "draw": 900, "dst": [
                    {{"time": 0, "w": 1, "h": 1}}, {{"time": 1, "x": 1}},
                    {{"time": 2, "x": 3}}, {{"time": 3, "x": 2}}]}}"#
            )
        }));
        write(root, &format!("d{f}.json"), format!("[{dsts}]").as_bytes());
    }
    let includes = join((0..1_024).map(|f| format!(r#"{{"include": "d{f}.json"}}"#)));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{sources}], "image": [{images}], "destination": [{includes}]"#
        ))
        .as_bytes(),
    );
}

fn check_f1(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.elements, as_u32(MAX_ELEMENTS));
    assert_eq!(u.keys, as_u32(MAX_SKIN_KEYS));
    assert_eq!(u.ops, as_u32(MAX_SKIN_OPS));
    assert_eq!(u.images, as_u32(MAX_ELEMENTS));
    assert_eq!(u.frames, as_u32(MAX_SKIN_FRAMES));
    assert_eq!(u.files, 16);
    assert_eq!(i.report.notes.len(), MAX_NOTES);
    assert!(i.report.suppressed > 0);
    assert_eq!(i.report.sources.len(), 1_025);
    assert_eq!(i.skin.ops().total(), MAX_SKIN_OPS);
    assert_eq!(i.skin.sheet().image_count(), MAX_ELEMENTS);
    assert_eq!(i.skin.sheet().frame_count(), MAX_SKIN_FRAMES);
    assert_eq!(i.skin.quad_capacity(), as_u32(MAX_ELEMENTS));
    // Every element keeps its four keys: with the gauge at 25 % every
    // condition holds, and each key's x shows at
    // its time (x 0, 1, 3, 2: no key lies on its neighbours' line).
    let mut d = Drawn::new(i.skin, 100);
    d.gauges[0].value = GaugeValue(25 * GaugeValue::PERCENT);
    let effects = [judged(0, Judgment::Perfect, 1)];
    for (ms, x) in [(0, 0_i16), (1, 1), (2, 3), (3, 2)] {
        let quads = d.frame(ms, if ms == 0 { &effects } else { &[] });
        assert_eq!(quads.len(), MAX_ELEMENTS, "{ms} ms");
        assert!(quads.iter().all(|q| q.rect[0] == f32::from(x)), "{ms} ms");
    }
}

/// The numbers F2 rotates through, all D4: the six rate aliases (3
/// operations each), then the sixteen readouts, 42 operations with their
/// availability guards; 60 a cycle.
const ROTATION: [i32; 22] = [
    102, 103, 115, 116, 155, 156, 90, 91, 160, 161, 162, 163, 164, 14, 314, 315, 316, 10, 310, 311,
    312, 313,
];

/// F2: 64 texts in 16 fonts, and rates, readouts and a count filling the
/// widget quads.
fn f2(root: &Path) {
    png(root, "n.png", 60, 6);
    for i in 0..MAX_FONTS {
        font(root, &format!("f{i}.ttf"));
    }
    let fonts = join((0..MAX_FONTS).map(|i| format!(r#"{{"id": "{i}", "path": "f{i}.ttf"}}"#)));
    let texts = join((0..MAX_TEXT_ELEMENTS).map(|i| {
        format!(
            r#"{{"id": "t{i}", "font": "{}", "constantText": "{}"}}"#,
            i % MAX_FONTS,
            "x".repeat(256)
        )
    }));
    let full = (MAX_WIDGET_QUADS as usize - MAX_TEXT_ELEMENTS - 7) / 11;
    assert_eq!(full, 1_483);
    let dsts = join(
        (0..MAX_TEXT_ELEMENTS)
            .map(|i| format!(r#"{{"id": "t{i}", "dst": [{{"w": 10, "h": 10}}]}}"#))
            .chain((0..full).map(|k| {
                format!(
                    r#"{{"id": "v11_{}", "dst": [{{"w": 6, "h": 6}}]}}"#,
                    k % ROTATION.len()
                )
            }))
            .chain([r#"{"id": "v7", "dst": [{"w": 6, "h": 6}]}"#.to_owned()]),
    );
    let rates = join(ROTATION.iter().enumerate().map(|(k, r)| {
        format!(
            r#"{{"id": "v11_{k}", "src": "n", "w": 60, "h": 6, "divx": 10, "digit": 11,
                 "ref": {r}}}"#
        )
    }));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "n", "path": "n.png"}}], "font": [{fonts}], "text": [{texts}],
               "value": [{rates},
                 {{"id": "v7", "src": "n", "w": 60, "h": 6, "divx": 10, "digit": 7, "ref": 110}}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn check_f2(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.texts, as_u32(MAX_TEXT_ELEMENTS));
    assert_eq!(u.fonts, as_u32(MAX_FONTS));
    assert_eq!(u.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(i.skin.text_slots(), MAX_TEXT_ELEMENTS);
    assert_eq!(i.skin.sheet().font_count(), MAX_FONTS);
    assert_eq!(i.skin.quad_capacity(), MAX_WIDGET_QUADS);
    // 67 whole cycles of 60 operations, then the rates and the three BPMs
    // (30), and the count's 1.
    assert_eq!(1_483, 67 * ROTATION.len() + 9);
    assert_eq!(u.ops, 4_051);
    let d4 = skin_beatoraja::NoteKind::Deviation(skin_beatoraja::DeviationId::D4);
    let deviations = i.report.notes.iter().filter(|n| n.kind == d4).count();
    assert_eq!(deviations, 1_483);
    // Drawn with every readout known: each number shows its digits and
    // nothing is dropped.
    let chart = bms_model::ChartBuilder::new(150_000)
        .tap(
            bms_model::Lane::new(1).unwrap(),
            bms_model::TimeUs(1_000_000),
        )
        .build()
        .unwrap();
    let range = skin_model::BpmRange {
        min_x100: 7_599,
        max_x100: 30_000,
    };
    let known = skin_model::FrameReadouts {
        bpm_current_x100: Some(15_075),
        hispeed_x100: Some(105),
        duration: Some(skin_model::DurationRatio {
            numerator: 6_675,
            denominator: 10,
        }),
        // All off, their amounts stored; the lanes draw none of them.
        covers: skin_model::CoverSettings {
            cover: 500,
            lift: 250,
            hidden: 120,
            cover_enabled: false,
            lift_enabled: false,
            hidden_enabled: false,
            adjusting: false,
        },
    };
    // In ROTATION's order at 0 ms, one note at 1 s: rates 100.00 and 0.00,
    // BPMs 300, 75 and 150.75, 0 s played, 1 s left, the covers, hi-speed
    // 1.05 and 667.5 ms.
    #[rustfmt::skip]
    let shown = [
        "100", "0", "0", "0", "0", "0", "300", "75", "150", "0", "0", "0", "1",
        "500", "250", "120", "375", "105", "1", "5", "668", "400",
    ];
    let mut d = Drawn::over_with(i.skin, &chart, Some(range));
    d.readouts = Some(known);
    assert_eq!(f2_numbers(&mut d), f2_expected(&shown));
    // Unknown, the tempo, hi-speed and duration numbers draw nothing.
    let mut d = Drawn::over_with(d.skin, &chart, None);
    d.readouts = Some(skin_model::FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        ..known
    });
    let mut hidden = shown;
    for k in [6, 7, 8, 17, 18, 19, 20, 21] {
        hidden[k] = "";
    }
    assert_eq!(f2_numbers(&mut d), f2_expected(&hidden));
}

/// What F2 shows when ROTATION's numbers show `shown` (empty when hidden):
/// its 1 483 numbers in turn, then the count's 0.
fn f2_expected(shown: &[&str; 22]) -> Vec<String> {
    (0..1_483)
        .map(|k| shown[k % shown.len()])
        .filter(|s| !s.is_empty())
        .chain(["0"])
        .map(str::to_owned)
        .collect()
}

/// The numbers F2's frame at 0 ms draws, in draw order, with none of its
/// quads dropped. Every image is a strip of ten digits, so a glyph's frame
/// index modulo 10 is its digit; the cells of one number advance right,
/// so a cell not right of the last starts the next number.
fn f2_numbers(d: &mut Drawn) -> Vec<String> {
    let quads = d.frame(0, &[]);
    assert_eq!(d.batch.dropped(), 0);
    let digit: std::collections::HashMap<_, _> = d
        .uvs
        .frames()
        .iter()
        .enumerate()
        .map(|(k, f)| {
            (
                f.uv.map(f32::to_bits),
                char::from_digit(u32::try_from(k % 10).unwrap(), 10).unwrap(),
            )
        })
        .collect();
    let mut numbers: Vec<String> = Vec::new();
    let mut last = f32::INFINITY;
    for q in &quads {
        if q.rect[0] <= last {
            numbers.push(String::new());
        }
        last = q.rect[0];
        numbers
            .last_mut()
            .unwrap()
            .push(digit[&q.uv.map(f32::to_bits)]);
    }
    numbers
}

/// F6: [`common::bitmap_fonts`] with each font also at the file size cap,
/// so the fonts fill the total font bytes.
fn f6(root: &Path) {
    common::bitmap_fonts(root, usize::try_from(MAX_FNT_BYTES).unwrap());
}

fn check_f6(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.texts, as_u32(MAX_TEXT_ELEMENTS));
    assert_eq!(u.fonts, as_u32(MAX_FONTS));
    assert_eq!(u.glyphs, as_u32(MAX_SHEET_GLYPHS));
    assert_eq!(u.kernings, as_u32(MAX_SHEET_KERNINGS));
    assert_eq!(u.images, as_u32(MAX_FONTS * MAX_FNT_PAGES));
    assert_eq!(u.files, as_u32(MAX_FONTS * MAX_FNT_PAGES));
    assert_eq!(u.widget_quads, as_u32(MAX_TEXT_ELEMENTS * 128));
    assert_eq!(i.report.notes, []);
    assert_eq!(i.skin.sheet().bitmap_font_count(), MAX_FONTS);
    let mut d = Drawn::new(i.skin, 10);
    d.attach_bitmap("");
    assert_eq!(d.frame(0, &[]).len(), MAX_TEXT_ELEMENTS * 128);
}

/// F7: 16,384 destinations of one four-entry image set over two one-frame
/// images, each repeated, chosen by the perfect count: each destination
/// keeps its own table, filling the choice entries.
fn f7(root: &Path) {
    png(root, "a.png", 2, 1);
    let dsts =
        join((0..MAX_ELEMENTS).map(|_| r#"{"id": "s", "dst": [{"w": 1, "h": 1}]}"#.to_owned()));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}],
               "image": [{{"id": "i", "src": "a", "w": 1, "h": 1}},
                         {{"id": "j", "src": "a", "x": 1, "w": 1, "h": 1}}],
               "imageset": [{{"id": "s", "images": ["i", "j", "i", "j"], "value": 110}}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn check_f7(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.choice_refs, as_u32(MAX_IMAGE_CHOICE_REFS));
    assert_eq!(i.skin.choice_ref_count(), MAX_IMAGE_CHOICE_REFS);
    let n = as_u32(MAX_ELEMENTS);
    assert_eq!((u.elements, u.keys, u.ops), (n, n, n));
    assert_eq!(u.widget_quads, 0);
    assert_eq!((u.images, u.frames, u.files), (2, 2, 1));
    assert_eq!(i.report.notes, []);
    assert_eq!(i.skin.quad_capacity(), n);
    assert_eq!(i.skin.ops().total(), MAX_ELEMENTS);
    assert_eq!(i.skin.sheet().image_count(), 2);
}

/// F8a: 16,384 type-4 sliders over one one-frame image, without lanes:
/// one quad each and nothing reserved.
fn f8a(root: &Path) {
    png(root, "a.png", 1, 1);
    let dsts =
        join((0..MAX_ELEMENTS).map(|_| r#"{"id": "s", "dst": [{"w": 1, "h": 1}]}"#.to_owned()));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}],
               "slider": [{{"id": "s", "src": "a", "w": 1, "h": 1, "type": 4, "angle": 2,
                            "range": 600}}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn check_f8a(_: &Header, i: Imported) {
    let u = i.report.usage;
    let n = as_u32(MAX_ELEMENTS);
    assert_eq!((u.elements, u.keys), (n, n));
    assert_eq!((u.images, u.frames, u.files), (1, 1, 1));
    assert_eq!(i.report.notes, []);
    assert!(i.scratch.is_none());
    assert_eq!(i.skin.quad_capacity(), n);
    let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over(i.skin, &chart);
    d.view.cover = 500;
    let q = d.frame(1_000, &[]);
    assert_eq!(q.len(), MAX_ELEMENTS);
    // 600 × 500 ‰ lowers each from 1 079 to 1 379.
    assert!(
        q.iter()
            .all(|q| common::play::round(q.rect) == [0, 1_379, 1, 1])
    );
}

/// F8b's paints: every element but the lanes and their two markers.
const PAINTS: usize = MAX_ELEMENTS - 3;

/// F8b: [`PAINTS`] SUDDEN+ paints over the lanes, each a four-entry image
/// set over four one-frame images of one file chosen by the gauge's
/// percent, and the lanes with LIFT and HIDDEN+.
fn f8b(root: &Path) {
    use common::lanes::{body, hidden, hidden_object, lift, lift_object, note};
    png(root, "a.png", 64, 64);
    let images = r#"{"id": "c0", "src": "a", "x": 48, "y": 48, "w": 1, "h": 1},
                    {"id": "c1", "src": "a", "x": 49, "y": 48, "w": 1, "h": 1},
                    {"id": "c2", "src": "a", "x": 50, "y": 48, "w": 1, "h": 1},
                    {"id": "c3", "src": "a", "x": 51, "y": 48, "w": 1, "h": 1}"#;
    let objects = format!(
        r#"{}, {}, {},
           "imageset": [{{"id": "s", "images": ["c0", "c1", "c2", "c3"], "value": 110}}]"#,
        note(""),
        lift_object(""),
        hidden_object("")
    );
    let paint = r#"{"id": "s", "offset": 4, "dst": [{"x": 100, "y": 900, "w": 10, "h": 10}]}"#;
    let mut dsts = vec![common::lanes::NOTES.to_owned(), lift(""), hidden("")];
    dsts.extend((0..PAINTS).map(|_| paint.to_owned()));
    let dsts: Vec<&str> = dsts.iter().map(String::as_str).collect();
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], {}"#,
            body(images, &objects, &dsts)
        ))
        .as_bytes(),
    );
}

fn check_f8b(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.elements, as_u32(MAX_ELEMENTS));
    assert_eq!(u.choice_refs, as_u32(4 * PAINTS));
    assert_eq!(i.skin.choice_ref_count(), 65_524);
    // The four choices share their file with the lanes' parts.
    assert_eq!(u.files, 1);
    assert_eq!(
        i.skin.quad_capacity(),
        as_u32(MAX_ELEMENTS) + skin_model::LANE_QUADS
    );
    assert_eq!(i.skin.quad_capacity(), 24_576);
    // Each paint charges its own selector.
    let ops = i.skin.ops().total();
    assert_eq!(ops, PAINTS, "{ops}");
    // Dense notes on every lane, both legacy parts and every paint.
    let mut chart = bms_model::ChartBuilder::new(150_000);
    for t in 0..2_000 {
        for lane in 0..8 {
            chart = chart.tap(
                bms_model::Lane::new(lane).unwrap(),
                bms_model::TimeUs(1_000_000 + 1_000 * t),
            );
        }
    }
    let chart = chart.build().unwrap();
    let mut d = Drawn::over(i.skin, &chart);
    d.gauges[0].value = GaugeValue::FULL;
    d.view.cover = 300;
    d.view.lift = 100;
    d.view.hidden = 500;
    let q = d.frame(1_000, &[]);
    let size = |w: i32, h: i32| {
        q.iter()
            .filter(|q| {
                let [_, _, qw, qh] = common::play::round(q.rect);
                [qw, qh] == [w, h]
            })
            .count()
    };
    assert_eq!(size(10, 10), PAINTS);
    let covers = q
        .iter()
        .filter(|q| common::play::round(q.rect)[2] == 430)
        .count();
    assert_eq!(covers, 2);
    assert!(q.len() > PAINTS + 2 + 1_000, "{}", q.len());
}

/// F5's judges: 5,127 with a 2-digit combo and one with a 1-digit combo.
const JUDGES: usize = 5_128;
/// Judges whose grade has 11 keys; the rest have 10.
const ELEVEN_KEYS: usize = 3_994;
/// Judges whose grade has the 7-op predicate; the rest the 17-op one.
const SEVEN_OPS: usize = 2_164;
/// The grades' offsets in turn: none, LIFT, SUDDEN+, both repeated.
const FOLLOWS: [&str; 4] = [
    "",
    r#""offset": 3, "#,
    r#""offsets": [4], "#,
    r#""offsets": [3, 4, 3], "offset": 4, "#,
];

/// F5: the play objects at the key, op and widget quad caps. Lanes of 48
/// distinct part images with a bar, SUDDEN+, LIFT and HIDDEN+; a 36-node
/// gauge of 1,000 flickering parts (1,001 quads); 5,128 judges, each one
/// grade of 10 or 11 timed keys under a 7- or 17-op predicate and a combo
/// of 2 digits (3 quads), one of 1 digit (2 quads).
fn f5(root: &Path) {
    png(root, "a.png", 256, 256);
    let cell = |id: &str, i: usize, w: usize, h: usize| {
        format!(
            r#"{{"id": "{id}", "src": "a", "x": {}, "y": {}, "w": {w}, "h": {h}}}"#,
            8 * (i % 32),
            20 * (i / 32)
        )
    };
    // Parts p{u}_{k}: note, start, end, body, active body, mine.
    let mut images: Vec<String> = (0..48)
        .map(|i| cell(&format!("p{}_{}", i / 6, i % 6), i, 8, 20))
        .collect();
    images.push(cell("bar", 48, 8, 2));
    images.push(cell("cv", 49, 8, 8));
    images.push(cell("g", 50, 8, 8));
    images.extend((0..36).map(|n| cell(&format!("n{n}"), 51 + n, 8, 8)));
    let list = |k: usize| join((0..8).map(|u| format!("\"p{u}_{k}\"")));
    let note = format!(
        r#"{{"id": "notes", "note": [{}], "lnstart": [{}], "lnend": [{}], "lnbody": [{}],
            "lnactive": [{}], "mine": [{}], "hcnstart": [{}], "hcnend": [{}], "hcnbody": [{}],
            "hcnactive": [{}], "hcndamage": [{}], "hcnreactive": [{}], "dst": [{}],
            "group": [{{"id": "bar", "dst": [{{"x": 0, "y": 0, "w": 8, "h": 2}}]}}]}}"#,
        list(0),
        list(1),
        list(2),
        list(3),
        list(4),
        list(5),
        list(1),
        list(2),
        list(3),
        list(4),
        list(3),
        list(4),
        common::lanes::lane_rects(200, 600)
    );
    let judges = join((0..JUDGES).map(|i| {
        let keys = if i < ELEVEN_KEYS { 11 } else { 10 };
        let op = if i < SEVEN_OPS { "240" } else { "-230, -231" };
        let track = join((0..keys).map(|t| {
            format!(
                r#"{{"time": {t}, "x": {}, "y": 500, "w": 80, "h": 20}}"#,
                100 + t
            )
        }));
        let value = if i + 1 < JUDGES { "v" } else { "v1" };
        let follow = FOLLOWS[i % FOLLOWS.len()];
        format!(
            r#"{{"id": "j{i}", "index": 0,
                "images": [{{"id": "g", "timer": 46, "op": [{op}], "loop": 0, {follow}"dst": [{track}]}}],
                "numbers": [{{"id": "{value}", "offset": 3, "offsets": [4, 3, 4], "dst": [{{"x": 10, "y": 0, "w": 6, "h": 6}}]}}]}}"#
        )
    }));
    let nodes = join((0..36).map(|n| format!("\"n{n}\"")));
    let dsts = join(
        [
            r#"{"id": "notes", "dst": [{"w": 0, "h": 0}]}"#.to_owned(),
            r#"{"id": "cv", "offset": 4, "dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]}"#
                .to_owned(),
            r#"{"id": "lc", "dst": [{"x": 20, "y": 0, "w": 430, "h": 200}]}"#.to_owned(),
            r#"{"id": "hid", "dst": [{"x": 20, "y": -400, "w": 430, "h": 600}]}"#.to_owned(),
            r#"{"id": "gg", "offset": 3, "dst": [{"x": 600, "y": 100, "w": 1000, "h": 10}]}"#
                .to_owned(),
        ]
        .into_iter()
        .chain((0..JUDGES).map(|i| format!(r#"{{"id": "j{i}", "dst": [{{"w": 0, "h": 0}}]}}"#))),
    );
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], "image": [{}],
               "value": [
                 {{"id": "v", "src": "a", "x": 0, "y": 200, "w": 60, "h": 6, "divx": 10, "digit": 2}},
                 {{"id": "v1", "src": "a", "x": 0, "y": 200, "w": 60, "h": 6, "divx": 10, "digit": 1}}],
               "note": {note},
               "liftCover": [{{"id": "lc", "src": "a", "x": 0, "y": 220, "w": 8, "h": 8,
                               "disapearLine": 200}}],
               "hiddenCover": [{{"id": "hid", "src": "a", "x": 0, "y": 240, "w": 8, "h": 8,
                                 "disapearLine": 200}}],
               "gauge": {{"id": "gg", "nodes": [{nodes}], "parts": 1000, "type": 3, "cycle": 100}},
               "judge": [{judges}],
               "destination": [{dsts}]"#,
            images.join(", ")
        ))
        .as_bytes(),
    );
}

fn check_f5(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.keys, as_u32(MAX_SKIN_KEYS));
    assert_eq!(u.ops, as_u32(MAX_SKIN_OPS));
    assert_eq!(u.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(u.elements, as_u32(JUDGES) + 5);
    assert!(i.scratch.is_some());
    // One image element, SUDDEN+'s paint: the widgets' worst cases, its
    // quad and the lanes' budget.
    assert_eq!(
        i.skin.quad_capacity(),
        MAX_WIDGET_QUADS + 1 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
    );
    // The snapshot: a PERFECT with combo 12, the gauge full, SUDDEN+ at
    // 300 ‰, LIFT at 100 ‰ and HIDDEN+ at 500 ‰, no note on screen.
    let chart = bms_model::ChartBuilder::new(150_000)
        .tap(
            bms_model::Lane::new(1).unwrap(),
            bms_model::TimeUs(100_000_000),
        )
        .build()
        .unwrap();
    let mut d = Drawn::over(i.skin, &chart);
    d.tally = common::play::tally(2, 12);
    d.gauges[0].value = GaugeValue::FULL;
    d.view.cover = 300;
    d.view.lift = 100;
    d.view.hidden = 500;
    let full = eval::JudgeEvent {
        seq: 0,
        time: bms_model::TimeUs(900_000),
        origin: None,
        kind: eval::JudgeEventKind::Gauge {
            gauge: 0,
            value: GaugeValue::FULL,
            reference: GaugeValue::FULL,
        },
    };
    d.frame(900, &[full]);
    let q = d.frame(1_000, &[judged(1_000, Judgment::Perfect, 12)]);
    // Each emission by its drawn size: every judge's 80×20 grade, the
    // 6×6 cells of "1" and "2" for each but the last, whose one-digit
    // combo draws "2" alone; SUDDEN+, LIFT and HIDDEN+ over the 430 px
    // lanes; the gauge's 1,000 one-pixel segments, and its tip while the
    // flicker shows it.
    let mut by_size = std::collections::BTreeMap::<[i32; 2], usize>::new();
    for quad in &q {
        let [_, _, w, h] = common::play::round(quad.rect);
        *by_size.entry([w, h]).or_default() += 1;
    }
    let count = |size: [i32; 2]| by_size.get(&size).copied().unwrap_or(0);
    assert_eq!(count([80, 20]), JUDGES, "{by_size:?}");
    assert_eq!(count([6, 6]), 2 * (JUDGES - 1) + 1, "{by_size:?}");
    let covers: usize = by_size
        .iter()
        .filter(|(s, _)| s[0] == 430)
        .map(|(_, n)| n)
        .sum();
    assert_eq!(covers, 3, "{by_size:?}");
    // In destination order: SUDDEN+'s paint whole, 162 below its
    // destination's −320, every row of its square 8×8 cell (the atlas
    // pages are square); then LIFT and HIDDEN+, cut to the lanes.
    let lane_wide: Vec<(&skin_model::Quad, [i32; 4])> = q
        .iter()
        .map(|quad| (quad, common::play::round(quad.rect)))
        .filter(|(_, r)| r[2] == 430)
        .collect();
    let rects: Vec<[i32; 4]> = lane_wide.iter().map(|&(_, r)| r).collect();
    assert_eq!(
        rects,
        [
            [20, -158, 430, 600],
            [20, 820, 430, 60],
            [20, 550, 430, 270]
        ],
        "{rects:?}"
    );
    let [u0, v0, u1, v1] = lane_wide[0].0.uv;
    assert!(
        ((v1 - v0) / (u1 - u0) - 1.0).abs() < 1e-6,
        "{:?}",
        lane_wide[0].0.uv
    );
    let gauge: usize = by_size
        .iter()
        .filter(|(s, _)| s[1] == 10)
        .map(|(_, n)| n)
        .sum();
    assert!((1_000..=1_001).contains(&gauge), "{by_size:?}");
    assert_eq!(q.len(), JUDGES + 2 * JUDGES - 1 + 3 + gauge, "{by_size:?}");
    // Each follower moves once: LIFT at 100 ‰ of the 600 px lane lifts
    // by 60, SUDDEN+ at 300 ‰ of the 540 px left lowers by 162. The
    // grades cycle through none, LIFT, SUDDEN+ and both, each combo
    // with its grade; the gauge follows LIFT.
    let rows = |size: [i32; 2]| {
        let mut rows = std::collections::BTreeMap::<i32, usize>::new();
        for quad in &q {
            let [_, y, w, h] = common::play::round(quad.rect);
            if [w, h] == size {
                *rows.entry(y).or_default() += 1;
            }
        }
        rows
    };
    let each = JUDGES / FOLLOWS.len();
    let grade = [(500, each), (560, each), (662, each), (722, each)];
    assert_eq!(rows([80, 20]).into_iter().collect::<Vec<_>>(), grade);
    let combo = rows([6, 6]);
    let tops: Vec<i32> = combo.keys().copied().collect();
    assert_eq!(tops.len(), FOLLOWS.len(), "{combo:?}");
    let first = tops.first().copied().unwrap_or_default();
    let moved: Vec<i32> = tops.iter().map(|y| y - first).collect();
    assert_eq!(moved, [0, 60, 162, 222], "{combo:?}");
    let gauge_rows = rows([1, 10]);
    assert_eq!(
        gauge_rows.keys().copied().collect::<Vec<_>>(),
        [910],
        "{gauge_rows:?}"
    );
}

/// A relative name of `len` bytes ending in `tail`, in folders of at most
/// 200-byte names.
fn long_name(len: usize, tail: &str) -> String {
    let mut s = String::new();
    while s.len() + tail.len() + 201 <= len {
        s += &"d".repeat(200);
        s.push('/');
    }
    let pad = len - s.len() - tail.len();
    s + &"e".repeat(pad) + tail
}

/// F3: 4,096 file attempts with long valid names: 2,048 admitted (as
/// long as the platform opens) and 2,048 missing (1,024 bytes).
fn f3(root: &Path) -> usize {
    // The absolute path must fit the platform's limit: 1,024 bytes on
    // macOS, so the admitted names are shorter there; 4,096 on Linux.
    let ok_len = if cfg!(target_os = "macos") {
        (1_000 - root.as_os_str().len()).min(1_024)
    } else {
        1_024
    };
    let names: Vec<String> = (0..MAX_FILES)
        .map(|i| {
            let tail = format!("/{i:04}.png");
            if i % 2 == 0 {
                long_name(ok_len, &tail)
            } else {
                long_name(1_024, &tail.replace(".png", "x.png"))
            }
        })
        .collect();
    for (i, n) in names.iter().enumerate() {
        if i % 2 == 0 {
            png(root, n, 1, 1);
        }
    }
    let sources = join(
        names
            .iter()
            .enumerate()
            .map(|(i, n)| format!(r#"{{"id": "s{i}", "path": "{n}"}}"#)),
    );
    let images =
        join((0..MAX_FILES).map(|i| format!(r#"{{"id": "{i}", "src": "s{i}", "w": 1, "h": 1}}"#)));
    let dsts =
        join((0..MAX_FILES).map(|i| format!(r#"{{"id": "{i}", "dst": [{{"w": 1, "h": 1}}]}}"#)));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{sources}], "image": [{images}], "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
    ok_len
}

fn check_f3(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.file_attempts, as_u32(MAX_FILES));
    assert_eq!(u.files, as_u32(MAX_FILES / 2));
    assert_eq!(i.skin.sheet().image_count(), MAX_FILES / 2);
}

/// F4: 32 MiB of input holding nodes within 1 % of the cap, every header
/// record table at its cap, 4,096 candidates and about 1,000 elements;
/// `more_nodes` and `more_bytes` added to the included file.
fn f4(root: &Path, more_nodes: usize, more_bytes: usize) {
    png(root, "a.png", 1, 1);
    for i in 0..MAX_LIST_ENTRIES {
        write(root, &format!("c/{i:04}.png"), b"");
    }
    let items_per = MAX_OPTION_ITEMS / MAX_OPTIONS;
    let options = join((0..MAX_OPTIONS).map(|o| {
        let items = join(
            (0..items_per)
                .map(|k| format!(r#"{{"name": "i{k}", "op": {}}}"#, 900 + o * items_per + k)),
        );
        format!(r#"{{"name": "o{o}", "item": [{items}]}}"#)
    }));
    let choices = join((0..MAX_FILE_CHOICES).map(|c| {
        let path = if c == 0 { "c/*.png" } else { "none/*.png" };
        format!(r#"{{"name": "f{c}", "path": "{path}"}}"#)
    }));
    let offsets =
        join((0..MAX_OFFSETS).map(|o| format!(r#"{{"name": "o{o}", "id": {}}}"#, 40 + o % 150)));
    let dsts = join((0..1_000).map(|_| r#"{"id": "i", "dst": [{"w": 1, "h": 1}]}"#.to_owned()));
    let mut head = common::hd(&format!(
        r#""property": [{options}], "filepath": [{choices}], "offset": [{offsets}],
           "source": [{{"id": "a", "path": "a.png"}}],
           "image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
           "destination": [{dsts}, {{"include": "p.json"}}]"#
    ));
    // Pad the entry with spaces to 16 MiB; the included array holds the
    // nodes and the rest of the input.
    let entry = 16 << 20;
    assert!(head.len() < entry);
    head.insert_str(1, &" ".repeat(entry - head.len()));
    write(root, "s.json", head.as_bytes());
    // The rest of the skin is about 64,400 nodes (measured: a pad 2,032,760
    // long reaches the cap), so this totals about 99.5 % of it.
    let used_nodes = 75_000;
    let nodes = MAX_NODES - used_nodes + more_nodes;
    let mut pad = String::with_capacity(16 << 20);
    pad.push_str(r#"[{"id": "i", "dst": [{"w": 1, "h": 1}], "pad": ["#);
    for k in 0..nodes {
        if k > 0 {
            pad.push(',');
        }
        pad.push('0');
    }
    pad.push_str("]}]");
    let rest = usize::try_from(MAX_INPUT_BYTES).unwrap() - entry;
    assert!(pad.len() < rest);
    let fill = rest - pad.len() + more_bytes;
    let _ = write!(pad, "{}", " ".repeat(fill));
    write(root, "p.json", pad.as_bytes());
}

fn check_f4(h: &Header, i: Imported) {
    assert_eq!(i.report.usage.elements, 1_001);
    assert_eq!(i.report.sources.len(), 2);
    assert_eq!(i.report.files.len(), MAX_FILE_CHOICES);
    assert_eq!(h.options.len(), MAX_OPTIONS);
    let items: usize = h.options.iter().map(|o| o.items.len()).sum();
    assert_eq!(items, MAX_OPTION_ITEMS);
    assert_eq!(h.files.len(), MAX_FILE_CHOICES);
    assert_eq!(h.offsets.len(), MAX_OFFSETS);
    assert_eq!(h.files[0].candidates.len(), MAX_LIST_ENTRIES);
}

/// F4's input reaches the input bytes exactly, two files at the file
/// cap, so a byte more is refused (by the file cap it meets first); its
/// nodes are within 1 % of the cap, so 1 % more are refused.
fn f4_reaches_its_input_caps(root: &Path) {
    let bytes: u64 = ["s.json", "p.json"]
        .iter()
        .map(|f| std::fs::metadata(root.join(f)).unwrap().len())
        .sum();
    assert_eq!(bytes, MAX_INPUT_BYTES);
    for (nodes, bytes, want) in [
        (0, 1, InputLimit::FileBytes),
        (MAX_NODES / 100, 0, InputLimit::Nodes),
    ] {
        let d = tempfile::tempdir().unwrap();
        f4(d.path(), nodes, bytes);
        match refused(d.path()) {
            ImportError::Input { limit, .. } => assert_eq!(limit, want),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn import_memory_is_measured() {
    // Positive control: a 4 MiB buffer grown to 8 MiB is 12 MiB live at
    // its peak.
    let ((), control) = peak(|| {
        let mut v: Vec<u8> = Vec::with_capacity(4 << 20);
        v.push(1);
        v.reserve_exact((8 << 20) - 1);
        black_box(&v);
    });
    assert!(control >= 12 << 20, "{control}");

    println!(
        "sizes: ElementDef {}, KeyDef {}, Program {}, ImageDef {}, Note {}, Report {}",
        size_of::<ElementDef>(),
        size_of::<KeyDef>(),
        size_of::<Program>(),
        size_of::<ImageDef>(),
        size_of::<Note>(),
        size_of::<Report>(),
    );
    println!(
        "sizes: JudgeDef {}, GaugeDef {}, LanesDef {}, Play {}, PlayContext {}, SkinFrame {}, \
         FrameReadouts {}, DurationRatio {}",
        size_of::<skin_model::JudgeDef>(),
        size_of::<skin_model::GaugeDef>(),
        size_of::<skin_model::LanesDef>(),
        size_of::<skin_model::Play>(),
        size_of::<skin_model::PlayContext<'_>>(),
        size_of::<skin_model::SkinFrame<'_>>(),
        size_of::<skin_model::FrameReadouts>(),
        size_of::<skin_model::DurationRatio>(),
    );
    println!(
        "sizes: ImageChoiceDef {}, Option<ImageChoicePart> {}, CoverPaintDef {}",
        size_of::<ImageChoiceDef>(),
        size_of::<Option<ImageChoicePart>>(),
        size_of::<skin_model::CoverPaintDef>(),
    );
    let mut peaks = Vec::new();
    let d = tempfile::tempdir().unwrap();
    f1(d.path());
    peaks.push(("F1 elements", measure(d.path(), check_f1)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f2(d.path());
    peaks.push(("F2 widgets", measure(d.path(), check_f2)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    let len = f3(d.path());
    println!("F3 admitted names: {len} bytes");
    peaks.push(("F3 files", measure(d.path(), check_f3)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f4(d.path(), 0, 0);
    f4_reaches_its_input_caps(d.path());
    peaks.push(("F4 input", measure(d.path(), check_f4)));
    drop(d);
    assert_eq!(ops_of("240"), 7);
    assert_eq!(ops_of("-230, -231"), 17);
    assert_eq!(SEVEN_OPS * 7 + (JUDGES - SEVEN_OPS) * 17, MAX_SKIN_OPS);
    assert_eq!(
        6 + 2 * JUDGES + ELEVEN_KEYS * 11 + (JUDGES - ELEVEN_KEYS) * 10,
        MAX_SKIN_KEYS
    );
    let d = tempfile::tempdir().unwrap();
    f5(d.path());
    peaks.push(("F5 play objects", measure(d.path(), check_f5)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f6(d.path());
    peaks.push(("F6 bitmap fonts", measure(d.path(), check_f6)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f7(d.path());
    peaks.push(("F7 image choices", measure(d.path(), check_f7)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f8a(d.path());
    peaks.push(("F8a sliders", measure(d.path(), check_f8a)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f8b(d.path());
    peaks.push(("F8b paints", measure(d.path(), check_f8b)));
    drop(d);
    for (name, p) in &peaks {
        println!("{name}: peak {p} B");
    }
    let n = peaks.iter().map(|&(_, p)| p).max().unwrap();
    println!("N = {n} B, bound {} B", n + n / 4);
    {
        for (name, p) in &peaks {
            assert!(*p <= BOUND, "{name}: {p} > {BOUND}");
        }
    }
}
