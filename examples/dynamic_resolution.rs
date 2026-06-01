//! Headless DLSS Super Resolution example demonstrating DYNAMIC RESOLUTION SCALING (DRS).
//!
//! Dynamic resolution scaling lets a renderer trade image quality for frame time on the fly: when
//! the GPU is under pressure it renders the scene at a *smaller* subrect of its input textures, and
//! DLSS still upscales that subrect to the same fixed output resolution. The key DLSS contract is
//! that the input textures are allocated **once at the maximum render resolution**
//! (`*render_resolution_range().end()`), and each frame the renderer tells DLSS which **subrect** of
//! those max-sized textures it actually rendered into via
//! [`DlssRenderParameters::partial_texture_size`]. DLSS must evaluate cleanly as that subrect
//! changes frame to frame — no reallocation, no context recreation, no `InvalidParameters`.
//!
//! This example mirrors `super_resolution.rs` (headless, NVIDIA-vendor check, cleared inputs, the
//! graceful `FeatureNotSupported` exit) but, instead of pinning the render resolution to the minimum
//! and keeping it fixed, it:
//!   1. Creates the [`DlssContext`] in [`DlssPerfQualityMode::Balanced`] for a 4K output.
//!   2. Reads the render-resolution range (`min..=max`) and allocates all SR input textures ONCE at
//!      the MAX render resolution.
//!   3. Loops ~16 frames, sweeping the per-frame `render_res` across `[min, max]` with a triangle
//!      wave over the frame index, recomputing `suggested_jitter(frame, render_res)` each frame and
//!      passing `partial_texture_size: Some(render_res)` so DLSS only consumes that subrect.
//!
//! The point is to prove DRS evaluates cleanly across the whole sweep. On a real RTX GPU the lead
//! should confirm none of the swept frames returns `InvalidParameters`.
//!
//! Run with the SDK + libclang environment variables set (see the crate README), e.g.:
//!
//! ```powershell
//! $env:DLSS_SDK='C:/Users/jorda/dlss_sdk'
//! $env:LIBCLANG_PATH='C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//! cargo run --example dynamic_resolution
//! ```
//!
//! If DLSS is not supported on this system (non-RTX GPU, missing driver, or a non-Dx12 device), the
//! example prints a message and exits cleanly rather than panicking.

use dlss_wgpu_dx12::{
    DlssContext, DlssError, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode,
    DlssRenderParameters, DlssSdk, DlssTexture,
};
use glam::UVec2;
use uuid::Uuid;
use wgpu::{
    Backends, Color, CommandEncoder, Device, DeviceDescriptor, Extent3d, Instance,
    InstanceDescriptor, LoadOp, Operations, PollType, PowerPreference, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, StoreOp, Texture, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsages,
};

/// A stable identifier for this application; NGX uses it to look up DLSS overrides.
const PROJECT_ID: Uuid = Uuid::from_u128(0x9b8d_2f41_6c7a_4e15_8d3b_a0f2_71e4_55c9);

/// Upscaled (output) resolution to drive DLSS at — a typical 4K target.
const UPSCALED_RESOLUTION: UVec2 = UVec2::new(3840, 2160);

/// How many frames to evaluate while sweeping the render resolution. A handful of frames over the
/// full sweep is enough to confirm DLSS accepts a varying subrect without complaining.
const FRAME_COUNT: u32 = 16;

fn main() {
    match run() {
        Ok(()) => {}
        Err(DlssError::FeatureNotSupported) => {
            eprintln!(
                "DLSS Super Resolution is not supported on this system (requires an NVIDIA RTX \
                 GPU, a recent driver, and the wgpu Dx12 backend). Skipping."
            );
        }
        Err(error) => {
            eprintln!("DLSS dynamic resolution example failed: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), DlssError> {
    // 1. Build a DX12 instance with wgpu's default shader compiler (DLSS needs no dxcompiler.dll).
    let mut instance_descriptor = InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = Backends::DX12;
    let instance = Instance::new(instance_descriptor);

    // 2. Pick a high-performance (discrete) adapter and confirm it is NVIDIA.
    let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("failed to acquire a DX12 adapter");

    let info = adapter.get_info();
    println!(
        "Selected adapter: {} ({:?}, backend {:?}, vendor 0x{:04x})",
        info.name, info.device_type, info.backend, info.vendor
    );
    const PCI_VENDOR_NVIDIA: u32 = 0x10de;
    if info.vendor != PCI_VENDOR_NVIDIA {
        eprintln!("Adapter is not an NVIDIA GPU; DLSS will not be available.");
        return Err(DlssError::FeatureNotSupported);
    }

    // 3. Create the device + queue. TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES lets the DLSS output
    //    texture expose read-write storage usage, matching what a real renderer would request.
    let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("dlss_dynamic_resolution"),
        required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ..Default::default()
    }))
    .expect("failed to create a DX12 device");

    // 4. Initialize the application-wide DLSS / NGX SDK once.
    let sdk = DlssSdk::new(PROJECT_ID, device.clone())?;

    // 5. Create a per-camera DLSS context at the chosen output resolution. Balanced gives a render
    //    resolution range with real headroom between min and max (DLAA would collapse it to a single
    //    point), which is exactly what dynamic resolution scaling sweeps across.
    let mut context = DlssContext::new(
        UPSCALED_RESOLUTION,
        DlssPerfQualityMode::Balanced,
        DlssFeatureFlags::AutoExposure,
        sdk,
        &device,
        &queue,
    )?;

    // 6. Query the render-resolution range DRS sweeps across. `min` is the cheapest (most upscaled)
    //    render resolution; `max` is the most expensive (least upscaled). The inputs are allocated at
    //    `max` and DLSS evaluates a varying subrect within `[min, max]`.
    let range = context.render_resolution_range();
    let min_render = *range.start();
    let max_render = *range.end();
    let upscaled_resolution = context.upscaled_resolution();
    println!(
        "DLSS render-resolution range (DRS sweep): {}x{} (min) .. {}x{} (max)",
        min_render.x, min_render.y, max_render.x, max_render.y
    );
    println!(
        "DLSS upscaled resolution: {}x{}",
        upscaled_resolution.x, upscaled_resolution.y
    );

    // 7. Allocate the DLSS input textures ONCE at the MAX render resolution. Every frame's subrect is
    //    a `[0,0]`-anchored window into these max-sized textures (DLSS evaluates the subrect, not the
    //    full allocation). The output is allocated at the upscaled resolution.
    let input_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;

    let color = color_target(
        &device,
        "dlss_color",
        max_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let depth = color_target(
        &device,
        "dlss_depth",
        max_render,
        TextureFormat::R32Float,
        input_usage,
    );
    let motion_vectors = color_target(
        &device,
        "dlss_motion_vectors",
        max_render,
        TextureFormat::Rg16Float,
        input_usage,
    );
    let dlss_output = color_target(
        &device,
        "dlss_output",
        upscaled_resolution,
        TextureFormat::Rgba16Float,
        TextureUsages::TEXTURE_BINDING | TextureUsages::STORAGE_BINDING,
    );

    // 8. Drive the evaluation loop, sweeping the render resolution every frame.
    for frame_number in 0..FRAME_COUNT {
        // Triangle wave over the frame index in [0, 1]: ramp up to the midpoint, then back down, so
        // the sweep visits the full `[min, max]` range (and its endpoints) over the run.
        let phase = frame_number as f32 / (FRAME_COUNT - 1).max(1) as f32;
        let triangle = 1.0 - (2.0 * phase - 1.0).abs(); // 0 -> 1 -> 0
        let render_res = lerp_resolution(min_render, max_render, triangle);

        println!(
            "frame {frame_number}: render subrect {}x{} (t={triangle:.3}) -> {}x{}",
            render_res.x, render_res.y, upscaled_resolution.x, upscaled_resolution.y
        );

        // Jitter must be recomputed for THIS frame's render resolution (the phase count depends on
        // the upscale ratio, which changes as the subrect changes).
        let jitter_offset = context.suggested_jitter(frame_number, render_res);

        // Produce (here: clear) the DLSS inputs and submit BEFORE evaluating DLSS, so the resource
        // transitions inside render() observe the post-render states. We clear the full max-sized
        // textures; DLSS only reads the `render_res` subrect.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_dynamic_resolution_inputs"),
        });
        clear_color(&mut encoder, &color, Color::BLACK);
        clear_color(&mut encoder, &depth, Color::WHITE);
        clear_color(&mut encoder, &motion_vectors, Color::TRANSPARENT);
        queue.submit([encoder.finish()]);

        context.render(
            DlssRenderParameters {
                color: DlssTexture { texture: &color },
                depth: DlssTexture { texture: &depth },
                motion_vectors: DlssTexture {
                    texture: &motion_vectors,
                },
                exposure: DlssExposure::Automatic,
                bias: None,
                dlss_output: DlssTexture {
                    texture: &dlss_output,
                },
                // Reset temporal history on the first frame (treat it as a camera cut).
                reset: frame_number == 0,
                jitter_offset,
                // The load-bearing DRS parameter: evaluate only THIS frame's render subrect of the
                // max-sized inputs. DLSS must accept this changing every frame without complaint.
                partial_texture_size: Some(render_res),
                motion_vector_scale: None,
            },
            &queue,
        )?;

        device
            .poll(PollType::wait_indefinitely())
            .map_err(|_| DlssError::PlatformError)?;
    }

    println!(
        "Success: evaluated {FRAME_COUNT} DLSS frames sweeping the render subrect across \
         {}x{}..{}x{} (inputs allocated once at {}x{}; output {}x{}).",
        min_render.x,
        min_render.y,
        max_render.x,
        max_render.y,
        max_render.x,
        max_render.y,
        upscaled_resolution.x,
        upscaled_resolution.y
    );

    Ok(())
}

/// Linearly interpolates a render resolution between `min` and `max` by `t` in `[0, 1]`, clamping
/// the result into `[min, max]` so floating-point round-off never produces a subrect outside the
/// range DLSS reported (which would risk `InvalidParameters`).
fn lerp_resolution(min: UVec2, max: UVec2, t: f32) -> UVec2 {
    let lerp = |lo: u32, hi: u32| -> u32 {
        let value = lo as f32 + (hi as f32 - lo as f32) * t;
        (value.round() as u32).clamp(lo, hi)
    };
    UVec2::new(lerp(min.x, max.x), lerp(min.y, max.y))
}

/// Creates a 2D color-renderable texture of the given format and usage.
fn color_target(
    device: &Device,
    label: &str,
    resolution: UVec2,
    format: TextureFormat,
    usage: TextureUsages,
) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Clears `texture` to `color` with a no-draw render pass (load = clear, store).
fn clear_color(encoder: &mut CommandEncoder, texture: &Texture, color: Color) {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("dlss_input_clear"),
        color_attachments: &[Some(RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: Operations {
                load: LoadOp::Clear(color),
                store: StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}
