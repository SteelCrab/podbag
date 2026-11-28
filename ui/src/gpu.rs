//! wgpu surface and the two renderers: quads (our shader) and text (glyphon). Without a
//! window (`Gpu::headless`) the same renderers draw into a texture that can be read
//! back (screenshots of a view whose window cannot show).

use std::sync::Arc;

use glyphon::{
    Buffer, Cache, ColorMode, CustomGlyph, FontSystem, Metrics, Resolution, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};
use winit::window::Window;

use crate::geom::{Color, Rect};
use crate::icons;
use crate::image_gpu::ImageRenderer;
use crate::scene::{Quad, Scene, layer};

const QUAD_FLOATS: usize = 20;

pub struct Gpu {
    /// None for a headless renderer.
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// True if the surface is sRGB: colors must be linearized for the quad shader.
    linear: bool,
    quad_pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_bytes: Vec<u8>,
    images: ImageRenderer,
    atlas: TextAtlas,
    viewport: Viewport,
    text: Vec<TextRenderer>,
    empty: Buffer,
    swash: SwashCache,
    pub adapter_info: String,
}

impl Gpu {
    pub fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display)),
        );
        let surface = instance.create_surface(window).expect("create surface");
        let (adapter, device, queue) = open_device(&instance, Some(&surface))
            .expect("no GPU adapter that can draw into the window, not even a software one");
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);

        // Prefer a non-sRGB format: blending then happens in sRGB space, like GPUI and browsers.
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        Self::with_device(Some(surface), device, queue, config, adapter_info)
    }

    /// A renderer without a window, `width` x `height` pixels; None without a GPU.
    pub fn headless(width: u32, height: u32) -> Option<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let (adapter, device, queue) = open_device(&instance, None)?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format: wgpu::TextureFormat::Rgba8Unorm,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        Some(Self::with_device(None, device, queue, config, adapter_info))
    }

    fn with_device(
        surface: Option<wgpu::Surface<'static>>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        adapter_info: String,
    ) -> Self {
        let format = config.format;
        let linear = format.is_srgb();
        let (quad_pipeline, globals, globals_bind) = quad_pipeline(&device, format);
        let instances = instance_buffer(&device, 256);
        let images = ImageRenderer::new(&device, format, &globals, linear);

        let cache = Cache::new(&device);
        let color_mode = if linear {
            ColorMode::Accurate
        } else {
            ColorMode::Web
        };
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, color_mode);
        let viewport = Viewport::new(&device, &cache);
        let text = (0..layer::COUNT)
            .map(|_| {
                TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None)
            })
            .collect();

        Self {
            surface,
            device,
            queue,
            config,
            linear,
            quad_pipeline,
            globals,
            globals_bind,
            instances,
            instance_bytes: Vec::new(),
            images,
            atlas,
            viewport,
            text,
            empty: Buffer::new_empty(Metrics::new(1., 1.)),
            swash: SwashCache::new(),
            adapter_info,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
    }

    /// The next surface texture. `Err(true)`: try again now (timeout, surface reset).
    /// `Err(false)`: occluded, wait for the window to show.
    pub fn acquire(&mut self) -> Result<wgpu::SurfaceTexture, bool> {
        let Some(surface) = &self.surface else {
            return Err(false);
        };
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => Ok(f),
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                surface.configure(&self.device, &self.config);
                Ok(f)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.device, &self.config);
                Err(true)
            }
            wgpu::CurrentSurfaceTexture::Occluded => Err(false),
            _ => Err(true),
        }
    }

    pub fn render(
        &mut self,
        frame: wgpu::SurfaceTexture,
        scene: &Scene,
        fonts: &mut FontSystem,
        scale: f32,
        clear: Color,
        window: &Window,
    ) {
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.draw(&view, scene, fonts, scale, clear);
        window.pre_present_notify();
        self.queue.present(frame);
        self.atlas.trim();
    }

    /// Draw `scene` into a texture and read it back: RGBA rows, top to bottom.
    pub fn capture(
        &mut self,
        scene: &Scene,
        fonts: &mut FontSystem,
        scale: f32,
        clear: Color,
    ) -> Vec<u8> {
        let (w, h) = (self.config.width, self.config.height);
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw(&view, scene, fonts, scale, clear);
        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        self.queue.submit(Some(encoder.finish()));
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        if let Ok(data) = buffer.get_mapped_range(..) {
            for y in 0..h as usize {
                let start = y * row as usize;
                out.extend_from_slice(&data[start..start + w as usize * 4]);
            }
        }
        // Window surfaces are often BGRA; the result is always RGBA.
        if matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            for px in out.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
        }
        out
    }

    /// The drawing size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Draw `scene` into `view`.
    fn draw(
        &mut self,
        view: &wgpu::TextureView,
        scene: &Scene,
        fonts: &mut FontSystem,
        scale: f32,
        clear: Color,
    ) {
        let (w, h) = (self.config.width, self.config.height);
        // All quads in one buffer: per layer, its background quads then its overlay quads.
        self.instance_bytes.clear();
        let mut ranges = [(0u32, 0u32, 0u32); layer::COUNT];
        let mut n = 0u32;
        for (i, l) in scene.layers.iter().enumerate() {
            let start = n;
            for q in &l.quads {
                push_quad(&mut self.instance_bytes, q, scale, self.linear);
            }
            n += l.quads.len() as u32;
            let mid = n;
            for q in &l.overlay {
                push_quad(&mut self.instance_bytes, q, scale, self.linear);
            }
            n += l.overlay.len() as u32;
            ranges[i] = (start, mid, n);
        }
        let needed = self.instance_bytes.len() as u64;
        if needed > self.instances.size() {
            self.instances = instance_buffer(
                &self.device,
                (needed as usize / (QUAD_FLOATS * 4)).next_power_of_two(),
            );
        }
        if needed > 0 {
            self.queue
                .write_buffer(&self.instances, 0, &self.instance_bytes);
        }
        let globals: Vec<u8> = [w as f32, h as f32, 0., 0.]
            .iter()
            .flat_map(|f| f.to_ne_bytes())
            .collect();
        self.queue.write_buffer(&self.globals, 0, &globals);
        let image_ranges =
            self.images
                .prepare(&self.device, &self.queue, scene, scale, self.linear);

        // Text and icons, one glyphon renderer per layer (they share the atlas).
        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );
        let mut glyphs: Vec<CustomGlyph> = Vec::new();
        for (i, l) in scene.layers.iter().enumerate() {
            glyphs.clear();
            glyphs.extend(l.icons.iter().map(|ic| CustomGlyph {
                id: ic.icon,
                left: 0.,
                top: 0.,
                width: ic.rect.w,
                height: ic.rect.h,
                color: Some(glyph_color(ic.color)),
                snap_to_physical_pixel: true,
                metadata: 0,
            }));
            let texts = l.texts.iter().map(|t| TextArea {
                buffer: &t.layout.buffer,
                left: t.x * scale,
                top: t.y * scale,
                scale,
                bounds: physical_bounds(&t.clip, scale),
                default_color: glyph_color(t.color),
                custom_glyphs: &[],
            });
            let icons = l
                .icons
                .iter()
                .zip(glyphs.chunks(1))
                .map(|(ic, g)| TextArea {
                    buffer: &self.empty,
                    left: ic.rect.x * scale,
                    top: ic.rect.y * scale,
                    scale,
                    bounds: physical_bounds(&ic.clip, scale),
                    default_color: glyph_color(ic.color),
                    custom_glyphs: g,
                });
            if let Err(e) = self.text[i].prepare_with_custom(
                &self.device,
                &self.queue,
                fonts,
                &mut self.atlas,
                &self.viewport,
                texts.chain(icons),
                &mut self.swash,
                icons::rasterize,
            ) {
                eprintln!("text prepare: {e}");
            }
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let [r, g, b, _] = if self.linear { to_linear(clear) } else { clear }.to_array();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: 1.,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for (i, (start, mid, end)) in ranges.iter().copied().enumerate() {
                if mid > start {
                    self.draw_quads(&mut pass, start..mid);
                }
                self.images.draw(&mut pass, image_ranges[i]);
                let _ = self.text[i].render(&self.atlas, &self.viewport, &mut pass);
                if end > mid {
                    self.draw_quads(&mut pass, mid..end);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn draw_quads(&self, pass: &mut wgpu::RenderPass<'_>, range: std::ops::Range<u32>) {
        pass.set_pipeline(&self.quad_pipeline);
        pass.set_bind_group(0, &self.globals_bind, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..4, range);
    }
}

/// The adapter that wgpu picks (a GPU first), then a software one: WARP on Windows,
/// llvmpipe or SwiftShader elsewhere. Machines without a working GPU driver, such as
/// virtual machines and CI runners, still get a device.
fn open_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    for fallback in [false, true] {
        let request = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: fallback,
            compatible_surface: surface,
            ..Default::default()
        };
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&request)) else {
            continue;
        };
        let descriptor = wgpu::DeviceDescriptor {
            label: Some("ui"),
            // Low limits keep the device usable on old GPUs and GL.
            required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                .using_resolution(adapter.limits()),
            ..Default::default()
        };
        match pollster::block_on(adapter.request_device(&descriptor)) {
            Ok((device, queue)) => return Some((adapter, device, queue)),
            Err(e) => eprintln!("podbag-ui: {}: {e}", adapter.get_info().name),
        }
    }
    None
}

fn physical_bounds(r: &Rect, scale: f32) -> TextBounds {
    TextBounds {
        left: (r.x * scale).floor() as i32,
        top: (r.y * scale).floor() as i32,
        right: (r.right() * scale).ceil() as i32,
        bottom: (r.bottom() * scale).ceil() as i32,
    }
}

fn glyph_color(c: Color) -> glyphon::Color {
    let [r, g, b, a] = c.to_u8();
    glyphon::Color::rgba(r, g, b, a)
}

pub(crate) fn to_linear(c: Color) -> Color {
    let f = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    Color {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
        a: c.a,
    }
}

fn push_quad(out: &mut Vec<u8>, q: &Quad, s: f32, linear: bool) {
    let (color, border) = if linear {
        (to_linear(q.color), to_linear(q.border_color))
    } else {
        (q.color, q.border_color)
    };
    let v: [f32; QUAD_FLOATS] = [
        q.rect.x * s,
        q.rect.y * s,
        q.rect.w * s,
        q.rect.h * s,
        q.clip.x * s,
        q.clip.y * s,
        q.clip.right() * s,
        q.clip.bottom() * s,
        color.r,
        color.g,
        color.b,
        color.a,
        border.r,
        border.g,
        border.b,
        border.a,
        q.radius * s,
        q.border * s,
        q.blur * s,
        0.,
    ];
    out.extend(v.iter().flat_map(|f| f.to_ne_bytes()));
}

fn instance_buffer(device: &wgpu::Device, count: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quads"),
        size: (count * QUAD_FLOATS * 4) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn quad_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::Buffer, wgpu::BindGroup) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quad"),
        source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
    });
    let globals = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("globals"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("globals"),
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
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("globals"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: globals.as_entire_binding(),
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        bind_group_layouts: &[Some(&layout)],
        ..Default::default()
    });
    let attrs = wgpu::vertex_attr_array![
        0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4
    ];
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quad"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: (QUAD_FLOATS * 4) as u64,
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
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
    (pipeline, globals, bind)
}

/// Windows always has WARP, so the app gets a device on a machine without a GPU, such as
/// the CI runner.
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_has_a_software_adapter() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let warp = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        }))
        .expect("WARP");
        assert_eq!(warp.get_info().device_type, wgpu::DeviceType::Cpu);
        assert!(open_device(&instance, None).is_some());
        assert!(Gpu::headless(64, 64).is_some());
    }
}
