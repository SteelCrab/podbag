//! The GPU side of images: one atlas texture with each image at the pixel size it is
//! drawn (a 1-pixel border of repeated edge pixels keeps filtering inside it), and a
//! pipeline of textured quads. When the atlas is full it is cleared and packed again
//! with what the frame draws.

use std::collections::HashMap;

use crate::image::{Image, ShelfPacker};
use crate::scene::{ImageDraw, Scene, layer};

const ATLAS: u32 = 1024;
const FLOATS: usize = 16;

/// The largest side an image is kept at (the GPU scales it up past that).
const MAX_SIDE: u32 = ATLAS - 2;

struct Slot {
    /// Texture coordinates: x, y, w, h.
    uv: [f32; 4],
}

pub(crate) struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_bind: wgpu::BindGroup,
    atlas_bind: wgpu::BindGroup,
    texture: wgpu::Texture,
    packer: ShelfPacker,
    /// (image, width, height asked for) to its place in the atlas.
    slots: HashMap<(Image, u32, u32), Slot>,
    instances: wgpu::Buffer,
    bytes: Vec<u8>,
}

/// The pixel size to keep an image at when drawn `w` x `h` logical pixels.
pub(crate) fn target_size(w: f32, h: f32, scale: f32) -> (u32, u32) {
    let (tw, th) = ((w * scale).round().max(1.), (h * scale).round().max(1.));
    let fit = (MAX_SIDE as f32 / tw.max(th)).min(1.);
    (
        ((tw * fit).round() as u32).max(1),
        ((th * fit).round() as u32).max(1),
    )
}

/// `w` x `h` premultiplied pixels with a border of their edge pixels around them.
pub(crate) fn with_border(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    let (bw, bh) = (w + 2, h + 2);
    let mut out = vec![0u8; (bw * bh * 4) as usize];
    for y in 0..bh {
        let sy = y.saturating_sub(1).min(h - 1);
        for x in 0..bw {
            let sx = x.saturating_sub(1).min(w - 1);
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * bw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&px[s..s + 4]);
        }
    }
    out
}

impl ImageRenderer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        globals: &wgpu::Buffer,
        linear: bool,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("images"),
            size: wgpu::Extent3d {
                width: ATLAS,
                height: ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if linear {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            },
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("images"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image globals"),
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
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image atlas"),
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
        let atlas_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            bind_group_layouts: &[Some(&globals_layout), Some(&atlas_layout)],
            ..Default::default()
        });
        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: (FLOATS * 4) as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        Self {
            pipeline,
            globals_bind,
            atlas_bind,
            texture,
            packer: ShelfPacker::new(ATLAS, ATLAS),
            slots: HashMap::new(),
            instances: instance_buffer(device, 64),
            bytes: Vec::new(),
        }
    }

    /// Put what `scene` draws in the atlas and the instance buffer: per layer, its
    /// range of instances.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &Scene,
        scale: f32,
        linear: bool,
    ) -> [(u32, u32); layer::COUNT] {
        let mut ranges = [(0u32, 0u32); layer::COUNT];
        // A second pass after a full atlas was cleared.
        for _ in 0..2 {
            self.bytes.clear();
            let mut n = 0u32;
            let mut full = false;
            'layers: for (i, l) in scene.layers.iter().enumerate() {
                let start = n;
                for d in &l.images {
                    match self.slot(queue, d, scale) {
                        Some(uv) => {
                            push(&mut self.bytes, d, uv, scale, linear);
                            n += 1;
                        }
                        None => {
                            full = true;
                            break 'layers;
                        }
                    }
                }
                ranges[i] = (start, n);
            }
            if !full {
                break;
            }
            self.packer.clear();
            self.slots.clear();
            ranges = [(0, 0); layer::COUNT];
        }
        let needed = self.bytes.len() as u64;
        if needed > self.instances.size() {
            self.instances =
                instance_buffer(device, (needed as usize / (FLOATS * 4)).next_power_of_two());
        }
        if needed > 0 {
            queue.write_buffer(&self.instances, 0, &self.bytes);
        }
        ranges
    }

    /// The texture coordinates of `d`'s image at its drawn size; uploads it the first
    /// time. None if the atlas is full.
    fn slot(&mut self, queue: &wgpu::Queue, d: &ImageDraw, scale: f32) -> Option<[f32; 4]> {
        let (tw, th) = target_size(d.rect.w, d.rect.h, scale);
        let key = (d.image, tw, th);
        if let Some(s) = self.slots.get(&key) {
            return Some(s.uv);
        }
        let (w, h, px) = d.image.data().resampled(tw, th);
        let (w, h, px) = if w > MAX_SIDE || h > MAX_SIDE {
            // Larger than the atlas allows: keep it at the largest size that fits.
            let f = MAX_SIDE as f32 / w.max(h) as f32;
            let (fw, fh) = (
                ((w as f32 * f) as u32).max(1),
                ((h as f32 * f) as u32).max(1),
            );
            d.image.data().resampled(fw, fh)
        } else {
            (w, h, px)
        };
        let (x, y) = self.packer.alloc(w + 2, h + 2)?;
        let data = with_border(w, h, &px);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((w + 2) * 4),
                rows_per_image: Some(h + 2),
            },
            wgpu::Extent3d {
                width: w + 2,
                height: h + 2,
                depth_or_array_layers: 1,
            },
        );
        let a = ATLAS as f32;
        let uv = [
            (x + 1) as f32 / a,
            (y + 1) as f32 / a,
            w as f32 / a,
            h as f32 / a,
        ];
        self.slots.insert(key, Slot { uv });
        Some(uv)
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, range: (u32, u32)) {
        if range.1 <= range.0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind, &[]);
        pass.set_bind_group(1, &self.atlas_bind, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..4, range.0..range.1);
    }
}

fn push(out: &mut Vec<u8>, d: &ImageDraw, uv: [f32; 4], s: f32, linear: bool) {
    let tint = match d.tint {
        Some(c) if linear => crate::gpu::to_linear(c).to_array(),
        Some(c) => c.to_array(),
        None => [0., 0., 0., -1.],
    };
    // Whole pixels, so an image kept at its drawn size maps texel to pixel.
    let (x, y) = ((d.rect.x * s).round(), (d.rect.y * s).round());
    let (w, h) = ((d.rect.w * s).round(), (d.rect.h * s).round());
    let v: [f32; FLOATS] = [
        x,
        y,
        w,
        h,
        d.clip.x * s,
        d.clip.y * s,
        d.clip.right() * s,
        d.clip.bottom() * s,
        uv[0],
        uv[1],
        uv[2],
        uv[3],
        tint[0],
        tint[1],
        tint[2],
        tint[3],
    ];
    out.extend(v.iter().flat_map(|f| f.to_ne_bytes()));
}

fn instance_buffer(device: &wgpu::Device, count: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("images"),
        size: (count * FLOATS * 4) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_kept_at_their_physical_size() {
        assert_eq!(target_size(20., 20., 2.), (40, 40));
        assert_eq!(target_size(72., 36., 1.5), (108, 54));
        // Never empty, never larger than the atlas.
        assert_eq!(target_size(0.1, 0.1, 1.), (1, 1));
        assert_eq!(target_size(4000., 2000., 1.), (MAX_SIDE, MAX_SIDE / 2));
    }

    #[test]
    fn the_border_repeats_edge_pixels() {
        // 2 x 1: A B
        let px = [1, 1, 1, 1, 2, 2, 2, 2];
        let out = with_border(2, 1, &px);
        let at = |x: usize, y: usize| out[(y * 4 + x) * 4];
        // 4 x 3 with the image in the middle row.
        for y in 0..3 {
            assert_eq!([at(0, y), at(1, y), at(2, y), at(3, y)], [1, 1, 2, 2]);
        }
    }
}
