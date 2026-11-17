//! The sprite shader's chart-image pool binding against the shader before
//! it (docs/stage3.md S10-35; design.md "Rendering and Skins", "Atlas
//! array"): atlas quads draw as they did, and pool quads sample the pool
//! with the level and filter the atlas's pages get.

use skin_model::chart_image::{ChartCells, Slot};

use super::{
    AtlasPages, AtlasTexture, Instance, PROJECTION_BYTES, extent, pipelines, skin_atlas,
    sprite_pipelines,
};
use crate::chart_pool::{ChartPool, Grid};
use crate::skin_atlas::PageLevels;
use crate::sprite::{FLAG_BASE, FLAG_NEAREST, FLAG_POOL};

/// The sprite shader as it was before the pool's binding: one texture
/// array, the layer in the low 16 bits of the flags.
const BEFORE_WGSL: &str = r"
// An `Instance`: its pivot and angle share location 3, and its flags hold
// the atlas layer (low 16 bits) and the nearest and base filter bits.
struct Inst {
    @location(0) rect: vec4f,
    @location(1) uv: vec4f,
    @location(2) color: vec4f,
    @location(3) turn: vec3f,
    @location(4) flags: u32,
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) @interpolate(flat) flags: u32,
};
// Half the canvas's size, in canvas pixels.
struct Proj {
    half: vec2f,
    pad: vec2f,
};
@group(0) @binding(0) var atlas: texture_2d_array<f32>;
@group(0) @binding(1) var smooth_samp: sampler;
@group(0) @binding(2) var nearest_samp: sampler;
@group(0) @binding(3) var<uniform> proj: Proj;

// Canvas pixels (origin top left, y down) to clip space; the viewport places
// the canvas on the target. Each corner turns clockwise on screen by the
// angle about the pivot; an unturned sprite keeps its corners exactly.
@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> VOut {
    let c = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let placed = i.rect.xy + c * i.rect.zw;
    let d = placed - i.turn.xy;
    let s = sin(i.turn.z);
    let k = cos(i.turn.z);
    let turned = i.turn.xy + vec2f(d.x * k - d.y * s, d.x * s + d.y * k);
    let p = select(turned, placed, i.turn.z == 0.0);
    var o: VOut;
    o.pos = vec4f(p.x / proj.half.x - 1.0, 1.0 - p.y / proj.half.y, 0.0, 1.0);
    o.uv = mix(i.uv.xy, i.uv.zw, c);
    o.color = i.color;
    o.flags = i.flags;
    return o;
}

// The atlas holds premultiplied texels and the colour arrives premultiplied,
// so their product is premultiplied. A skin's page is sampled bilinearly
// between its mip levels, or at the nearest texel of the nearest level with
// the nearest bit, or at the nearest texel of level 0 with the base bit;
// layer 0, the built-in atlas of 128 texels at its top left, is read at its
// nearest level-0 texel by integer texel: its uv, the built-in atlas's own,
// times 128, exact whatever the array's side. The two implicit-derivative
// samples are always taken, so they stay in uniform control flow; the
// base read has an explicit level, which needs no derivatives, so only a
// sprite with the base bit takes it. One result is chosen.
fn tinted(v: VOut) -> vec4f {
    let layer = i32(v.flags & 0xFFFFu);
    let smoothed = textureSample(atlas, smooth_samp, v.uv, layer);
    let nearest = textureSample(atlas, nearest_samp, v.uv, layer);
    let texel = clamp(vec2i(floor(v.uv * 128.0)), vec2i(0), vec2i(127));
    let builtin = textureLoad(atlas, texel, 0, 0);
    var page = select(smoothed, nearest, (v.flags & 0x10000u) != 0u);
    if (v.flags & 0x20000u) != 0u {
        page = textureSampleLevel(atlas, nearest_samp, v.uv, layer, 0.0);
    }
    return select(page, builtin, layer == 0) * v.color;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    return tinted(v);
}

// The multiply blend takes the straight colour, texel times tint: the
// premultiplied product divided by its alpha, black where that is 0 (a
// transparent texel's colour is gone; the batch holds no quad of tint
// alpha 0).
@fragment
fn fs_multiply(v: VOut) -> @location(0) vec4f {
    let c = tinted(v);
    return vec4f(select(vec3f(0.0), c.rgb / c.a, c.a > 0.0), c.a);
}
";

/// The target the quads are drawn on: a row per blend, a column per
/// filter and source.
const W: u32 = 576;
const H: u32 = 144;
/// Each quad's side, and the step between quads.
const QUAD: f32 = 40.0;
const STEP: f32 = 48.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// The atlas's side and the pool's: unequal.
const ATLAS_SIDE: u32 = 256;
const POOL_SIDE: u32 = 512;
/// What the target is cleared to, so that each blend changes it.
const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.25,
    g: 0.5,
    b: 0.75,
    a: 1.0,
};
const MODES: [u32; 3] = [0, FLAG_NEAREST, FLAG_BASE];

/// Level `k` of image `z`, `side >> k` texels square: stripes 4 texels
/// wide of a colour of its own (each level and image another) and that
/// colour at 60%, opaque. A wrong level or filter changes the pixels.
fn level(side: u32, z: u32, k: u32) -> Vec<u8> {
    let s = side >> k;
    let c = [30 + 60 * k, 200 - 40 * k - 30 * z, 60 + 50 * z + 10 * k];
    let mut out = Vec::new();
    for _ in 0..s {
        for x in 0..s {
            let dim = (x / 4) % 2 == 1;
            for v in c {
                let v = if dim { v * 6 / 10 } else { v };
                out.push(u8::try_from(v).unwrap());
            }
            out.push(255);
        }
    }
    out
}

/// Every level of image `z` at `side`.
fn image(side: u32, z: u32) -> Vec<Vec<u8>> {
    (0..4).map(|k| level(side, z, k)).collect()
}

/// An atlas array of `images` at `side` after the built-in layer.
fn atlas_of(gpu: &crate::Gpu, side: u32, images: &[Vec<Vec<u8>>]) -> AtlasTexture {
    let pages: Vec<PageLevels<'_>> = images
        .iter()
        .map(|i| [&i[0][..], &i[1][..], &i[2][..], &i[3][..]])
        .collect();
    let pages = AtlasPages {
        side,
        pages: &pages,
        text_layer: false,
        chart_layer: false,
    };
    let layers = skin_atlas::check_pages(&pages, &gpu.device().limits()).unwrap();
    AtlasTexture::upload(gpu, &pages, layers)
}

/// A quad at column `col` of row `row` sampling `uv` of `layer` with
/// `flags`, tinted.
fn quad(row: usize, col: usize, uv: [f32; 4], layer: u32, flags: u32) -> (usize, Instance) {
    let a = 0.85;
    let [x, y] = [col as f32 * STEP + 4.0, row as f32 * STEP + 4.0];
    (
        row,
        Instance {
            rect: [x, y, QUAD, QUAD],
            uv,
            color: [0.9 * a, 0.8 * a, 0.7 * a, a],
            pivot: [0.0, 0.0],
            angle: 0.0,
            flags: layer | flags,
        },
    )
}

/// The quads of `sources` (each a uv, a layer and the pool bit or not),
/// each under every filter, in a row per blend: alpha, additive, multiply.
fn quads(sources: &[([f32; 4], u32, u32)]) -> Vec<(usize, Instance)> {
    let mut out = Vec::new();
    for row in 0..3 {
        for (s, &(uv, layer, pool)) in sources.iter().enumerate() {
            for (m, mode) in MODES.into_iter().enumerate() {
                out.push(quad(row, s * 3 + m, uv, layer, mode | pool));
            }
        }
    }
    out
}

/// `quads` drawn with `pipes` (by blend) and `bind` over a cleared
/// target, read back: RGBA8 rows tightly packed.
fn draw(
    gpu: &crate::Gpu,
    pipes: [&wgpu::RenderPipeline; 3],
    bind: &wgpu::BindGroup,
    quads: &[(usize, Instance)],
) -> Vec<u8> {
    let device = gpu.device();
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pool shader"),
        size: extent(W, H),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let instances: Vec<Instance> = quads.iter().map(|&(_, i)| i).collect();
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pool shader"),
        size: (instances.len() * size_of::<Instance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    gpu.queue()
        .write_buffer(&buffer, 0, bytemuck::cast_slice(&instances));
    let row = W * 4;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let out = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pool shader"),
        size: u64::from(padded * H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(CLEAR),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..wgpu::RenderPassDescriptor::default()
        });
        pass.set_bind_group(0, bind, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        for (i, &(blend, _)) in (0..).zip(quads) {
            pass.set_pipeline(pipes[blend]);
            pass.draw(0..4, i..i + 1);
        }
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &out,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        extent(W, H),
    );
    gpu.queue().submit([encoder.finish()]);
    let slice = out.slice(..);
    slice.map_async(wgpu::MapMode::Read, |m| m.unwrap());
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap();
    let data = slice.get_mapped_range().unwrap();
    let mut pixels = Vec::new();
    for y in 0..H as usize {
        let start = y * padded as usize;
        pixels.extend_from_slice(&data[start..start + row as usize]);
    }
    pixels
}

/// The RGBA at the centre of the quad at column `col` of row `row`.
fn centre(pixels: &[u8], row: usize, col: usize) -> [u8; 4] {
    let x = (col as f32 * STEP + 4.0 + QUAD / 2.0) as usize;
    let y = (row as f32 * STEP + 4.0 + QUAD / 2.0) as usize;
    let i = (y * W as usize + x) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// The largest difference of one channel between `a` and `b`.
fn largest_difference(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// The sprite shader with the chart-image pool's binding (design.md
/// "Rendering and Skins", "Atlas array"), on WARP against the shader
/// before it, with patterned data whose mip levels differ, an atlas of
/// 256 and a pool of 512 texels, each quad shrunk about threefold, under
/// each of the linear, nearest and base filters and each of the alpha,
/// additive and multiply blends:
///
/// - atlas quads (layer 0's built-in read and a skin page) draw exactly
///   what they drew before;
/// - pool quads (pool layer 0, ordinary image data and not the built-in
///   read, and pool layer 1) draw what the shader before drew for the
///   same images as atlas pages: the same level and filter;
/// - the same quads without the pool bit sample the atlas instead, so the
///   bit decides, and the three filters and the two pool layers differ.
#[test]
fn the_pool_samples_as_the_atlas_does_and_the_atlas_is_unchanged() {
    let r = crate::test_common::offscreen(64, 64);
    let gpu = r.gpu();
    let device = gpu.device();
    let (alpha, additive, multiply, bindings) = pipelines(gpu, FORMAT);
    let half = [W as f32 / 2.0, H as f32 / 2.0, 0.0, 0.0];
    gpu.queue()
        .write_buffer(&bindings.projection, 0, bytemuck::cast_slice(&half));

    // The atlas: the built-in layer and one page of image 0.
    let atlas = atlas_of(gpu, ATLAS_SIDE, &[image(ATLAS_SIDE, 0)]);
    // The pool: images 1 and 2 in its layers 0 and 1, written whole.
    let pool_images = [image(POOL_SIDE, 1), image(POOL_SIDE, 2)];
    let cells = ChartCells {
        stage: Slot::new([256, 256]),
        banner: None,
    };
    let grid = Grid::of(cells, POOL_SIDE, &device.limits()).unwrap();
    let pool = ChartPool::create(gpu, grid, 2);
    for (z, levels) in (0..).zip(&pool_images) {
        for (k, bytes) in (0..).zip(levels) {
            let s = POOL_SIDE >> k;
            skin_atlas::write_region(gpu.queue(), pool.texture(), (z, k), [0, 0], [s, s], bytes);
        }
    }
    // The same images as pages of an atlas of the pool's side, layers 1
    // and 2, for the shader before to draw.
    let reference = atlas_of(gpu, POOL_SIDE, &pool_images);
    gpu.queue().submit([]);

    // The shader before, over the bindings it had.
    let old_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("before"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(PROJECTION_BYTES),
                },
                count: None,
            },
        ],
    });
    let old_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("before"),
        source: wgpu::ShaderSource::Wgsl(BEFORE_WGSL.into()),
    });
    let (old_alpha, old_additive, old_multiply) =
        sprite_pipelines(device, &old_layout, &old_module, FORMAT);
    let old_bind = |a: &AtlasTexture| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("before"),
            layout: &old_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(a.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&bindings.smooth),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&bindings.nearest),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: bindings.projection.as_entire_binding(),
                },
            ],
        })
    };
    let now = [&alpha, &additive, &multiply];
    let before = [&old_alpha, &old_additive, &old_multiply];
    let bind = bindings.bind(device, &atlas, Some(pool.view()));

    // Atlas quads: the built-in read and the page, as before.
    let builtin = [0.0, 0.0, 0.3, 0.3];
    let page = [0.1, 0.1, 0.6, 0.6];
    let atlas_quads = quads(&[(builtin, 0, 0), (page, 1, 0)]);
    let drawn = draw(gpu, now, &bind, &atlas_quads);
    assert!(
        drawn == draw(gpu, before, &old_bind(&atlas), &atlas_quads),
        "atlas quads draw as before"
    );

    // Pool quads: 128 texels of the pool across each, as the page's are.
    let cell = [0.1, 0.1, 0.35, 0.35];
    let pooled = draw(
        gpu,
        now,
        &bind,
        &quads(&[(cell, 0, FLAG_POOL), (cell, 1, FLAG_POOL)]),
    );
    let expected = draw(
        gpu,
        before,
        &old_bind(&reference),
        &quads(&[(cell, 1, 0), (cell, 2, 0)]),
    );
    let difference = largest_difference(&pooled, &expected);
    println!("largest difference from the pages drawn before: {difference}");
    assert!(difference <= 1, "pool quads differ by {difference}");

    // Without the pool bit the same quads read the atlas: layer 0's
    // built-in read and the page.
    let unflagged = draw(gpu, now, &bind, &quads(&[(cell, 0, 0), (cell, 1, 0)]));
    assert!(largest_difference(&pooled, &unflagged) > 32);
    for row in 0..3 {
        // Each filter and each layer its own pixels.
        let at = |col| centre(&pooled, row, col);
        for (a, b) in [(0, 1), (1, 2), (0, 2), (0, 3), (1, 4), (2, 5)] {
            assert_ne!(at(a), at(b), "row {row}, columns {a} and {b}");
        }
    }
}

/// The expressions of `block` (and of the blocks inside it) that take
/// implicit derivatives, a sample at an implicit level or a derivative,
/// inside a branch: `branch` says whether `block` is one.
fn implicit_in_branch(
    f: &wgpu::naga::Function,
    block: &wgpu::naga::Block,
    branch: bool,
    found: &mut Vec<String>,
) {
    use wgpu::naga::{Expression, SampleLevel, Statement};
    for statement in block.iter() {
        match statement {
            Statement::Emit(range) if branch => {
                for h in range.clone() {
                    match &f.expressions[h] {
                        Expression::ImageSample {
                            level: SampleLevel::Auto | SampleLevel::Bias(_),
                            ..
                        }
                        | Expression::Derivative { .. } => found.push(format!("{h:?}")),
                        _ => {}
                    }
                }
            }
            Statement::Block(b) => implicit_in_branch(f, b, branch, found),
            Statement::If { accept, reject, .. } => {
                implicit_in_branch(f, accept, true, found);
                implicit_in_branch(f, reject, true, found);
            }
            Statement::Loop {
                body, continuing, ..
            } => {
                implicit_in_branch(f, body, true, found);
                implicit_in_branch(f, continuing, true, found);
            }
            Statement::Switch { cases, .. } => {
                for c in cases {
                    implicit_in_branch(f, &c.body, true, found);
                }
            }
            _ => {}
        }
    }
}

/// Every implicit derivative the sprite shader takes, the atlas's two
/// samples and the pool's gradients, is taken in uniform control flow:
/// the pool's samples in their branch take explicit gradients or levels
/// (design.md "Rendering and Skins", "Atlas array"). A device may draw a
/// sample with implicit derivatives inside the branch as it should, so
/// the drawing alone does not show it.
#[test]
fn no_implicit_derivative_is_taken_in_a_branch() {
    let module = wgpu::naga::front::wgsl::parse_str(super::SPRITE_WGSL).unwrap();
    let (_, tinted) = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_deref() == Some("tinted"))
        .unwrap();
    let mut found = Vec::new();
    implicit_in_branch(tinted, &tinted.body, false, &mut found);
    assert!(found.is_empty(), "{found:?}");
    // The same walk finds one there.
    let moved = super::SPRITE_WGSL.replace(
        "textureSampleGrad(pool, smooth_samp, v.uv, layer, gx, gy)",
        "textureSample(pool, smooth_samp, v.uv, layer)",
    );
    assert_ne!(moved, super::SPRITE_WGSL);
    let module = wgpu::naga::front::wgsl::parse_str(&moved).unwrap();
    let (_, tinted) = module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_deref() == Some("tinted"))
        .unwrap();
    implicit_in_branch(tinted, &tinted.body, false, &mut found);
    assert_eq!(found.len(), 1, "{found:?}");
}
