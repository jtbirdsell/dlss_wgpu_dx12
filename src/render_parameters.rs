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
    /// The wgpu texture bound to NGX as a DLSS input or output. Its raw `ID3D12Resource` is extracted
    /// at the FFI boundary; it must be a DX12 texture from the same device as the DLSS context.
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
        /// A 1x1 exposure texture supplied by the application.
        exposure: DlssTexture<'a>,
        /// Optional multiplier applied to the exposure value (defaults to 1.0).
        exposure_scale: Option<f32>,
        /// Optional pre-exposure already baked into the color (defaults to 0.0).
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
    ///
    /// The ordered set of slots (and thus the count) is decided by the device-free
    /// [`barrier_slots`] helper, keeping the Manual-vs-Automatic and bias branch logic unit-testable
    /// without a real `wgpu::Texture`; this method only attaches the matching texture to each slot.
    pub(crate) fn barrier_list(&self) -> impl Iterator<Item = TextureTransition<&'a Texture>> {
        let manual_exposure = matches!(self.exposure, DlssExposure::Manual { .. });
        barrier_slots(manual_exposure, self.bias.is_some())
            .into_iter()
            .map(move |(slot, state)| TextureTransition {
                texture: match slot {
                    BarrierSlot::Color => self.color.texture,
                    BarrierSlot::Depth => self.depth.texture,
                    BarrierSlot::MotionVectors => self.motion_vectors.texture,
                    BarrierSlot::Exposure => match &self.exposure {
                        DlssExposure::Manual { exposure, .. } => exposure.texture,
                        // `barrier_slots` only yields `Exposure` when `manual_exposure` is true.
                        DlssExposure::Automatic => unreachable!(),
                    },
                    // `barrier_slots` only yields `Bias` when `self.bias.is_some()`.
                    BarrierSlot::Bias => self.bias.as_ref().unwrap().texture,
                    BarrierSlot::Output => self.dlss_output.texture,
                },
                selector: None,
                state,
            })
    }
}

/// Which [`DlssRenderParameters`] resource a barrier entry targets. Lets the barrier slot/order
/// logic be decided over plain predicates (device-free, so unit-testable) by [`barrier_slots`], with
/// the actual `wgpu::Texture` attached later by [`DlssRenderParameters::barrier_list`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BarrierSlot {
    Color,
    Depth,
    MotionVectors,
    Exposure,
    Bias,
    Output,
}

/// The ordered barrier slots + target states for an SR evaluation, as a pure function of whether
/// exposure is Manual and whether a bias mask is present. Inputs become shader-readable
/// (`RESOURCE`); the output becomes a UAV (`STORAGE_READ_WRITE`). The Manual exposure texture and
/// the bias mask each add exactly one extra `RESOURCE` transition when present. This is the single
/// source of truth for the barrier set and is exercised directly by the unit tests below.
fn barrier_slots(manual_exposure: bool, has_bias: bool) -> Vec<(BarrierSlot, TextureUses)> {
    let mut slots = vec![
        (BarrierSlot::Color, TextureUses::RESOURCE),
        (BarrierSlot::Depth, TextureUses::RESOURCE),
        (BarrierSlot::MotionVectors, TextureUses::RESOURCE),
    ];
    if manual_exposure {
        slots.push((BarrierSlot::Exposure, TextureUses::RESOURCE));
    }
    if has_bias {
        slots.push((BarrierSlot::Bias, TextureUses::RESOURCE));
    }
    slots.push((BarrierSlot::Output, TextureUses::STORAGE_READ_WRITE));
    slots
}

#[cfg(test)]
mod tests {
    use super::{BarrierSlot, barrier_slots};
    use wgpu::TextureUses;

    // (a) Manual exposure adds exactly the exposure transition that Automatic omits; every other
    // slot is identical.
    #[test]
    fn manual_exposure_adds_exposure_transition_vs_automatic() {
        let automatic = barrier_slots(false, false);
        let manual = barrier_slots(true, false);

        assert_eq!(manual.len(), automatic.len() + 1);
        assert!(
            !automatic
                .iter()
                .any(|(slot, _)| *slot == BarrierSlot::Exposure)
        );
        assert_eq!(
            manual
                .iter()
                .filter(|(slot, _)| *slot == BarrierSlot::Exposure)
                .count(),
            1
        );
        // The exposure texture is bound shader-readable, like the other inputs.
        assert!(manual.contains(&(BarrierSlot::Exposure, TextureUses::RESOURCE)));
        // Removing the exposure slot from the Manual set reproduces the Automatic set exactly
        // (same slots, same states, same order).
        let manual_without_exposure: Vec<_> = manual
            .iter()
            .copied()
            .filter(|(slot, _)| *slot != BarrierSlot::Exposure)
            .collect();
        assert_eq!(manual_without_exposure, automatic);
    }

    // (b) Some(bias) adds exactly one more transition than None, independent of the exposure mode.
    #[test]
    fn bias_adds_exactly_one_transition() {
        for &manual_exposure in &[false, true] {
            let without_bias = barrier_slots(manual_exposure, false);
            let with_bias = barrier_slots(manual_exposure, true);

            assert_eq!(with_bias.len(), without_bias.len() + 1);
            assert!(
                !without_bias
                    .iter()
                    .any(|(slot, _)| *slot == BarrierSlot::Bias)
            );
            assert_eq!(
                with_bias
                    .iter()
                    .filter(|(slot, _)| *slot == BarrierSlot::Bias)
                    .count(),
                1
            );
            assert!(with_bias.contains(&(BarrierSlot::Bias, TextureUses::RESOURCE)));
        }
    }

    // Guard the full validated set: the output is always last and is the only UAV; everything else
    // is shader-readable.
    #[test]
    fn output_is_last_and_only_uav() {
        let slots = barrier_slots(true, true);
        assert_eq!(
            slots.last(),
            Some(&(BarrierSlot::Output, TextureUses::STORAGE_READ_WRITE))
        );
        assert_eq!(
            slots
                .iter()
                .filter(|(_, state)| *state == TextureUses::STORAGE_READ_WRITE)
                .count(),
            1
        );
        assert_eq!(slots.len(), 6); // color, depth, motion, exposure, bias, output
    }
}
