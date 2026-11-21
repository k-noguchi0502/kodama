//! wgpu renderer: draws the terminal grid as instanced quads in a single pass.

pub mod boxdraw;
pub mod font;

use std::collections::HashMap;
use std::sync::Arc;

use unicode_width::UnicodeWidthChar;
use winit::window::Window;

use self::font::{FontId, FontSet, ShapedGlyph, Style};
use crate::config::{Colors, ImageFit, Rgb};
use crate::layout::Rect;
use crate::terminal::graphics::Image;
use crate::terminal::{Cell, Color, CursorShape, Point, Terminal, flags};

/// Atlases start small and double when full, up to their maximum size.
const ATLAS_SIZE: (u32, u32) = (512, 2048);
const COLOR_ATLAS_SIZE: (u32, u32) = (256, 1024);
const KIND_SOLID: u32 = 0;
const KIND_GLYPH: u32 = 1;
const KIND_COLOR_GLYPH: u32 = 2;
const KIND_IMAGE: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    srgb: f32,
    _pad: f32,
}

#[derive(Clone, Copy)]
struct GlyphInfo {
    /// Normalised atlas rect (x, y, w, h).
    uv: [f32; 4],
    /// Offset of the bitmap from the cell's top-left corner, in pixels.
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    advance: f32,
    /// Stored in the RGBA atlas and drawn with its own colours.
    color: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GlyphKey {
    Font(FontId, u16),
    /// Procedurally drawn box-drawing / block element.
    Builtin(char),
}

struct AtlasFull;

/// Shelf packing for one atlas texture.
struct Shelves {
    texture: wgpu::Texture,
    label: &'static str,
    format: wgpu::TextureFormat,
    size: u32,
    max_size: u32,
    bytes_per_pixel: u32,
    cursor_x: u32,
    cursor_y: u32,
    shelf_height: u32,
    /// An allocation failed since the last clear.
    full: bool,
}

impl Shelves {
    fn new(
        device: &wgpu::Device,
        label: &'static str,
        (size, max_size): (u32, u32),
        format: wgpu::TextureFormat,
        bytes_per_pixel: u32,
    ) -> Self {
        let size = size.min(device.limits().max_texture_dimension_2d);
        Self {
            texture: Self::create_texture(device, label, size, format),
            label,
            format,
            size,
            max_size: max_size.min(device.limits().max_texture_dimension_2d),
            bytes_per_pixel,
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
            full: false,
        }
    }

    fn create_texture(
        device: &wgpu::Device,
        label: &str,
        size: u32,
        format: wgpu::TextureFormat,
    ) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    /// Doubles the texture if it filled up. Its contents are discarded.
    fn grow(&mut self, device: &wgpu::Device) -> bool {
        if !self.full || self.size >= self.max_size {
            return false;
        }
        self.size = (self.size * 2).min(self.max_size);
        self.texture = Self::create_texture(device, self.label, self.size, self.format);
        log::info!("{} grown to {}px", self.label, self.size);
        true
    }

    fn clear(&mut self) {
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.shelf_height = 0;
        self.full = false;
    }

    fn allocate(&mut self, w: u32, h: u32) -> Result<(u32, u32), AtlasFull> {
        const GAP: u32 = 1;
        let size = self.size;
        if w + GAP > size || h + GAP > size {
            self.full = true;
            return Err(AtlasFull);
        }
        if self.cursor_x + w + GAP > size {
            self.cursor_x = 0;
            self.cursor_y += self.shelf_height + GAP;
            self.shelf_height = 0;
        }
        if self.cursor_y + h + GAP > size {
            self.full = true;
            return Err(AtlasFull);
        }
        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += w + GAP;
        self.shelf_height = self.shelf_height.max(h);
        Ok(pos)
    }

    /// Upload `bitmap` and return its normalised rect.
    fn insert(
        &mut self,
        queue: &wgpu::Queue,
        bitmap: &[u8],
        w: u32,
        h: u32,
    ) -> Result<[f32; 4], AtlasFull> {
        let (x, y) = self.allocate(w, h)?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            bitmap,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * self.bytes_per_pixel),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let s = self.size as f32;
        Ok([x as f32 / s, y as f32 / s, w as f32 / s, h as f32 / s])
    }
}

/// Glyph cache: coverage masks in a single-channel texture, colour glyphs
/// (emoji) in a separate RGBA one.
struct Atlas {
    mask: Shelves,
    color: Shelves,
    glyphs: HashMap<GlyphKey, Option<GlyphInfo>>,
}

impl Atlas {
    fn new(device: &wgpu::Device) -> Self {
        Self {
            mask: Shelves::new(
                device,
                "glyph atlas",
                ATLAS_SIZE,
                wgpu::TextureFormat::R8Unorm,
                1,
            ),
            color: Shelves::new(
                device,
                "colour glyph atlas",
                COLOR_ATLAS_SIZE,
                wgpu::TextureFormat::Rgba8Unorm,
                4,
            ),
            glyphs: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.glyphs.clear();
        self.mask.clear();
        self.color.clear();
    }

    /// Enlarges whichever texture filled up, emptying the cache since glyph
    /// rects are stored normalised. False when both are at their maximum.
    fn grow(&mut self, device: &wgpu::Device) -> bool {
        let grew = self.mask.grow(device) | self.color.grow(device);
        if grew {
            self.clear();
        }
        grew
    }

    fn get(
        &mut self,
        queue: &wgpu::Queue,
        font: &FontSet,
        key: GlyphKey,
    ) -> Result<Option<GlyphInfo>, AtlasFull> {
        if let Some(info) = self.glyphs.get(&key) {
            return Ok(*info);
        }

        let (bitmap, w, h, left, top, advance, color) = match key {
            GlyphKey::Builtin(c) => {
                let (w, h) = (font.cell_width as usize, font.cell_height as usize);
                let bitmap = boxdraw::rasterize(c, w, h, font.stroke as usize).unwrap_or_default();
                (bitmap, w, h, 0.0, 0.0, font.cell_width, false)
            }
            GlyphKey::Font(id, glyph) => {
                let g = font.rasterize(id, glyph);
                (
                    g.bitmap,
                    g.width,
                    g.height,
                    g.left,
                    font.baseline + g.top,
                    g.advance,
                    g.color,
                )
            }
        };

        let info = if w == 0 || h == 0 {
            None
        } else {
            let shelves = if color {
                &mut self.color
            } else {
                &mut self.mask
            };
            let uv = shelves.insert(queue, &bitmap, w as u32, h as u32)?;
            Some(GlyphInfo {
                uv,
                left,
                top,
                width: w as f32,
                height: h as f32,
                advance,
                color,
            })
        };
        self.glyphs.insert(key, info);
        Ok(info)
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    atlas: Atlas,
    pub font: FontSet,
    /// Shape runs of text so the font's ligatures apply.
    pub ligatures: bool,
    /// Opacity of the window background (default-colored cells).
    pub opacity: f32,
    /// How the compositor reads the surface's alpha.
    alpha_mode: wgpu::CompositeAlphaMode,
    srgb: bool,
    needs_reconfigure: bool,
    /// Top-left corner of the pane being built, and how far its grid reaches.
    origin: (f32, f32),
    fill_bottom: f32,
    fill_right: f32,
    /// Column and row pitch of the pane being built. Glyphs are anchored
    /// to the column pitch; box-drawing fills both so blocks meet.
    col_stride: f32,
    row_stride: f32,
    backgrounds: Vec<Instance>,
    foregrounds: Vec<Instance>,
    overlay: Vec<Instance>,
    /// Inline images below (negative z-index) and above the text, each
    /// drawn with its own texture: `(z, image uid, quad)`.
    images_below: Vec<(i32, u64, Instance)>,
    images_above: Vec<(i32, u64, Instance)>,
    image_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    /// Bound while drawing everything that isn't an image.
    no_image: wgpu::BindGroup,
    /// Textures of images on screen, dropped once no longer drawn.
    image_textures: HashMap<u64, ImageTexture>,
    /// window.background_image: drawn over the window background and the
    /// rects passed to `prepare`, under cell backgrounds and text.
    background_image: Option<BackgroundImage>,
    pub background_image_opacity: f32,
    pub background_image_fit: ImageFit,
    /// How many of `backgrounds` are those rects.
    n_rects: usize,
    /// Scratch buffers for shaping runs, reused across frames.
    run_text: String,
    run_cluster_cols: Vec<usize>,
}

struct ImageTexture {
    bind_group: wgpu::BindGroup,
    used: bool,
}

struct BackgroundImage {
    bind_group: wgpu::BindGroup,
    width: f32,
    height: f32,
}

/// Where an image of `image` size is drawn in a `screen` sized window, and
/// the part of it shown: `(x, y, w, h)` and `uv` (origin and size).
fn fit_image(fit: ImageFit, image: (f32, f32), screen: (f32, f32)) -> ([f32; 4], [f32; 4]) {
    let (iw, ih) = image;
    let (sw, sh) = screen;
    match fit {
        ImageFit::Stretch => ([0.0, 0.0, sw, sh], [0.0, 0.0, 1.0, 1.0]),
        ImageFit::Contain => {
            let scale = (sw / iw).min(sh / ih);
            let (w, h) = (iw * scale, ih * scale);
            (
                [((sw - w) / 2.0).round(), ((sh - h) / 2.0).round(), w, h],
                [0.0, 0.0, 1.0, 1.0],
            )
        }
        ImageFit::Cover => {
            let scale = (sw / iw).max(sh / ih);
            let (u, v) = (sw / (iw * scale), sh / (ih * scale));
            ([0.0, 0.0, sw, sh], [(1.0 - u) / 2.0, (1.0 - v) / 2.0, u, v])
        }
    }
}

/// Adjacent cells drawn with one shaping call.
struct Run {
    cells: Vec<(usize, char)>,
    style: Style,
    color: [f32; 4],
}

/// What the renderer needs to know besides the terminal grid.
struct FrameInfo<'a> {
    focused: bool,
    /// The bar cursor is hidden on the off half of its blink. Other shapes
    /// ignore this.
    cursor_on: bool,
    preedit: &'a str,
    marks: &'a [Mark],
    labels: &'a [(Point, String)],
    colors: &'a Colors,
    /// When false, cells draw glyphs only. The caller paints the background,
    /// so a tab's colour stops on the box edge instead of running on into
    /// the next tab through the column grid.
    cell_backgrounds: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MarkKind {
    /// Underlined (a link under the pointer).
    Link,
    /// A search match.
    Match,
    /// The search match being shown.
    Current,
    /// The copy mode cursor.
    Cursor,
}

/// Cells from `start` to `end` (inclusive) drawn highlighted.
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    pub start: Point,
    pub end: Point,
    pub kind: MarkKind,
}

/// One pane to draw: its terminal and where its grid starts.
pub struct PaneView<'a> {
    pub term: &'a Terminal,
    pub origin: (f32, f32),
    /// Whether this pane has keyboard focus (and the window is focused).
    pub focused: bool,
    /// Draw the bar cursor. False on the off half of a blink in the focused pane.
    pub cursor_on: bool,
    pub preedit: &'a str,
    pub marks: &'a [Mark],
    /// Quick select labels drawn over the text, starting at their cell.
    pub labels: &'a [(Point, String)],
    /// Drawn above every other pane and image (a search bar).
    pub overlay: bool,
    /// When false, cells draw glyphs only.
    pub cell_backgrounds: bool,
    /// Filled below this pane's own cells (and above the panes before it).
    pub backdrop: Option<(Rect, Rgb)>,
    /// Rows are spread so the grid ends at this y, and columns so it ends at
    /// `fill_right`. A full-screen program then meets the window edges
    /// instead of leaving the padding and the leftover pixels around it.
    pub fill_bottom: f32,
    pub fill_right: f32,
}

impl PaneView<'_> {
    fn frame<'a>(&'a self, colors: &'a Colors) -> FrameInfo<'a> {
        FrameInfo {
            focused: self.focused,
            cursor_on: self.cursor_on,
            preedit: self.preedit,
            marks: self.marks,
            labels: self.labels,
            colors,
            cell_backgrounds: self.cell_backgrounds,
        }
    }
}

/// Distance from one cell to the next so `count` of them fill `start..end`.
/// Never smaller than the font's cell size, so text is not squashed.
pub fn grid_pitch(start: f32, end: f32, count: usize, cell: f32) -> f32 {
    let span = end - start;
    let count = count.max(1) as f32;
    if span > count * cell {
        span / count
    } else {
        cell
    }
}

fn rgba(c: Rgb) -> [f32; 4] {
    [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
        1.0,
    ]
}

impl Renderer {
    /// Finds an adapter for the window, trying Vulkan / Metal / DX12 first:
    /// initialising the GL backend loads Mesa's GL driver and LLVM (~35 MB
    /// of resident memory) even when Vulkan ends up being used.
    async fn connect(window: &Arc<Window>) -> (wgpu::Surface<'static>, wgpu::Adapter) {
        for backends in [wgpu::Backends::PRIMARY, wgpu::Backends::all()] {
            let mut desc =
                wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone()));
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc.with_env());
            let Ok(surface) = instance.create_surface(window.clone()) else {
                continue;
            };
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: Some(&surface),
                    ..Default::default()
                })
                .await;
            if let Ok(adapter) = adapter {
                return (surface, adapter);
            }
        }
        panic!("no suitable GPU adapter found");
    }

    pub async fn new(window: Arc<Window>, font: FontSet) -> Self {
        let (surface, adapter) = Self::connect(&window).await;
        log::info!("GPU adapter: {:?}", adapter.get_info());

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("kodama device"),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await
            .expect("failed to create device");

        let caps = surface.get_capabilities(&adapter);
        // Blend in sRGB space like other terminals: prefer formats that store
        // our sRGB colors as-is. Float formats are presented as linear.
        let gamma_formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ];
        let format = gamma_formats
            .into_iter()
            .find(|f| caps.formats.contains(f))
            .unwrap_or(caps.formats[0]);
        let linear_output = !gamma_formats.contains(&format);
        log::info!("surface format: {format:?} (linear output: {linear_output})");
        // Blending with what is behind the window needs an alpha-aware mode;
        // it changes nothing while everything drawn is opaque.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(caps.alpha_modes[0]);
        log::info!("surface alpha mode: {alpha_mode:?}");
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let atlas = Atlas::new(&device);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
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
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let bind_group = Self::create_bind_group(
            &device,
            &bind_group_layout,
            &uniform_buffer,
            &atlas,
            &sampler,
        );

        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image bind group layout"),
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
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let no_image = Self::create_image_bind_group(
            &device,
            &queue,
            &image_layout,
            &image_sampler,
            1,
            1,
            &[0; 4],
        );

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(&image_layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
                        3 => Float32x4,
                        4 => Uint32,
                    ],
                })],
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
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let instance_capacity = 4096;
        let instance_buffer = Self::create_instance_buffer(&device, instance_capacity);

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            bind_group,
            uniform_buffer,
            instance_buffer,
            instance_capacity,
            atlas,
            font,
            ligatures: true,
            opacity: 1.0,
            alpha_mode,
            srgb: linear_output,
            needs_reconfigure: false,
            origin: (0.0, 0.0),
            fill_bottom: 0.0,
            fill_right: 0.0,
            col_stride: 0.0,
            row_stride: 0.0,
            backgrounds: Vec::new(),
            foregrounds: Vec::new(),
            overlay: Vec::new(),
            images_below: Vec::new(),
            images_above: Vec::new(),
            image_layout,
            image_sampler,
            no_image,
            image_textures: HashMap::new(),
            background_image: None,
            background_image_opacity: 0.25,
            background_image_fit: ImageFit::Cover,
            n_rects: 0,
            run_text: String::new(),
            run_cluster_cols: Vec::new(),
        }
    }

    fn create_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniforms: &wgpu::Buffer,
        atlas: &Atlas,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let mask_view = atlas
            .mask
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let color_view = atlas
            .color
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&color_view),
                },
            ],
        })
    }

    fn create_image_bind_group(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> wgpu::BindGroup {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// Sets or clears window.background_image from straight-alpha RGBA8.
    pub fn set_background_image(&mut self, image: Option<(Vec<u8>, usize, usize)>) {
        self.background_image = image.map(|(mut rgba, mut w, mut h)| {
            let max = self.device.limits().max_texture_dimension_2d as usize;
            if w > max || h > max {
                let scale = max as f32 / w.max(h) as f32;
                let (dw, dh) = (
                    ((w as f32 * scale) as usize).max(1),
                    ((h as f32 * scale) as usize).max(1),
                );
                rgba = font::downscale_rgba(&rgba, w, h, dw, dh);
                (w, h) = (dw, dh);
            }
            BackgroundImage {
                bind_group: Self::create_image_bind_group(
                    &self.device,
                    &self.queue,
                    &self.image_layout,
                    &self.image_sampler,
                    w as u32,
                    h as u32,
                    &rgba,
                ),
                width: w as f32,
                height: h as f32,
            }
        });
    }

    /// Uploads `img` unless its texture is cached, and marks it in use.
    fn upload_image(&mut self, img: &Image) {
        if let Some(texture) = self.image_textures.get_mut(&img.uid) {
            texture.used = true;
            return;
        }
        let max = self.device.limits().max_texture_dimension_2d;
        let (w, h) = (img.width as usize, img.height as usize);
        let bind_group = if img.width > max || img.height > max {
            let scale = max as f32 / img.width.max(img.height) as f32;
            let (dw, dh) = (
                ((w as f32 * scale) as usize).max(1),
                ((h as f32 * scale) as usize).max(1),
            );
            let pixels = crate::renderer::font::downscale_rgba(&img.rgba, w, h, dw, dh);
            Self::create_image_bind_group(
                &self.device,
                &self.queue,
                &self.image_layout,
                &self.image_sampler,
                dw as u32,
                dh as u32,
                &pixels,
            )
        } else {
            Self::create_image_bind_group(
                &self.device,
                &self.queue,
                &self.image_layout,
                &self.image_sampler,
                img.width,
                img.height,
                &img.rgba,
            )
        };
        self.image_textures.insert(
            img.uid,
            ImageTexture {
                bind_group,
                used: true,
            },
        );
    }

    /// Queues the visible part of every image placed in `term`'s pane.
    fn push_images(&mut self, term: &Terminal) {
        let graphics = term.graphics();
        if graphics.placements.is_empty() {
            return;
        }
        let (cw, ch) = self.cell_size();
        let (ox, oy) = self.origin;
        let stride = grid_pitch(oy, self.fill_bottom, term.rows(), ch);
        let col_stride = grid_pitch(ox, self.fill_right, term.cols(), cw);
        let top = term.abs_line(term.visible_line(0));
        let clip = (
            ox,
            oy,
            ox + term.cols() as f32 * col_stride,
            oy + term.rows() as f32 * stride,
        );
        for p in &graphics.placements {
            let Some(img) = graphics.image(p.image) else {
                continue;
            };
            let (w, h) = p.size((cw, ch));
            let x0 = ox + p.col as f32 * col_stride + p.offset.0 as f32;
            let y0 = oy + (p.line as f64 - top as f64) as f32 * stride + p.offset.1 as f32;
            let (cx0, cy0) = (x0.max(clip.0), y0.max(clip.1));
            let (cx1, cy1) = ((x0 + w).min(clip.2), (y0 + h).min(clip.3));
            if cx0 >= cx1 || cy0 >= cy1 || w <= 0.0 || h <= 0.0 {
                continue;
            }
            let (iw, ih) = (img.width as f32, img.height as f32);
            let [sx, sy, sw, sh] = p.src.map(|v| v as f32);
            let uv = [
                (sx + (cx0 - x0) / w * sw) / iw,
                (sy + (cy0 - y0) / h * sh) / ih,
                (cx1 - cx0) / w * sw / iw,
                (cy1 - cy0) / h * sh / ih,
            ];
            self.upload_image(img);
            let quad = Instance {
                pos: [cx0, cy0],
                size: [cx1 - cx0, cy1 - cy0],
                uv,
                color: [1.0; 4],
                kind: KIND_IMAGE,
            };
            let list = if p.z < 0 {
                &mut self.images_below
            } else {
                &mut self.images_above
            };
            list.push((p.z, img.uid, quad));
        }
    }

    fn create_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_font(&mut self, font: FontSet) {
        self.font = font;
        self.atlas.clear();
    }

    pub fn set_font_size(&mut self, px: f32) {
        self.font.set_size(px);
        self.atlas.clear();
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.font.cell_width, self.font.cell_height)
    }

    pub fn surface_size(&self) -> (f32, f32) {
        (self.config.width as f32, self.config.height as f32)
    }

    /// Grid dimensions that fit in a `w` x `h` pixel area.
    pub fn grid_size(&self, w: f32, h: f32) -> (usize, usize) {
        let (cw, ch) = self.cell_size();
        (
            ((w / cw).floor() as usize).max(1),
            ((h / ch).floor() as usize).max(1),
        )
    }

    /// Build the frame's instance data from the panes' terminal state. This
    /// is the only step that needs the terminals, so callers can release
    /// their locks before [`Renderer::present`] blocks on the swapchain.
    /// `rects` go under every pane; `lines` over the panes' text but under
    /// overlays such as the search bar.
    pub fn prepare(
        &mut self,
        panes: &[PaneView],
        rects: &[(Rect, Rgb)],
        lines: &[(Rect, Rgb)],
        colors: &Colors,
    ) {
        self.image_textures
            .values_mut()
            .for_each(|t| t.used = false);
        let build = |this: &mut Self| -> Result<(), AtlasFull> {
            this.backgrounds.clear();
            this.foregrounds.clear();
            this.images_below.clear();
            this.images_above.clear();
            for &(r, color) in rects {
                let mut c = rgba(color);
                // The focused pane's tint is part of the background.
                if color == colors.active_background {
                    c[3] = this.opacity;
                }
                Self::push_rect(&mut this.backgrounds, r.x, r.y, r.w, r.h, c);
            }
            this.n_rects = this.backgrounds.len();
            let (mut bg, mut fg) = (0, 0);
            for overlay in [false, true] {
                if overlay {
                    (bg, fg) = (this.backgrounds.len(), this.foregrounds.len());
                }
                for pane in panes.iter().filter(|p| p.overlay == overlay) {
                    if let Some((r, color)) = pane.backdrop {
                        Self::push_rect(&mut this.backgrounds, r.x, r.y, r.w, r.h, rgba(color));
                    }
                    this.origin = pane.origin;
                    this.fill_bottom = pane.fill_bottom;
                    this.fill_right = pane.fill_right;
                    this.build(pane.term, &pane.frame(colors))?;
                }
            }
            this.overlay.clear();
            for &(r, color) in lines {
                Self::push_rect(&mut this.overlay, r.x, r.y, r.w, r.h, rgba(color));
            }
            this.overlay.extend(this.backgrounds.drain(bg..));
            this.overlay.extend(this.foregrounds.drain(fg..));
            Ok(())
        };
        // Each failure grows an atlas (at most a few times) or, once both are
        // at full size, starts the cache over.
        let mut cleared = false;
        while build(self).is_err() {
            if self.atlas.grow(&self.device) {
                self.bind_group = Self::create_bind_group(
                    &self.device,
                    &self.bind_group_layout,
                    &self.uniform_buffer,
                    &self.atlas,
                    &self.sampler,
                );
            } else if !cleared {
                log::info!("glyph atlas full, rebuilding");
                self.atlas.clear();
                cleared = true;
            } else {
                log::warn!("glyph atlas too small for the current screen");
                break;
            }
        }
        self.image_textures.retain(|_, t| t.used);
        self.images_below.sort_by_key(|(z, ..)| *z);
        self.images_above.sort_by_key(|(z, ..)| *z);
    }

    /// Draw the prepared frame. Returns false if no frame was presented and
    /// the caller should try again.
    pub fn present(&mut self, colors: &Colors) -> bool {
        // Instance buffer layout: backgrounds, images below the text, text
        // and decorations, images above the text, overlays.
        let (n_bg, n_below, n_fg) = (
            self.backgrounds.len(),
            self.images_below.len(),
            self.foregrounds.len(),
        );
        let n_above = self.images_above.len();
        let backdrop_at = n_bg + n_below + n_fg + n_above + self.overlay.len();
        let backdrop = self.background_image.as_ref().map(|image| {
            let (rect, uv) = fit_image(
                self.background_image_fit,
                (image.width, image.height),
                (self.config.width as f32, self.config.height as f32),
            );
            Instance {
                pos: [rect[0], rect[1]],
                size: [rect[2], rect[3]],
                uv,
                color: [1.0, 1.0, 1.0, self.background_image_opacity.clamp(0.0, 1.0)],
                kind: KIND_IMAGE,
            }
        });
        let total = backdrop_at + usize::from(backdrop.is_some());
        if total > self.instance_capacity {
            self.instance_capacity = total.next_power_of_two();
            self.instance_buffer =
                Self::create_instance_buffer(&self.device, self.instance_capacity);
        }
        let stride = std::mem::size_of::<Instance>() as u64;
        let write = |offset: usize, data: &[Instance]| {
            if !data.is_empty() {
                self.queue.write_buffer(
                    &self.instance_buffer,
                    offset as u64 * stride,
                    bytemuck::cast_slice(data),
                );
            }
        };
        let quads =
            |images: &[(i32, u64, Instance)]| images.iter().map(|(.., q)| *q).collect::<Vec<_>>();
        write(0, &self.backgrounds);
        write(n_bg, &quads(&self.images_below));
        write(n_bg + n_below, &self.foregrounds);
        write(n_bg + n_below + n_fg, &quads(&self.images_above));
        write(n_bg + n_below + n_fg + n_above, &self.overlay);
        if let Some(backdrop) = backdrop {
            write(backdrop_at, &[backdrop]);
        }
        let uniforms = Uniforms {
            screen_size: [self.config.width as f32, self.config.height as f32],
            srgb: if self.srgb { 1.0 } else { 0.0 },
            _pad: 0.0,
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.needs_reconfigure = true;
                t
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            other => {
                log::debug!("skipping frame: {other:?}");
                return false;
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut bg = rgba(colors.background);
        if self.srgb {
            bg.iter_mut().take(3).for_each(|c| *c = c.powf(2.2));
        }
        let alpha = self.opacity.clamp(0.0, 1.0);
        match self.alpha_mode {
            wgpu::CompositeAlphaMode::PreMultiplied => {
                bg = [bg[0] * alpha, bg[1] * alpha, bg[2] * alpha, alpha]
            }
            wgpu::CompositeAlphaMode::PostMultiplied => bg[3] = alpha,
            _ => {}
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if total > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_bind_group(1, &self.no_image, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                let draw_images =
                    |pass: &mut wgpu::RenderPass, images: &[(i32, u64, Instance)], first: usize| {
                        for (i, (_, uid, _)) in images.iter().enumerate() {
                            if let Some(texture) = self.image_textures.get(uid) {
                                let at = (first + i) as u32;
                                pass.set_bind_group(1, &texture.bind_group, &[]);
                                pass.draw(0..4, at..at + 1);
                            }
                        }
                    };
                if let Some(image) = &self.background_image {
                    let n_rects = self.n_rects.min(n_bg) as u32;
                    pass.draw(0..4, 0..n_rects);
                    pass.set_bind_group(1, &image.bind_group, &[]);
                    pass.draw(0..4, backdrop_at as u32..backdrop_at as u32 + 1);
                    pass.set_bind_group(1, &self.no_image, &[]);
                    pass.draw(0..4, n_rects..n_bg as u32);
                } else {
                    pass.draw(0..4, 0..n_bg as u32);
                }
                draw_images(&mut pass, &self.images_below, n_bg);
                pass.set_bind_group(1, &self.no_image, &[]);
                let fg_start = (n_bg + n_below) as u32;
                pass.draw(0..4, fg_start..fg_start + n_fg as u32);
                draw_images(&mut pass, &self.images_above, n_bg + n_below + n_fg);
                if !self.overlay.is_empty() {
                    pass.set_bind_group(1, &self.no_image, &[]);
                    let start = (n_bg + n_below + n_fg + n_above) as u32;
                    pass.draw(0..4, start..start + self.overlay.len() as u32);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(surface_texture);
        if std::mem::take(&mut self.needs_reconfigure) {
            self.surface.configure(&self.device, &self.config);
        }
        true
    }

    fn resolve_colors(cell: &Cell, colors: &Colors) -> (Rgb, Rgb) {
        let resolve = |c: Color, default: Rgb, bold: bool| match c {
            Color::Default => default,
            Color::Indexed(i) if bold && i < 8 => colors.indexed(i + 8),
            Color::Indexed(i) => colors.indexed(i),
            Color::Rgb(r, g, b) => [r, g, b],
        };
        let bold = cell.flags() & flags::BOLD != 0;
        let mut fg = resolve(cell.fg, colors.foreground, bold);
        let mut bg = resolve(cell.bg, colors.background, false);
        if cell.flags() & flags::INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags() & flags::DIM != 0 {
            fg = fg.map(|v| (v as f32 * 0.66) as u8);
        }
        (fg, bg)
    }

    fn push_rect(list: &mut Vec<Instance>, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        list.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv: [0.0; 4],
            color,
            kind: KIND_SOLID,
        });
    }

    /// Draw an atlas glyph whose origin (pen position at the cell's top-left)
    /// is `(x, y)`.
    fn push_key(
        &mut self,
        key: GlyphKey,
        x: f32,
        y: f32,
        color: [f32; 4],
    ) -> Result<(), AtlasFull> {
        if let Some(g) = self.atlas.get(&self.queue, &self.font, key)? {
            self.foregrounds.push(Instance {
                pos: [x + g.left, y + g.top],
                size: [g.width, g.height],
                uv: g.uv,
                color,
                kind: if g.color {
                    KIND_COLOR_GLYPH
                } else {
                    KIND_GLYPH
                },
            });
        }
        Ok(())
    }

    /// Draw a single character in the cell at `(x, y)` without shaping.
    /// `slot` is the cell's pitch. Box-drawing and block elements are
    /// stretched to it: the bitmap is the font's cell size, and the extra
    /// fraction of a pixel that pads the grid out to the pane would
    /// otherwise show as a line between blocks (a bar visualiser).
    fn push_char(
        &mut self,
        c: char,
        style: Style,
        x: f32,
        y: f32,
        slot: (f32, f32),
        color: [f32; 4],
    ) -> Result<(), AtlasFull> {
        if boxdraw::is_builtin(c) {
            let key = GlyphKey::Builtin(c);
            if let Some(g) = self.atlas.get(&self.queue, &self.font, key)? {
                self.foregrounds.push(Instance {
                    pos: [x + g.left, y + g.top],
                    size: [slot.0.max(g.width), slot.1.max(g.height)],
                    uv: g.uv,
                    color,
                    kind: KIND_GLYPH,
                });
            }
            return Ok(());
        }
        let (font, glyph) = self.font.resolve(c, style);
        let key = GlyphKey::Font(font, glyph);
        // Centre double-width glyphs (CJK fonts are often narrower than two cells).
        let cells = c.width().unwrap_or(1).max(1) as f32;
        let x = match self.atlas.get(&self.queue, &self.font, key)? {
            Some(g) if cells > 1.0 => {
                x + ((cells * self.font.cell_width - g.advance) / 2.0)
                    .max(0.0)
                    .round()
            }
            _ => x,
        };
        self.push_key(key, x, y, color)
    }

    /// Draw a multi-char cluster (emoji sequence, combining marks) in the
    /// `width` pixels of its cell, centred when it is one picture.
    fn push_cluster(
        &mut self,
        text: &str,
        style: Style,
        x: f32,
        y: f32,
        width: f32,
        color: [f32; 4],
    ) -> Result<(), AtlasFull> {
        let (font, glyphs) = self.font.shape_cluster(text, style);
        if let [g] = glyphs[..] {
            let key = GlyphKey::Font(font, g.glyph);
            let x = match self.atlas.get(&self.queue, &self.font, key)? {
                Some(info) => x + ((width - info.advance) / 2.0).max(0.0).round(),
                None => x,
            };
            return self.push_key(key, x, y, color);
        }
        let mut pen = 0.0;
        for g in glyphs.iter() {
            self.push_key(
                GlyphKey::Font(font, g.glyph),
                x + pen + g.x_offset,
                y - g.y_offset,
                color,
            )?;
            pen += g.x_advance;
        }
        Ok(())
    }

    /// Draw a run of adjacent cells sharing style and color. Runs of two or
    /// more characters are shaped so the font's ligatures apply; every glyph
    /// stays anchored to the cell of the character it starts at.
    fn push_run(&mut self, run: &Run, row_y: f32) -> Result<(), AtlasFull> {
        let (cw, ox) = (self.col_stride, self.origin.0);
        if let [(col, c)] = run.cells[..] {
            return self.push_char(
                c,
                run.style,
                ox + col as f32 * cw,
                row_y,
                (cw, self.row_stride),
                run.color,
            );
        }

        let mut text = std::mem::take(&mut self.run_text);
        let mut cluster_cols = std::mem::take(&mut self.run_cluster_cols);
        text.clear();
        cluster_cols.clear();
        for &(col, c) in &run.cells {
            text.push(c);
            cluster_cols.resize(text.len(), col);
        }
        let (font, glyphs) = self.font.shape(&text, run.style);
        self.run_text = text;
        let result = self.push_shaped(run, font, &glyphs, &cluster_cols, row_y);
        self.run_cluster_cols = cluster_cols;
        result
    }

    fn push_shaped(
        &mut self,
        run: &Run,
        font: FontId,
        glyphs: &[ShapedGlyph],
        cluster_cols: &[usize],
        row_y: f32,
    ) -> Result<(), AtlasFull> {
        let (cw, ox) = (self.col_stride, self.origin.0);
        for g in glyphs {
            let col = cluster_cols
                .get(g.cluster as usize)
                .copied()
                .unwrap_or(run.cells[0].0);
            let x = ox + col as f32 * cw + g.x_offset;
            self.push_key(
                GlyphKey::Font(font, g.glyph),
                x,
                row_y - g.y_offset,
                run.color,
            )?;
        }
        Ok(())
    }

    fn flush_run(&mut self, run: &mut Run, row_y: f32) -> Result<(), AtlasFull> {
        if !run.cells.is_empty() {
            self.push_run(run, row_y)?;
            run.cells.clear();
        }
        Ok(())
    }

    fn build(&mut self, term: &Terminal, frame: &FrameInfo) -> Result<(), AtlasFull> {
        let colors = frame.colors;
        let (cw, ch) = self.cell_size();
        let (ox, oy) = self.origin;
        let stride = grid_pitch(oy, self.fill_bottom, term.rows(), ch);
        self.row_stride = stride;
        self.col_stride = grid_pitch(ox, self.fill_right, term.cols(), cw);
        let col_stride = self.col_stride;
        let stroke = self.font.stroke;
        let underline_y = self.font.underline_offset;
        let strike_y = (self.font.baseline - self.font.cell_height * 0.3).round();

        let (cursor_x, cursor_y) = term.cursor_position();
        let cursor_line = term.history_len() + cursor_y;
        let show_cursor = term.modes.show_cursor && frame.preedit.is_empty();
        let block_cursor = show_cursor && frame.focused && term.cursor_shape == CursorShape::Block;
        let selection = term.selection;
        self.push_images(term);
        let mut run = Run {
            cells: Vec::new(),
            style: Style::Regular,
            color: [0.0; 4],
        };

        for row in 0..term.rows() {
            let line = term.visible_line(row);
            let cells = term.line(line);
            let y = oy + row as f32 * stride;
            for col in 0..term.cols() {
                let cell = cells.get(col).copied().unwrap_or_default();
                if cell.flags() & flags::WIDE_SPACER != 0 {
                    continue;
                }
                let span = if cell.flags() & flags::WIDE != 0 {
                    2
                } else {
                    1
                };
                let x = ox + col as f32 * col_stride;
                // The last column reaches `fill_right`, so the row meets the
                // pane's right edge the way the last row meets the bottom.
                let width = if col + span >= term.cols() {
                    (self.fill_right - x).max(col_stride * span as f32)
                } else {
                    col_stride * span as f32
                };
                let (mut fg, mut bg) = Self::resolve_colors(&cell, colors);

                let point = Point { line, col };
                let mut link = false;
                let mut found = None;
                for m in frame
                    .marks
                    .iter()
                    .filter(|m| m.start <= point && point <= m.end)
                {
                    match m.kind {
                        MarkKind::Link => link = true,
                        kind => found = found.max(Some(kind)),
                    }
                }
                if let Some(kind) = found {
                    bg = match kind {
                        MarkKind::Cursor => colors.cursor,
                        MarkKind::Current => colors.search_current,
                        _ => colors.search_match,
                    };
                    fg = colors.background;
                }
                let selected = selection.is_some_and(|s| s.contains(point));
                if selected && found != Some(MarkKind::Cursor) {
                    bg = colors.selection;
                }
                // Each row's background fills the pitch, so the grid reaches
                // `fill_bottom` with no strip of window colour between rows.
                // A block cursor stays one cell tall on top of that.
                let line_bg = bg;
                let bg_h = if row + 1 == term.rows() {
                    (self.fill_bottom - y).max(stride)
                } else {
                    stride
                };
                if block_cursor && line == cursor_line && col == cursor_x {
                    bg = colors.cursor;
                    fg = colors.background;
                }
                if frame.cell_backgrounds {
                    if line_bg != colors.background || selected || found.is_some() {
                        Self::push_rect(&mut self.backgrounds, x, y, width, bg_h, rgba(line_bg));
                    }
                    if bg != line_bg {
                        Self::push_rect(&mut self.backgrounds, x, y, width, ch, rgba(bg));
                    }
                }

                let hidden = cell.flags() & flags::HIDDEN != 0;
                let color = rgba(fg);
                if hidden || cell.c() == ' ' {
                    self.flush_run(&mut run, y)?;
                } else if let Some(text) = cell.cluster() {
                    self.flush_run(&mut run, y)?;
                    let style = Style::new(
                        cell.flags() & flags::BOLD != 0,
                        cell.flags() & flags::ITALIC != 0,
                    );
                    self.push_cluster(text, style, x, y, width, color)?;
                } else {
                    let style = Style::new(
                        cell.flags() & flags::BOLD != 0,
                        cell.flags() & flags::ITALIC != 0,
                    );
                    let shapeable = self.ligatures
                        && cell.flags() & flags::WIDE == 0
                        && !boxdraw::is_builtin(cell.c())
                        && self.font.has_char(cell.c(), style);
                    if shapeable {
                        if run.style != style || run.color != color {
                            self.flush_run(&mut run, y)?;
                        }
                        run.style = style;
                        run.color = color;
                        run.cells.push((col, cell.c()));
                    } else {
                        self.flush_run(&mut run, y)?;
                        self.push_char(cell.c(), style, x, y, (width, bg_h), color)?;
                    }
                }
                if hidden {
                    continue;
                }
                if cell.flags() & flags::UNDERLINE != 0 || link {
                    Self::push_rect(
                        &mut self.foregrounds,
                        x,
                        y + underline_y,
                        width,
                        stroke,
                        color,
                    );
                }
                if cell.flags() & flags::STRIKE != 0 {
                    Self::push_rect(&mut self.foregrounds, x, y + strike_y, width, stroke, color);
                }
            }
            self.flush_run(&mut run, y)?;
        }

        // Cursor shapes other than the focused block (which is drawn inline above).
        let cursor_row = cursor_line as isize - term.visible_line(0) as isize;
        if show_cursor && !block_cursor && (0..term.rows() as isize).contains(&cursor_row) {
            let x = ox + cursor_x as f32 * col_stride;
            let y = oy + cursor_row as f32 * stride;
            let color = rgba(colors.cursor);
            let thick = (stroke * 2.0).max(2.0);
            // The bar is a little narrower and shorter than the cell, and
            // blinks only while this pane has focus.
            let bar_w = (stroke * 1.5).max(1.0);
            let inset = (ch * 0.1).max(1.0);
            let bar_h = (ch - inset * 2.0).max(bar_w);
            let fg = &mut self.foregrounds;
            match (frame.focused, term.cursor_shape) {
                (_, CursorShape::Bar) if frame.focused && !frame.cursor_on => {}
                (_, CursorShape::Bar) => Self::push_rect(fg, x, y + inset, bar_w, bar_h, color),
                (true, CursorShape::Underline) => {
                    Self::push_rect(fg, x, y + ch - thick, col_stride, thick, color)
                }
                _ => {
                    Self::push_rect(fg, x, y, col_stride, stroke, color);
                    Self::push_rect(fg, x, y + ch - stroke, col_stride, stroke, color);
                    Self::push_rect(fg, x, y, stroke, ch, color);
                    Self::push_rect(fg, x + col_stride - stroke, y, stroke, ch, color);
                }
            }
        }

        let top = term.visible_line(0);
        for (p, label) in frame.labels {
            if p.line < top || p.line >= top + term.rows() {
                continue;
            }
            let y = oy + (p.line - top) as f32 * stride;
            for (i, c) in label.chars().enumerate() {
                let x = ox + (p.col + i) as f32 * col_stride;
                Self::push_rect(
                    &mut self.foregrounds,
                    x,
                    y,
                    col_stride,
                    ch,
                    rgba(colors.brights[1]),
                );
                self.push_char(
                    c,
                    Style::Bold,
                    x,
                    y,
                    (col_stride, stride),
                    rgba(colors.background),
                )?;
            }
        }

        // IME pre-edit text, drawn over the grid at the cursor.
        if !frame.preedit.is_empty() && term.display_offset == 0 {
            let mut x = ox + cursor_x as f32 * col_stride;
            let y = oy + cursor_y as f32 * stride;
            let color = rgba(colors.foreground);
            for c in frame.preedit.chars() {
                let w = c.width().unwrap_or(0) as f32 * col_stride;
                if w == 0.0 {
                    continue;
                }
                Self::push_rect(&mut self.backgrounds, x, y, w, ch, rgba(colors.selection));
                self.push_char(c, Style::Regular, x, y, (w, stride), color)?;
                Self::push_rect(&mut self.foregrounds, x, y + underline_y, w, stroke, color);
                x += w;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_image_fits() {
        let wide = (200.0, 100.0);
        let screen = (100.0, 100.0);
        assert_eq!(
            fit_image(ImageFit::Stretch, wide, screen),
            ([0.0, 0.0, 100.0, 100.0], [0.0, 0.0, 1.0, 1.0])
        );
        // Cover shows the middle half of a 2:1 image in a square window.
        assert_eq!(
            fit_image(ImageFit::Cover, wide, screen),
            ([0.0, 0.0, 100.0, 100.0], [0.25, 0.0, 0.5, 1.0])
        );
        // Contain letterboxes it: 100x50, centred vertically.
        assert_eq!(
            fit_image(ImageFit::Contain, wide, screen),
            ([0.0, 25.0, 100.0, 50.0], [0.0, 0.0, 1.0, 1.0])
        );
    }
}
