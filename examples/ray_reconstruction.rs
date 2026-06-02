//! Headless DLSS Ray Reconstruction (DLSS-D) example.
//!
//! Exercises the full Ray Reconstruction path on real hardware without a window or surface: it
//! builds a DX12 [`wgpu::Instance`], picks an NVIDIA adapter, creates a [`DlssSdk`] and a
//! [`DlssRayReconstructionContext`], allocates the render-resolution guide buffers (noisy color,
//! diffuse/specular albedo, normals, roughness, depth, motion vectors) plus an upscaled output, and
//! drives a short evaluation loop.
//!
//! Requires the `ray-reconstruction` feature:
//!
//! ```powershell
//! $env:DLSS_SDK='C:/Users/jorda/dlss_sdk'
//! $env:LIBCLANG_PATH='C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//! cargo run --example ray_reconstruction --features ray-reconstruction
//! ```

use dlss_wgpu_dx12::{
    DepthType, DlssError, DlssFeatureFlags, DlssPerfQualityMode, DlssRayReconstructionContext,
    DlssRayReconstructionParameters, DlssSdk, DlssTexture, RoughnessMode,
};
use glam::UVec2;
use uuid::Uuid;
use wgpu::{
    Backends, Color, CommandEncoder, Device, DeviceDescriptor, Extent3d, Instance,
    InstanceDescriptor, LoadOp, Operations, PollType, PowerPreference, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, StoreOp, Texture, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsages,
};

const PROJECT_ID: Uuid = Uuid::from_u128(0x9b8d_2f41_6c7a_4e15_8d3b_a0f2_71e4_55c9);
const UPSCALED_RESOLUTION: UVec2 = UVec2::new(3840, 2160);
const FRAME_COUNT: u32 = 8;

fn main() {
    match run() {
        Ok(()) => {}
        Err(DlssError::FeatureNotSupported) => {
            eprintln!(
                "DLSS Ray Reconstruction is not supported on this system (requires an NVIDIA RTX \
                 GPU, a recent driver, and the wgpu Dx12 backend). Skipping."
            );
        }
        Err(error) => {
            eprintln!("DLSS Ray Reconstruction example failed: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), DlssError> {
    let mut instance_descriptor = InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = Backends::DX12;
    let instance = Instance::new(instance_descriptor);

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

    let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("dlss_ray_reconstruction"),
        required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ..Default::default()
    }))
    .expect("failed to create a DX12 device");

    let sdk = DlssSdk::new(PROJECT_ID, device.clone())?;

    // Ray Reconstruction: separate (unpacked) roughness texture and a hardware depth buffer.
    let mut context = DlssRayReconstructionContext::new(
        UPSCALED_RESOLUTION,
        DlssPerfQualityMode::Quality,
        RoughnessMode::Unpacked,
        DepthType::Linear,
        // Ray Reconstruction requires HDR color (enforced in DlssRayReconstructionContext::new too).
        DlssFeatureFlags::HighDynamicRange,
        sdk,
        &device,
        &queue,
    )?;

    let render_resolution = context.render_resolution();
    let upscaled_resolution = context.upscaled_resolution();
    println!(
        "DLSS-RR render resolution: {}x{}",
        render_resolution.x, render_resolution.y
    );
    println!(
        "DLSS-RR upscaled resolution: {}x{}",
        upscaled_resolution.x, upscaled_resolution.y
    );

    // Render-resolution guide buffers. A real path tracer would write these; here they are cleared.
    let input = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
    let color = target(
        &device,
        "rr_color",
        render_resolution,
        TextureFormat::Rgba16Float,
        input,
    );
    let diffuse_albedo = target(
        &device,
        "rr_diffuse",
        render_resolution,
        TextureFormat::Rgba16Float,
        input,
    );
    let specular_albedo = target(
        &device,
        "rr_specular",
        render_resolution,
        TextureFormat::Rgba16Float,
        input,
    );
    let normals = target(
        &device,
        "rr_normals",
        render_resolution,
        TextureFormat::Rgba16Float,
        input,
    );
    let roughness = target(
        &device,
        "rr_roughness",
        render_resolution,
        TextureFormat::R16Float,
        input,
    );
    let depth = target(
        &device,
        "rr_depth",
        render_resolution,
        TextureFormat::R32Float,
        input,
    );
    let motion_vectors = target(
        &device,
        "rr_motion",
        render_resolution,
        TextureFormat::Rg16Float,
        input,
    );
    let output = target(
        &device,
        "rr_output",
        upscaled_resolution,
        TextureFormat::Rgba16Float,
        TextureUsages::TEXTURE_BINDING | TextureUsages::STORAGE_BINDING,
    );

    for frame_number in 0..FRAME_COUNT {
        let jitter_offset = context.suggested_jitter(frame_number, render_resolution);

        // Produce (clear) the guide buffers and submit before evaluating, so the resource
        // transitions inside render() observe the post-render states.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_rr_inputs"),
        });
        for tex in [
            &color,
            &diffuse_albedo,
            &specular_albedo,
            &normals,
            &roughness,
            &depth,
            &motion_vectors,
        ] {
            clear(&mut encoder, tex, Color::BLACK);
        }
        queue.submit([encoder.finish()]);

        context.render(
            DlssRayReconstructionParameters {
                color: DlssTexture { texture: &color },
                diffuse_albedo: DlssTexture {
                    texture: &diffuse_albedo,
                },
                specular_albedo: DlssTexture {
                    texture: &specular_albedo,
                },
                normals: DlssTexture { texture: &normals },
                roughness: DlssTexture {
                    texture: &roughness,
                },
                depth: DlssTexture { texture: &depth },
                motion_vectors: DlssTexture {
                    texture: &motion_vectors,
                },
                output: DlssTexture { texture: &output },
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
        "Success: evaluated {FRAME_COUNT} DLSS Ray Reconstruction frames ({}x{} -> {}x{}).",
        render_resolution.x, render_resolution.y, upscaled_resolution.x, upscaled_resolution.y
    );
    Ok(())
}

fn target(
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

fn clear(encoder: &mut CommandEncoder, texture: &Texture, color: Color) {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("rr_input_clear"),
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
