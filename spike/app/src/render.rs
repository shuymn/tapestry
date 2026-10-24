//! wgpu renderer: one instanced sprite pipeline (alpha and additive) over a
//! procedural atlas, and the BGA as three R8 planes converted with BT.709 in
//! the shader (design D2 rendering, D3 open choice 5).

use crate::alloc::{Scope, scope};
use crate::scene::{ATLAS_SIZE, Frame, Instance, LAYOUT_H, LAYOUT_W, MAX_INSTANCES, build_atlas};
use wgpu::util::DeviceExt;

const SPRITE_WGSL: &str = r#"
struct Inst {
    @location(0) rect: vec4f,
    @location(1) uv: vec4f,
    @location(2) color: vec4f,
};
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
};
@group(0) @binding(0) var atlas: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> VOut {
    let c = vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let p = i.rect.xy + c * i.rect.zw;
    var o: VOut;
    o.pos = vec4f(p.x / 960.0 - 1.0, 1.0 - p.y / 540.0, 0.0, 1.0);
    o.uv = mix(i.uv.xy, i.uv.zw, c);
    o.color = i.color;
    return o;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    return textureSample(atlas, samp, v.uv) * v.color;
}
"#;

const BGA_WGSL: &str = r#"
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    let c = vec2f(f32((vi << 1u) & 2u), f32(vi & 2u));
    var o: VOut;
    o.pos = vec4f(c.x * 2.0 - 1.0, 1.0 - c.y * 2.0, 0.0, 1.0);
    o.uv = c;
    return o;
}

// BT.709, limited range.
@fragment
fn fs(v: VOut) -> @location(0) vec4f {
    let y = (textureSample(ty, samp, v.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(tu, samp, v.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let w = (textureSample(tv, samp, v.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3f(y + 1.5748 * w, y - 0.1873 * u - 0.4681 * w, y + 1.8556 * u);
    return vec4f(clamp(rgb, vec3f(0.0), vec3f(1.0)), 1.0);
}
"#;

struct Bga {
    planes: [wgpu::Texture; 3],
    bind: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    width: u32,
    height: u32,
    shown: bool,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    alpha: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    atlas_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    bga: Option<Bga>,
}

fn sprite_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    blend: wgpu::BlendState,
) -> wgpu::RenderPipeline {
    let attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sprites"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &attrs,
            })],
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

impl Renderer {
    /// Creates pipelines, the atlas and (when `bga` is the decoded size) the
    /// BGA planes, all before the countdown.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        bga: Option<(u32, u32)>,
    ) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: Some("atlas"),
                size: wgpu::Extent3d {
                    width: ATLAS_SIZE,
                    height: ATLAS_SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &build_atlas(),
        );
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas"),
            entries: &[tex_entry(0), samp_entry(1)],
        });
        let atlas_view = atlas.create_view(&Default::default());
        let atlas_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let sprite_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprites"),
            source: wgpu::ShaderSource::Wgsl(SPRITE_WGSL.into()),
        });
        let sprite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprites"),
            bind_group_layouts: &[Some(&atlas_layout)],
            immediate_size: 0,
        });
        let alpha = sprite_pipeline(
            &device,
            &sprite_layout,
            &sprite_module,
            format,
            wgpu::BlendState::ALPHA_BLENDING,
        );
        let additive = sprite_pipeline(
            &device,
            &sprite_layout,
            &sprite_module,
            format,
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
        );
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (MAX_INSTANCES * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bga = bga.map(|(width, height)| {
            let plane = |w, h, label| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let planes = [
                plane(width, height, "bga y"),
                plane(width / 2, height / 2, "bga u"),
                plane(width / 2, height / 2, "bga v"),
            ];
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("bga"),
                entries: &[tex_entry(0), tex_entry(1), tex_entry(2), samp_entry(3)],
            });
            let views: Vec<wgpu::TextureView> = planes
                .iter()
                .map(|t| t.create_view(&Default::default()))
                .collect();
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("bga"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&views[0]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&views[1]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&views[2]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("bga"),
                source: wgpu::ShaderSource::Wgsl(BGA_WGSL.into()),
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("bga"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("bga"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(format.into())],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            });
            Bga {
                planes,
                bind,
                pipeline,
                width,
                height,
                shown: false,
            }
        });
        Self {
            device,
            queue,
            alpha,
            additive,
            atlas_bind,
            instances,
            bga,
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Uploads one decoded BGA frame (I420 planes) with `write_texture`.
    pub fn upload_bga(&mut self, y: &[u8], u: &[u8], v: &[u8]) {
        let Some(bga) = &mut self.bga else {
            return;
        };
        let (w, h) = (bga.width, bga.height);
        for (tex, data, pw, ph) in [
            (&bga.planes[0], y, w, h),
            (&bga.planes[1], u, w / 2, h / 2),
            (&bga.planes[2], v, w / 2, h / 2),
        ] {
            scope(Scope::WriteTexture, || {
                self.queue.write_texture(
                    tex.as_image_copy(),
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pw),
                        rows_per_image: Some(ph),
                    },
                    wgpu::Extent3d {
                        width: pw,
                        height: ph,
                        depth_or_array_layers: 1,
                    },
                )
            });
        }
        bga.shown = true;
    }

    /// Records and submits one frame into `target` (letterboxed 16:9).
    pub fn draw(&self, target: &wgpu::TextureView, size: (u32, u32), frame: &Frame) {
        let a = frame.alpha.len();
        let b = frame.additive.len();
        scope(Scope::WriteBuffer, || {
            self.queue
                .write_buffer(&self.instances, 0, bytemuck::cast_slice(&frame.alpha));
            self.queue.write_buffer(
                &self.instances,
                (a * std::mem::size_of::<Instance>()) as u64,
                bytemuck::cast_slice(&frame.additive),
            );
        });
        let mut encoder = scope(Scope::CreateCommandEncoder, || {
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None })
        });
        scope(Scope::OtherLibrary, || {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let (w, h) = (size.0 as f32, size.1 as f32);
            let s = (w / LAYOUT_W).min(h / LAYOUT_H);
            let (vw, vh) = (LAYOUT_W * s, LAYOUT_H * s);
            pass.set_viewport((w - vw) / 2.0, (h - vh) / 2.0, vw, vh, 0.0, 1.0);
            if let Some(bga) = self.bga.as_ref().filter(|b| b.shown) {
                pass.set_pipeline(&bga.pipeline);
                pass.set_bind_group(0, &bga.bind, &[]);
                pass.draw(0..3, 0..1);
            }
            pass.set_bind_group(0, &self.atlas_bind, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            pass.set_pipeline(&self.alpha);
            pass.draw(0..4, 0..a as u32);
            pass.set_pipeline(&self.additive);
            pass.draw(0..4, a as u32..(a + b) as u32);
        });
        let cmd = scope(Scope::OtherLibrary, || encoder.finish());
        scope(Scope::Submit, || {
            self.queue.submit([cmd]);
        });
    }

    /// Copies `texture` (RGBA8, `size`) back and returns its bytes.
    pub fn read_back(&self, texture: &wgpu::Texture, size: (u32, u32)) -> Vec<u8> {
        let row = size.0 * 4;
        let padded = row.div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded * size.1),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(size.1),
                },
            },
            wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let data = slice.get_mapped_range().expect("mapped readback");
        let mut out = Vec::with_capacity((row * size.1) as usize);
        for y in 0..size.1 as usize {
            let start = y * padded as usize;
            out.extend_from_slice(&data[start..start + row as usize]);
        }
        out
    }
}
