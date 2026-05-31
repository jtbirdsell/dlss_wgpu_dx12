use crate::{DlssError, hal::raw_resource, nvsdk_ngx::ID3D12Resource};
use glam::{UVec2, Vec2};
use std::ptr;
use wgpu::{Texture, TextureTransition, TextureUses};

/// A texture used as a DLSS input or output.
///
/// On D3D12, NGX binds the raw `ID3D12Resource` directly — there is no image view, format, or
/// subresource at the NGX boundary (unlike the Vulkan path), so only the [`wgpu::Texture`] is
/// needed.
#[derive(Clone, Copy)]
pub struct DlssTexture<'a> {
    pub texture: &'a Texture,
}

impl<'a> DlssTexture<'a> {
    pub(crate) fn raw(&self) -> *mut ID3D12Resource {
        // SAFETY: DLSS contexts are created from a Dx12 device, so callers pass Dx12 textures;
        // the resource lives as long as `self.texture`. A non-Dx12 texture yields null, which NGX
        // rejects with a clear error rather than UB.
        unsafe { raw_resource(self.texture) }.unwrap_or(ptr::null_mut())
    }
}

/// Camera exposure used by DLSS.
pub enum DlssExposure<'a> {
    /// Exposure controlled by the application via a 1x1 exposure texture.
    Manual {
        exposure: DlssTexture<'a>,
        exposure_scale: Option<f32>,
        pre_exposure: Option<f32>,
    },
    /// Auto-exposure handled by DLSS (requires [`crate::DlssFeatureFlags::AutoExposure`]).
    Automatic,
}

/// Input and output resources for a DLSS Super Resolution evaluation.
pub struct DlssRenderParameters<'a> {
    /// Main color view of your camera (render resolution).
    pub color: DlssTexture<'a>,
    /// Depth buffer (render resolution).
    pub depth: DlssTexture<'a>,
    /// Screen-space motion vectors.
    pub motion_vectors: DlssTexture<'a>,
    /// Camera exposure settings.
    pub exposure: DlssExposure<'a>,
    /// Optional per-pixel bias to make DLSS more reactive.
    pub bias: Option<DlssTexture<'a>>,
    /// The texture DLSS writes the upscaled result to (upscaled resolution, UAV-capable).
    pub dlss_output: DlssTexture<'a>,
    /// Whether DLSS should reset its temporal history (e.g. on camera cuts).
    pub reset: bool,
    /// Subpixel jitter that was applied to the camera this frame.
    pub jitter_offset: Vec2,
    /// Optionally evaluate only a subrect of the inputs rather than the full textures.
    pub partial_texture_size: Option<UVec2>,
    /// Optional scaling factor applied to [`Self::motion_vectors`] values.
    pub motion_vector_scale: Option<Vec2>,
}

impl<'a> DlssRenderParameters<'a> {
    pub(crate) fn validate(&self) -> Result<(), DlssError> {
        // Reject null required resources (a non-Dx12 or destroyed texture resolves to null) up front
        // rather than handing a null ID3D12Resource to NGX evaluate.
        let required = [
            self.color.raw(),
            self.depth.raw(),
            self.motion_vectors.raw(),
            self.dlss_output.raw(),
        ];
        if required.iter().any(|p| p.is_null()) {
            return Err(DlssError::MissingInput);
        }
        Ok(())
    }

    /// Resource transitions to apply before evaluating: inputs must be shader-readable and the
    /// output must be a UAV. Routed through wgpu's tracker so it stays consistent with NGX's needs.
    pub(crate) fn barrier_list(&self) -> impl Iterator<Item = TextureTransition<&'a Texture>> {
        fn input<'a>(texture: &DlssTexture<'a>) -> TextureTransition<&'a Texture> {
            TextureTransition {
                texture: texture.texture,
                selector: None,
                state: TextureUses::RESOURCE,
            }
        }

        [
            Some(input(&self.color)),
            Some(input(&self.depth)),
            Some(input(&self.motion_vectors)),
            match &self.exposure {
                DlssExposure::Manual { exposure, .. } => Some(input(exposure)),
                DlssExposure::Automatic => None,
            },
            self.bias.as_ref().map(input),
            Some(TextureTransition {
                texture: self.dlss_output.texture,
                selector: None,
                state: TextureUses::STORAGE_READ_WRITE,
            }),
        ]
        .into_iter()
        .flatten()
    }
}
