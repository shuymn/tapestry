//! The header's record caps are checked before anything is gathered for
//! a record: one option with far more items than the cap allows refuses
//! with no allocation sized by the array. Likewise an image set's entries
//! are charged before its table is gathered, a shared selector's ops
//! before its copy is made, and the lanes' bar groups and each group's
//! keys before their ids are copied, and a strip's digit offsets before they
//! are copied.
//!
//! A global allocator records the largest single allocation made on a
//! thread that opts in (a thread-local flag), and how many were of one
//! watched size, so the test harness's own threads do not disturb it. This file holds a single test for the same
//! reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{play, skin};
use skin_beatoraja::limits::MAX_OPTION_ITEMS;
use skin_beatoraja::{
    Config, Definition, ImportError, ImportLimit, InputLimit, import, read_header, resolve,
};
use skin_model::{
    MAX_BAR_GROUPS, MAX_IMAGE_CHOICE_REFS, MAX_SKIN_KEYS, MAX_SKIN_OPS, PLAY, Scope, Ty, compile,
};

struct Largest;

static LARGEST: AtomicUsize = AtomicUsize::new(0);
static WATCHED: AtomicUsize = AtomicUsize::new(usize::MAX);
static SEEN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
        if size == WATCHED.load(Ordering::Relaxed) {
            SEEN.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: delegates to `System`; only records a size, which never
// allocates.
unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Largest = Largest;

/// The largest single allocation `f` makes on this thread.
fn largest<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    TRACK.with(|t| t.set(false));
    (out, LARGEST.load(Ordering::Relaxed))
}

#[test]
fn option_items_are_counted_before_anything_is_gathered() {
    // Positive control: an allocation of 2 MiB is seen.
    let ((), seen) = largest(|| {
        black_box(vec![0u8; 2 << 20]);
    });
    assert!(seen >= 2 << 20, "{seen}");

    // One option whose item array is 64 times the cap.
    const ITEMS: usize = 64 * MAX_OPTION_ITEMS;
    let items = vec!["{}"; ITEMS].join(",");
    let d = skin(&[(
        "s.json",
        &play(&format!(r#""property": [{{"item": [{items}]}}]"#)),
    )]);
    let mut def = Definition::read(d.path(), "s.json").unwrap();
    let mut resolved = resolve(&mut def, &BTreeSet::new()).unwrap();
    let (result, seen) = largest(|| resolved.header().map(|_| ()));
    assert!(
        matches!(
            result,
            Err(ImportError::Input {
                limit: InputLimit::OptionItems,
                ..
            })
        ),
        "{result:?}"
    );
    // The items kept up to the cap, doubled while they grow, bound every
    // allocation; one id per item of the array would be 4 MiB.
    let bound = 2 * MAX_OPTION_ITEMS * size_of::<skin_beatoraja::OptionItem>();
    assert!(seen <= bound, "{seen} > {bound}");

    // One image set of 16 times the choice cap: refused before its 4 MiB
    // of item ids, or its entries, are gathered.
    const ENTRIES: usize = 16 * MAX_IMAGE_CHOICE_REFS;
    let items = vec!["null"; ENTRIES].join(",");
    let d = skin(&[(
        "s.json",
        &play(&format!(
            r#""w": 1920, "h": 1080, "imageset": [{{"id": "s", "images": [{items}]}}],
               "destination": [{{"id": "s", "dst": [{{"w": 1, "h": 1}}]}}]"#
        )),
    )]);
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    let config = Config::defaults(&h);
    let (result, seen) = largest(|| import(&mut def, &h, &config).map(|_| ()));
    assert!(
        matches!(
            result,
            Err(ImportError::Limit {
                limit: ImportLimit::ChoiceRefs,
                ..
            })
        ),
        "{result:?}"
    );
    let bound = 1 << 20;
    assert!(seen <= bound, "{seen} > {bound}");

    // The gauge type's selector (34 ops) kept by a set, image destinations
    // of 7 ops each up to 3 short of the op cap, then the set again: its
    // cached selector is refused before it is copied. A copy of the 34
    // ops is watched: the compile and the cache's own copy make two.
    let scope = Scope::new(&PLAY);
    let one = compile("0", &scope, Ty::Int).unwrap();
    let ((), op) = largest(|| drop(black_box(one.clone())));
    let fillers = (MAX_SKIN_OPS - 34) / 7;
    assert_eq!(MAX_SKIN_OPS - 34 - fillers * 7, 3);
    let set = r#"{"id": "s", "dst": [{"w": 1, "h": 1}]}"#;
    let filler = r#"{"id": "i", "op": [240], "dst": [{"w": 1, "h": 1}]}"#;
    let dsts = std::iter::once(set)
        .chain(std::iter::repeat_n(filler, fillers))
        .chain([set])
        .collect::<Vec<_>>()
        .join(",");
    let d = skin(&[(
        "s.json",
        &play(&format!(
            r#""w": 1920, "h": 1080, "source": [{{"id": "a", "path": "a.png"}}],
               "image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
               "imageset": [{{"id": "s", "images": ["i"], "ref": 40}}],
               "destination": [{dsts}]"#
        )),
    )]);
    common::png(d.path(), "a.png", 1, 1);
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    let config = Config::defaults(&h);
    WATCHED.store(34 * op, Ordering::Relaxed);
    SEEN.store(0, Ordering::Relaxed);
    let (result, _) = largest(|| import(&mut def, &h, &config).map(|_| ()));
    WATCHED.store(usize::MAX, Ordering::Relaxed);
    assert!(
        matches!(
            result,
            Err(ImportError::Limit {
                limit: ImportLimit::Ops,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(SEEN.load(Ordering::Relaxed), 2);

    // Bar groups 16 times their cap, then one group of 4 times the skin's
    // keys: each list's ids are gathered once by the resolution and
    // refused before the import copies them.
    let groups = vec!["{}"; 16 * MAX_BAR_GROUPS].join(",");
    let keys = vec![r#"{"w": 1, "h": 1}"#; 4 * MAX_SKIN_KEYS].join(",");
    for (group, n, want) in [
        (groups, 16 * MAX_BAR_GROUPS, ImportLimit::BarGroups),
        (
            format!(r#"{{"id": "bar", "dst": [{keys}]}}"#),
            4 * MAX_SKIN_KEYS,
            ImportLimit::Keys,
        ),
    ] {
        let note = common::lanes::note(&format!(r#""group": [{group}]"#));
        let d = skin(&[(
            "s.json",
            &common::hd(&format!(
                r#""source": [{{"id": "a", "path": "a.png"}}], {}"#,
                common::lanes::body("", &note, &[common::lanes::NOTES])
            )),
        )]);
        common::png(d.path(), "a.png", 64, 64);
        let (mut def, h) = read_header(d.path(), "s.json").unwrap();
        let config = Config::defaults(&h);
        WATCHED.store(n * size_of::<u32>(), Ordering::Relaxed);
        SEEN.store(0, Ordering::Relaxed);
        let (result, _) = largest(|| import(&mut def, &h, &config).map(|_| ()));
        WATCHED.store(usize::MAX, Ordering::Relaxed);
        assert!(
            matches!(result, Err(ImportError::Limit { limit, .. }) if limit == want),
            "{result:?}"
        );
        assert_eq!(SEEN.load(Ordering::Relaxed), 1, "{want:?}");
    }

    // Digit offsets: the last number's ten offsets (160 B) are copied
    // when the cap admits them, and not when their charge passes it. The
    // refused imports of nine and of ten make as many allocations of that
    // size; the admitted one of ten makes more.
    let ten = 10 * size_of::<skin_model::DigitOffset>();
    let mut seen = Vec::new();
    for (judges, last, kept) in [(2_729, 10, true), (2_730, 10, false), (2_730, 9, false)] {
        let d = skin(&[("s.json", &common::kept_offsets(judges, last))]);
        common::png(d.path(), "n.png", 120, 20);
        let (mut def, h) = read_header(d.path(), "s.json").unwrap();
        let config = Config::defaults(&h);
        WATCHED.store(ten, Ordering::Relaxed);
        SEEN.store(0, Ordering::Relaxed);
        let (result, _) = largest(|| import(&mut def, &h, &config).map(|_| ()));
        WATCHED.store(usize::MAX, Ordering::Relaxed);
        if kept {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(
                matches!(
                    result,
                    Err(ImportError::Limit {
                        limit: ImportLimit::DigitOffsets,
                        ..
                    })
                ),
                "{result:?}"
            );
        }
        seen.push(SEEN.load(Ordering::Relaxed));
    }
    let [admitted, refused, nine] = seen[..] else {
        panic!("{seen:?}");
    };
    assert_eq!(refused, nine, "{seen:?}");
    assert!(admitted > refused, "{seen:?}");
}
