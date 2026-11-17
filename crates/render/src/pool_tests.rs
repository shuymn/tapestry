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

use bms_model::TimeUs;
use skin_model::chart_image::{
    ChartDir, ChartImage, ChartStretch, ChartUv, Fitted, POOL_LAYER, Slot, read,
};
use skin_model::{
    Anchor, Ease, ElementDef, End, Filter, Fit, KeptLayers, KeyDef, Place, Region, RegionDef, Root,
    ScreenKind, SelectionDefs, Side, Skin, SkinDef, SolidImageDef, Source, TimerRef, Track,
    Variant, WidgetDefs, selection_timer,
};

use super::{PoolWriteError, Renderer, SHARED_TEXELS, pool_fault};
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

/// The pool given up to another kind's candidate keeps its firm layer
/// when the device refuses the smaller array, and none when it refuses
/// that too: its charges go with it, and the candidate is admitted. The
/// next regrowth makes it again.
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
    // The two-layer array is refused, then the firm layer alone: no pool.
    r.pool_faults.set(2);
    let d = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([0, 0, 255, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Decision, &d, 3);
    assert_eq!(r.pool(), None);
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
    r.pool_faults.set(1);
    let result = crate::test_decl::solid_bundle(
        &r,
        &crate::test_decl::Look::left([0, 255, 0, 255]),
        [16, 16],
    );
    commit(&mut r, ScreenKind::Result, &result, 4);
    let firm = r.pool().unwrap();
    assert_eq!(firm.layers, 1);
    assert!(firm.generation > whole.generation);
    let a = r.admission();
    assert_eq!((a.firm, a.pool), (LAYER, 0));
    assert!(!r.take_result_release());
    assert!(r.installed(ScreenKind::Result).is_some());
}

/// Every pool array takes a new generation, from a counter of the
/// process: the install, a reduction for another kind's candidate, a
/// regrowth, a restore and a renderer built again; removing the selection
/// ends it, and a write of an ended generation is stale.
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
