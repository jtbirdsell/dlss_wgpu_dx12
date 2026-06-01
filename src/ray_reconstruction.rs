//! DLSS Ray Reconstruction (DLSS-D). A separate NGX feature
//! (`NVSDK_NGX_Feature_RayReconstruction`) that denoises + upscales path-traced input in one pass.
//! It is mutually exclusive with Super Resolution: create *either* a [`crate::DlssContext`] *or* a
//! [`DlssRayReconstructionContext`] for a given upscale pass, never both.

use crate::{
    DlssError, DlssFeatureFlags, DlssPerfQualityMode, DlssSdk, DlssTexture,
    hal::with_raw_command_list,
    jitter::halton_sequence,
    nvsdk_ngx::*,
};
use glam::{UVec2, Vec2};
use std::{
    iter, mem,
    ops::RangeInclusive,
    ptr,
    sync::{Arc, Mutex},
};
use wgpu::{TextureTransition, TextureUses};

/// How roughness is supplied to Ray Reconstruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RoughnessMode {
    /// Roughness is a dedicated input texture.
    #[default]
    Unpacked,
    /// Roughness is packed into the `.w` channel of the normals texture.
    Packed,
}

/// How depth is supplied to Ray Reconstruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DepthType {
    /// Linear view-space depth (RR's preferred input).
    Linear,
    /// Hardware (post-projection) depth, as in a standard depth buffer.
    #[default]
    Hardware,
}

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
    upscaled_resolution: UVec2,
    min_render_resolution: UVec2,
    max_render_resolution: UVec2,
    device: wgpu::Device,
    sdk: Arc<Mutex<DlssSdk>>,
    feature: *mut NVSDK_NGX_Handle,
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
        let locked_sdk = sdk.lock().unwrap();
        let perf_quality_value = perf_quality_mode.as_perf_quality_value(upscaled_resolution);

        let mut optimal = UVec2::ZERO;
        let mut min = UVec2::ZERO;
        let mut max = UVec2::ZERO;
        unsafe {
            let mut deprecated_sharpness = 0.0f32;
            // RR has its OWN optimal-settings call (distinct from SR's NGX_DLSS_GET_OPTIMAL_SETTINGS);
            // it primes RR-specific parameter state that NGX_D3D12_CREATE_DLSSD_EXT reads.
            check_ngx_result(NGX_DLSSD_GET_OPTIMAL_SETTINGS(
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

        let mut create_params = NVSDK_NGX_DLSSD_Create_Params {
            InDenoiseMode: NVSDK_NGX_DLSS_Denoise_Mode_NVSDK_NGX_DLSS_Denoise_Mode_DLUnified,
            InRoughnessMode: match roughness_mode {
                RoughnessMode::Unpacked => {
                    NVSDK_NGX_DLSS_Roughness_Mode_NVSDK_NGX_DLSS_Roughness_Mode_Unpacked
                }
                RoughnessMode::Packed => {
                    NVSDK_NGX_DLSS_Roughness_Mode_NVSDK_NGX_DLSS_Roughness_Mode_Packed
                }
            },
            InUseHWDepth: match depth_type {
                DepthType::Linear => NVSDK_NGX_DLSS_Depth_Type_NVSDK_NGX_DLSS_Depth_Type_Linear,
                DepthType::Hardware => NVSDK_NGX_DLSS_Depth_Type_NVSDK_NGX_DLSS_Depth_Type_HW,
            },
            InWidth: optimal.x,
            InHeight: optimal.y,
            InTargetWidth: upscaled_resolution.x,
            InTargetHeight: upscaled_resolution.y,
            InPerfQualityValue: perf_quality_value,
            InFeatureCreateFlags: feature_flags.as_flags(),
            InEnableOutputSubrects: feature_flags.contains(DlssFeatureFlags::OutputSubrect),
        };

        // DLSS 4 (SDK 310.x) requires an explicit Ray Reconstruction render preset: the legacy
        // A/B/C presets were removed and the implicit default is rejected with InvalidParameters.
        // Pin every quality-mode hint to preset D (the DLSS-4 transformer model) before create.
        let preset = NVSDK_NGX_RayReconstruction_Hint_Render_Preset_NVSDK_NGX_RayReconstruction_Hint_Render_Preset_D as u32;
        unsafe {
            for key in [
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_DLAA.as_ptr(),
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Quality.as_ptr(),
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Balanced.as_ptr(),
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_Performance.as_ptr(),
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_UltraPerformance.as_ptr(),
                NVSDK_NGX_Parameter_RayReconstruction_Hint_Render_Preset_UltraQuality.as_ptr(),
            ] {
                NVSDK_NGX_Parameter_SetUI(locked_sdk.parameters, key.cast(), preset);
            }
        }

        let mut command_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("dlss_rr_context_creation"),
        });

        let mut feature: *mut NVSDK_NGX_Handle = ptr::null_mut();
        let created = unsafe {
            with_raw_command_list(&mut command_encoder, |cmd_list| {
                check_ngx_result(NGX_D3D12_CREATE_DLSSD_EXT(
                    cmd_list,
                    1,
                    1,
                    &mut feature,
                    locked_sdk.parameters,
                    &mut create_params,
                ))
            })
        };
        match created {
            None => return Err(DlssError::FeatureNotSupported),
            Some(result) => result?,
        }

        queue.submit([command_encoder.finish()]);
        drop(locked_sdk);

        Ok(Self {
            upscaled_resolution,
            min_render_resolution: min,
            max_render_resolution: max,
            device: device.clone(),
            sdk,
            feature,
        })
    }

    /// Evaluates Ray Reconstruction, submitting the work on `queue`.
    ///
    /// Submit scene rendering that produces the inputs before calling this. The transitions and the
    /// NGX evaluate are recorded on separate internal encoders and submitted in order (see
    /// [`crate::DlssContext::render`] for the rationale — wgpu 29 forbids mixing encoding APIs).
    pub fn render(
        &mut self,
        params: DlssRayReconstructionParameters,
        queue: &wgpu::Queue,
    ) -> Result<(), DlssError> {
        let sdk = self.sdk.lock().unwrap();
        let partial = params
            .partial_texture_size
            .unwrap_or(self.max_render_resolution);
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

        // Separate encoders for the wgpu-tracked transitions and the raw NGX evaluate (see
        // DlssContext::render); submitted transitions-first.
        let mut barrier_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_rr_transitions"),
                });
        barrier_encoder.transition_resources(iter::empty(), params.barrier_list());

        let mut eval_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("dlss_rr_evaluate"),
                });
        let evaluated = unsafe {
            with_raw_command_list(&mut eval_encoder, |cmd_list| {
                check_ngx_result(NGX_D3D12_EVALUATE_DLSSD_EXT(
                    cmd_list,
                    self.feature,
                    sdk.parameters,
                    &mut eval,
                ))
            })
        };
        match evaluated {
            None => return Err(DlssError::FeatureNotSupported),
            Some(Err(e)) => return Err(e),
            Some(Ok(())) => {}
        }

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

    /// The upscaled (output) resolution.
    pub fn upscaled_resolution(&self) -> UVec2 {
        self.upscaled_resolution
    }

    /// The recommended render resolution, pre-upscaling.
    pub fn render_resolution(&self) -> UVec2 {
        self.min_render_resolution
    }

    /// Render-resolution range for dynamic resolution scaling.
    pub fn render_resolution_range(&self) -> RangeInclusive<UVec2> {
        self.min_render_resolution..=self.max_render_resolution
    }
}

impl Drop for DlssRayReconstructionContext {
    fn drop(&mut self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let _sdk = self.sdk.lock().unwrap();
        unsafe {
            if let Err(e) = check_ngx_result(NVSDK_NGX_D3D12_ReleaseFeature(self.feature)) {
                log::error!("Failed to release DlssRayReconstructionContext feature: {e}");
            }
        }
    }
}

// SAFETY: the raw NGX feature handle is only used while the owning SDK `Mutex` is held.
unsafe impl Send for DlssRayReconstructionContext {}
unsafe impl Sync for DlssRayReconstructionContext {}
