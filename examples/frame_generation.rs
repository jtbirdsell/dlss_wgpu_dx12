//! Interactive, animated DLSS Frame Generation (DLSS-G) example.
//!
//! This drives DLSS-G to generate interpolated frames over a wgpu-owned DX12 swapchain, using only
//! the crate's **public** Frame Generation API ([`Streamline`], [`FrameGenerationContext`],
//! [`Frame`]). It opens a visible window, scrolls a bright vertical bar horizontally at a constant
//! velocity (genuine, consistent motion), feeds DLSS-G matching motion vectors, and polls
//! `slDLSSGGetState` every 30 frames. After ~300 frames it prints a final line reporting the
//! maximum `numFramesActuallyPresented` it observed — `2` means DLSS-G generated an interpolated
//! frame between each pair of rendered frames.
//!
//! It is the safe-API counterpart of the hardware-proven `fg_spike` (which used hand-written
//! Streamline FFI and confirmed `numFramesActuallyPresented == 2` on an RTX 4090). It compiles in
//! CI but can only be *verified* on an RTX GPU + display.
//!
//! ## RUN REQUIREMENTS (read before running)
//!
//! 1. **`STREAMLINE_SDK` env var** must point at the Streamline SDK root so the crate can locate and
//!    signature-verify `sl.interposer.dll`. (Building the crate also needs `DLSS_SDK` +
//!    `LIBCLANG_PATH` set; those are build-time only.)
//! 2. **Stage these DLLs next to the example exe** (`target/debug/examples/`):
//!    `sl.interposer.dll`, `sl.common.dll`, `sl.dlss_g.dll`, `sl.reflex.dll`, `sl.pcl.dll`,
//!    `nvngx_dlssg.dll`. Without them `slInit` / DLSS-G load fails and no frames are generated.
//! 3. **The window must stay visible + focused / composited** while it runs. DLSS-G silently
//!    *declines* to present generated frames to a window that is not actually being composited to
//!    the screen — do not minimize it or cover it. This is requirement (1) of the proven recipe and
//!    was the single gate that flipped `numFramesActuallyPresented` from 1 to 2.
//! 4. Build/run with the Frame Generation feature enabled:
//!
//!    ```powershell
//!    $env:DLSS_SDK = 'C:/Users/jorda/dlss_sdk'
//!    $env:LIBCLANG_PATH = 'C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//!    $env:STREAMLINE_SDK = 'C:/path/to/streamline'   # so the interposer can be found at runtime
//!    cargo run --example frame_generation --features frame-generation
//!    ```
//!
//! ## The proven 2x recipe (why it generates)
//!
//! All five requirements below are baked in; the inline comments mark each one:
//!   1. The window is visible + foreground/focused (`with_visible(true).with_active(true)` +
//!      `focus_window()`).
//!   2. A non-vsync present mode (`Mailbox`, else `Immediate`, else `Fifo`) — Reflex/DLSS-G own the
//!      frame pacing.
//!   3. Genuine consistent motion: the bar scrolls at `VELOCITY` px/frame into *both* the back
//!      buffer and the HUD-less color texture, and the motion-vector texture is filled uniformly
//!      with `(-VELOCITY, 0)`; `FgConstants::with_pixel_motion` sets `mvec_scale = (1/w, 1/h)` so
//!      those pixel values are normalized to `[-1, 1]`.
//!   4. `camera_motion_included = true` — the mvec buffer carries the full motion; camera matrices
//!      stay identity.
//!   5. `Frame::acquire` is called every frame (it performs the mandatory
//!      `GetCurrentBackBufferIndex`).
//!
//! ## Ordering requirements (enforced by the API)
//!   * [`Streamline::init`] runs **before** `wgpu::Instance::new` (so the wgpu fork upgrades its DXGI
//!     factory to a Streamline proxy).
//!   * [`FrameGenerationContext::new`] runs **after** the device is created but **before**
//!     `surface.configure()` (so `slSetD3DDevice` precedes swapchain creation).

use std::sync::Arc;

use dlss_wgpu_dx12::{
    FgConstants, FgResource, FgResources, FgUi, FrameGenerationContext, FrameGenerationOptions,
    FrameGenerationState, Streamline, StreamlineError,
};
use glam::UVec2;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Render / swapchain resolution.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// How many frames to drive before exiting.
const MAX_FRAMES: u32 = 300;
/// Poll `slDLSSGGetState` (and print a status line) every this many frames.
const STATE_POLL_INTERVAL: u32 = 30;

/// Horizontal velocity of the bar, in render-resolution pixels per frame. Recipe requirement (3):
/// feed DLSS-G genuine, consistent motion so it ENABLES interpolation. A static scene makes DLSS-G
/// decline to generate.
const VELOCITY: f32 = 8.0;
/// Half-width (in pixels) of the bright vertical bar the shader draws (~240px-wide bar total).
const BAR_HALF_WIDTH: f32 = 120.0;

/// The motion vector we write uniformly into the mvec texture, in render-resolution pixels, using
/// the DLSS convention: per-pixel displacement from the CURRENT frame to the PREVIOUS frame. The bar
/// translates +VELOCITY px/frame in +X, so a surface point's previous position is VELOCITY px to the
/// LEFT of its current position => mvec = (-VELOCITY, 0). These PIXEL values are normalized to the
/// [-1, 1] range DLSS-G expects via `mvec_scale = (1/WIDTH, 1/HEIGHT)`, set through
/// [`FgConstants::with_pixel_motion`] (recipe requirement 3).
const MVEC_X: f64 = -(VELOCITY as f64);
const MVEC_Y: f64 = 0.0;

/// Inline WGSL: a fullscreen-triangle vertex shader + a fragment shader that lights pixels inside a
/// moving ~240px-wide vertical bar. The bar X position (`x_offset_px`) advances every frame on the
/// CPU and is delivered through a 16-byte uniform (`vec4<f32> = {x_offset_px, width, height, pad}`).
const BAR_WGSL: &str = r#"
struct Params {
    data: vec4<f32>, // x = x_offset_px, y = width, z = height, w = pad
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
    let px = in.uv.x * width;                  // pixel X of this fragment
    let bar = abs(px - x_off) < BAR_HALF_WIDTH; // ~240px-wide vertical bar
    if (bar) {
        return vec4<f32>(0.95, 0.85, 0.10, 1.0); // bright bar
    }
    return vec4<f32>(0.02, 0.02, 0.04, 1.0);     // dark background
}
"#;

/// Everything that exists after the wgpu device is up and the DLSS-G context is bound.
struct GpuState {
    window: Arc<Window>,
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    // The four DLSS-G inputs, all at render resolution (see the spike for the format rationale).
    depth_tex: wgpu::Texture,    // R32Float
    mvec_tex: wgpu::Texture,     // Rg16Float
    hudless_tex: wgpu::Texture,  // surface format
    ui_tex: wgpu::Texture,       // Rgba8Unorm (color + alpha)

    // --- Moving-bar render pipeline (genuine, consistent motion) ---
    /// Per-frame uniform (`vec4<f32>`): {x_offset_px, width, height, pad}.
    bar_uniform: wgpu::Buffer,
    bar_bind_group: wgpu::BindGroup,
    /// Pipeline whose color target is the surface/back-buffer format.
    bar_pipeline_surface: wgpu::RenderPipeline,
    /// Pipeline whose color target is the hudless texture's format (same format in practice, kept
    /// separate so differing formats still work).
    bar_pipeline_hudless: wgpu::RenderPipeline,

    /// The crate's per-camera DLSS-G feature. Owns the Streamline core API; `slShutdown` runs on its
    /// `Drop`.
    fg: FrameGenerationContext,
    /// Render resolution as a `UVec2`, for `FgConstants::with_pixel_motion`.
    render_resolution: UVec2,
    /// Next frame index to drive.
    frame_index: u32,
    /// Max `numFramesActuallyPresented` observed across the run (the success metric: 2 = generating).
    max_presented: u32,
    /// Last `numFramesActuallyPresented` observed.
    last_presented: u32,
}

impl GpuState {
    /// Builds the wgpu device/surface and the DLSS-G context in the proven order.
    ///
    /// `streamline` is the already-initialized [`Streamline`] handle (created **before** the
    /// `wgpu::Instance` in `main`). On success the context *moves* the Streamline core API out of
    /// this handle (the handle becomes inert), so we keep `streamline` alive in `App` only until
    /// `new` returns.
    fn new(window: Arc<Window>, streamline: &mut Streamline) -> Result<Self, StreamlineError> {
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

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("frame_generation device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("request_device");

        // --- Choose the surface format (prefer Bgra8Unorm) and present mode. ---
        let caps = surface.get_capabilities(&adapter);
        let format = if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
            wgpu::TextureFormat::Bgra8Unorm
        } else {
            caps.formats[0]
        };
        println!("Surface format: {format:?}");

        // Recipe requirement (2): a NON-VSYNC present mode. DLSS-G manages its own frame pacing
        // (Reflex-driven) and re-presents the generated + real frame; hard vsync (Fifo) throttles
        // the app's present rate and can suppress interpolation. Prefer Mailbox, else Immediate, else
        // Fifo (always supported) so Reflex/DLSS-G own the final pacing.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        println!("Present mode: {present_mode:?}");

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: WIDTH,
            height: HEIGHT,
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
        // below (triggering SL's interposed swapchain-creation hooks); if the device is not yet
        // registered, DLSS-G never binds. So we build the context here, then configure the surface.
        //
        // `FrameGenerationOptions::enabled()` sets mode=On / numFramesToGenerate=1 (classic 2x).
        // `.with_color_format(format)` tells SL the DXGI format of the back buffer + HUD-less color.
        // -----------------------------------------------------------------------------------------
        let options = FrameGenerationOptions::enabled().with_color_format(format);
        let fg = FrameGenerationContext::new(streamline, &device, &adapter, &options)?;
        println!(
            "DLSS-G enabled (mode On, numFramesToGenerate=1, color_format set from {format:?})"
        );

        // NOW create the swapchain (SL's device registration is in place).
        surface.configure(&device, &config);

        // --- The four DLSS-G input textures, all at render resolution. ---
        // depth = R32Float, mvec = Rg16Float, hudless = surface format (must match the back buffer),
        // ui = Rgba8Unorm (color + alpha). Cleared/filled each frame; that is enough for the probe.
        let depth_tex = make_input(&device, "fg depth (R32Float)", wgpu::TextureFormat::R32Float);
        let mvec_tex = make_input(
            &device,
            "fg motion vectors (Rg16Float)",
            wgpu::TextureFormat::Rg16Float,
        );
        // HUD-less color must share the swapchain back buffer's format (DLSS-G composites against it).
        let hudless_tex = make_input(&device, "fg HUD-less color (surface format)", format);
        // UI color+alpha needs an alpha channel.
        let ui_tex = make_input(
            &device,
            "fg UI color+alpha (Rgba8Unorm)",
            wgpu::TextureFormat::Rgba8Unorm,
        );

        // -----------------------------------------------------------------------------------------
        // Moving-bar pipeline. Draws a translating bright bar into BOTH the back buffer and the
        // hudless texture each frame so the scene genuinely moves frame-to-frame. The X offset is
        // delivered per-frame via a small uniform buffer updated with queue.write_buffer.
        // -----------------------------------------------------------------------------------------
        // Inject the Rust-side BAR_HALF_WIDTH into the WGSL so the bar width stays in sync with the
        // constant (WGSL cannot reference Rust consts directly).
        let bar_wgsl = BAR_WGSL.replace("BAR_HALF_WIDTH", &format!("{BAR_HALF_WIDTH:?}"));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fg bar shader"),
            source: wgpu::ShaderSource::Wgsl(bar_wgsl.into()),
        });
        let bar_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fg bar uniform"),
            size: 16, // vec4<f32>
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bar_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fg bar bgl"),
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
            label: Some("fg bar bind group"),
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
            label: Some("fg bar pipeline layout"),
            bind_group_layouts: &[Some(&bar_bgl)],
            immediate_size: 0,
        });
        // Helper that builds a bar pipeline for a given color-target format.
        let make_bar_pipeline = |target_format: wgpu::TextureFormat, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&bar_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let bar_pipeline_surface = make_bar_pipeline(format, "fg bar pipeline (surface)");
        let bar_pipeline_hudless =
            make_bar_pipeline(hudless_tex.format(), "fg bar pipeline (hudless)");

        Ok(Self {
            window,
            _instance: instance,
            surface,
            device,
            queue,
            config,
            depth_tex,
            mvec_tex,
            hudless_tex,
            ui_tex,
            bar_uniform,
            bar_bind_group,
            bar_pipeline_surface,
            bar_pipeline_hudless,
            fg,
            render_resolution: UVec2::new(WIDTH, HEIGHT),
            frame_index: 0,
            max_presented: 0,
            last_presented: 0,
        })
    }

    /// One frame of the demo, driving the crate's per-frame `Frame` sequence in the proven order.
    /// Returns `false` once we've hit `MAX_FRAMES` (request exit).
    fn render(&mut self) -> Result<bool, StreamlineError> {
        if self.frame_index >= MAX_FRAMES {
            return Ok(false);
        }
        let idx = self.frame_index;
        self.frame_index += 1;

        // --- Per-frame moving-bar offset (recipe requirement 3: genuine, consistent motion) ---
        // x_px = (frame_index * VELOCITY) % WIDTH. Advancing by a constant velocity gives DLSS-G the
        // consistent translation it needs to interpolate. Push it into the uniform.
        let x_offset_px = (idx as f32 * VELOCITY) % WIDTH as f32;
        let params: [f32; 4] = [x_offset_px, WIDTH as f32, HEIGHT as f32, 0.0];
        // Reinterpret the 4 f32s as 16 raw bytes for write_buffer (no bytemuck dependency).
        let params_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(params.as_ptr().cast::<u8>(), 16) };
        self.queue.write_buffer(&self.bar_uniform, 0, params_bytes);

        // --- (1) begin_frame: token + reflex sleep + simulation markers ---
        let frame = self.fg.begin_frame(idx)?;

        // --- (2) set_constants ---
        // Identity-camera defaults; `with_pixel_motion` sets mvec_scale = (1/w, 1/h) so our pixel
        // motion vectors are normalized to [-1, 1] (recipe requirement 3). `camera_motion_included`
        // is set true: the mvec buffer carries the FULL motion (uniform pan) and the camera matrices
        // stay identity, so DLSS-G uses the mvecs verbatim instead of synthesizing camera motion
        // (recipe requirement 4). `reset` is auto-forced true on frame 0 by `set_constants`.
        let mut consts = FgConstants::new().with_pixel_motion(self.render_resolution);
        consts.camera_motion_included = true;
        consts.camera_aspect_ratio = WIDTH as f32 / HEIGHT as f32;
        frame.set_constants(&consts)?;

        // --- (3) acquire (recipe requirement 5: this performs the mandatory
        //         GetCurrentBackBufferIndex every frame). On a transient unavailable surface,
        //         reconfigure and skip this frame. ---
        let (surface_tex, _bbi) = match frame.acquire(&self.surface) {
            Ok(v) => v,
            Err(StreamlineError::SurfaceUnavailable { status }) => {
                if idx % STATE_POLL_INTERVAL == 0 {
                    println!("frame {idx}: surface unavailable ({status}); reconfiguring");
                }
                // The `Frame` is dropped here (logged as aborted) — acceptable for a transient
                // reconfigure; the next frame restarts the sequence cleanly.
                self.surface.configure(&self.device, &self.config);
                return Ok(true);
            }
            Err(e) => return Err(e),
        };

        // --- (4) Record the scrolling scene + the dummy inputs on a render-only encoder. ---
        // This encoder uses ONLY the wgpu encoding API (render passes); the SL tag is recorded on a
        // separate raw-only encoder below, because wgpu forbids mixing the two encoding APIs on one
        // encoder.
        let surface_view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = self
            .depth_tex
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mvec_view = self
            .mvec_tex
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
                    label: Some("fg render"),
                });

        // Draw the moving bar into the swapchain back buffer (content must genuinely translate so
        // DLSS-G sees motion — not just a clear).
        draw_bar(
            &mut render_encoder,
            &surface_view,
            &self.bar_pipeline_surface,
            &self.bar_bind_group,
            "draw bar -> backbuffer",
        );
        // Clear depth (R32Float, treated as a plain float color target here) to a constant.
        clear_target(
            &mut render_encoder,
            &depth_view,
            wgpu::Color {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            "clear depth",
        );
        // Fill the motion vectors uniformly with the bar's translation (recipe requirement 3).
        // Rg16Float maps the clear color r->x (motion.x) and g->y (motion.y): (-VELOCITY, 0).
        clear_target(
            &mut render_encoder,
            &mvec_view,
            wgpu::Color {
                r: MVEC_X,
                g: MVEC_Y,
                b: 0.0,
                a: 0.0,
            },
            "fill mvec (uniform motion)",
        );
        // Draw the SAME moving bar into the HUD-less color: DLSS-G interpolates the hudless buffer,
        // so it must carry the identical translating content as the back buffer (recipe req. 3).
        draw_bar(
            &mut render_encoder,
            &hudless_view,
            &self.bar_pipeline_hudless,
            &self.bar_bind_group,
            "draw bar -> hudless",
        );
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

        // --- (5) tag the four DLSS-G inputs on a DEDICATED raw-only encoder. ---
        // The tag is recorded onto this encoder's raw ID3D12GraphicsCommandList; it must have no wgpu
        // passes on it. We submit [render, tag] in order so the tagged content is produced before SL
        // consumes the tags during Present.
        let mut tag_encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fg tag"),
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

        // --- (6) Submit the render commands first, then the tagging command list. ---
        self.queue
            .submit([render_encoder.finish(), tag_encoder.finish()]);

        // --- (7) end_render (PCL render-submit-end) ---
        frame.end_render();

        // --- (8) present (brackets the present with PCL markers and consumes the surface texture) ---
        frame.present(surface_tex);

        // The `Frame` borrows `self.fg` for its whole lifetime; drop it before querying state so the
        // immutable `self.fg.query_state()` borrow does not overlap the mutable `begin_frame` borrow.
        drop(frame);

        // --- (9) Periodically query + report DLSS-G state. ---
        if idx % STATE_POLL_INTERVAL == 0 {
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

/// Creates a 2D render-resolution input texture (TEXTURE_BINDING + RENDER_ATTACHMENT) of the given
/// format. All four DLSS-G inputs are this shape.
fn make_input(device: &wgpu::Device, label: &str, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

/// Draws the moving bar into `view` with the given pipeline (clears the dark background, lights the
/// bar). Used for both the back buffer and the HUD-less color.
fn draw_bar(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    label: &str,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
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

/// Clears `view` to `color` with a no-draw render pass (load = clear, store). Used for the depth,
/// motion-vector, and UI inputs.
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

/// winit 0.30 `ApplicationHandler`. The wgpu + DLSS-G state is created on `resumed` (when we first
/// have a window), and torn down on exit.
struct App {
    /// The Streamline handle, created in `main` BEFORE the wgpu instance. Moved into the context by
    /// `FrameGenerationContext::new` on success (it then becomes inert); kept here so its `Drop`
    /// still runs `slShutdown` if context creation fails.
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
                        .with_title("frame_generation (DLSS-G over wgpu DX12)")
                        .with_inner_size(winit::dpi::PhysicalSize::new(WIDTH, HEIGHT))
                        // Recipe requirement (1): DLSS-G will not present generated frames to a
                        // window that is not actually being composited. Force the window visible +
                        // foreground so a launch from a terminal does not leave it occluded.
                        .with_visible(true)
                        .with_active(true),
                )
                .expect("create_window"),
        );
        // Recipe requirement (1), continued: pull the window to the foreground/focused.
        window.focus_window();

        match GpuState::new(window, &mut self.streamline) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(e) => {
                eprintln!(
                    "Failed to set up DLSS Frame Generation: {e}\n\
                     (Requires an NVIDIA RTX GPU, a recent driver, the wgpu Dx12 backend, \
                     STREAMLINE_SDK set, and the SL DLLs staged beside the exe.)"
                );
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
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
    // STEP 1: Streamline::init() BEFORE any wgpu/DXGI/D3D12 object exists.
    //
    // The wgpu fork upgrades its DXGI factory to a Streamline proxy inside Instance::init ONLY if
    // sl.interposer.dll is already loaded. If we created the wgpu instance first, DLSS-G could never
    // bind to wgpu's swapchain. This is the single most important ordering rule.
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
    // STEP 2+: window + wgpu + DLSS-G per-frame loop (set up in App::resumed / GpuState::new).
    // -------------------------------------------------------------------------------------------
    let event_loop = EventLoop::new().expect("EventLoop::new");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        streamline,
        gpu: None,
    };
    event_loop.run_app(&mut app).expect("run_app");

    // -------------------------------------------------------------------------------------------
    // STEP 3: final summary. `slShutdown` runs when `app` (and its FrameGenerationContext, if any)
    // drops at the end of main.
    // -------------------------------------------------------------------------------------------
    let (max_presented, last_presented) = match app.gpu.as_ref() {
        Some(g) => (g.max_presented, g.last_presented),
        None => (0, 0),
    };
    let generating = max_presented >= 2;
    println!(
        "FG RESULT: numFramesActuallyPresented={max_presented} (last poll: {last_presented}) \
         => generating={generating} (need >= 2)"
    );
}
