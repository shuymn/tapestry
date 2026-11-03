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
    MAX_OPTION_ITEMS, MAX_OPTIONS, MAX_RESOLVED_DIGIT_OFFSETS,
};
use skin_beatoraja::{
    Config, Header, ImportError, ImportLimit, Imported, InputLimit, Note, Report, import,
    read_header,
};
use skin_model::{
    ElementDef, ImageChoiceDef, ImageChoicePart, ImageDef, KeyDef, MAX_BAR_GROUPS, MAX_ELEMENTS,
    MAX_FILES, MAX_FNT_BYTES, MAX_FNT_PAGES, MAX_FONTS, MAX_IMAGE_CHOICE_REFS, MAX_SHEET_GLYPHS,
    MAX_SHEET_KERNINGS, MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS, MAX_TEXT_ELEMENTS,
    MAX_WIDGET_QUADS, Program, Skin,
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

/// [`measure_prepared`] over an empty chart: every fixture's peak counts
/// a prepared play and its batch, runs included.
fn measure(root: &Path, check: impl FnOnce(&Header, Imported)) -> i64 {
    let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
    measure_prepared(root, &chart, check)
}

/// Reads and imports `s.json` of `root` and prepares a play of the import
/// over `chart` within the window, the header, definition and import kept
/// beside the play's state and batch, then checks them with `check`
/// outside it.
fn measure_prepared(
    root: &Path,
    chart: &bms_model::Chart,
    check: impl FnOnce(&Header, Imported),
) -> i64 {
    let ((def, header, imported, prepared), p) = peak(|| {
        let (mut def, header) = read_header(root, "s.json").unwrap();
        let imported = import(&mut def, &header, &Config::defaults(&header)).unwrap();
        let prepared = common::play::prepared(&imported.skin, chart);
        (def, header, imported, prepared)
    });
    drop(prepared);
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
    // but 241, the latest judgment's PERFECT, costs more than 4, and the
    // lists leave 241 out, so the cheaper is none.
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
        ..skin_model::FrameReadouts::UNAVAILABLE
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

/// F8b's paints: every element but the lanes, LIFT and HIDDEN+.
const PAINTS: usize = MAX_ELEMENTS - 3;

/// F8b: [`PAINTS`] SUDDEN+ paints over the lanes, each a four-entry image
/// set over four one-frame images of one file chosen by the gauge's
/// percent, after the lanes, LIFT's paint and HIDDEN+'s.
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
    // Dense notes on every lane, LIFT's and HIDDEN+'s paint and every paint.
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
const ELEVEN_KEYS: usize = 3_978;
/// The judge whose 11-key grade fits its square image inside its
/// destination (stretch 1): it keeps 11 attachment keys for its combo.
const FITTED: usize = 3;
/// Judges whose grade has the 7-op predicate; the rest the 17-op one.
const SEVEN_OPS: usize = 2_165;
/// The grades' offsets in turn: none, LIFT, SUDDEN+, both repeated.
const FOLLOWS: [&str; 4] = [
    "",
    r#""offset": 3, "#,
    r#""offsets": [4], "#,
    r#""offsets": [3, 4, 3], "offset": 4, "#,
];

/// F5's bar line groups: grey under a full gauge on the ready timer; red,
/// translucent and added on a PERFECT on the play timer; a smaller one
/// otherwise; the latter two over images of two frames.
const F5_GROUPS: &str = r#"
    {"id": "bar", "op": [240], "dst": [{"x": 21, "y": 200, "w": 428, "h": 2, "r": 128, "g": 128, "b": 128}]},
    {"id": "bar2", "timer": 41, "blend": 2, "op": [241], "dst": [{"x": 21, "y": 199, "w": 428, "h": 4, "a": 128, "r": 255, "g": 0, "b": 0}]},
    {"id": "bar3", "op": [-241], "dst": [{"x": 100, "y": 198, "w": 50, "h": 6}]}"#;

/// F5: the play objects at the key, op and widget quad caps. Lanes of 48
/// distinct part images with three bar line groups ([`F5_GROUPS`]),
/// SUDDEN+'s paint, LIFT's and HIDDEN+'s on two keys each on their own
/// timers, and a second LIFT of one key; a 36-node
/// gauge of 1,000 flickering parts (1,001 quads); 5,128 judges, each one
/// grade of 10 or 11 timed keys under a 7- or 17-op predicate and a combo
/// of 2 digits (3 quads) moved by two digit offset rows, one of 1 digit
/// (2 quads) by one: 10,255 offsets kept. One grade ([`FITTED`]) fits its
/// image inside its destination and keeps its 11 keys before the stretch
/// for its combo besides. Its first key lane reaches one pixel into the
/// second ([`F5_LANES`]).
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
    images.push(
        r#"{"id": "bar2", "src": "a", "x": 64, "y": 220, "w": 16, "h": 2, "divx": 2, "cycle": 100}"#
            .to_owned(),
    );
    images.push(
        r#"{"id": "bar3", "src": "a", "x": 96, "y": 220, "w": 8, "h": 8, "divy": 2, "cycle": 200}"#
            .to_owned(),
    );
    images.push(cell("cv", 49, 8, 8));
    images.push(cell("g", 50, 8, 8));
    images.extend((0..36).map(|n| cell(&format!("n{n}"), 51 + n, 8, 8)));
    let list = |k: usize| join((0..8).map(|u| format!("\"p{u}_{k}\"")));
    let note = format!(
        r#"{{"id": "notes", "note": [{}], "lnstart": [{}], "lnend": [{}], "lnbody": [{}],
            "lnactive": [{}], "mine": [{}], "hcnstart": [{}], "hcnend": [{}], "hcnbody": [{}],
            "hcnactive": [{}], "hcndamage": [{}], "hcnreactive": [{}], "dst": [{}],
            "size": [{F5_SIZES}], "group": [{F5_GROUPS}]}}"#,
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
        F5_LANES
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
        let stretch = if i == FITTED { r#""stretch": 1, "# } else { "" };
        format!(
            r#"{{"id": "j{i}", "index": 0,
                "images": [{{"id": "g", "timer": 46, "op": [{op}], "loop": 0, {follow}{stretch}"dst": [{track}]}}],
                "numbers": [{{"id": "{value}", "offset": 3, "offsets": [4, 3, 4], "dst": [{{"x": 10, "y": 0, "w": 6, "h": 6}}]}}]}}"#
        )
    }));
    let nodes = join((0..36).map(|n| format!("\"n{n}\"")));
    let dsts = join(
        [
            r#"{"id": "notes", "dst": [{"w": 0, "h": 0}]}"#.to_owned(),
            r#"{"id": "cv", "offset": 4, "dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]}"#
                .to_owned(),
            r#"{"id": "lc", "timer": 40, "dst": [{"time": 0, "x": 20, "y": 0, "w": 430, "h": 200}, {"time": 2000}]}"#.to_owned(),
            r#"{"id": "hid", "timer": 41, "dst": [{"time": 0, "x": 20, "y": -400, "w": 430, "h": 600}, {"time": 2000}]}"#.to_owned(),
            r#"{"id": "lc2", "dst": [{"x": 20, "y": 50, "w": 430, "h": 100}]}"#.to_owned(),
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
                 {{"id": "v", "src": "a", "x": 0, "y": 200, "w": 60, "h": 6, "divx": 10, "digit": 2,
                   "offset": [{{"x": 1}}, {{"x": 2}}]}},
                 {{"id": "v1", "src": "a", "x": 0, "y": 200, "w": 60, "h": 6, "divx": 10, "digit": 1,
                   "offset": [{{"x": 3}}]}}],
               "note": {note},
               "liftCover": [{{"id": "lc", "src": "a", "x": 0, "y": 220, "w": 8, "h": 8,
                               "disapearLine": 200}},
                             {{"id": "lc2", "src": "a", "x": 8, "y": 220, "w": 8, "h": 8,
                               "disapearLine": 100, "isDisapearLineLinkLift": true}}],
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

/// F5's note heights by source slot (keys 1 to 7, then the scratch):
/// unequal, some fractional, the reference (slot 0) 19.5.
const F5_SIZES: &str = "19.5, 20, 20.25, 21, 22, 23, 24, 7.75";

/// F5's lanes: the source's keys 50 wide from 100, the first 51 wide
/// over the second, and the scratch at 20, 79 wide; the span 20 to 450.
const F5_LANES: &str = r#"{"x": 100, "y": 200, "w": 51, "h": 600},
    {"x": 150, "y": 200, "w": 50, "h": 600}, {"x": 200, "y": 200, "w": 50, "h": 600},
    {"x": 250, "y": 200, "w": 50, "h": 600}, {"x": 300, "y": 200, "w": 50, "h": 600},
    {"x": 350, "y": 200, "w": 50, "h": 600}, {"x": 400, "y": 200, "w": 50, "h": 600},
    {"x": 20, "y": 200, "w": 79, "h": 600}"#;

/// F5's chart: a chord on every lane at 1 100 and 1 200 ms, bars at
/// 1 000 and 1 050 ms.
fn f5_chart() -> bms_model::Chart {
    let mut b = bms_model::ChartBuilder::new(150_000)
        .bar(bms_model::TimeUs(1_000_000))
        .bar(bms_model::TimeUs(1_050_000));
    for t in [1_100_000, 1_200_000] {
        for l in 0..8 {
            b = b.tap(bms_model::Lane::new(l).unwrap(), bms_model::TimeUs(t));
        }
    }
    b.build().unwrap()
}

fn check_f5(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.bar_groups, 3);
    assert_eq!(i.skin.bar_group_count(), 3);
    assert_eq!(u.keys, as_u32(MAX_SKIN_KEYS));
    assert_eq!(u.grade_anchor_keys, 11);
    assert_eq!(u.ops, as_u32(MAX_SKIN_OPS));
    assert_eq!(u.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(u.elements, as_u32(JUDGES) + 6);
    assert_eq!(u.resolved_digit_offsets, 3);
    assert_eq!(u.digit_offsets, as_u32(2 * (JUDGES - 1) + 1));
    assert_eq!(i.skin.digit_offset_count(), 10_255);
    assert!(i.scratch.is_some());
    // Four image elements, the covers' paint: the widgets' worst cases,
    // their quads and the lanes' budget with the judgment line.
    assert_eq!(skin_model::LANE_RESERVED, 1);
    assert_eq!(
        i.skin.quad_capacity(),
        MAX_WIDGET_QUADS + 4 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
    );
    // The snapshot: a PERFECT with combo 12, the gauge full, SUDDEN+ at
    // 300 ‰, LIFT at 100 ‰ and HIDDEN+ at 500 ‰, no note on screen, two
    // bars.
    let mut d = Drawn::over(i.skin, &f5_chart());
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
    // Each emission by its drawn size: every judge's 80×20 grade but the
    // fitted one's 20×20, the 6×6 cells of "1" and "2" for each but the last, whose one-digit
    // combo draws "2" alone; the four covers' paint over the 430 px
    // lanes; the gauge's 1,000 one-pixel segments, and its tip while the
    // flicker shows it.
    let mut by_size = std::collections::BTreeMap::<[i32; 2], usize>::new();
    for quad in &q {
        let [_, _, w, h] = common::play::round(quad.rect);
        *by_size.entry([w, h]).or_default() += 1;
    }
    let count = |size: [i32; 2]| by_size.get(&size).copied().unwrap_or(0);
    assert_eq!(count([80, 20]), JUDGES - 1, "{by_size:?}");
    assert_eq!(count([20, 20]), 1, "{by_size:?}");
    assert_eq!(count([6, 6]), 2 * (JUDGES - 1) + 1, "{by_size:?}");
    let xs = |size: [i32; 2]| {
        let mut xs = std::collections::BTreeMap::<i32, usize>::new();
        for quad in &q {
            let [x, _, w, h] = common::play::round(quad.rect);
            if [w, h] == size {
                *xs.entry(x).or_default() += 1;
            }
        }
        xs
    };
    // Every grade at 100, unshifted, the fitted one's paint at 130. Each
    // combo, the fitted grade's on its destination's corner at 100,
    // starts its 6 px cells half its baseline width left of 110: "12"
    // from 104, moved 1 and 2; the last judge's "2" from 107, moved 3.
    assert_eq!(
        xs([80, 20]).into_iter().collect::<Vec<_>>(),
        [(100, JUDGES - 1)]
    );
    assert_eq!(xs([20, 20]).into_iter().collect::<Vec<_>>(), [(130, 1)]);
    assert_eq!(
        xs([6, 6]).into_iter().collect::<Vec<_>>(),
        [(105, JUDGES - 1), (110, 1), (112, JUDGES - 1)]
    );
    let covers: usize = by_size
        .iter()
        .filter(|(s, _)| s[0] == 430)
        .map(|(_, n)| n)
        .sum();
    assert_eq!(covers, 4, "{by_size:?}");
    // In destination order: SUDDEN+'s paint whole, 162 below its
    // destination's −320, every row of its square 8×8 cell (the atlas
    // pages are square); then LIFT's raised 60 and cut at 880, HIDDEN+'s
    // raised 60 + 270 and cut at its line raised to 820, and the second
    // LIFT's raised 60 and cut at its linked line, 980 raised to 920.
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
            [20, 820, 430, 200],
            [20, 550, 430, 600],
            [20, 870, 430, 100]
        ],
        "{rects:?}"
    );
    let clips: Vec<Option<[i32; 4]>> = d
        .batch
        .runs()
        .iter()
        .flat_map(|r| (r.start..r.end).map(move |i| (i, r)))
        .filter(|&(i, _)| common::play::round(d.batch.quads()[i as usize].rect)[2] == 430)
        .map(|(_, r)| r.clip_rect.map(|c| common::play::round(c.rect())))
        .collect();
    assert_eq!(
        clips,
        [
            None,
            Some([20, 820, 430, 60]),
            Some([20, 550, 430, 270]),
            Some([20, 870, 430, 50])
        ]
    );
    assert!(
        lane_wide
            .iter()
            .all(|(q, _)| q.color == [1.0; 4] && q.angle == 0.0)
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
    // Each bar, the one at the line then the coming one 50 ms ahead, 50 px
    // higher at the original lane's 600 ms over 600 px, draws the
    // full-gauge group then the PERFECT one, before the notes; the third
    // group is off.
    let bars = |q: &[skin_model::Quad]| -> Vec<([i32; 4], [f32; 4], [u32; 4])> {
        q.iter()
            .filter(|q| matches!(common::play::round(q.rect), [_, _, 428 | 50, 2 | 4 | 6]))
            .map(|q| (common::play::round(q.rect), q.color, q.uv.map(f32::to_bits)))
            .collect()
    };
    let grey = [128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0, 1.0];
    let red = [1.0, 0.0, 0.0, 128.0 / 255.0];
    let shown = bars(&q);
    let looks: Vec<([i32; 4], [f32; 4])> = shown.iter().map(|&(r, c, _)| (r, c)).collect();
    assert_eq!(
        looks,
        [
            ([21, 878, 428, 2], grey),
            ([21, 877, 428, 4], red),
            ([21, 828, 428, 2], grey),
            ([21, 827, 428, 4], red),
        ]
    );
    assert_ne!(shown[0].2, shown[1].2);
    // The two chords' taps, each at its lane's height (the bars are at
    // most 6 tall), in the source's order: the earlier chord first, each
    // keys 1 to 7 then the scratch.
    let taps: Vec<[i32; 4]> = q
        .iter()
        .map(|q| common::play::round(q.rect))
        .filter(|r| r[3] >= 7 && matches!(r[2], 50 | 51 | 79))
        .collect();
    let xs: Vec<i32> = taps.iter().map(|r| r[0]).collect();
    let chord = [100, 150, 200, 250, 300, 350, 400, 20];
    assert_eq!(xs, [chord, chord].concat(), "{taps:?}");
    assert!(taps[0][1] > taps[8][1], "{taps:?}");
    assert_eq!(taps[0][2], 51);
    assert_eq!(
        q.len(),
        JUDGES + 2 * JUDGES - 1 + 4 + gauge + 4 + 16,
        "{by_size:?}"
    );
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
    let grade = [(500, each), (560, each), (662, each - 1), (722, each)];
    assert_eq!(rows([80, 20]).into_iter().collect::<Vec<_>>(), grade);
    // The fitted grade, listing both offsets, moves as the others that do;
    // its combo with it once, among theirs below.
    assert_eq!(rows([20, 20]).into_iter().collect::<Vec<_>>(), [(662, 1)]);
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
    // On a GREAT 50 ms later the second bar is at the line and the first
    // gone: the full-gauge group, then the third in its own size.
    let q = d.frame(1_050, &[judged(1_050, Judgment::Great, 13)]);
    let shown = bars(&q);
    let looks: Vec<([i32; 4], [f32; 4])> = shown.iter().map(|&(r, c, _)| (r, c)).collect();
    assert_eq!(
        looks,
        [([21, 878, 428, 2], grey), ([100, 876, 50, 6], [1.0; 4])]
    );
    assert_ne!(shown[0].2, shown[1].2);
    // At 1 100 ms the first chord is on the line raised by LIFT (820):
    // each lane's note at its own height on one lower edge, painted by
    // its head's timeline row then the source's lane order, keys 1 to 7
    // then the scratch.
    let q = d.frame(1_100, &[]);
    let chord: Vec<(f32, f32, f32)> = q
        .iter()
        .filter(|q| [51.0, 50.0, 79.0].contains(&q.rect[2]) && q.rect[1] + q.rect[3] == 820.0)
        .map(|q| (q.rect[0], q.rect[1] + q.rect[3], q.rect[3]))
        .collect();
    assert_eq!(
        chord,
        [
            (100.0, 820.0, 19.5),
            (150.0, 820.0, 20.0),
            (200.0, 820.0, 20.25),
            (250.0, 820.0, 21.0),
            (300.0, 820.0, 22.0),
            (350.0, 820.0, 23.0),
            (400.0, 820.0, 24.0),
            (20.0, 820.0, 7.75),
        ]
    );
}

/// F10's LIFT destinations: every element but the lanes.
const LIFTS: usize = MAX_ELEMENTS - 1;

/// How F10 is varied past its caps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum F10 {
    /// At the caps.
    Caps,
    /// One destination more.
    OneMore,
    /// The last destination with two keys more, trimmed: its gate's two
    /// more pass the key cap.
    GateKeys,
    /// The same untrimmed: no gate, within the key cap.
    Untrimmed,
}

/// The `i`th F10 destination's y (skin pixels, y up): its 100 px
/// destination's upper edge between 783 and 879 (y down), crossed by
/// the line at 880 at every lift of 0.
fn f10_y(i: usize) -> usize {
    101 + i % 97
}

/// F10: the lanes and [`LIFTS`] destinations of one LIFT record, each of
/// two keys on the play timer fitting its 16×16 frame to 430 px wide
/// (430 tall) over its 430×100 destination, trimmed at the lanes' bottom:
/// two paint keys and two gate keys each, 65,533 keys in all, every clip
/// its own.
fn f10(root: &Path, variant: F10) {
    use common::lanes::{NOTES, body, note};
    png(root, "a.png", 64, 64);
    let objects = format!(
        r#"{},
           "liftCover": [{{"id": "lc", "src": "a", "x": 16, "y": 24, "w": 16, "h": 16, "disapearLine": 200}},
                         {{"id": "lu", "src": "a", "x": 16, "y": 24, "w": 16, "h": 16}}]"#,
        note("")
    );
    let n = if variant == F10::OneMore {
        LIFTS + 1
    } else {
        LIFTS
    };
    let mut dsts = vec![NOTES.to_owned()];
    dsts.extend((0..n).map(|i| {
        let y = f10_y(i);
        let last = i + 1 == n;
        let id = if last && variant == F10::Untrimmed {
            "lu"
        } else {
            "lc"
        };
        let more = if last && matches!(variant, F10::GateKeys | F10::Untrimmed) {
            format!(r#", {{"time": 2000, "y": {y}}}, {{"time": 3000, "y": {y}}}"#)
        } else {
            String::new()
        };
        format!(
            r#"{{"id": "{id}", "timer": 41, "stretch": 4, "dst": [{{"time": 0, "x": 20, "y": {y}, "w": 430, "h": 100}}, {{"time": 1000, "y": {}}}{more}]}}"#,
            y + 1
        )
    }));
    let dsts: Vec<&str> = dsts.iter().map(String::as_str).collect();
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], {}"#,
            body("", &objects, &dsts)
        ))
        .as_bytes(),
    );
}

/// F10's chart: one note far ahead.
fn f10_chart() -> bms_model::Chart {
    bms_model::ChartBuilder::new(150_000)
        .tap(
            bms_model::Lane::new(1).unwrap(),
            bms_model::TimeUs(100_000_000),
        )
        .build()
        .unwrap()
}

fn check_f10(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.elements, as_u32(MAX_ELEMENTS));
    assert_eq!(u.keys, 65_533);
    // One paint image besides the lanes' parts.
    assert_eq!(u.images, 7);
    // A quad for each paint and the lanes' budget with the judgment line:
    // no cover reserves lane quads.
    assert_eq!(
        i.skin.quad_capacity(),
        as_u32(LIFTS) + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
    );
    let mut d = Drawn::over(i.skin, &f10_chart());
    d.frame(0, &[]);
    // Every paint 430 tall, each under its own clip, in destination order.
    let paints: Vec<([i32; 4], Option<[i32; 4]>)> = d
        .clipped(0)
        .into_iter()
        .filter(|(r, _)| r[3] == 430)
        .collect();
    assert_eq!(paints.len(), LIFTS);
    for (k, (r, clip)) in paints.iter().enumerate() {
        let top = 1_080 - i32::try_from(f10_y(k)).unwrap() - 100;
        assert_eq!(*r, [20, top - 165, 430, 430], "{k}");
        assert_eq!(*clip, Some([20, top, 430, 880 - top]), "{k}");
    }
    let clipped_runs = d
        .batch
        .runs()
        .iter()
        .filter(|r| r.clip_rect.is_some())
        .count();
    assert_eq!(clipped_runs, LIFTS);
}

/// F10 past its caps: one destination more, or a gate's keys more.
fn f10_refuses_past_its_caps() {
    for (variant, want) in [
        (F10::OneMore, Some(ImportLimit::Elements)),
        (F10::GateKeys, Some(ImportLimit::Keys)),
        (F10::Untrimmed, None),
    ] {
        let d = tempfile::tempdir().unwrap();
        f10(d.path(), variant);
        match want {
            Some(want) => match refused(d.path()) {
                ImportError::Limit { limit, .. } => assert_eq!(limit, want),
                other => panic!("{other:?}"),
            },
            None => {
                let (mut def, header) = read_header(d.path(), "s.json").unwrap();
                let i = import(&mut def, &header, &Config::defaults(&header)).unwrap();
                assert_eq!(i.report.usage.keys, 65_533);
            }
        }
    }
}

/// F9: the bar line groups at their cap, one key and the one-op
/// condition `cover.enabled` each, over one image, and SUDDEN+.
fn f9(root: &Path) {
    use common::lanes::{body, cover, note};
    png(root, "a.png", 64, 64);
    // Each group's index in its colour, red the low byte, green the high.
    let groups = join((0..MAX_BAR_GROUPS).map(|g| {
        format!(
            r#"{{"id": "bar", "op": [271], "dst": [{{"x": 20, "y": 200, "w": 430, "h": 2, "r": {}, "g": {}}}]}}"#,
            g % 256,
            g / 256
        )
    }));
    let objects = note(&format!(r#""group": [{groups}]"#));
    let dsts = [common::lanes::NOTES.to_owned(), cover("")];
    let dsts: Vec<&str> = dsts.iter().map(String::as_str).collect();
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], {}"#,
            body("", &objects, &dsts)
        ))
        .as_bytes(),
    );
}

/// F9's chart: a note and a bar on the line at 1 s.
fn f9_chart() -> bms_model::Chart {
    bms_model::ChartBuilder::new(150_000)
        .tap(
            bms_model::Lane::new(1).unwrap(),
            bms_model::TimeUs(1_000_000),
        )
        .bar(bms_model::TimeUs(1_000_000))
        .build()
        .unwrap()
}

fn check_f9(_: &Header, i: Imported) {
    let u = i.report.usage;
    let n = as_u32(MAX_BAR_GROUPS);
    assert_eq!(u.bar_groups, n);
    assert_eq!(i.skin.bar_group_count(), MAX_BAR_GROUPS);
    assert_eq!((u.elements, u.keys, u.ops), (2, n + 2, n));
    assert_eq!(i.skin.ops().total(), MAX_BAR_GROUPS);
    assert_eq!(u.images, 8);
    // The groups add no quads of their own to the batch.
    assert_eq!(
        i.skin.quad_capacity(),
        1 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
    );
    // The bar's groups spend the lanes' quads in group order, then one
    // drop; the note on the line gets none; SUDDEN+ still draws.
    let mut d = Drawn::over(i.skin, &f9_chart());
    d.view.cover = 300;
    let q = d.frame(1_000, &[]);
    let bars: Vec<usize> = q
        .iter()
        .filter(|q| common::play::round(q.rect) == [20, 878, 430, 2])
        .map(|q| {
            let [r, g] = [q.color[0], q.color[1]].map(|c| (c * 255.0).round() as usize);
            g * 256 + r
        })
        .collect();
    assert!(
        bars.iter().copied().eq(0..skin_model::LANE_QUADS as usize),
        "{:?}",
        &bars[..bars.len().min(8)]
    );
    let notes = q
        .iter()
        .filter(|q| common::play::round(q.rect)[2] == 50)
        .count();
    assert_eq!(notes, 0);
    assert_eq!(d.batch.dropped(), 1);
    let covers = q
        .iter()
        .filter(|q| common::play::round(q.rect)[2..] == [430, 600])
        .count();
    assert_eq!(covers, 1, "{:?}", q.last().map(|q| q.rect));
    assert_eq!(q.len(), skin_model::LANE_QUADS as usize + 1);
}

/// F11: lanes whose 48 mapped parts each name a distinct id with no
/// image, every required array well formed, explicit equal sizes, no
/// image record and no file; `extra` more `size` entries past the eight.
fn f11(root: &Path, extra: usize) {
    use common::lanes::{NOTES, indexed_ids as ids, lane_rects};
    let mut sizes = String::with_capacity(2 * extra + 64);
    sizes.push_str("20, 20, 20, 20, 20, 20, 20, 20");
    for _ in 0..extra {
        sizes.push_str(",0");
    }
    let note = format!(
        r#""note": {{"id": "notes", "note": {n}, "lnstart": {s}, "lnend": {e},
            "lnbody": {b}, "lnactive": {a}, "hcnstart": {s}, "hcnend": {e},
            "hcnbody": {b}, "hcnactive": {a}, "hcndamage": {b}, "hcnreactive": {a},
            "mine": {m}, "size": [{sizes}], "dst": [{dst}]}}"#,
        n = ids("n"),
        s = ids("s"),
        e = ids("e"),
        b = ids("b"),
        a = ids("a"),
        m = ids("m"),
        dst = lane_rects(200, 600),
    );
    write(
        root,
        "s.json",
        common::hd(&format!(r#""image": [], {note}, "destination": [{NOTES}]"#)).as_bytes(),
    );
}

/// F11's chart: every lane dense with taps from 1 s, a long note on
/// model lane 1 across it and mines between the taps, more notes than
/// the lanes' quads.
fn f11_chart() -> bms_model::Chart {
    use bms_model::{Lane, TimeUs};
    let mut b = bms_model::ChartBuilder::new(150_000).long(
        Lane::new(1).unwrap(),
        TimeUs(900_000),
        TimeUs(3_000_000),
    );
    for l in [0, 2, 3, 4, 5, 6, 7] {
        for k in 0..3_000 {
            let at = 1_000_000 + k * 400;
            b = b.tap(Lane::new(l).unwrap(), TimeUs(at));
            if l == 2 {
                b = b.mine(Lane::new(l).unwrap(), TimeUs(at + 200), 10);
            }
        }
    }
    b.build().unwrap()
}

fn check_f11(i: Imported, chart: &bms_model::Chart) {
    let u = i.report.usage;
    assert_eq!(u.note_fallbacks, 48);
    assert_eq!((u.images, u.frames), (3, 3));
    assert_eq!((u.file_attempts, u.files), (0, 0));
    let fallbacks = i
        .report
        .notes
        .iter()
        .filter(|n| n.severity == skin_beatoraja::Severity::Fallback)
        .count();
    assert_eq!(fallbacks, 48);
    // The paint is in the atlas: a tap white, a mine red, the long note
    // yellow; the lanes spend their quads and drop the rest.
    let limits = skin_model::AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    let atlas = i
        .skin
        .atlas(skin_model::Variant::Lo, limits, &|| false)
        .unwrap();
    let mut d = Drawn::over(i.skin, chart);
    let q = d.frame(1_000, &[]);
    assert_eq!(q.len(), skin_model::LANE_QUADS as usize);
    assert!(d.batch.dropped() > 0);
    let seen: std::collections::BTreeSet<[u8; 4]> = q
        .iter()
        .map(|q| common::play::quad_texel(&atlas, q))
        .collect();
    let want = [[255, 0, 0, 255], [255, 255, 0, 255], [255, 255, 255, 255]];
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), want);
}

/// F13a: one one-digit value whose `offset` table holds `rows` rows, the
/// one cell reading the first, and an unknown `pad` array of `pad` more
/// values.
fn f13a(root: &Path, rows: usize, pad: usize) {
    png(root, "a.png", 120, 20);
    let mut table = String::with_capacity(9 * rows);
    for k in 0..rows {
        if k > 0 {
            table.push(',');
        }
        let _ = write!(table, r#"{{"x":{}}}"#, k % 7 + 1);
    }
    let mut zeros = String::with_capacity(2 * pad);
    for k in 0..pad {
        if k > 0 {
            zeros.push(',');
        }
        zeros.push('0');
    }
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}],
               "value": [{{"id": "v", "src": "a", "w": 120, "h": 20, "divx": 10, "digit": 1,
                 "ref": 110, "offset": [{table}], "pad": [{zeros}]}}],
               "destination": [{{"id": "v", "dst": [{{"x": 0, "y": 0, "w": 12, "h": 20}}]}}]"#
        ))
        .as_bytes(),
    );
}

fn check_f13a(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.resolved_digit_offsets, as_u32(MAX_RESOLVED_DIGIT_OFFSETS));
    assert_eq!(u.digit_offsets, 1);
    assert_eq!(i.skin.digit_offset_count(), 1);
    assert_eq!(u.elements, 1);
}

/// The value records of F13c and F13d: `n` of `record`, unreferenced.
fn offset_tables(root: &Path, n: usize, record: &str) {
    let mut values = String::with_capacity((record.len() + 1) * n);
    for k in 0..n {
        if k > 0 {
            values.push(',');
        }
        values.push_str(record);
    }
    write(
        root,
        "s.json",
        common::hd(&format!(r#""value": [{values}]"#)).as_bytes(),
    );
}

/// F13c's empty tables: two values each, as many as the node cap holds.
const EMPTY_TABLES: usize = MAX_NODES / 2 - 1_000;

/// definition read before the window: the import's own working storage.
fn import_peak(root: &Path) -> i64 {
    let (mut def, header) = read_header(root, "s.json").unwrap();
    let config = Config::defaults(&header);
    let (imported, p) = peak(|| import(&mut def, &header, &config).unwrap());
    assert_eq!(imported.report.usage.note_fallbacks, 48);
    p
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

/// F14a's judges: each one fitted three-key grade with three attachment
/// keys, a one-key one-digit combo and a one-key outer destination.
const ANCHORED: usize = 8_192;

/// F14a: [`ANCHORED`] judges at the key and widget quad caps, three
/// eighths of the keys (24,576) attachment keys. With `extra`, the first
/// combo has a second key, leaving the last attachment no room.
fn f14a(root: &Path, extra: bool) {
    png(root, "a.png", 64, 64);
    let judges = join((0..ANCHORED).map(|i| {
        let combo = if extra && i == 0 {
            r#""timer": 46, "dst": [{"time": 0, "x": 10, "y": 0, "w": 6, "h": 6}, {"time": 100}]"#
        } else {
            r#""dst": [{"x": 10, "y": 0, "w": 6, "h": 6}]"#
        };
        format!(
            "\n{{\"id\": \"j{i}\", \"index\": 0, \"images\": [{{\"id\": \"g\", \"timer\": 46, \
             \"stretch\": 1, \"dst\": [{{\"time\": 0, \"x\": 100, \"y\": 500, \"w\": 80, \"h\": 20}}, \
             {{\"time\": 50, \"x\": 110}}, {{\"time\": 100, \"x\": 120}}]}}], \
             \"numbers\": [{{\"id\": \"v\", {combo}}}]}}"
        )
    }));
    let dsts =
        join((0..ANCHORED).map(|i| format!(r#"{{"id": "j{i}", "dst": [{{"w": 0, "h": 0}}]}}"#)));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}],
               "image": [{{"id": "g", "src": "a", "w": 8, "h": 8}}],
               "value": [{{"id": "v", "src": "a", "y": 8, "w": 60, "h": 6, "divx": 10, "digit": 1}}],
               "judge": [{judges}],
               "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

fn check_f14a(_: &Header, i: Imported) {
    let u = i.report.usage;
    assert_eq!(u.keys, as_u32(MAX_SKIN_KEYS));
    assert_eq!(u.grade_anchor_keys, as_u32(3 * ANCHORED));
    assert_eq!(u.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(u.elements, as_u32(ANCHORED));
    assert!(i.report.notes.is_empty(), "{:?}", i.report.notes);
    // A PERFECT with combo 5 at the grades' first key: each paint the
    // 20×20 square fitted at 130, each "5" in its 6 px cell at 10 − 3 of
    // the destination at 100, on its bottom 580.
    let mut d = Drawn::new(i.skin, 100);
    d.tally = common::play::tally(0, 5);
    let q = d.frame(1_000, &[judged(1_000, Judgment::Perfect, 5)]);
    let mut by_rect = std::collections::BTreeMap::<[i32; 4], usize>::new();
    for quad in &q {
        *by_rect.entry(common::play::round(quad.rect)).or_default() += 1;
    }
    assert_eq!(
        by_rect.into_iter().collect::<Vec<_>>(),
        [([107, 574, 6, 6], ANCHORED), ([130, 560, 20, 20], ANCHORED)]
    );
    // Half way to the second key the destination is at 105, its paint at
    // 135.
    let q = d.frame(1_025, &[]);
    let mut xs = std::collections::BTreeMap::<i32, usize>::new();
    for quad in &q {
        *xs.entry(common::play::round(quad.rect)[0]).or_default() += 1;
    }
    assert_eq!(
        xs.into_iter().collect::<Vec<_>>(),
        [(112, ANCHORED), (135, ANCHORED)]
    );
}

/// F14a with a key more: the last judge's attachment is refused at its
/// grade before it is built.
fn f14a_refuses_past_the_key_cap(root: &Path) {
    f14a(root, true);
    let text = std::fs::read_to_string(root.join("s.json")).unwrap();
    let last = text
        .lines()
        .position(|l| l.contains(&format!("\"id\": \"j{}\", \"index\"", ANCHORED - 1)))
        .unwrap();
    match refused(root) {
        ImportError::Limit {
            limit: ImportLimit::Keys,
            at: Some(at),
        } => {
            assert_eq!(at.line as usize, last + 1, "{at:?}");
        }
        other => panic!("{other:?}"),
    }
}

/// F14b: [`MAX_ELEMENTS`] empty judge definitions no element draws, as
/// defined and as loaded: the inline attachment slots cost their size
/// with no attachment.
fn f14b(root: &Path) -> i64 {
    use skin_model::{Anchor, Fit, JudgeDef, Region, RegionDef, Sheet, SkinDef, WidgetDefs};
    let ((), p) = peak(|| {
        let judges = (0..MAX_ELEMENTS)
            .map(|_| JudgeDef {
                grades: Default::default(),
                combos: Default::default(),
                shift: false,
            })
            .collect();
        let def = SkinDef {
            fit: Fit::Adapt,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet: Sheet::new(root, Vec::new(), Vec::new()).unwrap(),
            lanes: None,
            elements: Vec::new(),
            widgets: WidgetDefs {
                judges,
                ..WidgetDefs::default()
            },
            bomb_up_to: None,
        };
        black_box(Skin::new(def).unwrap());
    });
    p
}

/// The F15 gauges: twelve nodes, each an 8×8 cell of `a`, of one gauge of
/// `parts` parts and `fields`, drawn by `dsts`.
fn f15(root: &Path, parts: usize, fields: &str, dsts: &[&str]) {
    png(root, "a.png", 64, 64);
    let images = join((0..12).map(|k| {
        format!(
            r#"{{"id": "n{k}", "src": "a", "x": {}, "y": {}, "w": 8, "h": 8}}"#,
            8 * (k % 8),
            8 * (k / 8)
        )
    }));
    let nodes = join((0..12).map(|k| format!("\"n{k}\"")));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], "image": [{images}],
               "gauge": {{"id": "g", "nodes": [{nodes}], "parts": {parts}{fields}}},
               "destination": [{}]"#,
            dsts.join(", ")
        ))
        .as_bytes(),
    );
}

const F15_DST: &str = r#"{"id": "g", "dst": [{"x": 100, "y": 500, "w": 1000, "h": 10}]}"#;
const FLICKER: &str = r#", "type": 3, "range": 1, "cycle": 330"#;

/// F15a: one gauge of 16 383 flickering parts, the widget budget whole.
fn f15a(root: &Path, parts: usize) {
    f15(root, parts, FLICKER, &[F15_DST]);
}

/// The node costs of every F15 gauge: twelve images of one frame from
/// one file.
fn check_nodes(i: &Imported) {
    let u = i.report.usage;
    assert_eq!((u.images, u.frames, u.files), (12, 12, 1));
}

fn check_f15a(_: &Header, i: Imported) {
    check_nodes(&i);
    assert_eq!(i.report.usage.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(i.skin.quad_capacity(), MAX_WIDGET_QUADS);
    // At the start value, 20 %: 3 276 lit, the tip over the last of them
    // right after it, then the rest.
    let mut d = Drawn::new(i.skin, 100);
    let q = d.frame(1_000, &[]);
    assert_eq!(q.len(), 16_384);
    assert_eq!(q[3_276].rect, q[3_275].rect);
    assert!(q[3_277].rect[0] > q[3_276].rect[0]);
    assert!(q.windows(2).filter(|w| w[0].rect == w[1].rect).count() == 1);
}

/// F15b: one still gauge of 16 384 parts (a range of 0).
fn f15b(root: &Path) {
    f15(root, 16_384, r#", "range": 0"#, &[F15_DST]);
}

fn check_f15b(_: &Header, i: Imported) {
    check_nodes(&i);
    assert_eq!(i.report.usage.widget_quads, MAX_WIDGET_QUADS);
    // At half the full gauge 8 192 lit, the tip on the last; below the
    // border of 80 % the first 13 107. By node: front below 8 191, the
    // tip below 1, back below 4 915, back above 3 277.
    let mut d = Drawn::new(i.skin, 100);
    d.gauges[0].value = GaugeValue(GaugeValue::FULL.0 / 2);
    let q = d.frame(1_000, &[]);
    assert_eq!(q.len(), 16_384);
    let mut by_uv = std::collections::HashMap::<[u32; 4], usize>::new();
    for quad in &q {
        *by_uv.entry(quad.uv.map(f32::to_bits)).or_default() += 1;
    }
    let mut counts: Vec<usize> = by_uv.into_values().collect();
    counts.sort_unstable();
    assert_eq!(counts, [1, 3_277, 4_915, 8_191]);
}

/// F15c: two destinations of one gauge of 8 191 flickering parts, one
/// additive, the other on READY and lower; with `third`, a third.
fn f15c(root: &Path, third: bool) {
    let mut dsts = vec![
        r#"{"id": "g", "blend": 2, "dst": [{"x": 100, "y": 500, "w": 1000, "h": 10}]}"#,
        r#"{"id": "g", "timer": 40, "dst": [{"x": 100, "y": 400, "w": 1000, "h": 10}]}"#,
    ];
    if third {
        dsts.push(F15_DST);
    }
    f15(root, 8_191, FLICKER, &dsts);
}

fn check_f15c(_: &Header, i: Imported) {
    check_nodes(&i);
    assert_eq!(i.report.usage.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(i.report.usage.elements, 2);
    // The first destination's 8 192 quads, then the second's, lower.
    let mut d = Drawn::new(i.skin, 100);
    let q = d.frame(1_000, &[]);
    assert_eq!(q.len(), 16_384);
    let ys: Vec<i32> = q.iter().map(|q| common::play::round(q.rect)[1]).collect();
    assert!(ys[..8_192].iter().all(|&y| y == 570), "{:?}", &ys[..2]);
    assert!(
        ys[8_192..].iter().all(|&y| y == 670),
        "{:?}",
        &ys[8_192..8_194]
    );
}

/// F15 past the budget: the flicker of 16 384 parts and F15c's third
/// destination are refused at their destination.
fn f15_refuses_past_the_budget() {
    for variant in 0..2 {
        let d = tempfile::tempdir().unwrap();
        if variant == 0 {
            f15a(d.path(), 16_384);
        } else {
            f15c(d.path(), true);
        }
        match refused(d.path()) {
            ImportError::Limit {
                limit: ImportLimit::WidgetQuads,
                at: Some(_),
            } => {}
            other => panic!("{variant}: {other:?}"),
        }
    }
}

/// F16's lanes: the keys 60 wide every 40 from 100, each 20 into the
/// next, the scratch at 20, 79 wide.
const F16_LANES: &str = r#"{"x": 100, "y": 200, "w": 60, "h": 600},
    {"x": 140, "y": 200, "w": 60, "h": 600}, {"x": 180, "y": 200, "w": 60, "h": 600},
    {"x": 220, "y": 200, "w": 60, "h": 600}, {"x": 260, "y": 200, "w": 60, "h": 600},
    {"x": 300, "y": 200, "w": 60, "h": 600}, {"x": 340, "y": 200, "w": 60, "h": 600},
    {"x": 20, "y": 200, "w": 79, "h": 600}"#;

/// F16: overlapping lanes of 48 distinct parts and three bar groups.
fn f16(root: &Path) {
    png(root, "a.png", 256, 256);
    let cell = |id: &str, i: usize, h: usize| {
        format!(
            r#"{{"id": "{id}", "src": "a", "x": {}, "y": {}, "w": 8, "h": {h}}}"#,
            8 * (i % 32),
            20 * (i / 32)
        )
    };
    let mut images: Vec<String> = (0..48)
        .map(|i| cell(&format!("p{}_{}", i / 6, i % 6), i, 20))
        .collect();
    images.extend(
        ["b0", "b1", "b2"]
            .iter()
            .zip(48..)
            .map(|(id, i)| cell(id, i, 2)),
    );
    let list = |k: usize| join((0..8).map(|u| format!("\"p{u}_{k}\"")));
    let groups = join((0..3).map(|g| {
        format!(
            r#"{{"id": "b{g}", "dst": [{{"x": 20, "y": {}, "w": 430, "h": {}}}]}}"#,
            200 - g,
            2 + 2 * g
        )
    }));
    write(
        root,
        "s.json",
        common::hd(&format!(
            r#""source": [{{"id": "a", "path": "a.png"}}], "image": [{}],
               "note": {{"id": "notes", "note": [{}], "lnstart": [{}], "lnend": [{}],
                 "lnbody": [{}], "lnactive": [{}], "mine": [{}], "hcnstart": [{}],
                 "hcnend": [{}], "hcnbody": [{}], "hcnactive": [{}], "hcndamage": [{}],
                 "hcnreactive": [{}], "dst": [{F16_LANES}], "group": [{groups}]}},
               "destination": [{{"id": "notes", "dst": [{{"w": 0, "h": 0}}]}}]"#,
            images.join(", "),
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
        ))
        .as_bytes(),
    );
}

/// F16a's chart: 1 100 chords within 400 ms from 1 s, 8 800 taps in
/// view, past the quad budget; bars at 1 000 and 1 050 ms.
fn f16a_chart() -> bms_model::Chart {
    let mut b = bms_model::ChartBuilder::new(150_000)
        .bar(bms_model::TimeUs(1_000_000))
        .bar(bms_model::TimeUs(1_050_000));
    for k in 0..1_100 {
        for l in 0..8 {
            let t = bms_model::TimeUs(1_000_000 + k * 360);
            b = b.tap(bms_model::Lane::new(l).unwrap(), t);
        }
    }
    b.build().unwrap()
}

fn check_f16a(_: &Header, i: Imported) {
    assert!(
        i.report
            .notes
            .iter()
            .all(|n| !matches!(n.kind, skin_beatoraja::NoteKind::UnsupportedField { .. }))
    );
    assert_eq!(i.report.usage.images, 51);
    // The two bars' three groups take six of the 8 192 quads, the taps
    // the rest: 1 023 chords whole, keys 1 to 7 then the scratch, then
    // keys 1 and 2 of the next; one drop.
    let mut d = Drawn::over(i.skin, &f16a_chart());
    let q = d.frame(900, &[]);
    let rects: Vec<[i32; 4]> = q.iter().map(|q| common::play::round(q.rect)).collect();
    let bars = rects.iter().filter(|r| r[2] == 430).count();
    let taps: Vec<i32> = rects.iter().filter(|r| r[3] == 20).map(|r| r[0]).collect();
    assert_eq!(bars, 6);
    assert_eq!(bars + taps.len(), skin_model::LANE_QUADS as usize);
    let chord = [100, 140, 180, 220, 260, 300, 340, 20];
    let want: Vec<i32> = chord.iter().copied().cycle().take(taps.len()).collect();
    assert_eq!(taps, want);
    assert_eq!(d.batch.dropped(), 1);
}

/// F16b's chart: `LANE_VISITS` long notes, 2 048 a lane, then with
/// `tap` a tap on the scratch.
fn f16b_chart(tap: bool) -> bms_model::Chart {
    let mut b = bms_model::ChartBuilder::new(150_000);
    let per_lane = i64::from(skin_model::LANE_VISITS) / 8;
    for l in 0..8 {
        for k in 0..per_lane {
            let t = 1_000_000 + k * 20;
            b = b.long(
                bms_model::Lane::new(l).unwrap(),
                bms_model::TimeUs(t),
                bms_model::TimeUs(t + 10),
            );
        }
    }
    if tap {
        b = b.tap(
            bms_model::Lane::new(0).unwrap(),
            bms_model::TimeUs(9_000_000),
        );
    }
    b.build().unwrap()
}

/// F16b: with no scroll left nothing ahead is visited or drawn
/// (stage3.md R6), so neither the long notes up to the visit budget nor
/// a tap past them drops anything.
fn check_f16b(_: &Header, i: Imported) {
    for (tap, dropped) in [(false, 0), (true, 0)] {
        let mut d = Drawn::over(i.skin.clone(), &f16b_chart(tap));
        d.view.cover = 1_000;
        let q = d.frame(900, &[]);
        assert!(
            q.iter().all(|q| common::play::round(q.rect)[3] != 20),
            "{tap}"
        );
        assert_eq!(d.batch.dropped(), dropped, "{tap}");
    }
}

/// Loads BMS `text` as a session does, at `speed`.
fn loaded_at(text: &str, speed: bms_model::Speed) -> bms_model::Chart {
    let dir = tempfile::tempdir().unwrap();
    let opts = bms_parser::LoadOptions {
        speed,
        ..bms_parser::LoadOptions::default()
    };
    bms_parser::load_bms(text.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart
}

/// F17c's chart: 499 999 STOPs, 600 to a measure at 1 000 BPM, then one
/// tap: 1 000 000 timeline rows, each at its own µs, so the playhead
/// keeps a representative per row with no endpoint but the last.
fn f17c_text() -> String {
    let mut text = String::from("#BPM 1000\n#WAV01 a.wav\n#STOP01 1\n");
    let (stops, per) = (499_999_usize, 600);
    let measures = stops.div_ceil(per);
    for m in 1..=measures {
        let n = per.min(stops - (m - 1) * per);
        let _ = writeln!(text, "#{m:03}09:{}", "01".repeat(n));
    }
    let _ = writeln!(text, "#{:03}11:01", measures + 1);
    text
}

/// F17d's chart: 800 measures of 24 µs at 1 000 BPM, each with 600 BGM
/// objects (25 rows to a µs) and a chord of taps on every lane at its
/// start, whose position the bar line's and the BGM's µs take; the
/// representatives are the stored µs, a few per measure.
fn f17d_text() -> String {
    let mut text = String::from("#BPM 1000\n#WAV01 a.wav\n");
    let chord = ["16", "11", "12", "13", "14", "15", "18", "19"];
    for m in 0..800 {
        let _ = writeln!(text, "#{m:03}02:0.0001\n#{m:03}01:{}", "01".repeat(600));
        for c in chord {
            let _ = writeln!(text, "#{m:03}{c}:01");
        }
    }
    text
}

/// What a play prepared over `chart` holds of its own and how many
/// representatives its playhead keeps, two prepared at once.
fn prepared_witness(skin: &Skin, chart: &bms_model::Chart, name: &str) {
    let ctx = common::play::context();
    let (a, b) = (
        skin.prepare(chart, &ctx).unwrap(),
        skin.prepare(chart, &ctx).unwrap(),
    );
    let reps = a.playhead().representatives().len();
    println!(
        "{name}: {} rows, {reps} representatives ({} B), play {} B, two at once {} B, \
         the timeline shared ({} B once)",
        chart.timeline.points().len(),
        a.playhead().heap_bytes(),
        a.resident_bytes(),
        a.resident_bytes() + b.resident_bytes(),
        size_of_val(chart.timeline.points()) + size_of_val(chart.timeline.bar_ids()),
    );
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
        "sizes: DigitOffset {}, DigitsDef {}, NumberDef {}, SolidImageDef {}, Usage {}, \
         NoteKind {}",
        size_of::<skin_model::DigitOffset>(),
        size_of::<skin_model::DigitsDef>(),
        size_of::<skin_model::NumberDef>(),
        size_of::<skin_model::SolidImageDef>(),
        size_of::<skin_beatoraja::Usage>(),
        size_of::<skin_beatoraja::NoteKind>(),
    );
    println!(
        "sizes: ImageChoiceDef {}, Option<ImageChoicePart> {}, CoverPaintDef {}, Run {}, Quad {}",
        size_of::<ImageChoiceDef>(),
        size_of::<Option<ImageChoicePart>>(),
        size_of::<skin_model::CoverPaintDef>(),
        size_of::<skin_model::Run>(),
        size_of::<skin_model::Quad>(),
    );
    println!(
        "sizes: Track {}, Option<Track> {}, GradePart {}, ComboPart {}",
        size_of::<skin_model::Track>(),
        size_of::<Option<skin_model::Track>>(),
        size_of::<skin_model::GradePart>(),
        size_of::<skin_model::ComboPart>(),
    );
    println!(
        "sizes: chart Note {}, TimelinePoint {}, TimelineId {}, ChartTimeline {}, \
         ExactPlayhead {}, SourceLanes {}",
        size_of::<bms_model::Note>(),
        size_of::<bms_model::TimelinePoint>(),
        size_of::<bms_model::TimelineId>(),
        size_of::<bms_model::ChartTimeline>(),
        size_of::<skin_model::ExactPlayhead>(),
        size_of::<skin_model::SourceLanes>(),
    );
    // A group's work in a play: its frame record and its heap slot of 2 B,
    // 32 768 B of heap at the cap.
    let scratch = Skin::bar_group_scratch_bytes();
    println!(
        "sizes: BarGroupDef {}, bar group scratch {scratch} (frame record {}), \
         at the cap {} (heap {})",
        size_of::<skin_model::BarGroupDef>(),
        scratch - 2,
        MAX_BAR_GROUPS * scratch,
        MAX_BAR_GROUPS * 2,
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
    // The groups' conditions: the full gauge (7), and with the judges'
    // region a PERFECT (1) and its negation (2), as F5's total checks.
    assert_eq!(
        SEVEN_OPS * 7 + (JUDGES - SEVEN_OPS) * 17 + 7 + 1 + 2,
        MAX_SKIN_OPS
    );
    assert_eq!(
        11 + 2 * JUDGES + ELEVEN_KEYS * 11 + (JUDGES - ELEVEN_KEYS) * 10 + 11,
        MAX_SKIN_KEYS
    );
    let d = tempfile::tempdir().unwrap();
    f5(d.path());
    peaks.push((
        "F5 play objects",
        measure_prepared(d.path(), &f5_chart(), check_f5),
    ));
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
    let d = tempfile::tempdir().unwrap();
    f9(d.path());
    peaks.push((
        "F9 bar groups",
        measure_prepared(d.path(), &f9_chart(), check_f9),
    ));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f10(d.path(), F10::Caps);
    peaks.push((
        "F10 trimmed covers",
        measure_prepared(d.path(), &f10_chart(), check_f10),
    ));
    drop(d);
    f10_refuses_past_its_caps();
    let chart = f11_chart();
    let d = tempfile::tempdir().unwrap();
    f11(d.path(), 0);
    peaks.push((
        "F11 fallback parts",
        measure_prepared(d.path(), &chart, |_, i| check_f11(i, &chart)),
    ));
    let small = import_peak(d.path());
    drop(d);
    // A `size` array near the node cap: the import peak with its input
    // stays in the bound, and the size reader keeps no more than eight
    // sizes, so the import's own working storage does not grow with it.
    let d = tempfile::tempdir().unwrap();
    let extra = MAX_NODES - 2_000;
    f11(d.path(), extra);
    peaks.push((
        "F11 long size array",
        measure_prepared(d.path(), &chart, |_, i| {
            assert_eq!(i.report.usage.note_fallbacks, 48);
        }),
    ));
    let large = import_peak(d.path());
    drop(d);
    println!("F11 import alone: {small} B, with {extra} more sizes {large} B");
    assert!(
        large - small < 4_096,
        "the size reader grew with the array: {small} B to {large} B"
    );
    // F13a: the resolution cap in one table, one row kept; a row more is
    // refused before it is read.
    let rows = MAX_RESOLVED_DIGIT_OFFSETS;
    let d = tempfile::tempdir().unwrap();
    f13a(d.path(), rows, 0);
    peaks.push(("F13a long offset table", measure(d.path(), check_f13a)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f13a(d.path(), rows + 1, 0);
    match refused(d.path()) {
        ImportError::Input {
            limit: InputLimit::DigitOffsets,
            at: Some(_),
        } => {}
        other => panic!("{other:?}"),
    }
    drop(d);
    // The same table with the input near the node cap: each row two
    // values, the rest of the skin about a dozen.
    let d = tempfile::tempdir().unwrap();
    let pad = MAX_NODES - 2 * rows - 2_000;
    f13a(d.path(), rows, pad);
    peaks.push(("F13a near the node cap", measure(d.path(), check_f13a)));
    drop(d);
    // F13c: empty tables cost their input alone, as much as the same
    // records with an unknown field.
    let d = tempfile::tempdir().unwrap();
    offset_tables(d.path(), EMPTY_TABLES, r#"{"offset":[]}"#);
    let empty = measure(d.path(), |_, i| {
        assert_eq!(i.report.usage.resolved_digit_offsets, 0);
        assert_eq!(i.report.usage.digit_offsets, 0);
    });
    peaks.push(("F13c empty offset tables", empty));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    offset_tables(d.path(), EMPTY_TABLES, r#"{"offsex":[]}"#);
    let unknown = measure(d.path(), |_, _| {});
    drop(d);
    println!("F13c {EMPTY_TABLES} empty tables: {empty} B, unknown fields {unknown} B");
    assert!(
        (empty - unknown).abs() < 4_096,
        "empty tables cost {empty} B against {unknown} B"
    );
    // F13d: the resolution cap in single-row tables, one range each.
    let d = tempfile::tempdir().unwrap();
    offset_tables(d.path(), rows, r#"{"offset":[{"x":1}]}"#);
    peaks.push((
        "F13d single-row offset tables",
        measure(d.path(), |_, i| {
            assert_eq!(
                i.report.usage.resolved_digit_offsets,
                as_u32(MAX_RESOLVED_DIGIT_OFFSETS)
            );
            assert_eq!(i.report.usage.digit_offsets, 0);
        }),
    ));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    offset_tables(d.path(), rows, r#"{"offsex":[{"x":1}]}"#);
    println!(
        "F13d with unknown fields instead: {} B",
        measure(d.path(), |_, _| {})
    );
    drop(d);
    // F14a: the key cap reached with three eighths of it (24,576)
    // attachment keys, and the last attachment refused with a key more.
    assert_eq!(ANCHORED * (3 + 3 + 1 + 1), MAX_SKIN_KEYS);
    assert_eq!(ANCHORED * 2, MAX_WIDGET_QUADS as usize);
    let d = tempfile::tempdir().unwrap();
    f14a(d.path(), false);
    peaks.push(("F14a grade attachments", measure(d.path(), check_f14a)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f14a_refuses_past_the_key_cap(d.path());
    drop(d);
    // F14b: empty judge definitions at the widget table cap.
    let d = tempfile::tempdir().unwrap();
    peaks.push(("F14b empty judges", f14b(d.path())));
    drop(d);
    // F15: gauges at the widget budget.
    println!(
        "sizes: GaugeDef {}, Motion {}, GaugeParts {}",
        size_of::<skin_model::GaugeDef>(),
        size_of::<skin_model::Motion>(),
        size_of::<skin_model::GaugeParts>(),
    );
    let d = tempfile::tempdir().unwrap();
    f15a(d.path(), 16_383);
    peaks.push(("F15a flickering gauge", measure(d.path(), check_f15a)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f15b(d.path());
    peaks.push(("F15b still gauge", measure(d.path(), check_f15b)));
    drop(d);
    let d = tempfile::tempdir().unwrap();
    f15c(d.path(), false);
    peaks.push(("F15c shared gauge", measure(d.path(), check_f15c)));
    drop(d);
    f15_refuses_past_the_budget();
    // F16: overlapping lanes at the lanes' budgets.
    let d = tempfile::tempdir().unwrap();
    f16(d.path());
    peaks.push((
        "F16a overlapping lanes",
        measure_prepared(d.path(), &f16a_chart(), check_f16a),
    ));
    peaks.push((
        "F16b lane visits",
        measure_prepared(d.path(), &f16b_chart(true), check_f16b),
    ));
    // F17: the exact playhead prepared over parser-loaded charts at the
    // timeline's extremes, on F16's lanes; the chart is loaded outside
    // the window, the play's representatives counted in it.
    let f17c = loaded_at(&f17c_text(), bms_model::Speed::NORMAL);
    assert_eq!(f17c.timeline.points().len(), 1_000_000);
    peaks.push((
        "F17c playhead at the row cap",
        measure_prepared(d.path(), &f17c, |_, i| {
            prepared_witness(&i.skin, &f17c, "F17c");
            let ctx = common::play::context();
            let play = i.skin.prepare(&f17c, &ctx).unwrap();
            assert_eq!(play.playhead().representatives().len(), 1_000_000);
            drop(play);
            // The chart and the play as a practice page holds them: the
            // 1x chart retained, the 50 % chart loaded beside it, and a
            // play prepared over each, all in one window, every shared
            // allocation counted once.
            let text = f17c_text();
            let (live, p) = peak(|| {
                let normal = loaded_at(&text, bms_model::Speed::NORMAL);
                let mapped = loaded_at(&text, bms_model::Speed::SLOWEST);
                let a = i.skin.prepare(&normal, &ctx).unwrap();
                let b = i.skin.prepare(&mapped, &ctx).unwrap();
                (normal, mapped, a, b)
            });
            println!("F17c 1x and 50 % charts with a play over each: peak {p} B");
            drop(live);
        }),
    ));
    let f17d = loaded_at(&f17d_text(), bms_model::Speed::NORMAL);
    peaks.push((
        "F17d dense one-µs buckets",
        measure_prepared(d.path(), &f17d, |_, i| {
            prepared_witness(&i.skin, &f17d, "F17d");
            let ctx = common::play::context();
            let play = i.skin.prepare(&f17d, &ctx).unwrap();
            let rows = f17d.timeline.points();
            let reps = play.playhead().representatives().len();
            assert!(reps * 10 < rows.len(), "{reps} of {}", rows.len());
            // Each chord's µs takes the chord's position.
            for n in &f17d.lanes[1] {
                assert_eq!(play.playhead().position_at(n.time), n.pos);
            }
        }),
    ));
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
