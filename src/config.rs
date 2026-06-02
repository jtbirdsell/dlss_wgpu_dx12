//! Hand-authored public configuration types for the DLSS features.
//!
//! These are the construction "knobs" callers pass when creating a context: the perf/quality tier,
//! the feature flags, and (for Ray Reconstruction) how roughness and depth are supplied. They live
//! here, separate from the generated NGX FFI in [`crate::nvsdk_ngx`], so the public API reads as a
//! deliberate set rather than ad hoc. All four are re-exported from the crate root.

use crate::nvsdk_ngx::*;
use glam::UVec2;

/// How much DLSS should upscale by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum DlssPerfQualityMode {
    /// Let DLSS decide based on the output resolution.
    #[default]
    Auto,
    /// Anti-aliasing only, no upscaling.
    Dlaa,
    /// Highest quality: the smallest upscale factor.
    Quality,
    /// Balanced quality and performance.
    Balanced,
    /// Higher performance: a larger upscale factor.
    Performance,
    /// Maximum performance: the largest upscale factor.
    UltraPerformance,
}

impl DlssPerfQualityMode {
    pub(crate) fn as_perf_quality_value(
        &self,
        upscaled_resolution: UVec2,
    ) -> NVSDK_NGX_PerfQuality_Value {
        match self {
            Self::Auto => {
                // Multiply in u64 before the f32 cast so a pathological resolution cannot overflow
                // u32 (debug panic / wrong tier in release); the product fits f32 fine afterwards.
                let mega_pixels = (upscaled_resolution.x as u64 * upscaled_resolution.y as u64)
                    as f32
                    / 1_000_000.0;

                if mega_pixels < 2.03 {
                    NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA
                } else if mega_pixels < 3.68 {
                    NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality
                } else if mega_pixels < 8.29 {
                    NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf
                } else {
                    NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance
                }
            }
            Self::Dlaa => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA,
            Self::Quality => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality,
            Self::Balanced => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_Balanced,
            Self::Performance => NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf,
            Self::UltraPerformance => {
                NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance
            }
        }
    }
}

bitflags::bitflags! {
    /// Flags for creating a DLSS context.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct DlssFeatureFlags: NVSDK_NGX_DLSS_Feature_Flags {
        /// Use an HDR color texture instead of an SDR one.
        const HighDynamicRange = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_IsHDR;
        /// Motion vectors are at the upscaled (output) resolution instead of render resolution.
        const LowResolutionMotionVectors = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVLowRes;
        /// Motion vectors contain jitter.
        const JitteredMotionVectors = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_MVJittered;
        /// Camera uses a reverse (inverted) depth buffer.
        const InvertedDepth = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_DepthInverted;
        /// Have DLSS apply auto-exposure.
        const AutoExposure = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_AutoExposure;
        /// Use a 4-channel RGBA color texture instead of a 3-channel RGB one.
        const AlphaUpscaling = NVSDK_NGX_DLSS_Feature_Flags_NVSDK_NGX_DLSS_Feature_Flags_AlphaUpscaling;
        /// Allow DLSS to write to a subrect of the output texture. (Not part of the NGX flag set.)
        const OutputSubrect = 256;
    }
}

impl DlssFeatureFlags {
    pub(crate) fn as_flags(&self) -> NVSDK_NGX_DLSS_Feature_Flags {
        let mut flags = *self;
        flags.remove(DlssFeatureFlags::OutputSubrect);
        flags.bits()
    }
}

/// How roughness is supplied to Ray Reconstruction.
#[cfg(feature = "ray-reconstruction")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RoughnessMode {
    /// Roughness is a dedicated input texture.
    #[default]
    Unpacked,
    /// Roughness is packed into the `.w` channel of the normals texture.
    Packed,
}

/// How depth is supplied to Ray Reconstruction.
#[cfg(feature = "ray-reconstruction")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DepthType {
    /// Linear view-space depth (RR's preferred input).
    Linear,
    /// Hardware (post-projection) depth, as in a standard depth buffer.
    #[default]
    Hardware,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_quality_fixed_modes_map_correctly() {
        let res = UVec2::new(3840, 2160); // value is irrelevant for the non-Auto modes
        assert_eq!(
            DlssPerfQualityMode::Dlaa.as_perf_quality_value(res),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA
        );
        assert_eq!(
            DlssPerfQualityMode::Quality.as_perf_quality_value(res),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality
        );
        assert_eq!(
            DlssPerfQualityMode::Balanced.as_perf_quality_value(res),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_Balanced
        );
        assert_eq!(
            DlssPerfQualityMode::Performance.as_perf_quality_value(res),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf
        );
        assert_eq!(
            DlssPerfQualityMode::UltraPerformance.as_perf_quality_value(res),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance
        );
    }

    #[test]
    fn perf_quality_auto_megapixel_ladder() {
        let auto = DlssPerfQualityMode::Auto;
        // 1280x720 = 0.92 MP (< 2.03) → DLAA.
        assert_eq!(
            auto.as_perf_quality_value(UVec2::new(1280, 720)),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_DLAA
        );
        // 1920x1080 = 2.07 MP (2.03..3.68) → MaxQuality.
        assert_eq!(
            auto.as_perf_quality_value(UVec2::new(1920, 1080)),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxQuality
        );
        // 3840x1600 = 6.14 MP (3.68..8.29) → MaxPerf.
        assert_eq!(
            auto.as_perf_quality_value(UVec2::new(3840, 1600)),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_MaxPerf
        );
        // 3840x2160 = 8.29 MP (>= 8.29) → UltraPerformance.
        assert_eq!(
            auto.as_perf_quality_value(UVec2::new(3840, 2160)),
            NVSDK_NGX_PerfQuality_Value_NVSDK_NGX_PerfQuality_Value_UltraPerformance
        );
    }

    #[test]
    fn as_flags_strips_the_synthetic_output_subrect_bit() {
        // OutputSubrect (bit 256) is crate-invented and is NOT part of the NGX flag set, so it must
        // never reach NGX's InFeatureCreateFlags; if as_flags stopped stripping it, NGX would get an
        // undefined flag and likely reject feature creation.
        let with_subrect = DlssFeatureFlags::AutoExposure | DlssFeatureFlags::OutputSubrect;
        assert_eq!(
            with_subrect.as_flags(),
            DlssFeatureFlags::AutoExposure.as_flags()
        );
        // ...but it is still observable on the original flags (it drives InEnableOutputSubrects).
        assert!(with_subrect.contains(DlssFeatureFlags::OutputSubrect));
        // And bit 256 never leaks through as_flags for any combination.
        assert_eq!(with_subrect.as_flags() & 256, 0);
    }
}
