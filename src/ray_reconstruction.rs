//! DLSS Ray Reconstruction (DLSS-D). A separate NGX feature
//! (`NVSDK_NGX_Feature_RayReconstruction`) that denoises + upscales path-traced input in one pass.
//! It is mutually exclusive with Super Resolution: create *either* a [`crate::DlssContext`] *or* a
//! [`DlssRayReconstructionContext`] for a given upscale pass, never both.

use crate::{
    DepthType, DlssError, DlssFeatureFlags, DlssPerfQualityMode, DlssSdk, DlssTexture,
    RoughnessMode, ngx_feature::NgxFeature, nvsdk_ngx::*,
};
use glam::{UVec2, Vec2};
use std::{
    mem,
    ops::RangeInclusive,
    sync::{Arc, Mutex},
};
use wgpu::{TextureTransition, TextureUses};

/// Inputs and output for a Ray Reconstruction evaluation. All guide buffers are at render
/// resolution; `output` is at the upscaled resolution and must be UAV-capable.
pub struct DlssRayReconstructionParameters<'a> {
    /// Noisy ray-traced color input.
    pub color: DlssTexture<'a>,
    /// 3-channel linear diffuse albedo.
    pub diffuse_albedo: DlssTexture<'a>,
    /// Specular albedo (e.g. via NVIDIA's EnvBRDFApprox2).
    pub specular_albedo: DlssTexture<'a>,
    /// Normalized shading normals (with roughness packed into `.w` if [`RoughnessMode::Packed`]).
    pub normals: DlssTexture<'a>,
    /// Linear roughness (ignored when using [`RoughnessMode::Packed`]).
    pub roughness: DlssTexture<'a>,
    /// Depth buffer (linear or hardware, per the context's [`DepthType`]).
    pub depth: DlssTexture<'a>,
    /// Screen-space motion vectors.
    pub motion_vectors: DlssTexture<'a>,
    /// Denoised, upscaled destination.
    pub output: DlssTexture<'a>,
    /// Reset temporal history (e.g. on camera cuts).
    pub reset: bool,
    /// Subpixel jitter applied to the camera this frame.
    pub jitter_offset: Vec2,
    /// Optionally evaluate only a subrect of the inputs.
    pub partial_texture_size: Option<UVec2>,
    /// Optional scale applied to motion-vector values.
    pub motion_vector_scale: Option<Vec2>,
}

impl<'a> DlssRayReconstructionParameters<'a> {
    /// Reject null required resources (a non-Dx12 or destroyed texture resolves to null via
    /// `DlssTexture::raw`) before handing pointers to NGX evaluate — mirrors
    /// [`crate::DlssRenderParameters`]'s validation, which the SR path runs and RR previously lacked.
    /// `roughness` is intentionally not required here: with [`RoughnessMode::Packed`] it rides in
    /// `normals.w` and NGX ignores the separate texture.
    fn validate(&self) -> Result<(), DlssError> {
        let required = [
            self.color.raw(),
            self.diffuse_albedo.raw(),
            self.specular_albedo.raw(),
            self.normals.raw(),
            self.depth.raw(),
            self.motion_vectors.raw(),
            self.output.raw(),
        ];
        if required.iter().any(|p| p.is_null()) {
            return Err(DlssError::MissingInput);
        }
        Ok(())
    }

    fn barrier_list(&self) -> impl Iterator<Item = TextureTransition<&'a wgpu::Texture>> {
        fn input<'a>(t: &DlssTexture<'a>) -> TextureTransition<&'a wgpu::Texture> {
            TextureTransition {
                texture: t.texture,
                selector: None,
                state: TextureUses::RESOURCE,
            }
        }
        [
            input(&self.color),
            input(&self.diffuse_albedo),
            input(&self.specular_albedo),
            input(&self.normals),
            input(&self.roughness),
            input(&self.depth),
            input(&self.motion_vectors),
            TextureTransition {
                texture: self.output.texture,
                selector: None,
                state: TextureUses::STORAGE_READ_WRITE,
            },
        ]
        .into_iter()
    }
}

/// A per-camera DLSS Ray Reconstruction feature. Cache and recreate only when settings change.
pub struct DlssRayReconstructionContext {
    feature: NgxFeature,
}

impl DlssRayReconstructionContext {
    /// Creates a Ray Reconstruction context.
    #[allow(clippy::too_many_arguments)] // a feature-creation constructor; mirrors NGX's parameters
    pub fn new(
        upscaled_resolution: UVec2,
        perf_quality_mode: DlssPerfQualityMode,
        roughness_mode: RoughnessMode,
        depth_type: DepthType,
        feature_flags: DlssFeatureFlags,
        sdk: Arc<Mutex<DlssSdk>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Self, DlssError> {
        // DLSS Ray Reconstruction mandates HDR color and render-resolution ("low res") motion
        // vectors at creation; NGX otherwise fails with FAIL_InvalidParameter ("HDR Color required"
        // / "Low resolution Motion Vectors required"). Enforce both so callers cannot omit them.
        let feature_flags = feature_flags
            | DlssFeatureFlags::HighDynamicRange
            | DlssFeatureFlags::LowResolutionMotionVectors;
        let perf_quality_value = perf_quality_mode.as_perf_quality_value(upscaled_resolution);

        let feature = NgxFeature::create(
            device,
            queue,
            sdk,
            upscaled_resolution,
            perf_quality_mode,
            // RR has its OWN optimal-settings call (distinct from SR's NGX_DLSS_GET_OPTIMAL_SETTINGS);
            // it primes RR-specific parameter state that NGX_D3D12_CREATE_DLSSD_EXT reads.
            |parameters| {
                let mut optimal = UVec2::ZERO;
                let mut min = UVec2::ZERO;
                let mut max = UVec2::ZERO;
                // SAFETY: out-params are valid locals; `parameters` is the locked NGX parameter block.
                unsafe {
                    let mut deprecated_sharpness = 0.0f32;
                    check_ngx_result(NGX_DLSSD_GET_OPTIMAL_SETTINGS(
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
            // Pin the DLSS-4 render preset, then create the RR feature at the optimal resolution.
            |cmd_list, resolutions, parameters, feature_out| {
                let mut create_params = NVSDK_NGX_DLSSD_Create_Params {
                    InDenoiseMode:
                        NVSDK_NGX_DLSS_Denoise_Mode_NVSDK_NGX_DLSS_Denoise_Mode_DLUnified,
                    InRoughnessMode: match roughness_mode {
                        RoughnessMode::Unpacked => {
                            NVSDK_NGX_DLSS_Roughness_Mode_NVSDK_NGX_DLSS_Roughness_Mode_Unpacked
                        }
                        RoughnessMode::Packed => {
                            NVSDK_NGX_DLSS_Roughness_Mode_NVSDK_NGX_DLSS_Roughness_Mode_Packed
                        }
                    },
                    InUseHWDepth: match depth_type {
                        DepthType::Linear => {
                            NVSDK_NGX_DLSS_Depth_Type_NVSDK_NGX_DLSS_Depth_Type_Linear
                        }
                        DepthType::Hardware => {
                            NVSDK_NGX_DLSS_Depth_Type_NVSDK_NGX_DLSS_Depth_Type_HW
                        }
                    },
                    InWidth: resolutions.optimal.x,
                    InHeight: resolutions.optimal.y,
                    InTargetWidth: resolutions.upscaled.x,
                    InTargetHeight: resolutions.upscaled.y,
                    InPerfQualityValue: perf_quality_value,
                    InFeatureCreateFlags: feature_flags.as_flags(),
                    InEnableOutputSubrects: feature_flags.contains(DlssFeatureFlags::OutputSubrect),
                };

                // DLSS 4 (SDK 310.x) requires an explicit Ray Reconstruction render preset: the
                // legacy A/B/C presets were removed and the implicit default is rejected with
                // InvalidParameters. Pin every quality-mode hint to preset D (the DLSS-4 transformer
                // model) before create.
                let preset = NVSDK_NGX_RayReconstruction_Hint_Render_Preset_NVSDK_NGX_RayReconstruction_Hint_Render_Preset_D as u32;
                // SAFETY: `parameters` is the locked NGX parameter block; `cmd_list`/`feature_out` are
                // valid; `create_params` lives on the stack through the call.
                unsafe {
                    for key in [
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_DLAA.as_ptr(),
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Quality.as_ptr(),
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Balanced.as_ptr(),
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Performance
                            .as_ptr(),
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_UltraPerformance
                            .as_ptr(),
                        NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_UltraQuality
                            .as_ptr(),
                    ] {
                        NVSDK_NGX_Parameter_SetUI(parameters, key.cast(), preset);
                    }
                    NGX_D3D12_CREATE_DLSSD_EXT(
                        cmd_list,
                        1,
                        1,
                        feature_out,
                        parameters,
                        &mut create_params,
                    )
                }
            },
        )?;
        Ok(Self { feature })
    }

    /// Evaluates DLSS Ray Reconstruction, submitting the work on `queue`.
    pub fn render(
        &mut self,
        params: DlssRayReconstructionParameters,
        queue: &wgpu::Queue,
    ) -> Result<(), DlssError> {
        params.validate()?;
        let partial = params
            .partial_texture_size
            .unwrap_or(self.feature.max_render_resolution());
        let mv_scale = params.motion_vector_scale.unwrap_or(Vec2::ONE);

        // The eval-params struct has ~89 (mostly optional) fields; zero-initialize and set only
        // the buffers we provide. All-zero is valid: null pointers / 0.0 / 0 for every field.
        let mut eval: NVSDK_NGX_D3D12_DLSSD_Eval_Params = unsafe { mem::zeroed() };
        eval.pInColor = params.color.raw();
        eval.pInOutput = params.output.raw();
        eval.pInDepth = params.depth.raw();
        eval.pInMotionVectors = params.motion_vectors.raw();
        eval.pInDiffuseAlbedo = params.diffuse_albedo.raw();
        eval.pInSpecularAlbedo = params.specular_albedo.raw();
        eval.pInNormals = params.normals.raw();
        eval.pInRoughness = params.roughness.raw();
        eval.InJitterOffsetX = params.jitter_offset.x;
        eval.InJitterOffsetY = params.jitter_offset.y;
        eval.InRenderSubrectDimensions = NVSDK_NGX_Dimensions {
            Width: partial.x,
            Height: partial.y,
        };
        eval.InReset = params.reset as _;
        eval.InMVScaleX = mv_scale.x;
        eval.InMVScaleY = mv_scale.y;

        self.feature.evaluate(
            queue,
            params.barrier_list(),
            |cmd_list, feature, parameters| {
                // SAFETY: `cmd_list`/`feature`/`parameters` are the open list, the live feature
                // handle, and the locked NGX params; `eval` lives on the stack through the call.
                unsafe { NGX_D3D12_EVALUATE_DLSSD_EXT(cmd_list, feature, parameters, &mut eval) }
            },
        )
    }

    /// Suggested subpixel camera jitter (Halton sequence) for a given frame.
    pub fn suggested_jitter(&self, frame_number: u32, render_resolution: UVec2) -> Vec2 {
        self.feature
            .suggested_jitter(frame_number, render_resolution)
    }

    /// Suggested mip bias for sampling textures at the render resolution.
    pub fn suggested_mip_bias(&self, render_resolution: UVec2) -> f32 {
        self.feature.suggested_mip_bias(render_resolution)
    }

    /// The upscaled (output) resolution.
    pub fn upscaled_resolution(&self) -> UVec2 {
        self.feature.upscaled_resolution()
    }

    /// The recommended (optimal) render resolution for the chosen quality mode, pre-upscaling. This is
    /// the resolution NGX was created with (`InWidth`/`InHeight`), so the caller must render here to
    /// avoid a per-frame feature recreate / suboptimal reconstruction. Use
    /// [`Self::render_resolution_range`] for dynamic scaling between min and max.
    pub fn render_resolution(&self) -> UVec2 {
        self.feature.render_resolution()
    }

    /// Render-resolution range for dynamic resolution scaling.
    pub fn render_resolution_range(&self) -> RangeInclusive<UVec2> {
        self.feature.render_resolution_range()
    }
}
