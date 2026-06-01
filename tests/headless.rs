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
    let (device, _queue) = match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
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
    eprintln!("FrameGenerationContext bound; DLSS-G enabled = {}", ctx.is_enabled());

    // (3) Drive the surface-free part of one frame: slGetNewFrameToken + slReflexSleep + the
    // simulation markers + slSetConstants. acquire/tag/present need a real swapchain surface and are
    // covered by the examples, so we stop here and let the Frame drop (a windowless frame is
    // intentionally never presented; Drop logs the abort, which is expected).
    {
        let frame = ctx.begin_frame(0).expect("begin_frame(0) should succeed on hardware");
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
