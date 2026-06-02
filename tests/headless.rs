//! Headless integration test for DLSS SDK initialization and graceful degradation.
//!
//! This validates the *contract* of [`DlssSdk::new`] without ever requiring DLSS to actually
//! run: against any real wgpu Dx12 device the call must resolve to either `Ok(_)` (DLSS-capable
//! NVIDIA RTX GPU + driver) or `Err(DlssError::FeatureNotSupported)` (anything else), and it must
//! never panic. No surface is created — everything is fully offscreen.
//!
//! The test is `#[ignore]`d because CI runners lack NVIDIA GPUs (and frequently any Dx12 adapter
//! at all). Run it locally on a real machine with:
//!
//! ```text
//! cargo test --test headless -- --ignored --nocapture
//! ```

use dlss_wgpu_dx12::{DlssError, DlssSdk};

/// Hardware / local-only test: spin up an offscreen wgpu Dx12 device and assert that DLSS SDK
/// initialization degrades gracefully. Ignored by default because it needs a Dx12 GPU (and is only
/// meaningful on NVIDIA RTX hardware).
#[test]
#[ignore = "hardware/local test: requires a Dx12 GPU; CI runners lack NVIDIA GPUs"]
fn sdk_init_is_ok_or_feature_not_supported() {
    let _guard = hardware_test_guard();

    // Dx12 only: this crate is the DX12 sibling of dlss_wgpu, so never enumerate other backends.
    // InstanceDescriptor is not Default in wgpu 29; build it from the canonical constructor.
    // Instance::new takes the descriptor by value.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::DX12;
    let instance = wgpu::Instance::new(descriptor);

    // Prefer a discrete / high-performance adapter — on a laptop with both an iGPU and an NVIDIA
    // dGPU this is what selects the RTX part that can actually run DLSS.
    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    })) {
        Ok(adapter) => adapter,
        // No Dx12 adapter present (typical on CI / VMs): skip rather than fail.
        Err(e) => {
            eprintln!("skipping: no Dx12 adapter available ({e})");
            return;
        }
    };

    let info = adapter.get_info();
    eprintln!(
        "using adapter: {:?} (vendor {:#06x}, backend {:?})",
        info.name, info.vendor, info.backend
    );

    // Offscreen device + queue. Default features/limits are enough; DlssSdk reaches through the HAL
    // for the raw ID3D12Device, so nothing extra is required here.
    let (device, _queue) =
        match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("dlss_wgpu_dx12 headless test device"),
            ..Default::default()
        })) {
            Ok(device_queue) => device_queue,
            Err(e) => {
                eprintln!("skipping: could not create a Dx12 device ({e})");
                return;
            }
        };

    // The contract under test: init must either succeed or degrade to FeatureNotSupported, and it
    // must never panic. A non-RTX or non-Dx12 device yields Err(FeatureNotSupported); a DLSS-capable
    // GPU yields Ok. Any other error variant is a contract violation.
    let project_id = uuid::Uuid::new_v4();
    match DlssSdk::new(project_id, device) {
        Ok(_sdk) => {
            eprintln!("DLSS Super Resolution is supported on this device");
        }
        Err(DlssError::FeatureNotSupported) => {
            eprintln!("DLSS not supported on this device (graceful degradation, as expected)");
        }
        Err(other) => panic!(
            "DlssSdk::new must return Ok or Err(FeatureNotSupported), got Err({other:?}): {other}"
        ),
    }
}

/// Hardware / local-only test for the DLSS Frame Generation path. This is the ONLY automated test
/// that drives the real Streamline interposer + the unsafe `crate::hal` reach-through on hardware:
/// `Streamline::init` (loads + signature-verifies `sl.interposer.dll`), `FrameGenerationContext::new`
/// (the `with_raw_device` / `adapter_luid` reach-through, `slSetD3DDevice`, feature-function
/// resolution, Reflex, `slDLSSGSetOptions`), one surface-free per-frame step (token + Reflex sleep +
/// simulation markers + `slSetConstants`), `query_state` decode, and the `Drop` teardown
/// (`slDLSSGSetOptions(eOff)` + device idle + `slShutdown`).
///
/// It is deliberately **windowless**, so it does NOT exercise the surface-bound reach-through
/// (`current_back_buffer_index`, `raw_resource`, `with_raw_command_list`) or actual frame
/// generation (`numFramesActuallyPresented == 2` needs a visible, composited, foreground window) —
/// those remain covered by the `frame_generation` / `sr_plus_fg` examples (the manual validation
/// path). It asserts only the *contract*: each step is `Ok` (or a typed, expected error) and nothing
/// panics. The headless unit tests in `src/streamline/frame_gen.rs` cover the call-order state
/// machine itself against a mock.
///
/// Ignored by default: it needs an NVIDIA Dx12 GPU, the Streamline SDK (`STREAMLINE_SDK` + a
/// signed interposer), and the SL plugin DLLs staged next to the test binary. Run locally with:
///
/// ```text
/// cargo test --features frame-generation --test headless -- --ignored --nocapture
/// ```
#[cfg(feature = "frame-generation")]
#[test]
#[ignore = "hardware/local test: needs an NVIDIA Dx12 GPU + Streamline SDK; CI lacks both"]
fn frame_generation_context_and_frame_contract() {
    use dlss_wgpu_dx12::{
        FgConstants, FrameGenerationContext, FrameGenerationOptions, Streamline, StreamlineError,
    };

    let _guard = hardware_test_guard();

    // (1) Streamline MUST be initialized BEFORE the wgpu::Instance is created: the wgpu fork upgrades
    // its DXGI factory to a Streamline proxy inside Instance::init only if the interposer is already
    // loaded into the process.
    let mut streamline = match Streamline::init() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: Streamline::init failed (no STREAMLINE_SDK / interposer?): {e}");
            return;
        }
    };

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::DX12;
    let instance = wgpu::Instance::new(descriptor);

    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    })) {
        Ok(adapter) => adapter,
        Err(e) => {
            eprintln!("skipping: no Dx12 adapter available ({e})");
            return;
        }
    };
    let info = adapter.get_info();
    // DLSS-G is NVIDIA-only (PCI vendor 0x10DE); on anything else it would just report
    // FeatureNotSupported, so skip to keep the test meaningful where it can actually run.
    if info.vendor != 0x10DE {
        eprintln!(
            "skipping: adapter is not NVIDIA (vendor {:#06x}); DLSS-G unavailable",
            info.vendor
        );
        return;
    }

    let (device, _queue) =
        match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("dlss_wgpu_dx12 FG headless test device"),
            ..Default::default()
        })) {
            Ok(device_queue) => device_queue,
            Err(e) => {
                eprintln!("skipping: could not create a Dx12 device ({e})");
                return;
            }
        };

    // (2) Bind the DLSS-G context (must be before any surface.configure(); we never configure one).
    // This drives the unsafe HAL reach-through (with_raw_device, adapter_luid), slSetD3DDevice,
    // feature-function resolution, Reflex, and slDLSSGSetOptions on real hardware.
    let options = FrameGenerationOptions::enabled();
    let mut ctx = match FrameGenerationContext::new(&mut streamline, &device, &adapter, &options) {
        Ok(ctx) => ctx,
        Err(StreamlineError::FeatureNotSupported(r)) => {
            eprintln!("skipping: DLSS-G not supported on this system ({r:?})");
            return;
        }
        Err(other) => panic!(
            "FrameGenerationContext::new must be Ok or Err(FeatureNotSupported), got Err({other:?}): {other}"
        ),
    };
    eprintln!(
        "FrameGenerationContext bound; DLSS-G enabled = {}",
        ctx.is_enabled()
    );

    // (3) Drive the surface-free part of one frame: slGetNewFrameToken + slReflexSleep + the
    // simulation markers + slSetConstants. acquire/tag/present need a real swapchain surface and are
    // covered by the examples, so we stop here and let the Frame drop (a windowless frame is
    // intentionally never presented; Drop logs the abort, which is expected).
    {
        let frame = ctx
            .begin_frame(0)
            .expect("begin_frame(0) should succeed on hardware");
        frame
            .set_constants(&FgConstants::new())
            .expect("set_constants should succeed on hardware");
    }

    // (4) query_state must decode without error on real hardware. We do NOT assert that DLSS-G is
    // generating (that needs a visible/composited/foreground window); we log the value for manual
    // inspection instead.
    match ctx.query_state() {
        Ok(state) => eprintln!(
            "DLSS-G query_state -> status={:?} numFramesActuallyPresented={}",
            state.status_text, state.num_frames_actually_presented
        ),
        Err(e) => panic!("query_state should decode on real hardware, got {e}"),
    }

    // (5) ctx drops here: slDLSSGSetOptions(eOff) + device idle + slShutdown must not panic.
}

// ---------------------------------------------------------------------------------------------
// Real end-to-end NGX evaluate tests (SR + RR).
//
// These are the only automated tests that run a REAL NGX DLSS evaluate on hardware and assert on the
// output — the init contract test above proves nothing past `DlssSdk::new`. They are `#[ignore]`d
// (CI has no GPU) and skip-safe (a missing GPU / SDK / DLL is a graceful skip, never a failure). They
// self-stage their NGX runtime DLL next to the test binary, so no manual setup is needed beyond
// `DLSS_SDK` pointing at the SDK clone. Run with:
//
//   cargo test --test headless -- --ignored --nocapture
//   cargo test --features ray-reconstruction --test headless -- --ignored --nocapture
//
// See docs/TESTING.md for the full validation matrix and the n=1 caveat.

/// Serialize the hardware tests within the test process. NGX (and Streamline) are process-global
/// singletons: `DlssSdk::new` runs `NVSDK_NGX_D3D12_Init` and its `Drop` runs the matching
/// `Shutdown`, so two of these tests running concurrently (the default multi-threaded test runner)
/// would tear NGX down out from under each other (observed as `NotInitialized` + an access
/// violation). Every hardware test holds this lock for its whole body, making them run one at a time
/// regardless of `--test-threads`. Poison-tolerant so one test's panic doesn't cascade.
fn hardware_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Copy `<DLSS_SDK>/lib/Windows_x86_64/rel/<dll_name>` next to the running test binary so NGX's
/// `LoadLibrary` finds it (the executable's directory is always searched first). Returns `false`
/// (a graceful skip) if `DLSS_SDK` is unset or the DLL is missing.
fn stage_ngx_dll(dll_name: &str) -> bool {
    let sdk = match std::env::var_os("DLSS_SDK") {
        Some(s) => std::path::PathBuf::from(s),
        None => {
            eprintln!("skipping: DLSS_SDK is not set (cannot stage {dll_name})");
            return false;
        }
    };
    let src = sdk
        .join("lib")
        .join("Windows_x86_64")
        .join("rel")
        .join(dll_name);
    if !src.exists() {
        eprintln!("skipping: {dll_name} not found at {}", src.display());
        return false;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let dst = exe
        .parent()
        .expect("test binary has a parent dir")
        .join(dll_name);
    match std::fs::copy(&src, &dst) {
        Ok(_) => true,
        Err(e) => {
            eprintln!(
                "skipping: could not stage {dll_name} -> {}: {e}",
                dst.display()
            );
            false
        }
    }
}

/// Build an offscreen NVIDIA DX12 device + queue, or `None` (a graceful skip) if no NVIDIA Dx12
/// adapter is present or device creation fails. `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` is
/// required so the `Rgba16Float` DLSS output can expose read-write storage usage.
fn request_nvidia_dx12_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::DX12;
    let instance = wgpu::Instance::new(descriptor);

    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    })) {
        Ok(adapter) => adapter,
        Err(e) => {
            eprintln!("skipping: no Dx12 adapter available ({e})");
            return None;
        }
    };
    let info = adapter.get_info();
    eprintln!(
        "using adapter: {:?} (vendor {:#06x})",
        info.name, info.vendor
    );
    if info.vendor != 0x10DE {
        eprintln!("skipping: adapter is not NVIDIA; DLSS is unavailable");
        return None;
    }

    match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("dlss_wgpu_dx12 evaluate test device"),
        required_features: wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ..Default::default()
    })) {
        Ok(device_queue) => Some(device_queue),
        Err(e) => {
            eprintln!("skipping: could not create a Dx12 device ({e})");
            None
        }
    }
}

/// A 2D color-renderable texture (mirrors the helper in `examples/super_resolution.rs`).
fn color_target(
    device: &wgpu::Device,
    label: &str,
    resolution: glam::UVec2,
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

/// Clear `texture` to `color` via a no-draw render pass (mirrors the SR example helper).
fn clear_color(encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, color: wgpu::Color) {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("dlss_input_clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
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

/// The `f16` bit pattern for `1.0` — the per-channel value of a white `Rgba16Float` clear. Used as
/// the pre-evaluate output sentinel: NGX overwriting it proves the evaluate actually wrote the
/// output, independent of the (synthetic) image content it produces.
const F16_ONE: u16 = 0x3C00;

/// Copy an `Rgba16Float` texture (8 bytes/texel) back to the CPU as little-endian `u16`s (the raw
/// `f16` bit patterns). `res.x` must be a multiple of 32 so the 8-bytes-per-texel row is 256-byte
/// aligned (no `copy_texture_to_buffer` padding needed).
fn read_back_rgba16f(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    res: glam::UVec2,
) -> Vec<u16> {
    let bytes_per_row = res.x * 8;
    assert_eq!(
        bytes_per_row % 256,
        0,
        "test output width must keep the Rgba16Float row 256-byte aligned"
    );
    let size = (bytes_per_row * res.y) as u64;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dlss_output_readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("dlss_output_readback_copy"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(res.y),
            },
        },
        wgpu::Extent3d {
            width: res.x,
            height: res.y,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll after readback copy");

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll for buffer map");
    rx.recv().expect("map channel").expect("buffer map");

    let data = slice.get_mapped_range();
    let pixels: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    drop(data);
    buffer.unmap();
    pixels
}

/// Assert that a real NGX evaluate wrote the output: the texture was pre-filled with the white
/// sentinel ([`F16_ONE`] per channel), so after evaluation a large fraction of the `u16` values must
/// **differ** from the sentinel. (DLSS may legitimately produce a dark image from synthetic flat
/// inputs, so we assert "NGX overwrote the sentinel", not "the output is bright".) Also logs how much
/// of the output is non-zero, for manual inspection.
fn assert_ngx_wrote_output(pixels: &[u16], label: &str) {
    let total = pixels.len();
    let changed = pixels.iter().filter(|&&v| v != F16_ONE).count();
    let nonzero = pixels.iter().filter(|&&v| v != 0).count();
    let changed_frac = changed as f32 / total as f32;
    eprintln!(
        "{label}: NGX overwrote {:.1}% of the sentinel ({:.1}% of the output is non-zero)",
        changed_frac * 100.0,
        nonzero as f32 / total as f32 * 100.0,
    );
    assert!(
        changed_frac > 0.5,
        "{label}: the NGX evaluate did not write the output (>{:.0}% still the pre-eval white \
         sentinel) — the evaluate returned Ok but produced no output",
        (1.0 - changed_frac) * 100.0
    );
}

/// Hardware / local-only: run a REAL DLSS Super Resolution evaluate end to end and confirm NGX wrote
/// a non-trivial upscaled output. Builds an offscreen Dx12 device, an SDK + SR context, allocates the
/// inputs (non-black color so the output is non-zero) + a readable output, drives an 8-frame evaluate
/// loop, reads the output back, and asserts every `render()` succeeded and the output is non-zero.
#[test]
#[ignore = "hardware/local test: needs an NVIDIA RTX GPU + DLSS SDK; CI has no GPU"]
fn dlss_super_resolution_evaluates_and_writes_output() {
    use dlss_wgpu_dx12::{
        DlssContext, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode, DlssRenderParameters,
        DlssTexture,
    };
    use glam::UVec2;
    use wgpu::{Color, TextureFormat, TextureUsages};

    let _guard = hardware_test_guard();

    if !stage_ngx_dll("nvngx_dlss.dll") {
        return;
    }
    let Some((device, queue)) = request_nvidia_dx12_device() else {
        return;
    };

    let sdk = match DlssSdk::new(uuid::Uuid::new_v4(), device.clone()) {
        Ok(sdk) => sdk,
        Err(DlssError::FeatureNotSupported) => {
            eprintln!("skipping: DLSS Super Resolution not supported on this device");
            return;
        }
        Err(e) => panic!("DlssSdk::new must be Ok or FeatureNotSupported, got: {e}"),
    };

    // 1080p output, Quality mode -> DLSS picks a smaller render resolution. 1920*8 = 15360 = 256*60,
    // so the output readback needs no row padding.
    let upscaled = UVec2::new(1920, 1080);
    let mut context = DlssContext::new(
        upscaled,
        DlssPerfQualityMode::Quality,
        DlssFeatureFlags::AutoExposure,
        sdk,
        &device,
        &queue,
    )
    .expect("DlssContext::new should succeed on a DLSS-capable device");
    let render_res = context.render_resolution();
    eprintln!("SR: render {render_res:?} -> upscaled {upscaled:?}");

    let input_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
    let color = color_target(
        &device,
        "sr_color",
        render_res,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let depth = color_target(
        &device,
        "sr_depth",
        render_res,
        TextureFormat::R32Float,
        input_usage,
    );
    let motion = color_target(
        &device,
        "sr_motion",
        render_res,
        TextureFormat::Rg16Float,
        input_usage,
    );
    let output = color_target(
        &device,
        "sr_output",
        upscaled,
        TextureFormat::Rgba16Float,
        TextureUsages::TEXTURE_BINDING
            | TextureUsages::STORAGE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::RENDER_ATTACHMENT,
    );

    // Pre-fill the output with the white sentinel; assert_ngx_wrote_output proves NGX overwrote it.
    let mut sentinel = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("sr_output_sentinel"),
    });
    clear_color(&mut sentinel, &output, Color::WHITE);
    queue.submit([sentinel.finish()]);

    const FRAMES: u32 = 8;
    for frame in 0..FRAMES {
        let jitter = context.suggested_jitter(frame, render_res);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sr_inputs"),
        });
        // A NON-black color so the upscaled output is non-trivially non-zero (a black input would
        // upscale to black and defeat the output assertion).
        clear_color(
            &mut encoder,
            &color,
            Color {
                r: 0.25,
                g: 0.5,
                b: 0.75,
                a: 1.0,
            },
        );
        clear_color(&mut encoder, &depth, Color::WHITE);
        clear_color(&mut encoder, &motion, Color::TRANSPARENT);
        queue.submit([encoder.finish()]);

        context
            .render(
                DlssRenderParameters {
                    color: DlssTexture { texture: &color },
                    depth: DlssTexture { texture: &depth },
                    motion_vectors: DlssTexture { texture: &motion },
                    exposure: DlssExposure::Automatic,
                    bias: None,
                    dlss_output: DlssTexture { texture: &output },
                    reset: frame == 0,
                    jitter_offset: jitter,
                    partial_texture_size: Some(render_res),
                    motion_vector_scale: None,
                },
                &queue,
            )
            .expect("SR evaluate should succeed on real hardware");
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after SR evaluate");
    }

    let pixels = read_back_rgba16f(&device, &queue, &output, upscaled);
    assert_ngx_wrote_output(&pixels, "SR");
}

/// Hardware / local-only: run a REAL DLSS Ray Reconstruction evaluate end to end and confirm NGX
/// wrote a non-trivial output. Same shape as the SR test, with RR's seven guide inputs and the
/// `nvngx_dlssd.dll` model.
#[cfg(feature = "ray-reconstruction")]
#[test]
#[ignore = "hardware/local test: needs an NVIDIA RTX GPU + DLSS SDK; CI has no GPU"]
fn dlss_ray_reconstruction_evaluates_and_writes_output() {
    use dlss_wgpu_dx12::{
        DepthType, DlssFeatureFlags, DlssPerfQualityMode, DlssRayReconstructionContext,
        DlssRayReconstructionParameters, DlssTexture, RoughnessMode,
    };
    use glam::UVec2;
    use wgpu::{Color, TextureFormat, TextureUsages};

    let _guard = hardware_test_guard();

    if !stage_ngx_dll("nvngx_dlssd.dll") {
        return;
    }
    let Some((device, queue)) = request_nvidia_dx12_device() else {
        return;
    };

    let sdk = match DlssSdk::new(uuid::Uuid::new_v4(), device.clone()) {
        Ok(sdk) => sdk,
        Err(DlssError::FeatureNotSupported) => {
            eprintln!("skipping: DLSS not supported on this device");
            return;
        }
        Err(e) => panic!("DlssSdk::new must be Ok or FeatureNotSupported, got: {e}"),
    };

    let upscaled = UVec2::new(1920, 1080);
    // RR forces HDR + low-res motion vectors internally; Packed roughness rides in normals.w.
    let mut context = DlssRayReconstructionContext::new(
        upscaled,
        DlssPerfQualityMode::Quality,
        RoughnessMode::Packed,
        DepthType::Hardware,
        DlssFeatureFlags::empty(),
        sdk,
        &device,
        &queue,
    )
    .expect("DlssRayReconstructionContext::new should succeed on a DLSS-capable device");
    let render_res = context.render_resolution();
    eprintln!("RR: render {render_res:?} -> upscaled {upscaled:?}");

    let input_usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
    let color = color_target(
        &device,
        "rr_color",
        render_res,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let diffuse = color_target(
        &device,
        "rr_diffuse",
        render_res,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let specular = color_target(
        &device,
        "rr_specular",
        render_res,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let normals = color_target(
        &device,
        "rr_normals",
        render_res,
        TextureFormat::Rgba16Float,
        input_usage,
    );
    let roughness = color_target(
        &device,
        "rr_roughness",
        render_res,
        TextureFormat::R16Float,
        input_usage,
    );
    let depth = color_target(
        &device,
        "rr_depth",
        render_res,
        TextureFormat::R32Float,
        input_usage,
    );
    let motion = color_target(
        &device,
        "rr_motion",
        render_res,
        TextureFormat::Rg16Float,
        input_usage,
    );
    let output = color_target(
        &device,
        "rr_output",
        upscaled,
        TextureFormat::Rgba16Float,
        TextureUsages::TEXTURE_BINDING
            | TextureUsages::STORAGE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::RENDER_ATTACHMENT,
    );

    // Pre-fill the output with the white sentinel; assert_ngx_wrote_output proves NGX overwrote it.
    let mut sentinel = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rr_output_sentinel"),
    });
    clear_color(&mut sentinel, &output, Color::WHITE);
    queue.submit([sentinel.finish()]);

    const FRAMES: u32 = 8;
    for frame in 0..FRAMES {
        let jitter = context.suggested_jitter(frame, render_res);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rr_inputs"),
        });
        clear_color(
            &mut encoder,
            &color,
            Color {
                r: 0.25,
                g: 0.5,
                b: 0.75,
                a: 1.0,
            },
        );
        clear_color(
            &mut encoder,
            &diffuse,
            Color {
                r: 0.5,
                g: 0.5,
                b: 0.5,
                a: 1.0,
            },
        );
        clear_color(
            &mut encoder,
            &specular,
            Color {
                r: 0.1,
                g: 0.1,
                b: 0.1,
                a: 1.0,
            },
        );
        // Packed roughness in normals.w; a unit-ish normal so RR has a plausible guide.
        clear_color(
            &mut encoder,
            &normals,
            Color {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                a: 0.5,
            },
        );
        clear_color(
            &mut encoder,
            &roughness,
            Color {
                r: 0.5,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
        );
        clear_color(&mut encoder, &depth, Color::WHITE);
        clear_color(&mut encoder, &motion, Color::TRANSPARENT);
        queue.submit([encoder.finish()]);

        context
            .render(
                DlssRayReconstructionParameters {
                    color: DlssTexture { texture: &color },
                    diffuse_albedo: DlssTexture { texture: &diffuse },
                    specular_albedo: DlssTexture { texture: &specular },
                    normals: DlssTexture { texture: &normals },
                    roughness: DlssTexture {
                        texture: &roughness,
                    },
                    depth: DlssTexture { texture: &depth },
                    motion_vectors: DlssTexture { texture: &motion },
                    output: DlssTexture { texture: &output },
                    reset: frame == 0,
                    jitter_offset: jitter,
                    partial_texture_size: Some(render_res),
                    motion_vector_scale: None,
                },
                &queue,
            )
            .expect("RR evaluate should succeed on real hardware");
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll after RR evaluate");
    }

    let pixels = read_back_rgba16f(&device, &queue, &output, upscaled);
    assert_ngx_wrote_output(&pixels, "RR");
}

/// Hardware / local-only: a DLAA context must render at the OUTPUT resolution (DLAA is
/// anti-aliasing only — no upscaling), so `render_resolution()` and both ends of
/// `render_resolution_range()` collapse to the upscaled resolution. Guards the DLAA branch + the H1
/// fix (render_resolution returns the optimal NGX was created with) on real hardware.
#[test]
#[ignore = "hardware/local test: needs an NVIDIA RTX GPU + DLSS SDK; CI has no GPU"]
fn dlss_dlaa_renders_at_output_resolution() {
    use dlss_wgpu_dx12::{DlssContext, DlssFeatureFlags, DlssPerfQualityMode, DlssSdk};
    use glam::UVec2;

    let _guard = hardware_test_guard();
    if !stage_ngx_dll("nvngx_dlss.dll") {
        return;
    }
    let Some((device, queue)) = request_nvidia_dx12_device() else {
        return;
    };

    let sdk = match DlssSdk::new(uuid::Uuid::new_v4(), device.clone()) {
        Ok(sdk) => sdk,
        Err(DlssError::FeatureNotSupported) => {
            eprintln!("skipping: DLSS not supported on this device");
            return;
        }
        Err(e) => panic!("DlssSdk::new must be Ok or FeatureNotSupported, got: {e}"),
    };

    let upscaled = UVec2::new(1920, 1080);
    let context = DlssContext::new(
        upscaled,
        DlssPerfQualityMode::Dlaa,
        DlssFeatureFlags::AutoExposure,
        sdk,
        &device,
        &queue,
    )
    .expect("DLAA context creation should succeed");

    assert_eq!(
        context.render_resolution(),
        upscaled,
        "DLAA must render at the output resolution (no upscaling)"
    );
    let range = context.render_resolution_range();
    assert_eq!(*range.start(), upscaled);
    assert_eq!(*range.end(), upscaled);
    eprintln!("DLAA render_resolution == {upscaled:?} (output); range is degenerate");
}
