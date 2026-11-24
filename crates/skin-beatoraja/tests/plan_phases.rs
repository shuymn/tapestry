//! The atlas planning's bulk operations timed between its asks (design.md
//! "Load Pipeline", the cooperative stop's 50 ms target): the stretch
//! from each ask to the next, labelled by the sites around it (the sort is
//! `PlanSort` → `PlanScan`, a side's table setup `PlanSide` →
//! `PlanPlace`), and the sheet unions the result skin's atlas step and its
//! release make: whole, as `Sheet::union` makes them unasked, and between
//! the asks of `Sheet::union_checked`, which the step calls. Run by hand,
//! its output kept:
//!
//! ```text
//! cargo test --release -p skin-beatoraja --features dev-tools --test plan_phases -- --ignored --nocapture
//! ```
//!
//! The inputs are a self-made sheet at the limits (65 536 cells over
//! 4 096 files), the union of two such sheets of one folder and, when
//! `PLAN_PHASES_SKINS` names them (`<root>|<entry>` separated by `;`),
//! real result skins imported from a folder outside the repository.

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::result::{best, facts, ready};
use skin_beatoraja::{Cancel, Config, LuaOutcome, read_lua_header};
use skin_model::{
    AtlasLimits, FileDef, ImageDef, KeptLayers, MAX_FILES, MAX_SKIN_FRAMES, ResultNumber, Root,
    Sheet, SheetBuilder, Variant,
};

/// A device's limits as the development PC's adapter reports them, the
/// render crate's one reserved layer.
const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 16_384,
    max_layers: 2_048,
    reserved_layers: 1,
};

/// Each plan is timed this many times; the worst stretch is reported.
const RUNS: usize = 5;

/// The cells of the limits sheet each file holds, a 4 × 4 grid.
const GRID: u16 = 4;

/// The site names, by [`skin_model::dev::last_site`]'s codes.
fn site_name(code: Option<u8>) -> &'static str {
    use skin_model::dev::site;
    match code {
        None => "start",
        Some(site::PLAN_RECTS) => "PlanRects",
        Some(site::PLAN_SORT) => "PlanSort",
        Some(site::PLAN_SCAN) => "PlanScan",
        Some(site::PLAN_SIDE) => "PlanSide",
        Some(site::PLAN_PLACE) => "PlanPlace",
        Some(site::PLAN_RECT_WORK) => "PlanRectWork",
        Some(site::PLAN_PLACE_WORK) => "PlanPlaceWork",
        Some(site::PLAN_END) => "PlanEnd",
        Some(site::UNION_WORK) => "UnionWork",
        Some(_) => "other",
    }
}

/// The worst and total time of each labelled stretch, and its count.
#[derive(Default)]
struct Stretches(BTreeMap<String, (Duration, Duration, u64)>);

impl Stretches {
    fn add(&mut self, label: String, d: Duration) {
        let e = self.0.entry(label).or_default();
        e.0 = e.0.max(d);
        e.1 += d;
        e.2 += 1;
    }

    fn print(&self, name: &str) {
        for (label, (worst, total, n)) in &self.0 {
            println!(
                "{name}: {label}: worst {:.3} ms, total {:.3} ms, {n} stretches",
                ms(*worst),
                ms(*total)
            );
        }
    }

    fn worst(&self) -> Duration {
        self.0.values().map(|e| e.0).max().unwrap_or_default()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

/// Runs `work` once with a cancel that never stops, timing every
/// stretch between two of its asks into `all`, labelled by the sites
/// around it; returns its result and its whole time.
fn timed<R>(all: &mut Stretches, work: impl FnOnce(&dyn Fn() -> bool) -> R) -> (R, Duration) {
    let at = RefCell::new((None::<u8>, Instant::now()));
    let stretches = RefCell::new(Vec::new());
    let cancel = || {
        let now = Instant::now();
        let site = skin_model::dev::last_site();
        let (prev, then) = at.replace((site, now));
        stretches.borrow_mut().push((
            format!("{} -> {}", site_name(prev), site_name(site)),
            now - then,
        ));
        false
    };
    let start = Instant::now();
    *at.borrow_mut() = (None, start);
    let r = work(&cancel);
    let end = Instant::now();
    let (last, then) = *at.borrow();
    stretches
        .borrow_mut()
        .push((format!("{} -> return", site_name(last)), end - then));
    for (label, d) in stretches.into_inner() {
        all.add(label, d);
    }
    (r, end - start)
}

/// Plans `sheet` [`RUNS`] times, timing every stretch between two asks,
/// and prints them with the plan's shape.
fn plan(name: &str, sheet: &Sheet) -> Duration {
    let mut all = Stretches::default();
    for _ in 0..RUNS {
        let (plan, whole) = timed(&mut all, |cancel| {
            sheet.plan_atlas(Variant::Lo, LIMITS, KeptLayers::NONE, cancel)
        });
        let outcome = match plan {
            Ok(p) => {
                let shape = p.shape();
                format!("side {}, pages {}", shape.side, shape.pages)
            }
            // A refused union: the step plans its own sheet instead.
            Err(e) => format!("refused: {e:?}"),
        };
        println!(
            "{name}: plan {:.3} ms, {outcome}, {} frames",
            ms(whole),
            sheet.frame_count()
        );
    }
    all.print(name);
    all.worst()
}

/// Times `a.union(b)`, which asks nothing, and `a.union_checked(b)`'s
/// stretches between its asks, [`RUNS`] times each; returns the union,
/// the worst whole union and the worst stretch.
fn union(name: &str, a: &Sheet, b: &Sheet) -> (Sheet, Duration, Duration) {
    let mut worst = Duration::ZERO;
    let mut out = None;
    for _ in 0..RUNS {
        let start = Instant::now();
        let u = a.union(b).expect("one folder");
        let d = start.elapsed();
        worst = worst.max(d);
        out = Some(u);
    }
    let u = out.unwrap();
    println!(
        "{name}: union worst {:.3} ms, {} + {} frames -> {} cells",
        ms(worst),
        a.frame_count(),
        b.frame_count(),
        u.frame_count()
    );
    let mut all = Stretches::default();
    let mut checked = Duration::ZERO;
    for _ in 0..RUNS {
        let (r, whole) = timed(&mut all, |cancel| a.union_checked(b, cancel));
        assert_eq!(
            r.unwrap().map(|c| c.frame_count()),
            Some(u.frame_count()),
            "the checked union is the union"
        );
        checked = checked.max(whole);
    }
    println!("{name}: union_checked worst {:.3} ms", ms(checked));
    all.print(&format!("{name} union_checked"));
    (u, worst, all.worst())
}

/// A file's cell size: mixed, 8 to 64 texels a side.
fn cell(f: u32) -> [u32; 2] {
    [8 + f * 37 % 57, 8 + f * 53 % 49]
}

/// Writes the limits folder's files: file `f` a `GRID` × `GRID` grid of
/// [`cell`]s, one colour.
fn write_files(dir: &Path) {
    for f in 0..u32::try_from(MAX_FILES).unwrap() {
        let [w, h] = cell(f);
        let (w, h) = (w * u32::from(GRID), h * u32::from(GRID));
        let file = fs::File::create(dir.join(format!("f{f:04}.png"))).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let mut w8 = e.write_header().unwrap();
        let px = [u8::try_from(f % 251).unwrap(), 9, 7, 255];
        let data: Vec<u8> = px
            .iter()
            .copied()
            .cycle()
            .take(usize::try_from(w * h * 4).unwrap())
            .collect();
        w8.write_image_data(&data).unwrap();
    }
}

/// A sheet of every file of `dir`, each cut into `grid` (columns, rows),
/// the files admitted in `order`.
fn limits_sheet(dir: &Path, grid: [u16; 2], order: impl Iterator<Item = u32>) -> Sheet {
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    for f in order {
        let file = b
            .admit_file(FileDef {
                path: format!("f{f:04}.png"),
                hi: None,
            })
            .unwrap();
        b.add_image(ImageDef {
            file,
            rect: None,
            grid,
            cycle: None,
            filter: skin_model::Filter::Nearest,
        })
        .unwrap();
    }
    b.finish()
}

/// The real result skins `PLAN_PHASES_SKINS` names, each imported twice
/// (a cleared and a failed view), as two layouts of one folder.
fn real_skins() -> Vec<(String, Sheet, Sheet)> {
    let Ok(list) = std::env::var("PLAN_PHASES_SKINS") else {
        return Vec::new();
    };
    list.split(';')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (root, entry) = s.split_once('|').expect("<root>|<entry>");
            let import = |cleared| {
                let cancel = Cancel::new();
                let h = read_lua_header(Path::new(root), entry, &cancel).unwrap();
                let view = ready(1_000, cleared, best(ResultNumber::Shown(-3)));
                match h
                    .import(&Config::defaults(h.header()), &facts(Some(&view)), &cancel)
                    .unwrap()
                {
                    LuaOutcome::Imported(i) => i.imported.skin.sheet().clone(),
                    LuaOutcome::Pending(p) => panic!("{p:?}"),
                }
            };
            (format!("{root}|{entry}"), import(true), import(false))
        })
        .collect()
}

#[test]
#[ignore = "a measurement run by hand (design.md \"Load Pipeline\")"]
fn plan_phases() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path());
    let files = u32::try_from(MAX_FILES).unwrap();
    let a = limits_sheet(dir.path(), [GRID, GRID], 0..files);
    assert_eq!(a.frame_count(), MAX_SKIN_FRAMES);
    // The other layout: each file cut into 8 × 2, admitted in reverse,
    // so every file of one is found far into the other's list.
    let b = limits_sheet(dir.path(), [2 * GRID, GRID / 2], (0..files).rev());
    let mut worst = Vec::new();
    worst.push(("limits plan", plan("limits", &a)));
    // The step's union: the layout's sheet with an earlier entry's.
    let (u, d, w) = union("limits step", &a, &b);
    worst.push(("limits step union (whole)", d));
    worst.push(("limits step union_checked (stretch)", w));
    worst.push(("limits union plan", plan("limits union", &u)));
    // The release's union: kept cells (an earlier union) with a dropped
    // entry's sheet.
    let (_, d, w) = union("limits release", &u, &b);
    worst.push(("limits release union (whole)", d));
    worst.push(("limits release union_checked (stretch)", w));
    for (name, one, two) in real_skins() {
        worst.push(("real plan", plan(&name, &one)));
        let (u, d, w) = union(&format!("{name} step"), &one, &two);
        worst.push(("real step union (whole)", d));
        worst.push(("real step union_checked (stretch)", w));
        worst.push(("real union plan", plan(&format!("{name} union"), &u)));
        let (_, d, w) = union(&format!("{name} release"), &u, &two);
        worst.push(("real release union (whole)", d));
        worst.push(("real release union_checked (stretch)", w));
    }
    for (name, d) in worst {
        println!(
            "worst: {name}: {:.3} ms{}",
            ms(d),
            if d > Duration::from_millis(25) {
                " OVER 25 ms"
            } else {
                ""
            }
        );
    }
}
