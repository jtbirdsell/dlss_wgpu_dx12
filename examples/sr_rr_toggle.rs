//! Headless example: ONE shared [`DlssSdk`] driving BOTH a Super Resolution context and a Ray
//! Reconstruction context, toggling between them across phases.
//!
//! ## SR and RR are MUTUALLY EXCLUSIVE *per pass* — read this first
//!
//! DLSS Super Resolution (SR, `DlssContext`) and DLSS Ray Reconstruction (RR,
//! `DlssRayReconstructionContext`) are two **different NGX features** that solve overlapping
//! problems: SR upscales an already-shaded image; RR denoises *and* upscales a noisy path-traced
//! image in one pass. A given upscale pass uses **exactly one of them, never both**. You do NOT feed
//! a frame through SR and then RR, and you do NOT evaluate both on the same rendered frame.
//!
//! This example holds BOTH contexts alive at once (a perfectly supported thing to do — they share
//! one [`DlssSdk`]) but **only evaluates one per frame**: frames `0..N` run SR, frames `N..2N` run
//! RR. That is the supported toggle pattern — e.g. an app that switches between a rasterized
//! pipeline (SR) and a path-traced pipeline (RR) at runtime keeps both contexts cached and dispatches
//! to whichever matches the current frame. The loud comments in the loop mark the single-feature-
//! per-pass boundary so nobody "optimizes" it into evaluating both on one pass (which is unsupported
//! and would corrupt history / waste the pass).
//!
//! Like the `super_resolution.rs` / `ray_reconstruction.rs` examples this is headless: it builds a
//! DX12 instance, picks an NVIDIA adapter, checks the vendor, allocates cleared inputs, drives the
//! toggle loop, and exits cleanly with [`DlssError::FeatureNotSupported`] on a non-RTX system.
//!
//! Requires the `ray-reconstruction` feature (SR is in the default feature set):
//!
//! ```powershell
//! $env:DLSS_SDK='C:/Users/jorda/dlss_sdk'
//! $env:LIBCLANG_PATH='C:/Users/jorda/AppData/Roaming/Python/Python314/site-packages/clang/native'
//! cargo run --example sr_rr_toggle --features ray-reconstruction
//! ```

use dlss_wgpu_dx12::{
    DepthType, DlssContext, DlssError, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode,
    DlssRayReconstructionContext, DlssRayReconstructionParameters, DlssRenderParameters, DlssSdk,
    DlssTexture, RoughnessMode,
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
/// Frames per phase: `N` SR frames followed by `N` RR frames (so `2 * N` frames total).
const FRAMES_PER_PHASE: u32 = 8;

fn main() {
    match run() {
        Ok(()) => {}
        Err(DlssError::FeatureNotSupported) => {
            eprintln!(
                "DLSS Super Resolution / Ray Reconstruction is not supported on this system \
                 (requires an NVIDIA RTX GPU, a recent driver, and the wgpu Dx12 backend). Skipping."
            );
        }
        Err(error) => {
            eprintln!("DLSS SR/RR toggle example failed: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), DlssError> {
    // --- Bring-up: instance + NVIDIA adapter + device, mirroring the two source examples. ---
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
        label: Some("dlss_sr_rr_toggle"),
        required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ..Default::default()
    }))
    .expect("failed to create a DX12 device");

    // --- ONE shared SDK, cloned into BOTH contexts. ----------------------------------------------
    // `DlssSdk::new` returns an `Arc<Mutex<DlssSdk>>`; cloning the Arc shares the single NGX SDK
    // instance (and its serializing mutex) between the SR and RR contexts. NGX is not thread-safe,
    // so the mutex guarantees their evaluates never overlap — which is also why a SINGLE pass can
    // only run one feature at a time.
    let sdk = DlssSdk::new(PROJECT_ID, device.clone())?;

    // SR context: standard upscaler. AutoExposure pairs with DlssExposure::Automatic below.
    let mut sr_ctx = DlssContext::new(
        UPSCALED_RESOLUTION,
        DlssPerfQualityMode::Quality,
        DlssFeatureFlags::AutoExposure,
        sdk.clone(),
        &device,
        &queue,
    )?;

    // RR context: denoise + upscale. Shares the SAME `sdk` Arc (cloned). HDR + low-res motion
    // vectors are enforced inside `DlssRayReconstructionContext::new`.
    let mut rr_ctx = DlssRayReconstructionContext::new(
        UPSCALED_RESOLUTION,
        DlssPerfQualityMode::Quality,
        RoughnessMode::Unpacked,
        DepthType::Linear,
        DlssFeatureFlags::HighDynamicRange,
        sdk,
        &device,
        &queue,
    )?;

    let sr_render = sr_ctx.render_resolution();
    let rr_render = rr_ctx.render_resolution();
    let upscaled = sr_ctx.upscaled_resolution();
    println!(
        "SR render resolution: {}x{}; RR render resolution: {}x{}; shared output: {}x{}",
        sr_render.x, sr_render.y, rr_render.x, rr_render.y, upscaled.x, upscaled.y
    );

    // --- Allocate BOTH input sets up front (cleared/zero-init is fine for a link-up demo). -------
    let input_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
    let output_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::STORAGE_BINDING;

    // SR inputs (color/depth/mvec) at SR render resolution + a dedicated SR output.
    let sr_color = target(
        &device,
        "sr_color",
        sr_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let sr_depth = target(
        &device,
        "sr_depth",
        sr_render,
        TextureFormat::R32Float,
        input_usage,
    );
    let sr_mvec = target(
        &device,
        "sr_mvec",
        sr_render,
        TextureFormat::Rg16Float,
        input_usage,
    );
    let sr_output = target(
        &device,
        "sr_output",
        upscaled,
        TextureFormat::Rgba16Float,
        output_usage,
    );

    // RR inputs: the 8 guide buffers from ray_reconstruction.rs at RR render resolution + an output.
    let rr_color = target(
        &device,
        "rr_color",
        rr_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let rr_diffuse = target(
        &device,
        "rr_diffuse",
        rr_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let rr_specular = target(
        &device,
        "rr_specular",
        rr_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let rr_normals = target(
        &device,
        "rr_normals",
        rr_render,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let rr_roughness = target(
        &device,
        "rr_roughness",
        rr_render,
        TextureFormat::R16Float,
        input_usage,
    );
    let rr_depth = target(
        &device,
        "rr_depth",
        rr_render,
        TextureFormat::R32Float,
        input_usage,
    );
    let rr_mvec = target(
        &device,
        "rr_mvec",
        rr_render,
        TextureFormat::Rg16Float,
        input_usage,
    );
    let rr_output = target(
        &device,
        "rr_output",
        upscaled,
        TextureFormat::Rgba16Float,
        output_usage,
    );

    // --- Toggle loop: SR for the first phase, RR for the second. ---------------------------------
    let total_frames = FRAMES_PER_PHASE * 2;
    for frame_number in 0..total_frames {
        // `phase_frame` resets to 0 at the start of each phase so each feature sees a clean
        // `reset == true` on its own first frame (a fresh temporal history per feature).
        let in_sr_phase = frame_number < FRAMES_PER_PHASE;
        let phase_frame = if in_sr_phase {
            frame_number
        } else {
            frame_number - FRAMES_PER_PHASE
        };

        if in_sr_phase {
            // =====================================================================================
            // SR PHASE. We evaluate ONLY Super Resolution this pass. The RR context is alive and
            // its inputs are allocated, but we DO NOT touch `rr_ctx` here: SR and RR are mutually
            // exclusive per pass, so evaluating both on this frame is UNSUPPORTED. Single feature
            // per pass — full stop.
            // =====================================================================================
            if phase_frame == 0 {
                println!("--- SR phase: evaluating DlssContext (Super Resolution) ---");
            }
            println!("frame {frame_number}: SR (Super Resolution)");

            let jitter_offset = sr_ctx.suggested_jitter(phase_frame, sr_render);

            // Produce (clear) the SR inputs and submit BEFORE evaluating, so render()'s transitions
            // observe the post-render states.
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sr_inputs"),
            });
            clear(&mut encoder, &sr_color, Color::BLACK);
            clear(&mut encoder, &sr_depth, Color::WHITE);
            clear(&mut encoder, &sr_mvec, Color::TRANSPARENT);
            queue.submit([encoder.finish()]);

            sr_ctx.render(
                DlssRenderParameters {
                    color: DlssTexture { texture: &sr_color },
                    depth: DlssTexture { texture: &sr_depth },
                    motion_vectors: DlssTexture { texture: &sr_mvec },
                    exposure: DlssExposure::Automatic,
                    bias: None,
                    dlss_output: DlssTexture {
                        texture: &sr_output,
                    },
                    reset: phase_frame == 0,
                    jitter_offset,
                    partial_texture_size: Some(sr_render),
                    motion_vector_scale: None,
                },
                &queue,
            )?;
        } else {
            // =====================================================================================
            // RR PHASE. We evaluate ONLY Ray Reconstruction this pass. The SR context is still alive
            // and its inputs are still allocated, but we DO NOT touch `sr_ctx` here: SR and RR are
            // mutually exclusive per pass. One feature per pass — never both on the same frame.
            // =====================================================================================
            if phase_frame == 0 {
                println!(
                    "--- RR phase: evaluating DlssRayReconstructionContext (Ray Reconstruction) ---"
                );
            }
            println!("frame {frame_number}: RR (Ray Reconstruction)");

            let jitter_offset = rr_ctx.suggested_jitter(phase_frame, rr_render);

            // Produce (clear) the RR guide buffers and submit BEFORE evaluating.
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rr_inputs"),
            });
            for tex in [
                &rr_color,
                &rr_diffuse,
                &rr_specular,
                &rr_normals,
                &rr_roughness,
                &rr_depth,
                &rr_mvec,
            ] {
                clear(&mut encoder, tex, Color::BLACK);
            }
            queue.submit([encoder.finish()]);

            rr_ctx.render(
                DlssRayReconstructionParameters {
                    color: DlssTexture { texture: &rr_color },
                    diffuse_albedo: DlssTexture {
                        texture: &rr_diffuse,
                    },
                    specular_albedo: DlssTexture {
                        texture: &rr_specular,
                    },
                    normals: DlssTexture {
                        texture: &rr_normals,
                    },
                    roughness: DlssTexture {
                        texture: &rr_roughness,
                    },
                    depth: DlssTexture { texture: &rr_depth },
                    motion_vectors: DlssTexture { texture: &rr_mvec },
                    output: DlssTexture {
                        texture: &rr_output,
                    },
                    reset: phase_frame == 0,
                    jitter_offset,
                    partial_texture_size: Some(rr_render),
                    motion_vector_scale: None,
                },
                &queue,
            )?;
        }

        device
            .poll(PollType::wait_indefinitely())
            .map_err(|_| DlssError::PlatformError)?;
    }

    println!(
        "Success: evaluated {FRAMES_PER_PHASE} SR frames then {FRAMES_PER_PHASE} RR frames \
         (one feature per pass; both contexts shared one DlssSdk)."
    );
    Ok(())
}

/// Creates a 2D texture of the given format and usage at `resolution`.
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

/// Clears `texture` to `color` with a no-draw render pass (load = clear, store).
fn clear(encoder: &mut CommandEncoder, texture: &Texture, color: Color) {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("sr_rr_input_clear"),
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
