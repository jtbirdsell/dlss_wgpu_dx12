use crate::{
    DlssError, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode, DlssRenderParameters, DlssSdk,
    ngx_feature::NgxFeature,
    nvsdk_ngx::*,
};
use glam::{UVec2, Vec2};
use std::{
    ops::RangeInclusive,
    ptr,
    sync::{Arc, Mutex},
};

/// A per-camera DLSS Super Resolution feature.
///
/// Creating a context is expensive; cache it and only recreate it when settings (output
/// resolution, perf/quality mode, or feature flags) change.
pub struct DlssContext {
    feature: NgxFeature,
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
        let perf_quality_value = perf_quality_mode.as_perf_quality_value(upscaled_resolution);
        let feature = NgxFeature::create(
            device,
            queue,
            sdk,
            upscaled_resolution,
            perf_quality_mode,
            // Query the render-resolution range DLSS recommends for this output + quality mode.
            |parameters| {
                let mut optimal = UVec2::ZERO;
                let mut min = UVec2::ZERO;
                let mut max = UVec2::ZERO;
                // SAFETY: the out-params are valid locals; `parameters` is the locked NGX parameter
                // block (held by the caller for the duration of this closure).
                unsafe {
                    let mut deprecated_sharpness = 0.0f32;
                    check_ngx_result(NGX_DLSS_GET_OPTIMAL_SETTINGS(
                        parameters,
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
                Ok((optimal, min, max))
            },
            // Create the DLSS Super Resolution feature at the optimal render resolution.
            |cmd_list, resolutions, parameters, feature_out| {
                let mut create_params = NVSDK_NGX_DLSS_Create_Params {
                    Feature: NVSDK_NGX_Feature_Create_Params {
                        InWidth: resolutions.optimal.x,
                        InHeight: resolutions.optimal.y,
                        InTargetWidth: resolutions.upscaled.x,
                        InTargetHeight: resolutions.upscaled.y,
                        InPerfQualityValue: perf_quality_value,
                    },
                    InFeatureCreateFlags: feature_flags.as_flags(),
                    InEnableOutputSubrects: feature_flags.contains(DlssFeatureFlags::OutputSubrect),
                };
                // SAFETY: `cmd_list`/`parameters` are the open list + locked params; `feature_out` is
                // a valid out-param; `create_params` lives on the stack through the call.
                unsafe {
                    NGX_D3D12_CREATE_DLSS_EXT(
                        cmd_list,
                        1, // CreationNodeMask (single-GPU)
                        1, // VisibilityNodeMask
                        feature_out,
                        parameters,
                        &mut create_params,
                    )
                }
            },
        )?;
        Ok(Self { feature })
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

        let partial_texture_size = render_parameters
            .partial_texture_size
            .unwrap_or(self.feature.max_render_resolution());

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

        self.feature.evaluate(
            queue,
            render_parameters.barrier_list(),
            |cmd_list, feature, parameters| {
                // SAFETY: `cmd_list`/`feature`/`parameters` are the open list, the live feature
                // handle, and the locked NGX params; `eval_params` lives on the stack through the call.
                unsafe {
                    NGX_D3D12_EVALUATE_DLSS_EXT(cmd_list, feature, parameters, &mut eval_params)
                }
            },
        )
    }

    /// Suggested subpixel camera jitter (Halton sequence) for a given frame.
    pub fn suggested_jitter(&self, frame_number: u32, render_resolution: UVec2) -> Vec2 {
        self.feature.suggested_jitter(frame_number, render_resolution)
    }

    /// Suggested mip bias for sampling textures at the render resolution.
    pub fn suggested_mip_bias(&self, render_resolution: UVec2) -> f32 {
        self.feature.suggested_mip_bias(render_resolution)
    }

    /// The upscaled (output) resolution DLSS will produce.
    pub fn upscaled_resolution(&self) -> UVec2 {
        self.feature.upscaled_resolution()
    }

    /// The recommended (optimal) render resolution for the chosen quality mode, pre-upscaling. This
    /// is the resolution NGX was created with (`InWidth`/`InHeight`), so the caller must render here
    /// to avoid a per-frame feature recreate / suboptimal reconstruction. Use
    /// [`Self::render_resolution_range`] for dynamic scaling between min and max.
    pub fn render_resolution(&self) -> UVec2 {
        self.feature.render_resolution()
    }

    /// Render-resolution range for dynamic resolution scaling.
    pub fn render_resolution_range(&self) -> RangeInclusive<UVec2> {
        self.feature.render_resolution_range()
    }
}
