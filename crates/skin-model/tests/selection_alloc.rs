//! A selection skin's per-frame build allocates nothing (docs/stage3.md
//! S10-09d): the song list's slots with every part and its row text, the
//! ratio bars and a slider's travel and press, numbers and
//! text over the selection fields, and every timer, frame after frame as
//! the timers move, over views that change between frames; and the
//! settings applied every frame (docs/stage3.md S10-09e), the volume's
//! number and ratio bar drawing them, and the application's readouts
//! changing every frame, numbers drawing them; and pointer input
//! (docs/stage3.md S10-09e): frames whose pointer moves across hover rects,
//! presses hitting chart and folder rows whose paths are long, an action,
//! and drags on a slider.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

mod selection_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use selection_support::{
    Fx, all, chart, digits, element, features, folder, int, labels, lamps, levels, list,
    list_element, load, ratio, rect, row_text, view,
};
use skin_model::{
    ActionDef, AppReadouts, Button, ClickType, FolderId, FontRef, FrameUv, GaugeLook, HoverRect,
    LocalTime, MasterVolume, NumberDef, OwnLamp, PointerDef, PointerHit, RatioBarDef, RatioForm,
    RatioSource, RowKey, RowRef, RowRefKind, RowScore, SelectionDefs, SelectionOrder,
    SelectionSettings, SliderDirection, Source, SpeedReference, SpeedView, TextDef, TextField,
    TextImage, TextTable, TextValue, WidgetDefs,
};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The allocations `f` makes on this thread.
fn counted(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    f();
    TRACK.with(|t| t.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

fn skin() -> Fx {
    let mut def = list(24, 11);
    def.text = [
        Some(row_text([0, -40, 200, 20])),
        Some(row_text([0, -40, 200, 20])),
        Some(row_text([0, -40, 200, 20])),
    ];
    def.lamp = lamps([0, -10, 10, 10]);
    def.level = levels([20, -10, 10, 10], 3);
    def.label = labels([40, -10, 10, 10]);
    def.clickable = (0..24).rev().collect();
    let elements = vec![
        list_element(),
        element(Source::Number(0), [500, 0, 10, 10]),
        element(Source::Text(0), [500, 50, 200, 20]),
        element(Source::Text(1), [500, 80, 200, 20]),
        element(Source::RatioBar(0), [800, 0, 20, 200]),
        element(Source::RatioBar(1), [900, 0, 20, 20]),
        element(Source::Number(1), [500, 120, 10, 10]),
        element(Source::RatioBar(2), [1000, 0, 200, 20]),
        element(Source::Number(2), [500, 140, 10, 10]),
        element(Source::Number(3), [500, 160, 10, 10]),
        // An action shown only under the pointer; the first number is too.
        element(Source::Image(5), [1_300, 0, 100, 40]),
    ];
    let hover = HoverRect {
        x: 0,
        y: 0,
        w: 100,
        h: 40,
    };
    let text = |value| TextDef {
        value,
        font: FontRef::Default,
        align: skin_model::Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    let skin = load(
        elements,
        WidgetDefs {
            numbers: vec![
                NumberDef {
                    digits: digits(6),
                    value: int("selected.ex"),
                },
                NumberDef {
                    digits: digits(6),
                    value: int("settings.volume"),
                },
                NumberDef {
                    digits: digits(6),
                    value: int("app.uptime_s"),
                },
                NumberDef {
                    digits: digits(6),
                    value: int("clock.second"),
                },
            ],
            texts: vec![
                text(TextValue::Field(TextField::FullTitle)),
                text(TextValue::Selection(skin_model::SelectionText::Place)),
            ],
            ..WidgetDefs::default()
        },
        SelectionDefs {
            song_list: Some(def),
            pointer: vec![
                PointerDef {
                    element: 10,
                    action: Some(ActionDef {
                        event: 12,
                        click: ClickType::Halves,
                    }),
                    hover: Some(hover),
                    search: false,
                    // Its region sampled each frame, as an imported
                    // stretched image's.
                    region: Some(rect([1_300, 0, 100, 40])),
                },
                PointerDef {
                    element: 1,
                    action: None,
                    hover: Some(hover),
                    search: false,
                    region: None,
                },
            ],
            ratio_bars: vec![
                RatioBarDef {
                    image: 3,
                    form: RatioForm::Bar { vertical: true },
                    source: RatioSource::ExRate,
                },
                RatioBarDef {
                    image: 4,
                    form: RatioForm::Slider {
                        direction: SliderDirection::Down,
                        range: 300,
                        changeable: true,
                    },
                    source: RatioSource::ListPosition,
                },
                RatioBarDef {
                    image: 3,
                    form: RatioForm::Bar { vertical: false },
                    source: RatioSource::MasterVolume,
                },
            ],
        },
    )
    .unwrap();
    Fx::new(skin)
}

/// The application's readouts of the frame at `ms`: every one moving with
/// it.
fn app(ms: i64) -> AppReadouts {
    let k = u8::try_from(ms / 16 % 60).unwrap();
    AppReadouts {
        fps: u16::from(k),
        uptime_us: ms * 1_000_000 + 3_600_000_000,
        local: LocalTime {
            year: 2_026 + u16::from(k),
            month: 1 + k % 12,
            day: 1 + k % 31,
            hour: k % 24,
            minute: k,
            second: 59 - k,
        },
    }
}

/// The settings of the frame at `ms`: every one moving with it.
fn settings(ms: i64) -> SelectionSettings {
    let k = u32::try_from(ms / 16).unwrap();
    SelectionSettings {
        display_offset_ms: i32::try_from(ms).unwrap() - 1_000,
        master_volume: MasterVolume::of_gain(f32::from(u8::try_from(k % 101).unwrap()) / 100.0),
        speed: if k % 2 == 0 {
            SpeedView::Classic
        } else {
            SpeedView::Floating {
                reference: SpeedReference::Main,
                green: 1 + k,
            }
        },
        order: [
            SelectionOrder::Title,
            SelectionOrder::Artist,
            SelectionOrder::Level,
            SelectionOrder::Modified,
        ][usize::try_from(k % 4).unwrap()],
        gauge: (k % 3 > 0).then_some(GaugeLook::Normal),
        covers_on: [k % 2 == 0, k % 3 == 0, k % 5 == 0],
        play_to_end: k % 2 == 1,
        assignment: [
            None,
            Some(rules::Assignment::ComboPriority),
            Some(rules::Assignment::NearestTiesEarlier),
        ][usize::try_from(k % 3).unwrap()],
    }
}

#[test]
fn a_selection_build_allocates_nothing() {
    let mut fx = skin();
    let line = TextImage::new(
        FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 0,
        },
        [0.0, 0.0, 1.0, 1.0],
        1.0,
    );
    let rows: Vec<_> = (0..40)
        .map(|i| {
            let mut r = match i % 7 {
                0 => folder(&format!("f{i}"), 3),
                1 => all(40),
                _ => chart(&format!("c{i}")),
            };
            r.level_value = i;
            r.difficulty = u8::try_from(i % 6).ok().filter(|&d| d > 0);
            r.features = features(i % 2 == 0, i % 3 == 0);
            r.random = i % 4 == 0;
            r.score = match i % 3 {
                0 => RowScore::Unplayed,
                1 => RowScore::NoBest { plays: 2 },
                _ => RowScore::Best {
                    plays: 3,
                    ex: 100,
                    bp: 2,
                    lamp: OwnLamp::Cleared {
                        gauge: 0,
                        rank: 1,
                        look: GaugeLook::Hard,
                    },
                },
            };
            r
        })
        .collect();
    let mut total = 0;
    for cursor in [0_usize, 5, 20, 39] {
        let mut v = view(rows.clone(), Some(cursor));
        v.position = ratio(u64::try_from(cursor).unwrap(), 39);
        if let Some(c) = v.selected.as_mut() {
            c.max_ex = Some(300);
            c.judge_class = u8::try_from(cursor % 5).ok();
        }
        fx.view(&v);
        let texts = TextTable::new(vec![line; fx.skin.text_slots()]);
        let row_texts = TextTable::new(vec![line; fx.skin.song_list_slots()]);
        fx.skin
            .attach_selection_text(&mut fx.sel, texts, row_texts)
            .unwrap();
        // One build outside the count: the batch reaches its size.
        fx.frame(0);
        assert!(!fx.batch.quads().is_empty());
        for ms in (0..2_000).step_by(16) {
            fx.timers.change = Some(TimeUs(0));
            fx.timers.panels[0] = (ms > 1_000).then_some(TimeUs(1_000_000));
            fx.app = app(ms);
            total += counted(|| {
                fx.skin
                    .update_selection_settings(&mut fx.sel, &settings(ms))
                    .unwrap();
                fx.frame(ms);
                std::hint::black_box(fx.skin.drag_at(&fx.sel, [910.0, 30.0], fx.target));
            });
        }
    }
    assert_eq!(total, 0);
    // Row text in a bitmap font: laid out at the view, drawn glyph by
    // glyph without allocating.
    let (_dir, skin) = selection_support::bitmap_list(5, 2);
    let mut fx = Fx::new(skin);
    let mut bitmap_total = 0;
    for cursor in [0_usize, 2, 4] {
        let rows = vec![
            chart("AB"),
            chart("BA"),
            chart("ABAB"),
            chart("B"),
            chart("A"),
        ];
        fx.view(&view(rows, Some(cursor)));
        fx.skin.attach_selection_bitmap_text(&mut fx.sel);
        fx.frame(0);
        assert!(fx.batch.quads().len() > 5);
        for ms in (0..500).step_by(16) {
            bitmap_total += counted(|| fx.frame(ms));
        }
    }
    assert_eq!(bitmap_total, 0);
    // Pointer input over 30 chart rows and 30 folder rows whose paths are
    // 260 bytes: a press names a row by its fixed-size identity, never its
    // path.
    let mut fx = crate::skin();
    let long = |i: usize| PathBuf::from(format!("{}{i:03}", "p".repeat(257)));
    let rows: Vec<_> = (0..60)
        .map(|i| {
            let mut r = if i < 30 {
                chart(&format!("c{i}"))
            } else {
                folder(&format!("f{i}"), 2)
            };
            assert_eq!(long(i).as_os_str().len(), 260);
            r.key = if i < 30 {
                RowKey::Chart(long(i))
            } else {
                RowKey::Folder(FolderId::new(long(i)))
            };
            r.index = Some(u32::try_from(i).unwrap());
            r
        })
        .collect();
    let mut pointer_total = 0;
    let (mut actions, mut drags) = (0, 0);
    let mut hits = [0_u32; 2];
    // The cursor at 11 puts chart rows 0 to 23 in the slots, at 41 folder
    // rows 30 to 53.
    for (cursor, kind, first) in [(11, RowRefKind::Chart, 0), (41, RowRefKind::Folder, 30)] {
        let mut v = view(rows.clone(), Some(cursor));
        v.position = ratio(u64::try_from(cursor).unwrap(), 59);
        v.list_revision = 9;
        fx.view(&v);
        let texts = TextTable::new(vec![line; fx.skin.text_slots()]);
        let row_texts = TextTable::new(vec![line; fx.skin.song_list_slots()]);
        fx.skin
            .attach_selection_text(&mut fx.sel, texts, row_texts)
            .unwrap();
        fx.frame(0);
        pointer_total += counted(|| {
            // The pointer in and out of the hover rects, frame by frame.
            for k in 0..100_u16 {
                let x = if k % 2 == 0 { 1_350.0 } else { 1_500.0 };
                fx.pointer = Some([x, 20.0 + f32::from(k % 20)]);
                fx.frame(i64::from(k) * 16);
                let press = fx
                    .skin
                    .hit_at(&fx.sel, [1_310.0, 20.0], fx.target, Button::Right);
                if let Some(PointerHit::Action { event: 12, .. }) = press.hit {
                    actions += 1;
                }
            }
            // 50 presses on the rows, slot by slot.
            for k in 0..50_u16 {
                let slot = k % 24;
                let want = PointerHit::Row {
                    row: RowRef {
                        list_revision: 9,
                        index: first + u32::from(slot),
                        kind,
                    },
                    button: Button::Left,
                };
                let at = [110.0, 100.0 * f32::from(slot) + 25.0];
                if fx.skin.hit_at(&fx.sel, at, fx.target, Button::Left).hit == Some(want) {
                    hits[usize::from(kind == RowRefKind::Folder)] += 1;
                }
            }
            // 25 drags along the list's slider.
            for k in 0..25_u16 {
                let at = [910.0, 20.0 + 10.0 * f32::from(k)];
                if fx.skin.drag_at(&fx.sel, at, fx.target).is_some() {
                    drags += 1;
                }
            }
        });
    }
    assert_eq!(pointer_total, 0);
    assert_eq!((actions, hits, drags), (100, [50, 50], 50));
    assert_eq!(size_of::<RowRef>(), 16);
    // The positive control: the counter sees an allocation on this thread
    // (design.md "Scope of No Allocation").
    assert!(
        counted(|| {
            std::hint::black_box(vec![0_u8; 1]);
        }) > 0
    );
}
