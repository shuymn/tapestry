//! The selection's chart-image pool on WARP (design.md "Rendering and
//! Skins", chart images): a write into a slot's cells at every level with
//! its uvs, a refused write clearing the slot, a stale one not written, the
//! pool reduced to its firm layer and to none, and a new generation at each
//! array made, none after removal (AC-S13). A selection atlas that keeps a
//! chart layer has no pool.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use skin_model::chart_image::{
    ChartDir, ChartImage, ChartStretch, ChartUv, Fitted, POOL_LAYER, Slot, read,
};
use skin_model::{
    Anchor, AppReadouts, Ease, ElementDef, End, Filter, Fit, KeptLayers, KeyDef, Layout, Place,
    Region, RegionDef, Root, ScreenKind, SelectionDefs, SelectionFrame, SelectionTimers, Side,
    Skin, SkinDef, SolidImageDef, Source, TimerRef, Track, Variant, WidgetDefs, selection_timer,
};

use super::{
    Class, DestroyRecord, Destroyed, Part, PoolWriteError, RenderError, Renderer, SHARED_TEXELS,
    pool_fault,
};
use crate::{ClipTable, SkinBundle};

/// One layer of the pool of [`pool_bundle`]'s skin at 1 920×1 080: its
/// page side raised to 2 048.
pub(crate) const LAYER: u64 = 2_048 * 2_048;

fn element(source: Source, [x, y, w, h]: [i32; 4]) -> ElementDef {
    let mut props = [None; 9];
    props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
    ElementDef {
        source,
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(selection_timer::SCREEN),
        visible: None,
        blend: skin_model::Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

/// A selection skin over `dir` drawing the stage file at (0, 0) 400×600
/// and the banner at (800, 0) 400×200, both filling their destinations.
fn chart_skin(dir: &Path) -> Skin {
    let mut b = skin_model::SheetBuilder::new(&Root::new(dir).unwrap());
    b.add_solid_image(SolidImageDef {
        size: [16, 16],
        color: [255, 255, 255, 255],
        filter: Filter::Nearest,
        grid: None,
    })
    .unwrap();
    let chart = |which| Source::ChartImage {
        which,
        filter: Filter::Nearest,
        stretch: ChartStretch::Fill,
    };
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements: vec![
            element(chart(ChartImage::Stage), [0, 0, 400, 600]),
            element(chart(ChartImage::Banner), [800, 0, 400, 200]),
        ],
        widgets: WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    };
    Skin::new_selection(def, SelectionDefs::default()).unwrap()
}

fn bundle_of(skin: Skin, atlas: skin_model::Atlas) -> Arc<SkinBundle> {
    Arc::new(
        SkinBundle::new(
            Arc::new(skin),
            atlas,
            ClipTable::new(Vec::new()).unwrap(),
            Side::Left,
        )
        .unwrap(),
    )
}

/// A selection bundle over `dir` drawing the stage file and the banner,
/// its atlas keeping no chart layer: its images live in the pool. At
/// 1 920×1 080 its cells are 448×640 and 448×256, eight slots to a layer
/// of 2 048, four layers (32 slots) within the pool's texels.
pub(crate) fn pool_bundle(r: &Renderer, dir: &Path) -> Arc<SkinBundle> {
    let skin = chart_skin(dir);
    let atlas = skin
        .sheet()
        .atlas_keeping(Variant::Lo, r.atlas_limits(), KeptLayers::NONE, &|| false)
        .unwrap();
    assert!(skin.has_chart_images());
    let b = bundle_of(skin, atlas);
    assert_eq!(b.chart_layer(), None);
    b
}

fn commit(r: &mut Renderer, kind: ScreenKind, b: &Arc<SkinBundle>, request: u64) {
    let token = r.install(kind, b, request).unwrap();
    r.commit(token).unwrap();
}

/// A `w`×`h` picture of `rgb`, read and fitted into `cell`.
fn picture(cell: Slot, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) -> Fitted {
    let d = tempfile::tempdir().unwrap();
    let file = File::create(d.path().join("p.png")).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    let dir = ChartDir::new(d.path()).unwrap();
    read(&dir, "p.png", &[], cell, &|| false, &|_| None)
        .image
        .unwrap()
}

/// The `[w, h]` texels at `[x, y]` of level `k` of pool layer `z`, read
/// back: RGBA8 rows tightly packed.
fn read_pool(r: &Renderer, z: u32, k: u32, [x, y]: [u32; 2], [w, h]: [u32; 2]) -> Vec<u8> {
    let device = r.gpu.device();
    let row = w * 4;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pool"),
        size: u64::from(padded * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: r.pool.as_ref().unwrap().texture(),
            mip_level: k,
            origin: wgpu::Origin3d { x, y, z },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    r.gpu.queue().submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |m| m.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range().unwrap();
    let mut out = Vec::new();
    for y in 0..h as usize {
        let start = y * padded as usize;
        out.extend_from_slice(&data[start..start + row as usize]);
    }
    out
}

/// A selection whose atlas keeps a chart layer, which no selection skin's
/// does (its images live in the pool), has no pool and is charged its
/// atlas alone.
#[test]
fn a_selection_keeping_a_chart_layer_has_no_pool() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let skin = chart_skin(dir.path());
    assert!(!skin.kept_layers().chart);
    let keeping = KeptLayers {
        chart: true,
        ..KeptLayers::NONE
    };
    let atlas = skin
        .sheet()
        .atlas_keeping(Variant::Lo, r.atlas_limits(), keeping, &|| false)
        .unwrap();
    let b = bundle_of(skin, atlas);
    assert!(b.chart_layer().is_some());
    commit(&mut r, ScreenKind::Selection, &b, 1);
    assert_eq!(r.pool(), None);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (0, 0));
    assert_eq!(a.charge(ScreenKind::Selection), b.atlas_texels());
}

/// A write puts each image's levels at its cell's origin `>> k` in the
/// slot's layer, and the slot's table gives their uvs, the layer marked
/// as the pool's, keyed to the pool; other slots and other generations
/// give no table.
#[test]
fn a_pool_write_lands_in_its_cells_at_every_level() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let b = pool_bundle(&r, dir.path());
    assert_eq!(r.pool_chart_key(), None);
    commit(&mut r, ScreenKind::Selection, &b, 1);
    let l = r.pool().unwrap();
    let (stage_cell, banner_cell) = (l.cells.stage.unwrap(), l.cells.banner.unwrap());
    assert_eq!(
        (stage_cell.size(), banner_cell.size()),
        ([448, 640], [448, 256])
    );
    assert_eq!((l.side, l.layers, l.slots), (2_048, 4, 32));
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 3 * LAYER));
    assert_eq!(a.charge(ScreenKind::Selection), b.atlas_texels() + LAYER);

    let stage = picture(stage_cell, [300, 500], [200, 10, 10]);
    let banner = picture(banner_cell, [400, 100], [10, 200, 10]);
    // Slot 9: the second layer's second slot.
    r.write_pool(l.generation, 9, Some(&stage), Some(&banner))
        .unwrap();
    let t = r.pool_table(l.generation, 9).unwrap();
    assert_eq!(t.key(), r.pool_chart_key());
    assert!(t.key().is_some());
    let (stage_at, banner_at) = ([448, 0], [448, 640]);
    let uv = |f, at, cell| ChartUv::at(f, at, cell, 2_048, 1 | POOL_LAYER);
    assert_eq!(t.get(ChartImage::Stage), uv(&stage, stage_at, stage_cell));
    assert_eq!(
        t.get(ChartImage::Banner),
        uv(&banner, banner_at, banner_cell)
    );
    assert!(t.get(ChartImage::Stage).is_some() && t.get(ChartImage::Banner).is_some());
    assert_eq!(t.get(ChartImage::Back), None);
    for (f, at) in [(&stage, stage_at), (&banner, banner_at)] {
        for k in 0..4 {
            let size = f.level_size(k).unwrap();
            let got = read_pool(&r, 1, k as u32, at.map(|o| o >> k), size);
            assert!(got == *f.levels()[k], "level {k} at {at:?}");
        }
    }
    assert_eq!(r.pool_table(l.generation, 8), None, "not written");
    assert_eq!(r.pool_table(0, 9), None, "no generation");
    assert_eq!(r.pool_table(l.generation + 1, 9), None);
}

/// A fit that does not lie in its cell, or a write the device refuses, is
/// refused and leaves the slot without a table; a generation that is not
/// live, or a slot it does not have, is stale and writes nothing.
#[test]
fn a_refused_pool_write_clears_its_slot() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 1);
    let l = r.pool().unwrap();
    let (stage_cell, banner_cell) = (l.cells.stage.unwrap(), l.cells.banner.unwrap());
    let stage = picture(stage_cell, [300, 500], [200, 10, 10]);
    let banner = picture(banner_cell, [400, 100], [10, 200, 10]);
    r.write_pool(l.generation, 0, Some(&stage), Some(&banner))
        .unwrap();
    assert!(r.pool_table(l.generation, 0).is_some());

    // A stage file's fit, taller than the banner's cell, as the banner.
    assert_eq!(
        r.write_pool(l.generation, 0, Some(&stage), Some(&stage)),
        Err(PoolWriteError::Refused)
    );
    assert_eq!(r.pool_table(l.generation, 0), None, "cleared");
    r.write_pool(l.generation, 0, Some(&stage), None).unwrap();
    let t = r.pool_table(l.generation, 0).unwrap();
    assert!(t.get(ChartImage::Stage).is_some() && t.get(ChartImage::Banner).is_none());

    r.pool_write_fault = Some(pool_fault);
    assert_eq!(
        r.write_pool(l.generation, 0, Some(&stage), Some(&banner)),
        Err(PoolWriteError::Refused)
    );
    assert_eq!(r.pool_table(l.generation, 0), None, "cleared");

    assert_eq!(
        r.write_pool(l.generation + 1, 1, Some(&stage), None),
        Err(PoolWriteError::Stale)
    );
    assert_eq!(
        r.write_pool(l.generation, l.slots, Some(&stage), None),
        Err(PoolWriteError::Stale)
    );
    assert_eq!(r.pool_table(l.generation, 1), None);
}

/// The pool given up to another kind's candidate is retired at once, its
/// smaller array owed; made once the retired one is destroyed, it keeps
/// its firm layer when the device refuses the smaller array, and none
/// when it refuses that too: its charges go with it, and the candidate is
/// admitted. The next regrowth makes it again.
#[test]
fn a_reduced_pool_keeps_its_firm_layer_or_none() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([255, 0, 0, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Play, &p, 1);
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 2);
    let tiny = p.atlas_texels();
    let sel = r.admission().charge(ScreenKind::Selection);
    // A decision candidate as large as play's skin fits with one optional
    // layer.
    r.state_charge(&p, SHARED_TEXELS - sel - tiny - LAYER);
    let d = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([0, 0, 255, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Decision, &d, 3);
    assert_eq!(r.pool(), None);
    assert!(r.pool_owed());
    crate::test_common::settle(&mut r);
    // The two-layer array is refused, then the firm layer alone: no pool.
    r.pool_faults.set(2);
    assert!(r.make_owed_pool());
    assert_eq!(r.pool(), None);
    assert!(!r.pool_owed());
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (0, 0));
    assert_eq!(a.charge(ScreenKind::Selection), sel - LAYER);
    // Regrown with what the kinds leave: the firm layer and one more.
    let l = r.regrow_pool().unwrap();
    assert_eq!(l.layers, 2);
    assert_eq!(r.pool(), Some(l));
    assert_eq!(r.admission().total(), SHARED_TEXELS);

    // Regrown whole, then a result candidate leaving one optional layer:
    // the two-layer array refused, the pool keeps its firm layer.
    r.state_charge(&p, tiny);
    let whole = r.regrow_pool().unwrap();
    assert_eq!(whole.layers, 4);
    r.state_charge(&p, SHARED_TEXELS - sel - 2 * tiny - LAYER);
    let result = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([0, 255, 0, 255]),
        [16, 16],
    );
    // The regrowth's retired array, still undestroyed, is destroyed in the
    // staging's call before the whole one is retired.
    assert_eq!(r.admission().retired_pool, 2 * LAYER);
    commit(&mut r, ScreenKind::Result, &result, 4);
    assert_eq!(r.admission().retired_pool, 4 * LAYER);
    crate::test_common::settle(&mut r);
    r.pool_faults.set(1);
    assert!(r.make_owed_pool());
    let firm = r.pool().unwrap();
    assert_eq!(firm.layers, 1);
    assert!(firm.generation > whole.generation);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 0));
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
}

/// Every pool array takes a new generation, from a counter of the
/// process: the install, the owed array after a reduction for another
/// kind's candidate, a regrowth, a restore and a renderer built again;
/// removing the selection ends it, and a write of an ended generation is
/// stale.
#[test]
fn each_pool_array_takes_a_new_generation_and_removal_ends_it() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([255, 0, 0, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Play, &p, 1);
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 2);
    let g1 = r.pool().unwrap().generation;
    assert_ne!(g1, 0);
    let tiny = p.atlas_texels();
    let sel = r.admission().charge(ScreenKind::Selection);
    r.state_charge(&p, SHARED_TEXELS - sel - tiny - LAYER);
    let d = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([0, 0, 255, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Decision, &d, 3);
    assert_eq!(r.pool(), None);
    crate::test_common::settle(&mut r);
    assert!(r.make_owed_pool());
    let g2 = r.pool().unwrap();
    assert_eq!(g2.layers, 2);
    assert!(g2.generation > g1);
    // Play's skin charged less: the pool regrows.
    r.state_charge(&p, tiny);
    let g3 = r.regrow_pool().unwrap();
    assert_eq!(g3.layers, 4);
    assert!(g3.generation > g2.generation);
    assert_eq!(r.regrow_pool(), Some(g3), "nothing more to grow");
    let stage = picture(g3.cells.stage.unwrap(), [30, 50], [200, 10, 10]);
    r.write_pool(g3.generation, 0, Some(&stage), None).unwrap();

    let installation = r.installation(ScreenKind::Selection).unwrap();
    r.restore(ScreenKind::Selection, &installation).unwrap();
    let g4 = r.pool().unwrap();
    assert_eq!(g4.layers, 1, "the firm layer until it regrows");
    assert!(g4.generation > g3.generation);
    assert_eq!(r.pool_table(g3.generation, 0), None);
    assert_eq!(r.pool_table(g4.generation, 0), None, "a new array is empty");
    assert_eq!(
        r.write_pool(g3.generation, 0, Some(&stage), None),
        Err(PoolWriteError::Stale)
    );

    r.remove(ScreenKind::Selection);
    assert_eq!(r.pool(), None);
    assert_eq!(r.pool_chart_key(), None);
    assert_eq!(
        r.write_pool(g4.generation, 0, Some(&stage), None),
        Err(PoolWriteError::Stale)
    );
    let a = r.admission();
    assert_eq!((a.firm, a.pool, a.charge(ScreenKind::Selection)), (0, 0, 0));
    assert_eq!(r.regrow_pool(), None, "no selection");

    let mut again = crate::test_common::offscreen(1_920, 1_080);
    again.restore(ScreenKind::Selection, &installation).unwrap();
    assert!(again.pool().unwrap().generation > g4.generation);
}

/// The "chart pool" textures the device's allocator holds now, by their
/// label (the placeholder's is another).
fn pool_allocations(r: &Renderer) -> usize {
    let report = r
        .gpu
        .device()
        .generate_allocator_report()
        .expect("the device's allocator report");
    report
        .allocations
        .iter()
        .filter(|a| a.name == "chart pool")
        .count()
}

/// Play's tiny skin and the pool's selection committed, play's charged so
/// that a tiny decision candidate fits with one optional layer: (play's
/// bundle, the selection's, a tiny candidate's charge).
fn pooled(r: &mut Renderer, dir: &Path) -> (Arc<SkinBundle>, Arc<SkinBundle>, u64) {
    let p = solid(r, [255, 0, 0, 255]);
    commit(r, ScreenKind::Play, &p, 1);
    let b = pool_bundle(r, dir);
    commit(r, ScreenKind::Selection, &b, 2);
    let tiny = p.atlas_texels();
    let sel = r.admission().charge(ScreenKind::Selection);
    r.state_charge(&p, SHARED_TEXELS - sel - tiny - LAYER);
    (p, b, tiny)
}

/// A tiny solid bundle of `rgba` over the left half.
fn solid(r: &Renderer, rgba: [u8; 4]) -> Arc<SkinBundle> {
    crate::test_decl::solid_bundle(r, &crate::test_decl::Look::left(rgba), [16, 16])
}

/// The destructions recorded since `from` of them were.
fn recorded_since(r: &Renderer, from: usize) -> Vec<DestroyRecord> {
    let all: Vec<_> = r.destructions().copied().collect();
    let n = r.destructions_recorded() - from;
    all[all.len() - n..].to_vec()
}

/// Polls the completion and destroys what is due, one heavy part a round,
/// until the list holds nothing: the destructions recorded meanwhile.
fn poll_until_destroyed(r: &mut Renderer) -> Vec<DestroyRecord> {
    let from = r.destructions_recorded();
    let began = Instant::now();
    while r.filled() > 0 {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "still filled after 10 s"
        );
        r.poll_completion().unwrap();
        let _ = r.destroy_due(true);
        std::thread::yield_now();
    }
    recorded_since(r, from)
}

/// Every destruction in `records` came once a completion covering its
/// last use was seen, on this thread.
#[track_caller]
fn each_after_its_completion(records: &[DestroyRecord]) {
    assert!(!records.is_empty(), "nothing destroyed");
    for d in records {
        assert!(d.last_use <= d.seen, "{d:?}");
        assert_eq!(d.thread, std::thread::current().id());
    }
}

/// A selection frame of `b` built at 1 920×1 080 and submitted: presented
/// unless the target is minimized.
fn selection_frame(r: &mut Renderer, b: &SkinBundle) -> bool {
    let mut sel = b.skin().prepare_selection();
    let layout = Layout::AUTHOR;
    let f = SelectionFrame {
        now: TimeUs(0),
        target: [1_920, 1_080],
        layout: &layout,
        timers: SelectionTimers::default(),
        app: AppReadouts::LAUNCH,
        pointer: None,
    };
    r.build_selection(&mut sel, &f, &[]).unwrap();
    r.submit_selection(&[]).unwrap().presented()
}

/// V8a with a queued last use `S + 1` while the newest seen covers `S`:
/// the pool's live array, retired by a reduction, is not destroyed at a
/// boundary before a completion covering its last use is seen; the
/// rolling watch submits the queued write, sees it, and the array is
/// destroyed then. The device's allocator lists the retired array until
/// its destruction and not after.
fn the_retired_pool_waits(r: &mut Renderer, tiny: u64, s: u64) {
    assert_eq!((r.gpu.seen(), r.gpu.latest()), (s, s));
    assert!(r.gpu.write_queued());
    assert_eq!(pool_allocations(r), 1, "the live pool");
    r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap();
    assert_eq!(r.pool(), None, "retired at the reduction");
    assert!(r.pool_owed());
    assert_eq!(r.filled(), 1);
    assert_eq!(pool_allocations(r), 1, "the retired pool dropped");
    assert_eq!(
        r.destroy_due(true),
        Destroyed::default(),
        "destroyed before its completion was seen"
    );
    assert!(r.pending_completion());
    let records = poll_until_destroyed(r);
    let [d] = records[..] else {
        panic!("{records:?}");
    };
    assert_eq!(
        (d.kind, d.class, d.part, d.in_call),
        (ScreenKind::Selection, Class::RetiredPool, Part::Pool, false)
    );
    assert_eq!(d.last_use, s + 1);
    assert!(d.seen > s, "no completion covering S + 1 seen: {d:?}");
    assert_eq!(pool_allocations(r), 0, "the retired pool kept");
}

// Revert: `Filled::due` true whatever the last use -> `destroy_due`
// before any poll destroys the pool, its write still queued.
#[test]
fn a_retired_pool_waits_for_its_queued_write() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let (_, b, tiny) = pooled(&mut r, dir.path());
    let l = r.pool().unwrap();
    // The newest seen covers S, the pool's creation included.
    let (s, _) = r.gpu.submit([]);
    super::wait_for_gpu(&r.gpu).unwrap();
    assert!(!r.gpu.write_queued());
    // A write queued, which S + 1 will carry.
    let stage = picture(l.cells.stage.unwrap(), [30, 50], [200, 10, 10]);
    r.write_pool(l.generation, 0, Some(&stage), None).unwrap();
    // Minimized: the selection frame returns before its submission.
    r.resize(0, 0);
    assert!(!selection_frame(&mut r, &b));
    the_retired_pool_waits(&mut r, tiny, s);
}

// Revert: `ChartPool::create` recording the latest submission as its last
// use, not the next -> the pool is destroyed at the first boundary, its
// creation's writes still queued.
#[test]
fn a_retired_pool_waits_for_its_creation() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let (_, _, tiny) = pooled(&mut r, dir.path());
    // No frame since: the creation's writes are still queued.
    let s = r.gpu.latest();
    the_retired_pool_waits(&mut r, tiny, s);
}

// Revert: `reduce_pool` making the smaller array at once -> `pool()` is a
// two-layer array after the reduction and a pool creation is counted.
#[test]
fn a_reduction_retires_the_pool_once_and_owes_the_smaller_array() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let (_, _, tiny) = pooled(&mut r, dir.path());
    let creations = r.pool_creations.get();
    r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap();
    assert_eq!(r.pool(), None);
    assert_eq!(r.pool_creations.get(), creations, "a pool array made");
    let a = r.admission();
    assert_eq!((a.firm, a.pool, a.retired_pool), (LAYER, LAYER, 4 * LAYER));
    // A second reduction while the array is owed lowers its layers only.
    r.admit_reclaiming(ScreenKind::Decision, tiny + LAYER)
        .unwrap();
    assert_eq!(r.filled(), 1, "retired again");
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 0));
    assert_eq!(r.pool_creations.get(), creations);
    // Owed until the retired one is destroyed.
    assert!(!r.make_owed_pool());
    assert_eq!(pool_allocations(&r), 1);
    crate::test_common::settle(&mut r);
    assert_eq!(pool_allocations(&r), 0);
    assert!(r.make_owed_pool());
    assert_eq!(r.pool().unwrap().layers, 1);
    assert_eq!(r.pool_creations.get(), creations + 1);
    assert!(!r.pool_owed());
}

// Revert: `reduce_pool` retiring the live array beside an undestroyed
// retired one -> no refusal where destruction is not allowed, and three
// pool arrays allocated at once.
#[test]
fn at_most_one_pool_array_waits_beside_the_live_one() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let (p, _, tiny) = pooled(&mut r, dir.path());
    // Reduced to two layers, then regrown whole: the two-layer array
    // retired beside the live one.
    r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap();
    crate::test_common::settle(&mut r);
    assert!(r.make_owed_pool());
    r.state_charge(&p, tiny);
    assert_eq!(r.regrow_pool().unwrap().layers, 4);
    assert_eq!(pool_allocations(&r), 2);
    assert_eq!(r.admission().retired_pool, 2 * LAYER);
    let sel = r.admission().charge(ScreenKind::Selection);
    r.state_charge(&p, SHARED_TEXELS - sel - tiny - LAYER);
    // Nothing destroyed where destruction is not allowed: refused.
    r.set_destroy_allowed(false);
    let e = r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap_err();
    assert!(
        matches!(
            e,
            RenderError::ListFull {
                kind: ScreenKind::Decision
            }
        ),
        "{e:?}"
    );
    assert!(e.is_refusal());
    assert_eq!(r.pool().unwrap().layers, 4, "the live pool kept");
    // Allowed: the retired one destroyed in the call first.
    r.set_destroy_allowed(true);
    let from = r.destructions_recorded();
    r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap();
    let records = recorded_since(&r, from);
    each_after_its_completion(&records);
    let got: Vec<_> = records
        .iter()
        .map(|d| (d.class, d.part, d.in_call))
        .collect();
    assert_eq!(got, [(Class::RetiredPool, Part::Pool, true)]);
    assert_eq!(r.pool(), None);
    assert_eq!(pool_allocations(&r), 1);
    assert_eq!(r.admission().retired_pool, 4 * LAYER);
}

// Revert: `pool_held` counting only a pool a reduction or a regrowth
// retired -> the new selection stages its own pool beside the removed
// one's, two pool arrays allocated with the removed one still filled.
#[test]
fn no_pool_array_is_made_while_an_entry_holds_one() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    // Room for every array: no admission destroys an entry in its call.
    let p = solid(&r, [255, 0, 0, 255]);
    commit(&mut r, ScreenKind::Play, &p, 1);
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 2);
    // The first commit's built-in resources destroyed: the list empty.
    crate::test_common::settle(&mut r);
    assert_eq!(r.filled(), 0);
    assert_eq!(pool_allocations(&r), 1);
    // Removed: its pool waits in the standing entry beside its atlas.
    r.remove(ScreenKind::Selection);
    assert_eq!(r.filled(), 1);
    assert_eq!(pool_allocations(&r), 1);
    // A new selection meanwhile stages with the placeholder and owes its
    // pool; neither the owed array nor a regrowth is made while the
    // removed one's pool is undestroyed.
    let again = pool_bundle(&r, dir.path());
    assert!(!Arc::ptr_eq(&again, &b));
    commit(&mut r, ScreenKind::Selection, &again, 3);
    assert_eq!(r.filled(), 1, "the removed one's entry kept");
    assert_eq!(r.pool(), None);
    assert!(r.pool_owed());
    assert!(!r.make_owed_pool());
    assert_eq!(r.regrow_pool(), None);
    assert_eq!(pool_allocations(&r), 1, "only the removed one's");
    // Destroyed, the owed array follows.
    crate::test_common::settle(&mut r);
    assert_eq!(pool_allocations(&r), 0);
    assert!(r.make_owed_pool());
    assert!(r.pool().is_some());
    assert_eq!(pool_allocations(&r), 1);
}

// Revert: `regrow_pool` leaving the filled entries out of its sum -> the
// pool regrows to four layers beside a removed result's entry, past the
// limit.
#[test]
fn the_regrowth_counts_the_filled_entries() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = solid(&r, [255, 0, 0, 255]);
    commit(&mut r, ScreenKind::Play, &p, 1);
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 2);
    let result = solid(&r, [0, 255, 0, 255]);
    commit(&mut r, ScreenKind::Result, &result, 3);
    // The result charged two layers, play what leaves the pool its firm
    // layer and one more beside them.
    r.state_charge(&result, 2 * LAYER);
    let sel = r.admission().charge(ScreenKind::Selection);
    r.state_charge(&p, SHARED_TEXELS - sel - 3 * LAYER);
    // A candidate as large as a layer: the pool down to its firm layer.
    r.admit_reclaiming(ScreenKind::Decision, LAYER).unwrap();
    crate::test_common::settle(&mut r);
    assert!(r.make_owed_pool());
    assert_eq!(r.pool().unwrap().layers, 1);
    // Removed, the result's entry still charges its two layers.
    r.remove(ScreenKind::Result);
    assert_eq!(r.admission().filled, 2 * LAYER);
    assert_eq!(r.regrow_pool().unwrap().layers, 2, "grown past the entry");
    crate::test_common::settle(&mut r);
    assert_eq!(r.regrow_pool().unwrap().layers, 4);
}

// Revert: `destroy_due` without its `due` filter -> a record whose last
// use is past the completion seen at its destruction, at the first site
// whose array a frame drew.
#[test]
fn every_retirement_site_destroys_only_after_its_last_use() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    // Play's first commit: the built-in resources it replaced.
    let p = solid(&r, [255, 0, 0, 255]);
    commit(&mut r, ScreenKind::Play, &p, 1);
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    assert_eq!(
        (records[0].kind, records[0].class, records[0].id),
        (ScreenKind::Play, Class::Replaced, None)
    );
    // A staging's replaced candidate.
    let d1 = solid(&r, [0, 255, 0, 255]);
    let _ = r.install(ScreenKind::Decision, &d1, 2).unwrap();
    let d2 = solid(&r, [0, 255, 255, 255]);
    let _ = r.install(ScreenKind::Decision, &d2, 3).unwrap();
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    assert_eq!(
        (records[0].kind, records[0].class, records[0].id),
        (ScreenKind::Decision, Class::Other, Some(d1.atlas_id()))
    );
    r.discard();
    poll_until_destroyed(&mut r);
    // A selection with its pool, drawn by a frame, replaced by another:
    // its pool destroyed before its atlas.
    let b = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b, 4);
    assert!(selection_frame(&mut r, &b));
    let b2 = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &b2, 5);
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    let parts: Vec<_> = records.iter().map(|d| (d.class, d.part)).collect();
    assert_eq!(
        parts,
        [
            (Class::Replaced, Part::Pool),
            (Class::Replaced, Part::Array)
        ]
    );
    // A reduction after a frame drew the pool: not due before the poll.
    assert!(selection_frame(&mut r, &b2));
    let tiny = p.atlas_texels();
    let sel = r.admission().charge(ScreenKind::Selection);
    r.state_charge(&p, SHARED_TEXELS - sel - tiny - LAYER);
    r.admit_reclaiming(ScreenKind::Decision, tiny).unwrap();
    assert_eq!(r.destroy_due(true).heavy, None, "drawn and not yet seen");
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    assert_eq!(records[0].class, Class::RetiredPool);
    // A regrowth after a frame drew the owed array.
    assert!(r.make_owed_pool());
    r.state_charge(&p, tiny);
    assert!(selection_frame(&mut r, &b2));
    assert_eq!(r.regrow_pool().unwrap().layers, 4);
    assert_eq!(r.destroy_due(true).heavy, None, "drawn and not yet seen");
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    assert_eq!(records[0].class, Class::RetiredPool);
    // A removal after a frame: the pool, then the atlas.
    assert!(selection_frame(&mut r, &b2));
    r.remove(ScreenKind::Selection);
    assert_eq!(r.destroy_due(true).heavy, None, "drawn and not yet seen");
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    let parts: Vec<_> = records.iter().map(|d| (d.kind, d.part)).collect();
    assert_eq!(
        parts,
        [
            (ScreenKind::Selection, Part::Pool),
            (ScreenKind::Selection, Part::Array)
        ]
    );
    // Play's replaced installation; an error return, an upload the device
    // refused.
    let q = solid(&r, [0, 0, 255, 255]);
    commit(&mut r, ScreenKind::Play, &q, 6);
    r.fail_staged_buffers(true);
    let e = r.install(ScreenKind::Decision, &d1, 7).unwrap_err();
    r.fail_staged_buffers(false);
    assert!(
        matches!(e, RenderError::Atlas(crate::UploadError::Device(_))),
        "{e:?}"
    );
    assert_eq!(r.filled(), 2, "play's replaced and the refused array");
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    let got: Vec<_> = records.iter().map(|d| (d.kind, d.class, d.id)).collect();
    assert_eq!(
        got,
        [
            (ScreenKind::Play, Class::Replaced, Some(p.atlas_id())),
            (ScreenKind::Decision, Class::Other, Some(d1.atlas_id()))
        ]
    );
}

// Revert: R5's release left standing in the result's entry (not destroyed
// in the call) -> the in-call records hold the decision candidate alone,
// and three entries stay filled.
#[test]
fn one_staging_retires_four_arrays() {
    let mut r = crate::test_common::offscreen(1_920, 1_080);
    let dir = tempfile::tempdir().unwrap();
    let p = solid(&r, [255, 0, 0, 255]);
    commit(&mut r, ScreenKind::Play, &p, 1);
    let s = pool_bundle(&r, dir.path());
    commit(&mut r, ScreenKind::Selection, &s, 2);
    let result = solid(&r, [0, 255, 0, 255]);
    commit(&mut r, ScreenKind::Result, &result, 3);
    crate::test_common::settle(&mut r);
    let tiny = p.atlas_texels();
    // A decision candidate staged; the selection charged so that a tiny
    // play candidate fits only with the pool at its firm layer and the
    // result released.
    let d = solid(&r, [0, 0, 255, 255]);
    let _ = r.install(ScreenKind::Decision, &d, 4).unwrap();
    r.state_charge(&s, SHARED_TEXELS - LAYER - tiny);
    let from = r.destructions_recorded();
    // The play candidate's own upload refused by the device.
    r.fail_staged_buffers(true);
    let b = solid(&r, [255, 255, 0, 255]);
    let e = r.install(ScreenKind::Play, &b, 5).unwrap_err();
    r.fail_staged_buffers(false);
    assert!(
        matches!(e, RenderError::Atlas(crate::UploadError::Device(_))),
        "{e:?}"
    );
    // The candidate and the released result destroyed in the call; the
    // pool and the refused array retired.
    let in_call = recorded_since(&r, from);
    each_after_its_completion(&in_call);
    let got: Vec<_> = in_call.iter().map(|x| (x.kind, x.id, x.in_call)).collect();
    assert_eq!(
        got,
        [
            (ScreenKind::Decision, Some(d.atlas_id()), true),
            (ScreenKind::Result, Some(result.atlas_id()), true)
        ]
    );
    assert!(r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_none());
    assert_eq!(r.filled(), 2);
    let records = poll_until_destroyed(&mut r);
    each_after_its_completion(&records);
    let mut got: Vec<_> = records.iter().map(|x| (x.kind, x.class, x.part)).collect();
    got.sort_by_key(|&(k, _, _)| k == ScreenKind::Selection);
    assert_eq!(
        got,
        [
            (ScreenKind::Play, Class::Other, Part::Array),
            (ScreenKind::Selection, Class::RetiredPool, Part::Pool)
        ]
    );
}
