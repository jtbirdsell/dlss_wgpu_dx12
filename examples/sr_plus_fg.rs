//! DLSS **Super Resolution (raw NGX)** AND **Frame Generation (Streamline)** together.
//!
//! This drives the full combined DLSS pipeline over a wgpu-owned DX12 swapchain, using only the
//! crate's **public** API for both halves:
//!   * NGX Super Resolution ([`DlssSdk`] + [`DlssContext`] + [`DlssRenderParameters`]) upscales a
//!     low-resolution scene to the output resolution, and
//!   * Streamline Frame Generation ([`Streamline`] + [`FrameGenerationContext`] + [`Frame`])
//!     interpolates an extra frame between each pair of rendered frames.
//!
//! It is the union of the two existing examples (`super_resolution.rs` headless SR + the
//! interactive `frame_generation.rs`), wired so that **one set of per-frame constants drives both
//! features**: the FG common-constants are derived from the *same* [`DlssRenderParameters`] used for
//! the SR evaluate via [`FgConstants::from_render_parameters`] (the new SR->FG bridge). That keeps
//! jitter, motion-vector scale, and history-reset identical across SR and FG — a notorious source of
//! drift when the two are integrated independently (NGX SR reads motion vectors in render-resolution
//! *pixels*, while Streamline FG normalizes them to `[-1, 1]`; the bridge converts the convention so
//! the single shared mvec buffer is interpreted consistently by both).
//!
//! Like `frame_generation.rs`, it opens a visible window, scrolls a bright vertical bar at a constant
//! velocity (genuine, consistent motion both features need), drives ~300 frames, polls
//! `slDLSSGGetState` every 30 frames, and prints a final line reporting the maximum
//! `numFramesActuallyPresented` it observed — `2` means DLSS-G generated an interpolated frame
//! between each pair of (already SR-upscaled) frames. It compiles in CI but can only be *verified*
//! on an RTX GPU + display.
//!
//! ## The SR -> FG handoff (the load-bearing part of this example)
//!
//! Per frame the data flows:
//!   1. Draw the scrolling bar at **render** resolution into the SR color input; fill depth + mvec.
//!   2. NGX SR upscales `SR color (render-res)` -> `SR output (output-res, a UAV / STORAGE texture)`.
//!   3. **Blit** the SR output -> the FG **hud-less** color (output-res). This fullscreen-triangle
//!      pass also *format-converts* `Rgba8Unorm` (UAV-capable, what DLSS wrote) -> `Bgra8Unorm` (the
//!      swapchain back-buffer format DLSS-G composites against). Sampling the SR output inside this
//!      render pass auto-transitions it `STORAGE/UAV` -> `RESOURCE` (shader-read) via the bind group.
//!   4. **Copy** the hud-less color -> the acquired back buffer (both `Bgra8Unorm`, output-res), so
//!      the upscaled image is what actually reaches the screen.
//!   5. Tag the FG inputs (depth + mvec at **render** res — "same as DLSS-SR"; hud-less + UI at
//!      **output** res, matching the back buffer) on a dedicated raw-only encoder.
//!   6. Submit `[render_encoder, tag_encoder]` in order, end the render phase, and present. DLSS-G
//!      interpolates using the tagged hud-less color + depth + mvec at `Present`.
//!
//! Note the deliberate resolution split: **depth & motion vectors stay at render resolution** (DLSS-G
//! is happy to consume them at the same resolution DLSS-SR did), while **hud-less color & UI are at
//! output resolution** because they must match the swapchain back buffer's size and format.
//!
//! ## RUN REQUIREMENTS (read before running) — identical to `frame_generation.rs`, plus SR's DLL
//!
//! 1. **`STREAMLINE_SDK` env var** must point at the Streamline SDK root so the crate can locate and
//!    signature-verify `sl.interposer.dll`. (Building also needs `DLSS_SDK` + `LIBCLANG_PATH`; those
//!    are build-time only.)
//! 2. **Stage these DLLs next to the example exe** (`target/debug/examples/`):
//!    `sl.interposer.dll`, `sl.common.dll`, `sl.dlss_g.dll`, `sl.reflex.dll`, `sl.pcl.dll`,
//!    `nvngx_dlssg.dll` (Frame Generation) **and `nvngx_dlss.dll`** (Super Resolution — required here
//!    because this example also runs raw NGX SR; `frame_generation.rs` did not need it). Without them
//!    `slInit` / DLSS-G load or the NGX SR feature fails.
//! 3. **The window must stay visible + focused / composited** while it runs. DLSS-G silently
//!    *declines* to present generated frames to a window that is not actually composited — do not
//!    minimize it or cover it. (`with_visible(true).with_active(true)` + `focus_window()`.)
//! 4. A **non-vsync present mode** (`Mailbox`, else `Immediate`, else `Fifo`) so Reflex/DLSS-G own
//!    frame pacing.
//! 5. Build/run with **both** features enabled:
//!
//!    ```powershell
//!    $env:DLSS_SDK = 'C:/Users/jorda/dlss_sdk'
//!    $env:LIBCLANG_PATH = 'C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//!    $env:STREAMLINE_SDK = 'C:/path/to/streamline'   # so the interposer can be found at runtime
//!    cargo run --example sr_plus_fg --features "super-resolution frame-generation"
//!    ```
//!
//! ## Ordering requirements (enforced by the API)
//!   * [`Streamline::init`] runs **before** `wgpu::Instance::new` (so the wgpu fork upgrades its DXGI
//!     factory to a Streamline proxy).
//!   * [`DlssSdk::new`] (NGX) runs **after** the device is created.
//!   * [`FrameGenerationContext::new`] runs **after** the device but **before** `surface.configure()`
//!     (so `slSetD3DDevice` precedes swapchain creation).
//!   * [`DlssContext::new`] (the SR feature) is created after the surface is configured.

use std::sync::Arc;

use dlss_wgpu_dx12::{
    DlssContext, DlssError, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode,
    DlssRenderParameters, DlssSdk, DlssTexture, FgConstants, FgResource, FgResources, FgUi,
    FrameGenerationContext, FrameGenerationOptions, FrameGenerationState, Streamline,
    StreamlineError,
};
use glam::{UVec2, Vec2};
use uuid::Uuid;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// A stable identifier for the NGX (Super Resolution) side; NGX uses it to look up DLSS overrides.
const PROJECT_ID: Uuid = Uuid::from_u128(0x9b8d_2f41_6c7a_4e15_8d3b_a0f2_71e4_55c9);

/// OUTPUT (swapchain / back-buffer) resolution. SR upscales *to* this; the FG hud-less + UI buffers
/// and the back buffer are all this size. A common 1080p target.
const OUTPUT_WIDTH: u32 = 1920;
const OUTPUT_HEIGHT: u32 = 1080;

/// How many frames to drive before exiting.
const MAX_FRAMES: u32 = 300;
/// Poll `slDLSSGGetState` (and print a status line) every this many frames.
const STATE_POLL_INTERVAL: u32 = 30;

/// Horizontal velocity of the bar, in **render-resolution** pixels per frame. Both features need
/// genuine, consistent motion; a static scene makes DLSS-G decline to generate.
const VELOCITY: f32 = 8.0;
/// Half-width (in render-resolution pixels) of the bright vertical bar (~240px-wide bar total).
const BAR_HALF_WIDTH: f32 = 120.0;

/// The motion vector we write uniformly into the mvec texture, in **render-resolution** pixels, using
/// the DLSS convention: per-pixel displacement from the CURRENT frame to the PREVIOUS frame. The bar
/// translates +VELOCITY px/frame in +X, so a surface point's previous position is VELOCITY px to the
/// LEFT of its current position => mvec = (-VELOCITY, 0). Both SR and FG read this SAME buffer; the
/// FG side normalizes it to `[-1, 1]` via `FgConstants::from_render_parameters` (which divides the
/// SR `motion_vector_scale`, default `(1, 1)`, by the render resolution). Rg16Float maps the clear
/// color r->x (motion.x), g->y (motion.y).
const MVEC_X: f64 = -(VELOCITY as f64);
const MVEC_Y: f64 = 0.0;

/// Inline WGSL: a fullscreen-triangle vertex shader + a fragment shader that lights pixels inside a
/// moving ~240px-wide vertical bar. The bar X position (`x_offset_px`) advances every frame on the
/// CPU and is delivered through a 16-byte uniform (`vec4<f32> = {x_offset_px, width, height, pad}`).
/// This draws the SR *input* scene at render resolution.
const BAR_WGSL: &str = r#"
struct Params {
    data: vec4<f32>, // x = x_offset_px, y = render_width, z = render_height, w = pad
};
@group(0) @binding(0) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Fullscreen triangle covering the whole clip space (-1..3).
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = p[vid];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // uv in [0,1]: (clip + 1) / 2, with V flipped so uv.y=0 is the top scanline.
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let width = params.data.y;
    let x_off = params.data.x;
    let px = in.uv.x * width;                  // pixel X of this fragment (render-res)
    let bar = abs(px - x_off) < BAR_HALF_WIDTH; // ~240px-wide vertical bar
    if (bar) {
        return vec4<f32>(0.95, 0.85, 0.10, 1.0); // bright bar
    }
    return vec4<f32>(0.02, 0.02, 0.04, 1.0);     // dark background
}
"#;

/// Inline WGSL: a fullscreen-triangle pass that samples the SR output texture and writes it out. Used
/// to blit the SR output (`Rgba8Unorm`, the UAV DLSS wrote to) into the FG hud-less color
/// (`Bgra8Unorm`, the back-buffer format). Both are at OUTPUT resolution, so this is a 1:1 sample
/// that also performs the Rgba->Bgra format conversion the swapchain needs.
const BLIT_WGSL: &str = r#"
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var src: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = p[vid];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}
"#;

/// A combined error so a single per-frame `render()` can propagate both halves of the pipeline: the
/// NGX SR path returns [`DlssError`], the Streamline FG path returns [`StreamlineError`].
enum DemoError {
    Sr(DlssError),
    Fg(StreamlineError),
}

impl From<DlssError> for DemoError {
    fn from(e: DlssError) -> Self {
        DemoError::Sr(e)
    }
}

impl From<StreamlineError> for DemoError {
    fn from(e: StreamlineError) -> Self {
        DemoError::Fg(e)
    }
}

impl std::fmt::Display for DemoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DemoError::Sr(e) => write!(f, "DLSS Super Resolution error: {e}"),
            DemoError::Fg(e) => write!(f, "DLSS Frame Generation error: {e}"),
        }
    }
}

/// Everything that exists after the wgpu device is up and both DLSS features are bound.
struct GpuState {
    window: Arc<Window>,
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    // --- Resolutions ---
    /// Output / swapchain resolution (SR upscales to this; FG buffers + back buffer are this size).
    output_resolution: UVec2,
    /// SR render (input) resolution, read from `DlssContext::render_resolution()`.
    render_resolution: UVec2,

    // --- SR (NGX) textures ---
    /// SR color input: render-res, Rgba8Unorm. The scrolling scene is drawn here.
    sr_color: wgpu::Texture,
    /// SR + FG depth: render-res, R32Float. Shared by the SR evaluate and the FG tag (render-res).
    depth_tex: wgpu::Texture,
    /// SR + FG motion vectors: render-res, Rg16Float. Shared buffer; SR reads pixels, FG normalizes.
    mvec_tex: wgpu::Texture,
    /// SR output (UAV): output-res, Rgba8Unorm (Rgba8Unorm IS UAV-capable; Bgra8Unorm is NOT). DLSS
    /// writes the upscaled image here as a STORAGE texture.
    sr_output: wgpu::Texture,

    // --- FG (Streamline) textures (output-res, matching the back buffer) ---
    /// FG hud-less color: output-res, **surface format (Bgra8Unorm)**. The blit writes the upscaled
    /// SR output here; DLSS-G interpolates from it. COPY_SRC so we can also copy it to the back buffer.
    hudless_tex: wgpu::Texture,
    /// FG UI color+alpha: output-res, Rgba8Unorm. Cleared fully transparent (no UI in this demo).
    ui_tex: wgpu::Texture,

    // --- Moving-bar render pipeline (draws the SR color input at render-res) ---
    /// Per-frame uniform (`vec4<f32>`): {x_offset_px, render_width, render_height, pad}.
    bar_uniform: wgpu::Buffer,
    bar_bind_group: wgpu::BindGroup,
    /// Pipeline whose color target is the SR color input's format (Rgba8Unorm).
    bar_pipeline: wgpu::RenderPipeline,

    // --- Blit pipeline (SR output Rgba8Unorm -> hud-less Bgra8Unorm) ---
    blit_pipeline: wgpu::RenderPipeline,
    blit_bind_group: wgpu::BindGroup,

    /// The crate's per-camera SR feature (raw NGX). Upscales render-res -> output-res.
    sr_ctx: DlssContext,
    /// The crate's per-camera DLSS-G feature. Owns the Streamline core API; `slShutdown` on `Drop`.
    fg: FrameGenerationContext,

    /// Next frame index to drive.
    frame_index: u32,
    /// Max `numFramesActuallyPresented` observed across the run (the success metric: 2 = generating).
    max_presented: u32,
    /// Last `numFramesActuallyPresented` observed.
    last_presented: u32,
}

impl GpuState {
    /// Builds the wgpu device/surface and BOTH DLSS features in the proven order.
    ///
    /// `streamline` is the already-initialized [`Streamline`] handle (created **before** the
    /// `wgpu::Instance` in `main`). On success the FG context *moves* the Streamline core API out of
    /// this handle (the handle becomes inert).
    fn new(window: Arc<Window>, streamline: &mut Streamline) -> Result<Self, DemoError> {
        // wgpu::Instance is created AFTER Streamline::init (done in main) so the wgpu fork's
        // Instance::init upgrades the DXGI factory to a Streamline proxy. Creating the instance first
        // would mean DLSS-G can never bind to wgpu's swapchain.
        let mut inst_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        inst_desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(inst_desc);

        let surface = instance
            .create_surface(window.clone())
            .expect("create_surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("request HighPerformance DX12 adapter");

        let info = adapter.get_info();
        println!(
            "Adapter: {} ({:?}, backend={:?})",
            info.name, info.device_type, info.backend
        );

        // TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES lets the SR output texture expose read-write
        // storage (UAV) usage — DLSS writes the upscaled image into it as a STORAGE texture.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("sr_plus_fg device"),
            required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("request_device");

        // --- Choose the surface format (prefer Bgra8Unorm) and present mode. ---
        let caps = surface.get_capabilities(&adapter);
        let surface_format = if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
            wgpu::TextureFormat::Bgra8Unorm
        } else {
            caps.formats[0]
        };
        println!("Surface format: {surface_format:?}");

        // A NON-VSYNC present mode so Reflex/DLSS-G own frame pacing (hard vsync can suppress
        // interpolation). Prefer Mailbox, else Immediate, else Fifo (always supported).
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        println!("Present mode: {present_mode:?}");

        let output_resolution = UVec2::new(OUTPUT_WIDTH, OUTPUT_HEIGHT);
        let config = wgpu::SurfaceConfiguration {
            // RENDER_ATTACHMENT for normal use + COPY_DST so we can copy the hud-less color onto it.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format: surface_format,
            width: OUTPUT_WIDTH,
            height: OUTPUT_HEIGHT,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };

        // -----------------------------------------------------------------------------------------
        // CREATE THE DLSS-G CONTEXT *AFTER* THE DEVICE BUT *BEFORE* surface.configure().
        //
        // `FrameGenerationContext::new` runs `slSetD3DDevice(raw ID3D12Device)`, which Streamline
        // requires before any swapchain exists. wgpu creates the swapchain inside `surface.configure`
        // below; if the device is not yet registered, DLSS-G never binds.
        // `.with_color_format(surface_format)` tells SL the DXGI format of the back buffer + hud-less.
        // -----------------------------------------------------------------------------------------
        let fg_options = FrameGenerationOptions::enabled().with_color_format(surface_format);
        let fg = FrameGenerationContext::new(streamline, &device, &adapter, &fg_options)?;
        println!(
            "DLSS-G enabled (mode On, numFramesToGenerate=1, color_format from {surface_format:?})"
        );

        // NOW create the swapchain (SL's device registration is in place).
        surface.configure(&device, &config);

        // -----------------------------------------------------------------------------------------
        // SUPER RESOLUTION (NGX): init the SDK, then create the SR context at OUTPUT resolution.
        // The SR render (input) resolution is whatever DLSS recommends for this output + quality.
        // -----------------------------------------------------------------------------------------
        let sdk = DlssSdk::new(PROJECT_ID, device.clone())?;
        // Balanced quality mode for the output resolution. AutoExposure pairs with the
        // DlssExposure::Automatic used in the per-frame DlssRenderParameters below; motion vectors are
        // at render resolution (the default), so no LowResolutionMotionVectors flag is set — exactly
        // the flag choice super_resolution.rs makes for Automatic exposure.
        let sr_ctx = DlssContext::new(
            output_resolution,
            DlssPerfQualityMode::Balanced,
            DlssFeatureFlags::AutoExposure,
            sdk,
            &device,
            &queue,
        )?;
        let render_resolution = sr_ctx.render_resolution();
        println!(
            "DLSS-SR render resolution: {}x{} -> output {}x{}",
            render_resolution.x, render_resolution.y, output_resolution.x, output_resolution.y
        );

        // -----------------------------------------------------------------------------------------
        // Textures. SR inputs are render-res; SR output + FG buffers are output-res.
        // -----------------------------------------------------------------------------------------
        let input_usage =
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;

        // SR color input (render-res, Rgba8Unorm): the scene is drawn here.
        let sr_color = make_texture(
            &device,
            "sr color input (Rgba8Unorm, render-res)",
            render_resolution,
            wgpu::TextureFormat::Rgba8Unorm,
            input_usage,
        );
        // Depth (render-res, R32Float): shared by the SR evaluate and the FG tag.
        let depth_tex = make_texture(
            &device,
            "depth (R32Float, render-res)",
            render_resolution,
            wgpu::TextureFormat::R32Float,
            input_usage,
        );
        // Motion vectors (render-res, Rg16Float): shared buffer, fed to both SR and FG.
        let mvec_tex = make_texture(
            &device,
            "motion vectors (Rg16Float, render-res)",
            render_resolution,
            wgpu::TextureFormat::Rg16Float,
            input_usage,
        );
        // SR output (output-res, Rgba8Unorm UAV): DLSS writes the upscaled image here as a STORAGE
        // texture. Rgba8Unorm is UAV-capable; Bgra8Unorm is NOT, so the UAV output must be Rgba.
        let sr_output = make_texture(
            &device,
            "sr output (Rgba8Unorm UAV, output-res)",
            output_resolution,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        );
        // FG hud-less color (output-res, SURFACE format): must match the back buffer's format+size.
        // The blit writes the upscaled image here; COPY_SRC lets us also copy it to the back buffer.
        let hudless_tex = make_texture(
            &device,
            "fg hud-less color (surface format, output-res)",
            output_resolution,
            surface_format,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
        );
        // FG UI color+alpha (output-res, Rgba8Unorm): cleared fully transparent (no UI here).
        let ui_tex = make_texture(
            &device,
            "fg UI color+alpha (Rgba8Unorm, output-res)",
            output_resolution,
            wgpu::TextureFormat::Rgba8Unorm,
            input_usage,
        );

        // -----------------------------------------------------------------------------------------
        // Moving-bar pipeline (renders the SR color input at render-res).
        // -----------------------------------------------------------------------------------------
        let bar_wgsl = BAR_WGSL.replace("BAR_HALF_WIDTH", &format!("{BAR_HALF_WIDTH:?}"));
        let bar_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bar shader"),
            source: wgpu::ShaderSource::Wgsl(bar_wgsl.into()),
        });
        let bar_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bar uniform"),
            size: 16, // vec4<f32>
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bar_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bar bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bar_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bar bind group"),
            layout: &bar_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &bar_uniform,
                    offset: 0,
                    size: None,
                }),
            }],
        });
        let bar_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bar pipeline layout"),
            bind_group_layouts: &[Some(&bar_bgl)],
            immediate_size: 0,
        });
        let bar_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bar pipeline (sr color, Rgba8Unorm)"),
            layout: Some(&bar_layout),
            vertex: wgpu::VertexState {
                module: &bar_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &bar_shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: sr_color.format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        // -----------------------------------------------------------------------------------------
        // Blit pipeline: samples the SR output (Rgba8Unorm) and writes the hud-less (Bgra8Unorm),
        // performing the Rgba->Bgra format conversion the swapchain needs. Sampling the SR output
        // inside this render pass auto-transitions it from STORAGE/UAV (where DLSS left it) to
        // RESOURCE (shader-read) via the bind group.
        // -----------------------------------------------------------------------------------------
        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let sr_output_view = sr_output.create_view(&wgpu::TextureViewDescriptor::default());
        let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit bind group"),
            layout: &blit_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&blit_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&sr_output_view),
                },
            ],
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit pipeline layout"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit pipeline (sr output -> hud-less)"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    // Target the hud-less texture's format (surface / Bgra8Unorm).
                    format: hudless_tex.format(),
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            window,
            _instance: instance,
            surface,
            device,
            queue,
            config,
            output_resolution,
            render_resolution,
            sr_color,
            depth_tex,
            mvec_tex,
            sr_output,
            hudless_tex,
            ui_tex,
            bar_uniform,
            bar_bind_group,
            bar_pipeline,
            blit_pipeline,
            blit_bind_group,
            sr_ctx,
            fg,
            frame_index: 0,
            max_presented: 0,
            last_presented: 0,
        })
    }

    /// One frame of the combined demo. Returns `false` once we've hit `MAX_FRAMES` (request exit).
    fn render(&mut self) -> Result<bool, DemoError> {
        if self.frame_index >= MAX_FRAMES {
            return Ok(false);
        }
        let idx = self.frame_index;
        self.frame_index += 1;

        // --- Per-frame moving-bar offset (genuine, consistent motion) at RENDER resolution. ---
        let render_w = self.render_resolution.x as f32;
        let render_h = self.render_resolution.y as f32;
        let x_offset_px = (idx as f32 * VELOCITY) % render_w;
        let params: [f32; 4] = [x_offset_px, render_w, render_h, 0.0];
        // Reinterpret the 4 f32s as 16 raw bytes for write_buffer (no bytemuck dependency).
        let params_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(params.as_ptr().cast::<u8>(), 16) };
        self.queue.write_buffer(&self.bar_uniform, 0, params_bytes);

        // -----------------------------------------------------------------------------------------
        // STEP 2: Render the SR inputs at RENDER resolution and submit BEFORE the SR evaluate, so
        // wgpu's state tracker reflects their post-render states when DlssContext::render transitions
        // them. Draw the bar into the SR color input; clear depth to a constant; fill mvec uniformly.
        // -----------------------------------------------------------------------------------------
        let sr_color_view = self
            .sr_color
            .create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = self
            .depth_tex
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mvec_view = self
            .mvec_tex
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut input_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("sr inputs"),
                });
        // Draw the moving bar into the SR color input (render-res).
        draw_bar(
            &mut input_encoder,
            &sr_color_view,
            &self.bar_pipeline,
            &self.bar_bind_group,
        );
        // Clear depth (R32Float, treated as a plain float color target) to a constant.
        clear_target(
            &mut input_encoder,
            &depth_view,
            wgpu::Color {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            "clear depth",
        );
        // Fill the motion vectors uniformly with the bar's translation (-VELOCITY, 0). Rg16Float maps
        // r->x, g->y. Both SR and FG read this same buffer.
        clear_target(
            &mut input_encoder,
            &mvec_view,
            wgpu::Color {
                r: MVEC_X,
                g: MVEC_Y,
                b: 0.0,
                a: 0.0,
            },
            "fill mvec (uniform motion)",
        );
        // Submit the SR inputs BEFORE the SR evaluate (separate submit, as required).
        self.queue.submit([input_encoder.finish()]);

        // -----------------------------------------------------------------------------------------
        // STEP 3: Build the SR render parameters and evaluate. This is the SINGLE source of truth for
        // jitter / mvec-scale / reset that the FG constants are derived from below. No jitter in this
        // demo (jitter_offset = ZERO), so SR and FG agree on "no jitter". DlssContext::render submits
        // its own transition + evaluate command lists internally.
        // -----------------------------------------------------------------------------------------
        let sr_params = DlssRenderParameters {
            color: DlssTexture {
                texture: &self.sr_color,
            },
            depth: DlssTexture {
                texture: &self.depth_tex,
            },
            motion_vectors: DlssTexture {
                texture: &self.mvec_tex,
            },
            exposure: DlssExposure::Automatic,
            bias: None,
            dlss_output: DlssTexture {
                texture: &self.sr_output,
            },
            // Reset temporal history on the first frame (treat it as a camera cut).
            reset: idx == 0,
            // No jitter in this demo (the bar is the only motion; the camera is static).
            jitter_offset: Vec2::ZERO,
            // The SR inputs are allocated at `render_resolution` (= the context's min render res).
            // With `None`, the evaluate subrect defaults to `max_render_resolution`, which exceeds
            // the allocated textures and NGX rejects it (InvalidParameters). Pin the subrect to the
            // size actually allocated.
            partial_texture_size: Some(self.render_resolution),
            motion_vector_scale: None,
        };

        // Derive the FG common constants from the SAME DlssRenderParameters the SR evaluate uses, so
        // SR and FG share one source of jitter / mvec-scale / reset. The bridge converts the
        // motion-vector convention (SR reads render-res pixels; FG normalizes to [-1, 1]) by dividing
        // the SR mvec scale by the render resolution. The mvec buffer carries full motion, so
        // camera_motion_included = true (camera matrices stay identity). We build this BEFORE the SR
        // render call, which consumes `sr_params` (DlssRenderParameters is not Clone).
        let mut consts = FgConstants::from_render_parameters(&sr_params, self.render_resolution);
        consts.camera_motion_included = true;
        consts.camera_aspect_ratio =
            self.output_resolution.x as f32 / self.output_resolution.y as f32;

        // Evaluate SR: upscale SR color (render-res) -> SR output (output-res). DlssContext::render
        // submits its own transition + evaluate command lists internally.
        self.sr_ctx.render(sr_params, &self.queue)?;

        // -----------------------------------------------------------------------------------------
        // STEP 4: FG frame begins. begin_frame -> set_constants (the SR-derived consts) -> acquire.
        // -----------------------------------------------------------------------------------------
        let frame = self.fg.begin_frame(idx)?;
        frame.set_constants(&consts)?;

        // Acquire the back buffer (performs the mandatory GetCurrentBackBufferIndex every frame). On a
        // transient unavailable surface, reconfigure and skip this frame.
        let (backbuffer, _bbi) = match frame.acquire(&self.surface) {
            Ok(v) => v,
            Err(StreamlineError::SurfaceUnavailable { status }) => {
                if idx.is_multiple_of(STATE_POLL_INTERVAL) {
                    println!("frame {idx}: surface unavailable ({status}); reconfiguring");
                }
                // The `Frame` is dropped here (logged as aborted); the next frame restarts cleanly.
                self.surface.configure(&self.device, &self.config);
                return Ok(true);
            }
            Err(e) => return Err(DemoError::Fg(e)),
        };

        // -----------------------------------------------------------------------------------------
        // STEP 5: On a RENDER encoder: blit SR output -> hud-less (output-res), clear UI transparent,
        // then COPY hud-less -> back buffer (both Bgra8Unorm, output-res), putting the upscaled image
        // on screen.
        //
        // Resource states: DLSS left the SR output in STORAGE/UAV; the blit render pass samples it via
        // the bind group, which auto-transitions it to RESOURCE (shader-read). The hud-less is a
        // RENDER_ATTACHMENT during the blit; wgpu then transitions it to COPY_SRC for the copy below.
        // (At Present, SL consumes the hud-less tag expecting PIXEL_SHADER_RESOURCE; FgResource::new
        // defaults to that state — see the tag step.)
        // -----------------------------------------------------------------------------------------
        let backbuffer_view = backbuffer
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let hudless_view = self
            .hudless_tex
            .create_view(&wgpu::TextureViewDescriptor::default());
        let ui_view = self
            .ui_tex
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut render_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("sr+fg render"),
                });

        // Blit the upscaled SR output into the hud-less color (Rgba8Unorm -> Bgra8Unorm).
        {
            let mut rp = render_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit sr output -> hud-less"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &hudless_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Full-frame blit overwrites every pixel; Clear avoids a load.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.blit_pipeline);
            rp.set_bind_group(0, &self.blit_bind_group, &[]);
            rp.draw(0..3, 0..1);
        }
        // Clear the UI color+alpha to fully transparent (no UI in this demo).
        clear_target(
            &mut render_encoder,
            &ui_view,
            wgpu::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            "clear ui",
        );
        // Copy the hud-less color onto the acquired back buffer (both surface format, output-res), so
        // the upscaled image is what reaches the screen.
        render_encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.hudless_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &backbuffer.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.output_resolution.x,
                height: self.output_resolution.y,
                depth_or_array_layers: 1,
            },
        );
        // Keep the back-buffer view alive until after submit, even though the copy (not a draw) is
        // what writes it; binding it explicitly documents the back buffer is the final target.
        let _ = backbuffer_view;

        // -----------------------------------------------------------------------------------------
        // STEP 6: Tag the FG inputs on a DEDICATED raw-only encoder (no wgpu passes on it). depth and
        // motion_vectors are at RENDER resolution — that is correct, "same as DLSS-SR consumed". The
        // hud-less color and UI are at OUTPUT resolution, matching the back buffer. FgResource::new
        // assumes PIXEL_SHADER_RESOURCE (0x80), the state wgpu leaves TEXTURE_BINDING textures in.
        // -----------------------------------------------------------------------------------------
        let mut tag_encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sr+fg tag"),
            });
        frame.tag(
            &mut tag_encoder,
            &FgResources {
                depth: FgResource::new(&self.depth_tex),
                motion_vectors: FgResource::new(&self.mvec_tex),
                hudless_color: Some(FgResource::new(&self.hudless_tex)),
                ui: Some(FgUi::ColorAndAlpha(FgResource::new(&self.ui_tex))),
            },
        )?;

        // -----------------------------------------------------------------------------------------
        // STEP 7: Submit the render commands first, then the tagging command list, then end the
        // render phase and present. DLSS-G interpolates at Present using the tagged inputs.
        // -----------------------------------------------------------------------------------------
        self.queue
            .submit([render_encoder.finish(), tag_encoder.finish()]);
        frame.end_render();
        frame.present(backbuffer);

        // The `Frame` borrows `self.fg`; drop it before the immutable `query_state` borrow.
        drop(frame);

        // -----------------------------------------------------------------------------------------
        // STEP 8: Periodically query + report DLSS-G state.
        // -----------------------------------------------------------------------------------------
        if idx.is_multiple_of(STATE_POLL_INTERVAL) {
            match self.fg.query_state() {
                Ok(state) => self.report_state(idx, &state),
                Err(e) => println!("frame {idx}: query_state failed: {e}"),
            }
        }

        self.window.request_redraw();
        Ok(true)
    }

    /// Records the latest DLSS-G state, tracking the max `numFramesActuallyPresented` seen.
    fn report_state(&mut self, idx: u32, state: &FrameGenerationState) {
        self.last_presented = state.num_frames_actually_presented;
        self.max_presented = self.max_presented.max(state.num_frames_actually_presented);
        println!(
            "frame {idx}: presented={} status={} (ok={}) max_generate={} vram={}MiB",
            state.num_frames_actually_presented,
            state.status_text,
            state.is_ok,
            state.num_frames_to_generate_max,
            state.estimated_vram_usage_in_bytes / (1024 * 1024),
        );
    }
}

/// Creates a 2D color-renderable texture of the given resolution, format, and usage.
fn make_texture(
    device: &wgpu::Device,
    label: &str,
    resolution: UVec2,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Draws the moving bar into `view` (clears the dark background, lights the bar).
fn draw_bar(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("draw bar -> sr color"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.02,
                    g: 0.02,
                    b: 0.04,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind_group, &[]);
    rp.draw(0..3, 0..1);
}

/// Clears `view` to `color` with a no-draw render pass (load = clear, store).
fn clear_target(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    color: wgpu::Color,
    label: &str,
) {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(color),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}

/// winit 0.30 `ApplicationHandler`. The wgpu + DLSS state is created on `resumed`.
struct App {
    /// The Streamline handle, created in `main` BEFORE the wgpu instance. Moved into the FG context
    /// by `FrameGenerationContext::new` on success; kept here so its `Drop` still runs `slShutdown`
    /// if context creation fails.
    streamline: Streamline,
    gpu: Option<GpuState>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("sr_plus_fg (DLSS SR + FG over wgpu DX12)")
                        .with_inner_size(winit::dpi::PhysicalSize::new(OUTPUT_WIDTH, OUTPUT_HEIGHT))
                        // DLSS-G will not present generated frames to a window that is not actually
                        // composited. Force the window visible + foreground.
                        .with_visible(true)
                        .with_active(true),
                )
                .expect("create_window"),
        );
        window.focus_window();

        match GpuState::new(window, &mut self.streamline) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(e) => {
                eprintln!(
                    "Failed to set up DLSS SR + Frame Generation: {e}\n\
                     (Requires an NVIDIA RTX GPU, a recent driver, the wgpu Dx12 backend, \
                     STREAMLINE_SDK set, and the SL + NGX DLLs staged beside the exe.)"
                );
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(gpu) = self.gpu.as_mut() {
                    match gpu.render() {
                        Ok(true) => {}
                        Ok(false) => event_loop.exit(),
                        Err(e) => {
                            eprintln!("frame render failed: {e}");
                            event_loop.exit();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn main() {
    env_logger::init();

    // -------------------------------------------------------------------------------------------
    // STEP 1: Streamline::init() BEFORE any wgpu/DXGI/D3D12 object exists, so the wgpu fork upgrades
    // its DXGI factory to a Streamline proxy inside Instance::init. This is the single most important
    // ordering rule.
    // -------------------------------------------------------------------------------------------
    let streamline = match Streamline::init() {
        Ok(sl) => sl,
        Err(e) => {
            eprintln!(
                "Streamline::init failed: {e}\n\
                 DLSS Frame Generation cannot run. Common causes: STREAMLINE_SDK not set, missing \
                 or unsigned sl.interposer.dll, missing SL plugin DLLs beside the exe, a non-NVIDIA \
                 GPU, or hardware-accelerated GPU scheduling disabled."
            );
            std::process::exit(1);
        }
    };

    // -------------------------------------------------------------------------------------------
    // STEP 2+: window + wgpu + both DLSS features + per-frame loop (in App::resumed / GpuState::new).
    // -------------------------------------------------------------------------------------------
    let event_loop = EventLoop::new().expect("EventLoop::new");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        streamline,
        gpu: None,
    };
    event_loop.run_app(&mut app).expect("run_app");

    // -------------------------------------------------------------------------------------------
    // STEP 3: final summary. The SR context, FG context (slShutdown), and SDK drop with `app`.
    // -------------------------------------------------------------------------------------------
    let (max_presented, last_presented) = match app.gpu.as_ref() {
        Some(g) => (g.max_presented, g.last_presented),
        None => (0, 0),
    };
    let generating = max_presented >= 2;
    println!(
        "SR+FG RESULT: numFramesActuallyPresented={max_presented} (last poll: {last_presented}) \
         => generating={generating} (need >= 2)"
    );
}
