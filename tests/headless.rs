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
