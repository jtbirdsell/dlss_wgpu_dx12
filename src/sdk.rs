use crate::{feature_info::with_feature_info, hal::with_raw_device, nvsdk_ngx::*};
use std::{
    ptr,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Application-wide DLSS / NGX object. Create this once per application and share it (via the
/// returned `Arc<Mutex<_>>`) across all DLSS contexts.
///
/// NGX is **not** thread-safe, so every NGX call is serialized behind the `Mutex`.
///
/// **Drop order matters:** drop all [`crate::DlssContext`] /
/// `DlssRayReconstructionContext` instances (and submit any command encoder you passed to
/// their `new`/`render`) *before* the last `Arc<Mutex<DlssSdk>>` clone is dropped. The SDK's `Drop`
/// waits for the GPU to idle, then destroys the NGX parameters and shuts NGX down for the device;
/// releasing it while feature handles are still live is unsupported.
///
/// **Frame Generation does not use this object.** FG runs through a separate `Streamline` handle with
/// its own init/drop ordering (the `frame-generation` feature) — SR/RR and FG are independent NGX
/// entry points, so an app using both manages two lifecycles.
pub struct DlssSdk {
    pub(crate) parameters: *mut NVSDK_NGX_Parameter,
    pub(crate) device: wgpu::Device,
}

impl DlssSdk {
    /// Initializes the NGX SDK against `device`'s underlying D3D12 device.
    ///
    /// With the default `super-resolution` feature this also verifies that DLSS Super Resolution is
    /// available on this GPU + driver and returns [`DlssError::FeatureNotSupported`] if it is not, so
    /// construction currently requires SR support. In an RR-only build
    /// (`--no-default-features --features ray-reconstruction`) that SR-specific probe is skipped:
    /// NGX is initialized and a `DlssRayReconstructionContext` primes its own feature state on
    /// create.
    ///
    /// Returns [`DlssError::FeatureNotSupported`] if `device` is not a wgpu Dx12 device, or if the
    /// hardware/driver does not support DLSS — callers should then fall back to a plain device.
    ///
    /// `project_id` must be a GUID-style identifier (NGX rejects malformed ids).
    pub fn new(project_id: Uuid, device: wgpu::Device) -> Result<Arc<Mutex<Self>>, DlssError> {
        unsafe {
            let mut parameters: *mut NVSDK_NGX_Parameter = ptr::null_mut();

            // Initialize NGX with the raw ID3D12Device, then fetch the capability parameter block.
            let init = with_raw_device(&device, |raw_device| {
                with_feature_info(project_id, |feature_info| {
                    check_ngx_result(NVSDK_NGX_D3D12_Init_with_ProjectID(
                        feature_info.Identifier.v.ProjectDesc.ProjectId,
                        NVSDK_NGX_EngineType_NVSDK_NGX_ENGINE_TYPE_CUSTOM,
                        feature_info.Identifier.v.ProjectDesc.EngineVersion,
                        feature_info.ApplicationDataPath,
                        raw_device,
                        feature_info.FeatureInfo,
                        NVSDK_NGX_Version_NVSDK_NGX_Version_API,
                    ))?;
                    check_ngx_result(NVSDK_NGX_D3D12_GetCapabilityParameters(&mut parameters))
                })
            });
            match init {
                // `None` => the adapter/device is not the Dx12 backend.
                None => return Err(DlssError::FeatureNotSupported),
                Some(Err(e)) => return Err(e),
                Some(Ok(())) => {}
            }

            // DLSS Super Resolution capability probe, gated on `super-resolution` (on by default).
            // An RR-only build (`--no-default-features --features ray-reconstruction`) skips it, so
            // it neither pays the dead SR probe nor is rejected on a (hypothetical, non-existent on
            // NVIDIA hardware) SR-unavailable-but-RR-capable GPU. NGX is still initialized above with
            // the SuperSampling FeatureID; RR primes its own feature state at create, so skipping
            // this SR-only probe is sound.
            #[cfg(feature = "super-resolution")]
            {
                // Is DLSS Super Resolution actually supported by this hardware + driver?
                let mut dlss_supported: i32 = 0;
                let result = check_ngx_result(NVSDK_NGX_Parameter_GetI(
                    parameters,
                    NVSDK_NGX_Parameter_SuperSampling_Available.as_ptr().cast(),
                    &mut dlss_supported,
                ));
                // Factor the pure supported/unsupported decision out (unit-tested below without a
                // device); the NGX cleanup + error propagation below are unchanged.
                if !sr_capability_supported(result.is_ok(), dlss_supported) {
                    let _ = check_ngx_result(NVSDK_NGX_D3D12_DestroyParameters(parameters));
                    result?;
                    return Err(DlssError::FeatureNotSupported);
                }
            }

            Ok(Arc::new(Mutex::new(Self { parameters, device })))
        }
    }

    /// Returns the number of bytes of VRAM currently allocated by DLSS.
    pub fn vram_allocated_bytes(&self) -> Result<u64, DlssError> {
        let mut bytes: u64 = 0;
        unsafe { check_ngx_result(NGX_DLSS_GET_STATS(self.parameters, &mut bytes))? };
        Ok(bytes)
    }
}

impl Drop for DlssSdk {
    fn drop(&mut self) {
        // Ensure the GPU is idle before tearing down NGX state.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        unsafe {
            with_raw_device(&self.device, |raw_device| {
                let _ = check_ngx_result(NVSDK_NGX_D3D12_DestroyParameters(self.parameters));
                let _ = check_ngx_result(NVSDK_NGX_D3D12_Shutdown1(raw_device));
            });
        }
    }
}

/// Pure decision for the DLSS Super Resolution capability probe in [`DlssSdk::new`], factored out so
/// the supported/unsupported branch is unit-assertable without a real NGX device. SR is supported
/// iff the capability query itself succeeded (`probe_ok`) *and* NGX reported a non-zero availability
/// flag (`dlss_supported`). A failed query (`!probe_ok`) or a zero flag both mean "not supported"; the
/// caller then destroys the parameter block and returns [`DlssError::FeatureNotSupported`].
#[cfg(feature = "super-resolution")]
pub(crate) fn sr_capability_supported(probe_ok: bool, dlss_supported: i32) -> bool {
    probe_ok && dlss_supported != 0
}

// SAFETY: the raw `parameters` pointer is only ever touched while the owning `Mutex` is held, and
// NGX tolerates use from a single thread at a time. The wgpu `Device` is itself `Send + Sync`.
unsafe impl Send for DlssSdk {}
unsafe impl Sync for DlssSdk {}

#[cfg(all(test, feature = "super-resolution"))]
mod tests {
    use super::sr_capability_supported;

    #[test]
    fn sr_capability_decision_truth_table() {
        // Supported ONLY when the capability query succeeded AND NGX reports a non-zero flag.
        assert!(
            sr_capability_supported(true, 1),
            "probe ok + non-zero availability flag must report SR supported"
        );
        assert!(
            sr_capability_supported(true, i32::MAX),
            "any non-zero availability flag counts as supported"
        );
        // A zero availability flag means unsupported even though the query itself succeeded — this is
        // the non-RTX / unsupported-driver branch that previously had zero off-hardware coverage.
        assert!(
            !sr_capability_supported(true, 0),
            "probe ok but zero availability flag must report SR unsupported"
        );
        // A failed capability query is unsupported regardless of whatever was left in the out-param.
        assert!(
            !sr_capability_supported(false, 0),
            "failed probe must report unsupported"
        );
        assert!(
            !sr_capability_supported(false, 1),
            "a failed probe must report unsupported even if the stale out-param looks non-zero"
        );
    }
}
