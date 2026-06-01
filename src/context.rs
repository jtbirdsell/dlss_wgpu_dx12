use crate::{
    DlssError, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode, DlssRenderParameters, DlssSdk,
    hal::with_raw_command_list,
    jitter::halton_sequence,
    nvsdk_ngx::*,
};
use glam::{UVec2, Vec2};
use std::{
    iter,
    ops::RangeInclusive,
    ptr,
    sync::{Arc, Mutex},
};

/// A per-camera DLSS Super Resolution feature.
///
/// Creating a context is expensive; cache it and only recreate it when settings (output
/// resolution, perf/quality mode, or feature flags) change.
pub struct DlssContext {
    upscaled_resolution: UVec2,
    optimal_render_resolution: UVec2,
    min_render_resolution: UVec2,
    max_render_resolution: UVec2,
    device: wgpu::Device,
    sdk: Arc<Mutex<DlssSdk>>,
    feature: *mut NVSDK_NGX_Handle,
}

impl DlssContext {
    /// Creates a new DLSS context for a camera outputting at `upscaled_resolution`.
    pub fn new(
        upscaled_resolution: UVec2,
        perf_quality_mode: DlssPerfQualityMode,
        feature_flags: DlssFeatureFlags,
        sdk: Arc<Mutex<DlssSdk>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Self, DlssError> {
        let locked_sdk = sdk.lock().unwrap();
        let perf_quality_value = perf_quality_mode.as_perf_quality_value(upscaled_resolution);

        // Query the render-resolution range DLSS recommends for this output + quality mode.
        let mut optimal = UVec2::ZERO;
        let mut min = UVec2::ZERO;
        let mut max = UVec2::ZERO;
        unsafe {
            let mut deprecated_sharpness = 0.0f32;
            check_ngx_result(NGX_DLSS_GET_OPTIMAL_SETTINGS(
                locked_sdk.parameters,
                upscaled_resolution.x,
                upscaled_resolution.y,
                perf_quality_value,
                &mut optimal.x,
                &mut optimal.y,
                &mut max.x,
                &mut max.y,
                &mut min.x,
                &mut min.y,
                &mut deprecated_sharpness,
            ))?;
        }
        if perf_quality_mode == DlssPerfQualityMode::Dlaa {
            optimal = upscaled_resolution;
            min = upscaled_resolution;
            max = upscaled_resolution;
        }

        let mut create_params = NVSDK_NGX_DLSS_Create_Params {
            Feature: NVSDK_NGX_Feature_Create_Params {
                InWidth: optimal.x,
                InHeight: optimal.y,
                InTargetWidth: upscaled_resolution.x,
                InTargetHeight: upscaled_resolution.y,
                InPerfQualityValue: perf_quality_value,
            },
            InFeatureCreateFlags: feature_flags.as_flags(),
            InEnableOutputSubrects: feature_flags.contains(DlssFeatureFlags::OutputSubrect),
        };

        // NGX records initialization work onto a command list; use a throwaway encoder + submit.
        let mut command_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_context_creation"),
        });

        let mut feature: *mut NVSDK_NGX_Handle = ptr::null_mut();
        let created = unsafe {
            with_raw_command_list(&mut command_encoder, |cmd_list| {
                check_ngx_result(NGX_D3D12_CREATE_DLSS_EXT(
                    cmd_list,
                    1, // CreationNodeMask (single-GPU)
                    1, // VisibilityNodeMask
                    &mut feature,
                    locked_sdk.parameters,
                    &mut create_params,
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
            upscaled_resolution,
            optimal_render_resolution: optimal,
            min_render_resolution: min,
            max_render_resolution: max,
            device: device.clone(),
            sdk,
            feature,
        })
    }

    /// Evaluates DLSS Super Resolution, submitting the work on `queue`.
    ///
    /// Submit any scene rendering that produces the inputs *before* calling this, so wgpu's state
    /// tracker reflects their current states and the GPU executes the scene first. The required
    /// resource transitions and the NGX evaluate are recorded on separate internal command encoders
    /// (wgpu 29 forbids mixing the wgpu and raw encoding APIs on one encoder, and
    /// `transition_resources` is deferred to `finish()`), then submitted in order.
    pub fn render(
        &mut self,
        render_parameters: DlssRenderParameters,
        queue: &wgpu::Queue,
    ) -> Result<(), DlssError> {
        render_parameters.validate()?;
        let sdk = self.sdk.lock().unwrap();

        let partial_texture_size = render_parameters
            .partial_texture_size
            .unwrap_or(self.max_render_resolution);

        let (exposure, exposure_scale, pre_exposure) = match &render_parameters.exposure {
            DlssExposure::Manual {
                exposure,
                exposure_scale,
                pre_exposure,
            } => (
                exposure.raw(),
                exposure_scale.unwrap_or(1.0),
                pre_exposure.unwrap_or(0.0),
            ),
            DlssExposure::Automatic => (ptr::null_mut(), 0.0, 0.0),
        };

        let mut eval_params = NVSDK_NGX_D3D12_DLSS_Eval_Params {
            Feature: NVSDK_NGX_D3D12_Feature_Eval_Params {
                pInColor: render_parameters.color.raw(),
                pInOutput: render_parameters.dlss_output.raw(),
                InSharpness: 0.0,
            },
            pInDepth: render_parameters.depth.raw(),
            pInMotionVectors: render_parameters.motion_vectors.raw(),
            InJitterOffsetX: render_parameters.jitter_offset.x,
            InJitterOffsetY: render_parameters.jitter_offset.y,
            InRenderSubrectDimensions: NVSDK_NGX_Dimensions {
                Width: partial_texture_size.x,
                Height: partial_texture_size.y,
            },
            InReset: render_parameters.reset as _,
            InMVScaleX: render_parameters.motion_vector_scale.unwrap_or(Vec2::ONE).x,
            InMVScaleY: render_parameters.motion_vector_scale.unwrap_or(Vec2::ONE).y,
            pInTransparencyMask: ptr::null_mut(),
            pInExposureTexture: exposure,
            pInBiasCurrentColorMask: render_parameters
                .bias
                .as_ref()
                .map_or(ptr::null_mut(), |bias| bias.raw()),
            InColorSubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InDepthSubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InMVSubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InTranslucencySubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InBiasCurrentColorSubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InOutputSubrectBase: NVSDK_NGX_Coordinates { X: 0, Y: 0 },
            InPreExposure: pre_exposure,
            InExposureScale: exposure_scale,
            InIndicatorInvertXAxis: 0,
            InIndicatorInvertYAxis: 0,
            GBufferSurface: NVSDK_NGX_D3D12_GBuffer {
                pInAttrib: [ptr::null_mut(); 16],
            },
            InToneMapperType: NVSDK_NGX_ToneMapperType_NVSDK_NGX_TONEMAPPER_STRING,
            pInMotionVectors3D: ptr::null_mut(),
            pInIsParticleMask: ptr::null_mut(),
            pInAnimatedTextureMask: ptr::null_mut(),
            pInDepthHighRes: ptr::null_mut(),
            pInPositionViewSpace: ptr::null_mut(),
            InFrameTimeDeltaInMsec: 0.0,
            pInRayTracingHitDistance: ptr::null_mut(),
            pInMotionVectorsReflections: ptr::null_mut(),
        };

        // Resource transitions go through the wgpu API (its tracker knows the correct before-states)
        // on a dedicated encoder; the NGX evaluate uses the raw command list on a SEPARATE encoder.
        // wgpu 29 panics if both APIs touch one encoder, and `transition_resources` is deferred to
        // `finish()`, so it must precede the evaluate in submission order rather than share its list.
        let mut barrier_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_sr_transitions"),
                });
        barrier_encoder.transition_resources(iter::empty(), render_parameters.barrier_list());

        let mut eval_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_sr_evaluate"),
                });
        let evaluated = unsafe {
            with_raw_command_list(&mut eval_encoder, |cmd_list| {
                check_ngx_result(NGX_D3D12_EVALUATE_DLSS_EXT(
                    cmd_list,
                    self.feature,
                    sdk.parameters,
                    &mut eval_params,
                ))
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

    /// Suggested subpixel camera jitter (Halton sequence) for a given frame.
    pub fn suggested_jitter(&self, frame_number: u32, render_resolution: UVec2) -> Vec2 {
        let ratio = self.upscaled_resolution.x as f32 / render_resolution.x as f32;
        let phase_count = (8.0 * ratio * ratio) as u32;
        let i = frame_number % phase_count.max(1);

        Vec2 {
            x: halton_sequence(i, 2),
            y: halton_sequence(i, 3),
        } - 0.5
    }

    /// Suggested mip bias for sampling textures at the render resolution.
    pub fn suggested_mip_bias(&self, render_resolution: UVec2) -> f32 {
        (render_resolution.x as f32 / self.upscaled_resolution.x as f32).log2() - 1.0
    }

    /// The upscaled (output) resolution DLSS will produce.
    pub fn upscaled_resolution(&self) -> UVec2 {
        self.upscaled_resolution
    }

    /// The recommended (optimal) render resolution for the chosen quality mode, pre-upscaling. This
    /// is the resolution NGX was created with (`InWidth`/`InHeight`), so the caller must render here
    /// to avoid a per-frame feature recreate / suboptimal reconstruction. Use
    /// [`Self::render_resolution_range`] for dynamic scaling between min and max.
    pub fn render_resolution(&self) -> UVec2 {
        self.optimal_render_resolution
    }

    /// Render-resolution range for dynamic resolution scaling.
    pub fn render_resolution_range(&self) -> RangeInclusive<UVec2> {
        self.min_render_resolution..=self.max_render_resolution
    }
}

impl Drop for DlssContext {
    fn drop(&mut self) {
        // Wait for the GPU to finish using the feature before releasing it.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        // Never panic across the FFI boundary in Drop. A `.unwrap()` here would panic on a poisoned
        // mutex (e.g. a prior panic while another context held the lock during NGX FFI), turning into
        // a double-panic -> process abort during unwind. Recover the guard instead: a poisoned lock
        // does not invalidate the NGX parameter pointer, so ReleaseFeature can still run.
        let _sdk = self.sdk.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            if let Err(e) = check_ngx_result(NVSDK_NGX_D3D12_ReleaseFeature(self.feature)) {
                log::error!("Failed to release DlssContext feature: {e}");
            }
        }
    }
}

// SAFETY: the raw NGX feature handle is only used while the owning SDK `Mutex` is held.
unsafe impl Send for DlssContext {}
unsafe impl Sync for DlssContext {}
