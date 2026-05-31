//! Headless DLSS Super Resolution example.
//!
//! Exercises the full DLSS Super Resolution path on real hardware without a window or surface:
//! it builds a DX12 [`wgpu::Instance`], picks an NVIDIA adapter, creates a [`wgpu::Device`] +
//! [`wgpu::Queue`], initializes a [`DlssSdk`] and a [`DlssContext`], allocates render- and
//! upscaled-resolution textures, then drives a short evaluation loop and submits the work to the
//! GPU.
//!
//! The textures are never populated with a real render — DLSS will happily upscale uninitialized
//! (cleared) inputs, which is all that is needed to confirm the integration links, binds the raw
//! `ID3D12Resource` handles, and evaluates end to end on this machine.
//!
//! Run with the SDK + libclang environment variables set (see the crate README), e.g.:
//!
//! ```powershell
//! $env:DLSS_SDK='C:/Users/jorda/dlss_sdk'
//! $env:LIBCLANG_PATH='C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//! cargo run --example super_resolution
//! ```
//!
//! If DLSS is not supported on this system (non-RTX GPU, missing driver, or a non-Dx12 device),
//! the example prints a message and exits cleanly rather than panicking.

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

/// How many frames to evaluate. DLSS is temporal, so a handful of frames is enough to confirm the
/// feature accumulates history without complaining.
const FRAME_COUNT: u32 = 8;

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
            eprintln!("DLSS Super Resolution example failed: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), DlssError> {
    // 1. Build a DX12 instance with wgpu's default shader compiler, so the example runs without
    //    shipping dxcompiler.dll. A real app that authors Shader Model 6+ HLSL would instead build
    //    its instance with `dlss_wgpu_dx12::dxc_instance_descriptor()` and ship dxcompiler.dll.
    //    DLSS itself needs neither.
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
        label: Some("dlss_super_resolution"),
        required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ..Default::default()
    }))
    .expect("failed to create a DX12 device");

    // 4. Initialize the application-wide DLSS / NGX SDK once.
    let sdk = DlssSdk::new(PROJECT_ID, device.clone())?;

    // 5. Create a per-camera DLSS context at the chosen output resolution + quality mode.
    let mut context = DlssContext::new(
        UPSCALED_RESOLUTION,
        DlssPerfQualityMode::Quality,
        // AutoExposure pairs with DlssExposure::Automatic below; motion vectors are at render
        // resolution (the default), so no LowResolutionMotionVectors flag is set.
        DlssFeatureFlags::AutoExposure,
        sdk,
        &device,
        &queue,
    )?;

    // Render at the recommended (lowest-cost) render resolution and let DLSS upscale to the output.
    // Inputs are allocated at exactly this size and the eval subrect is pinned to match it via
    // `partial_texture_size` below. (A dynamic-resolution renderer would instead allocate at the max
    // of `render_resolution_range()` and vary the subrect per frame.)
    let render_resolution = context.render_resolution();
    let upscaled_resolution = context.upscaled_resolution();
    println!(
        "DLSS render resolution: {}x{}",
        render_resolution.x, render_resolution.y
    );
    println!(
        "DLSS upscaled resolution: {}x{}",
        upscaled_resolution.x, upscaled_resolution.y
    );

    // 6. Allocate the DLSS input textures at render resolution and the output at upscaled
    //    resolution. Inputs are sampled (TEXTURE_BINDING) and cleared each frame via render passes
    //    (RENDER_ATTACHMENT); the output is a UAV (STORAGE_BINDING).
    let input_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;

    let color = color_target(
        &device,
        "dlss_color",
        render_resolution,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    // DLSS depth bound as a sampled R32Float resource (no depth-stencil semantics needed at the
    // NGX boundary on D3D12).
    let depth = color_target(
        &device,
        "dlss_depth",
        render_resolution,
        TextureFormat::R32Float,
        input_usage,
    );
    let motion_vectors = color_target(
        &device,
        "dlss_motion_vectors",
        render_resolution,
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

    // 7. Drive the evaluation loop.
    for frame_number in 0..FRAME_COUNT {
        let jitter_offset = context.suggested_jitter(frame_number, render_resolution);

        // Produce (here: clear) the DLSS inputs and submit that work BEFORE evaluating DLSS, so the
        // resource transitions inside render() observe the post-render states. A real renderer would
        // draw the scene here instead of clearing.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_super_resolution_inputs"),
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
                partial_texture_size: Some(render_resolution),
                motion_vector_scale: None,
            },
            &queue,
        )?;

        device
            .poll(PollType::wait_indefinitely())
            .map_err(|_| DlssError::PlatformError)?;
    }

    println!(
        "Success: evaluated {FRAME_COUNT} DLSS Super Resolution frames \
         ({}x{} -> {}x{}).",
        render_resolution.x, render_resolution.y, upscaled_resolution.x, upscaled_resolution.y
    );

    Ok(())
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
