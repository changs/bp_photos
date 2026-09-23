//! GPU pipeline: source upload with mips, per-preset resources, rendering and readback.

use wgpu::util::DeviceExt;

use crate::loader::{Decoded, Pixels};
use crate::preset::Preset;

/// Crop rectangle in normalised source coordinates: `[x0, y0, x1, y1]`.
pub type Crop = [f32; 4];
pub const FULL_CROP: Crop = [0.0, 0.0, 1.0, 1.0];

pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    mip_srgb: wgpu::RenderPipeline,
    mip_f16: wgpu::RenderPipeline,
    /// Texture + sampler, for mip generation.
    src_layout: wgpu::BindGroupLayout,
    /// Texture + sampler + colour matrix, for preset rendering.
    source_layout: wgpu::BindGroupLayout,
    preset_layout: wgpu::BindGroupLayout,
    target_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    identity_lut: wgpu::TextureView,
}

pub struct Source {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

pub struct GpuPreset {
    bind_group: wgpu::BindGroup,
}

pub struct Target {
    pub texture: wgpu::Texture,
    /// sRGB view we render into (the GPU encodes linear shader output).
    pub view: wgpu::TextureView,
    /// Plain `Rgba8Unorm` view of the same bytes, for egui: it expects gamma-encoded
    /// textures and would otherwise receive decoded (too dark) linear values.
    pub display_view: wgpu::TextureView,
    strength: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

fn texture_entry(binding: u32, dim: wgpu::TextureViewDimension) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: dim,
            multisampled: false,
        },
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

const SAMPLER_ENTRY: wgpu::BindGroupLayoutEntry = wgpu::BindGroupLayoutEntry {
    binding: 1,
    visibility: wgpu::ShaderStages::FRAGMENT,
    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
    count: None,
};

fn f16_bytes(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(|v| half::f16::from_f32(v).to_bits().to_le_bytes()).collect()
}

impl Gpu {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let src_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("source"),
            entries: &[texture_entry(0, wgpu::TextureViewDimension::D2), SAMPLER_ENTRY],
        });
        let source_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("source + gamut"),
            entries: &[texture_entry(0, wgpu::TextureViewDimension::D2), SAMPLER_ENTRY, uniform_entry(2)],
        });
        let preset_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preset"),
            entries: &[
                uniform_entry(0),
                texture_entry(1, wgpu::TextureViewDimension::D2),
                texture_entry(2, wgpu::TextureViewDimension::D3),
            ],
        });
        let target_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("target"),
            entries: &[uniform_entry(0)],
        });

        let make_pipeline = |label: &str, source: &str, layouts: &[Option<&wgpu::BindGroupLayout>], format: wgpu::TextureFormat| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: layouts,
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(format.into())],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let shader = include_str!("shader.wgsl");
        let mip = include_str!("mip.wgsl");
        let pipeline =
            make_pipeline("preset", shader, &[Some(&source_layout), Some(&preset_layout), Some(&target_layout)], OUTPUT_FORMAT);
        let mip_srgb = make_pipeline("mip srgb", mip, &[Some(&src_layout)], wgpu::TextureFormat::Rgba8UnormSrgb);
        let mip_f16 = make_pipeline("mip f16", mip, &[Some(&src_layout)], wgpu::TextureFormat::Rgba16Float);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let identity: Vec<f32> =
            (0..8).flat_map(|i| [(i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32, 1.0]).collect();
        let identity_lut = Self::lut_texture(&device, &queue, 2, &f16_bytes(identity));

        Self { device, queue, pipeline, mip_srgb, mip_f16, src_layout, source_layout, preset_layout, target_layout, sampler, identity_lut }
    }

    /// Runs every pipeline once on a tiny image, so shader compilation and driver setup
    /// happen at startup instead of when the first photo opens.
    pub fn warm_up(&self, preset: &GpuPreset) {
        for p3 in [false, true] {
            let img = Decoded { width: 8, height: 8, pixels: Pixels::Srgb8(vec![128; 8 * 8 * 4]), p3 };
            let src = self.upload(&img);
            let target = self.create_target(8, 8, wgpu::TextureUsages::empty());
            let mut encoder = self.device.create_command_encoder(&Default::default());
            self.render(&mut encoder, &src, preset, &target, 1.0, FULL_CROP);
            self.queue.submit([encoder.finish()]);
        }
        let img = Decoded { width: 8, height: 8, pixels: Pixels::LinearF16(vec![0; 8 * 8 * 4]), p3: false };
        self.upload(&img);
        _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }

    pub fn max_dim(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    fn lut_texture(device: &wgpu::Device, queue: &wgpu::Queue, size: u32, rgba_f16: &[u8]) -> wgpu::TextureView {
        device
            .create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some("lut"),
                    size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: size },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D3,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                rgba_f16,
            )
            .create_view(&Default::default())
    }

    /// Uploads a decoded photo and builds its full mip chain on the GPU.
    pub fn upload(&self, img: &Decoded) -> Source {
        let (format, bytes, bpp): (_, &[u8], u32) = match &img.pixels {
            Pixels::Srgb8(p) => (wgpu::TextureFormat::Rgba8UnormSrgb, p, 4),
            Pixels::LinearF16(p) => (wgpu::TextureFormat::Rgba16Float, bytemuck::cast_slice(p), 8),
        };
        let size = wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 };
        let mips = 32 - img.width.max(img.height).leading_zeros();
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("source"),
            size,
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(img.width * bpp), rows_per_image: Some(img.height) },
            size,
        );

        let pipeline = if format == wgpu::TextureFormat::Rgba16Float { &self.mip_f16 } else { &self.mip_srgb };
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let level_view = |level| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        for level in 1..mips {
            let src = level_view(level - 1);
            let dst = level_view(level);
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.src_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);

        let view = texture.create_view(&Default::default());
        // Linear-light colour matrix applied after sampling (rows as vec4s).
        let gamut: [[f32; 4]; 3] = if img.p3 {
            [[1.2249, -0.2247, 0.0, 0.0], [-0.0420, 1.0419, 0.0, 0.0], [-0.0197, -0.0786, 1.0979, 0.0]]
        } else {
            [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]]
        };
        let gamut = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("gamut"),
            contents: bytemuck::cast_slice(&gamut),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("source"),
            layout: &self.source_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: gamut.as_entire_binding() },
            ],
        });
        Source { _texture: texture, bind_group, width: img.width, height: img.height }
    }

    pub fn create_preset(&self, preset: &Preset) -> GpuPreset {
        let uniforms = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preset params"),
            contents: bytemuck::cast_slice(&preset.uniforms()),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let curve = self
            .device
            .create_texture_with_data(
                &self.queue,
                &wgpu::TextureDescriptor {
                    label: Some("curve"),
                    size: wgpu::Extent3d { width: 256, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &f16_bytes(preset.curve_table().into_iter().flat_map(|[r, g, b]| [r, g, b, 1.0])),
            )
            .create_view(&Default::default());
        let lut = preset.lut.as_ref().map(|lut| {
            Self::lut_texture(&self.device, &self.queue, lut.size, &f16_bytes(lut.data.iter().flat_map(|[r, g, b]| [*r, *g, *b, 1.0])))
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preset"),
            layout: &self.preset_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&curve) },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(lut.as_ref().unwrap_or(&self.identity_lut)),
                },
            ],
        });
        GpuPreset { bind_group }
    }

    pub fn create_target(&self, width: u32, height: u32, extra_usage: wgpu::TextureUsages) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | extra_usage,
            view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
        });
        let view = texture.create_view(&Default::default());
        let display_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            ..Default::default()
        });
        let strength = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("strength"),
            contents: bytemuck::cast_slice(&[1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("target"),
            layout: &self.target_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: strength.as_entire_binding() }],
        });
        Target { texture, view, display_view, strength, bind_group, width, height }
    }

    /// Records a render of `preset` applied to `src` into `target`.
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        src: &Source,
        preset: &GpuPreset,
        target: &Target,
        strength: f32,
        crop: Crop,
    ) {
        let [x0, y0, x1, y1] = crop;
        let params = [strength, 0.0, 0.0, 0.0, x0, y0, x1 - x0, y1 - y0];
        self.queue.write_buffer(&target.strength, 0, bytemuck::cast_slice(&params));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("preset"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &src.bind_group, &[]);
        pass.set_bind_group(1, &preset.bind_group, &[]);
        pass.set_bind_group(2, &target.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Renders every preset into one grid image (`cols` cells per row, each `cell` pixels) and
    /// starts reading it back; poll [`Readback::try_take`] until the data arrives.
    pub fn render_grid(&self, src: &Source, presets: &[GpuPreset], crop: Crop, cell: (u32, u32), cols: u32) -> Readback {
        let rows = (presets.len() as u32).div_ceil(cols);
        let (w, h) = (cell.0 * cols, cell.1 * rows);
        let target = self.create_target(w, h, wgpu::TextureUsages::COPY_SRC);
        let [x0, y0, x1, y1] = crop;
        self.queue.write_buffer(&target.strength, 0, bytemuck::cast_slice(&[1.0f32, 0.0, 0.0, 0.0, x0, y0, x1 - x0, y1 - y0]));
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preset grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &src.bind_group, &[]);
            pass.set_bind_group(2, &target.bind_group, &[]);
            for (i, preset) in presets.iter().enumerate() {
                let (cx, cy) = ((i as u32 % cols) * cell.0, (i as u32 / cols) * cell.1);
                // The full-screen triangle fills whatever viewport it is given.
                pass.set_viewport(cx as f32, cy as f32, cell.0 as f32, cell.1 as f32, 0.0, 1.0);
                pass.set_bind_group(1, &preset.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid readback"),
            size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ready.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            if r.is_ok() {
                flag.store(true, std::sync::atomic::Ordering::Release);
            }
        });
        Readback { buffer, ready, width: w, height: h, padded }
    }

    /// Renders at the given size (the source size for exports) and reads the result back as RGB8.
    pub fn render_image(
        &self,
        src: &Source,
        preset: &GpuPreset,
        strength: f32,
        crop: Crop,
        w: u32,
        h: u32,
    ) -> Result<image::RgbImage, String> {
        let target = self.create_target(w, h, wgpu::TextureUsages::COPY_SRC);
        let padded = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.render(&mut encoder, src, preset, &target, strength, crop);
        encoder.copy_texture_to_buffer(
            target.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| _ = tx.send(r));
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

        let data = slice.get_mapped_range().map_err(|e| e.to_string())?;
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for row in data.chunks_exact(padded as usize) {
            for px in row[..(w * 4) as usize].chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        drop(data);
        buffer.unmap();
        image::RgbImage::from_raw(w, h, rgb).ok_or_else(|| "readback size mismatch".into())
    }
}

/// An in-flight GPU → CPU copy of an RGBA8 image.
pub struct Readback {
    buffer: wgpu::Buffer,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub width: u32,
    pub height: u32,
    padded: u32,
}

impl Readback {
    /// Tightly packed RGBA8 pixels once the copy has finished (non-blocking).
    pub fn try_take(&self, device: &wgpu::Device) -> Option<Vec<u8>> {
        _ = device.poll(wgpu::PollType::Poll);
        if !self.ready.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let data = self.buffer.slice(..).get_mapped_range().ok()?;
        let row = (self.width * 4) as usize;
        let mut out = Vec::with_capacity(row * self.height as usize);
        for r in data.chunks_exact(self.padded as usize) {
            out.extend_from_slice(&r[..row]);
        }
        drop(data);
        self.buffer.unmap();
        Some(out)
    }
}
