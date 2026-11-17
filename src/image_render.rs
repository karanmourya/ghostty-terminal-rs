//! `image_render` — textured quads for the kitty graphics protocol.
//!
//! Images decoded from `ESC _ G ...` / `ESC P G ...` sequences are uploaded
//! once as GPU textures and drawn as screen-space rectangles behind the text.

use std::collections::HashMap;
use wgpu::util::DeviceExt;

use crate::term::KittyImage;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TexQuad {
    rect: [f32; 4],
    uv: [f32; 4],
    tint: [f32; 4],
    image_id: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

struct Entry {
    // Kept alive so the texture view in `bind_group` stays valid.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    uniform_buf: wgpu::Buffer,
    uniform_bg: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
    entries: HashMap<u32, Entry>,
    quads: Vec<TexQuad>,
    version: u64,
    format: wgpu::TextureFormat,
}

impl ImageRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kitty image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kitty uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kitty uniform bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kitty uniform bg"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kitty sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kitty texture bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
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
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kitty layout"),
            bind_group_layouts: &[&uniform_layout, &layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kitty pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TexQuad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 32,
                            shader_location: 2,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let capacity = 64;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kitty instances"),
            size: (capacity * std::mem::size_of::<TexQuad>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            sampler,
            layout,
            uniform_buf,
            uniform_bg,
            instances,
            capacity,
            entries: HashMap::new(),
            quads: Vec::new(),
            version: u64::MAX,
            format,
        }
    }

    /// Upload any images not yet on the GPU. `version` changes when the term
    /// adds or removes images, so deletions are handled too.
    fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        version: u64,
        images: &[&KittyImage],
    ) {
        if version == self.version {
            return;
        }
        self.version = version;
        let live: Vec<u32> = images.iter().map(|img| img.id).collect();
        self.entries.retain(|id, _| live.contains(id));
        for img in images {
            let id = img.id;
            if self.entries.contains_key(&id) {
                continue;
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("kitty texture"),
                size: wgpu::Extent3d {
                    width: img.width.max(1),
                    height: img.height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            // write_texture requires 256-byte row alignment.
            let bpr = (img.width.max(1) * 4).div_ceil(256) * 256;
            let mut padded = vec![0u8; (bpr * img.height.max(1)) as usize];
            for row in 0..img.height.max(1) as usize {
                let src = row * img.width as usize * 4;
                let dst = row * bpr as usize;
                let n = (img.width as usize * 4).min(img.rgba.len().saturating_sub(src));
                if n > 0 {
                    padded[dst..dst + n].copy_from_slice(&img.rgba[src..src + n]);
                }
            }
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &padded,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(img.height.max(1)),
                },
                wgpu::Extent3d {
                    width: img.width.max(1),
                    height: img.height.max(1),
                    depth_or_array_layers: 1,
                },
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kitty texture bg"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.entries.insert(
                id,
                Entry {
                    _texture: texture,
                    bind_group,
                },
            );
        }
        let _ = self.format;
    }

    pub fn begin(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: [f32; 2],
        version: u64,
        images: &[&KittyImage],
    ) {
        self.sync(device, queue, version, images);
        queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen,
                _pad: [0.0; 2],
            }),
        );
        self.quads.clear();
    }

    pub fn push(&mut self, device: &wgpu::Device, image_id: u32, x: f32, y: f32, w: f32, h: f32) {
        if !self.entries.contains_key(&image_id) {
            return;
        }
        self.quads.push(TexQuad {
            rect: [x, y, w, h],
            uv: [0.0, 0.0, 1.0, 1.0],
            tint: [1.0, 1.0, 1.0, 1.0],
            image_id,
            _pad: [0; 3],
        });
        if self.quads.len() > self.capacity {
            self.capacity = self.quads.len().next_power_of_two();
            self.instances = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("kitty instances"),
                contents: bytemuck::cast_slice(&self.quads),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            });
        }
    }

    pub fn upload(&self, queue: &wgpu::Queue) {
        if !self.quads.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.quads));
        }
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if self.quads.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bg, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        for (i, q) in self.quads.iter().enumerate() {
            if let Some(entry) = self.entries.get(&q.image_id) {
                pass.set_bind_group(1, &entry.bind_group, &[]);
                pass.draw(0..6, i as u32..i as u32 + 1);
            }
        }
    }
}
