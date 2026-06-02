//! The shared NGX-feature core behind [`crate::DlssContext`] (Super Resolution) and
//! [`crate::DlssRayReconstructionContext`] (Ray Reconstruction).
//!
//! The two public contexts are the same wrapper with the NGX call names + create/eval param structs
//! swapped. Everything that does NOT differ — the feature-create command-encoder dance, the
//! transitions-then-evaluate submission, the resolution getters, the GPU-idle-then-release `Drop`,
//! and the `Send`/`Sync` rationale — lives here once, so a fix to the encoder ordering, the SDK
//! `Mutex` discipline, or the unsafe FFI contract is made in a single place rather than kept in sync
//! by hand (they had already drifted). The variant parts (the GET_OPTIMAL_SETTINGS query, the
//! CREATE_*_EXT call, and the EVALUATE_*_EXT call) are supplied by the contexts as closures.

use crate::{
    DlssError, DlssPerfQualityMode, DlssSdk, hal::with_raw_command_list, jitter::halton_sequence,
    nvsdk_ngx::*,
};
use glam::{UVec2, Vec2};
use std::{
    iter,
    ops::RangeInclusive,
    ptr,
    sync::{Arc, Mutex},
};
use wgpu::TextureTransition;

/// The render/target resolutions an NGX feature was created with (after any DLAA collapse).
#[derive(Clone, Copy)]
pub(crate) struct Resolutions {
    /// The upscaled (output) resolution.
    pub upscaled: UVec2,
    /// The optimal render resolution NGX was created with (`InWidth`/`InHeight`).
    pub optimal: UVec2,
    /// The minimum render resolution for dynamic-resolution scaling.
    pub min: UVec2,
    /// The maximum render resolution for dynamic-resolution scaling.
    pub max: UVec2,
}

/// A created NGX feature (SR or RR) plus the mechanics both public contexts share.
pub(crate) struct NgxFeature {
    resolutions: Resolutions,
    device: wgpu::Device,
    sdk: Arc<Mutex<DlssSdk>>,
    feature: *mut NVSDK_NGX_Handle,
}

impl NgxFeature {
    /// Create an NGX feature on a throwaway command encoder and submit it. `optimal_settings` runs
    /// the feature-specific `GET_OPTIMAL_SETTINGS` query (returning `(optimal, min, max)`); the shared
    /// DLAA override then collapses them to the output resolution; `create_feature` performs the
    /// feature-specific `CREATE_*_EXT` with the open command list + the (locked) NGX parameters,
    /// writing the feature handle. The SDK `Mutex` is held across the whole create (NGX is not
    /// thread-safe).
    pub(crate) fn create(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        sdk: Arc<Mutex<DlssSdk>>,
        upscaled_resolution: UVec2,
        perf_quality_mode: DlssPerfQualityMode,
        optimal_settings: impl FnOnce(
            *mut NVSDK_NGX_Parameter,
        ) -> Result<(UVec2, UVec2, UVec2), DlssError>,
        create_feature: impl FnOnce(
            *mut ID3D12GraphicsCommandList,
            Resolutions,
            *mut NVSDK_NGX_Parameter,
            &mut *mut NVSDK_NGX_Handle,
        ) -> NVSDK_NGX_Result,
    ) -> Result<Self, DlssError> {
        // Poison-tolerant lock: NGX is serialized and a poisoned guard does not invalidate the
        // parameter pointer, so recovering it is sound and avoids a panic.
        let locked_sdk = sdk.lock().unwrap_or_else(|p| p.into_inner());

        let (mut optimal, mut min, mut max) = optimal_settings(locked_sdk.parameters)?;
        if perf_quality_mode == DlssPerfQualityMode::Dlaa {
            // DLAA is anti-aliasing only: render at the output resolution (no upscaling).
            optimal = upscaled_resolution;
            min = upscaled_resolution;
            max = upscaled_resolution;
        }
        let resolutions = Resolutions {
            upscaled: upscaled_resolution,
            optimal,
            min,
            max,
        };

        // NGX records initialization work onto a command list; use a throwaway encoder + submit.
        let mut command_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_feature_creation"),
        });
        let mut feature: *mut NVSDK_NGX_Handle = ptr::null_mut();
        // SAFETY: `with_raw_command_list` hands the closure the encoder's open raw command list; the
        // closure forwards it (with the locked NGX parameters and a valid `&mut feature` out-param)
        // to the NGX CREATE_*_EXT export.
        let created = unsafe {
            with_raw_command_list(&mut command_encoder, |cmd_list| {
                check_ngx_result(create_feature(
                    cmd_list,
                    resolutions,
                    locked_sdk.parameters,
                    &mut feature,
                ))
            })
        };
        match created {
            None => return Err(DlssError::FeatureNotSupported), // encoder is not a Dx12 encoder
            Some(result) => result?,
        }

        queue.submit([command_encoder.finish()]);
        drop(locked_sdk);

        Ok(Self {
            resolutions,
            device: device.clone(),
            sdk,
            feature,
        })
    }

    /// Record the resource transitions + the raw NGX evaluate on two ordered encoders and submit
    /// them transitions-first. `barriers` is the wgpu transition list (inputs → shader-readable,
    /// output → UAV); `evaluate` performs the feature-specific `EVALUATE_*_EXT` with the open command
    /// list, the feature handle, and the (locked) NGX parameters.
    pub(crate) fn evaluate<'a>(
        &self,
        queue: &wgpu::Queue,
        barriers: impl Iterator<Item = TextureTransition<&'a wgpu::Texture>>,
        evaluate: impl FnOnce(
            *mut ID3D12GraphicsCommandList,
            *mut NVSDK_NGX_Handle,
            *mut NVSDK_NGX_Parameter,
        ) -> NVSDK_NGX_Result,
    ) -> Result<(), DlssError> {
        let sdk = self.sdk.lock().unwrap_or_else(|p| p.into_inner());

        // Transitions go through the wgpu API (its tracker knows the correct before-states) on a
        // dedicated encoder; the NGX evaluate uses the raw command list on a SEPARATE encoder. wgpu
        // 29 panics if both APIs touch one encoder, and `transition_resources` is deferred to
        // `finish()`, so it must precede the evaluate in submission order.
        let mut barrier_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_transitions"),
                });
        barrier_encoder.transition_resources(iter::empty(), barriers);

        let mut eval_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_evaluate"),
                });
        // SAFETY: `with_raw_command_list` hands the closure the encoder's open raw command list; the
        // closure forwards it (with the feature handle + locked NGX parameters) to the NGX
        // EVALUATE_*_EXT export.
        let evaluated = unsafe {
            with_raw_command_list(&mut eval_encoder, |cmd_list| {
                check_ngx_result(evaluate(cmd_list, self.feature, sdk.parameters))
            })
        };
        match evaluated {
            None => return Err(DlssError::FeatureNotSupported),
            Some(Err(e)) => return Err(e),
            Some(Ok(())) => {}
        }

        // Transitions first, then the evaluate, as one ordered submission.
        queue.submit([barrier_encoder.finish(), eval_encoder.finish()]);
        Ok(())
    }

    pub(crate) fn upscaled_resolution(&self) -> UVec2 {
        self.resolutions.upscaled
    }

    pub(crate) fn render_resolution(&self) -> UVec2 {
        self.resolutions.optimal
    }

    pub(crate) fn max_render_resolution(&self) -> UVec2 {
        self.resolutions.max
    }

    pub(crate) fn render_resolution_range(&self) -> RangeInclusive<UVec2> {
        self.resolutions.min..=self.resolutions.max
    }

    pub(crate) fn suggested_jitter(&self, frame_number: u32, render_resolution: UVec2) -> Vec2 {
        let ratio = self.resolutions.upscaled.x as f32 / render_resolution.x as f32;
        let phase_count = (8.0 * ratio * ratio) as u32;
        let i = frame_number % phase_count.max(1);

        Vec2 {
            x: halton_sequence(i, 2),
            y: halton_sequence(i, 3),
        } - 0.5
    }

    pub(crate) fn suggested_mip_bias(&self, render_resolution: UVec2) -> f32 {
        (render_resolution.x as f32 / self.resolutions.upscaled.x as f32).log2() - 1.0
    }
}

impl Drop for NgxFeature {
    fn drop(&mut self) {
        // Wait for the GPU to finish using the feature before releasing it.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        // Poison-tolerant: never double-panic across FFI in Drop.
        let _sdk = self.sdk.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            // Never panic across the FFI boundary in Drop; log and move on.
            if let Err(e) = check_ngx_result(NVSDK_NGX_D3D12_ReleaseFeature(self.feature)) {
                log::error!("Failed to release DLSS feature: {e}");
            }
        }
    }
}

// SAFETY: the raw NGX feature handle is only used while the owning SDK `Mutex` is held.
unsafe impl Send for NgxFeature {}
unsafe impl Sync for NgxFeature {}
