#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unused)]

// Generated NGX bindings. The opaque COM types (ID3D12Device, ID3D12Resource,
// ID3D12GraphicsCommandList, IDXGIAdapter, ...) are emitted by bindgen as zero-sized structs;
// at call sites we cast windows-rs handles to `*mut <opaque>` via `Interface::as_raw()`.
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

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

/// Errors returned by DLSS / the NGX SDK. Each variant maps an `NVSDK_NGX_Result_FAIL_*` code; see
/// the `Display` text (`#[error]`) for the full NGX description.
#[derive(thiserror::Error, Debug)]
pub enum DlssError {
    /// The NGX SDK or the requested feature is not supported on this system / hardware / graphics API.
    #[error(
        "The NGX SDK or a specific feature is not supported by the current system, hardware, and/or graphics API."
    )]
    FeatureNotSupported,
    /// An error occurred in the underlying platform (graphics API, OS, or a system library).
    #[error(
        "An error occurred within the underlying platform (graphics API, OS, or a system library such as NvAPI). Consult the NGX logs and the graphics API's validation layers."
    )]
    PlatformError,
    /// Feature creation failed because an identical feature already exists.
    #[error(
        "The NGX feature could not be created because a feature with identical parameters already exists, and the feature does not support multiple identical instances."
    )]
    FeatureAlreadyExists,
    /// No feature was found for the provided handle.
    #[error("A feature associated with the provided handle could not be found.")]
    FeatureNotFound,
    /// A provided parameter had an incorrect value/type, or a required parameter was missing.
    #[error(
        "One or more provided parameters had an incorrect value or type, or a required parameter was not provided."
    )]
    InvalidParameters,
    /// The feature's scratch buffer was missing or too small.
    #[error(
        "The feature requires a scratch buffer, but none was provided or the provided buffer is too small."
    )]
    ScratchBufferTooSmall,
    /// An NGX call was made before the SDK was initialized.
    #[error(
        "A function that requires the NGX SDK to be initialized was called before the SDK was properly initialized."
    )]
    NotInitialized,
    /// An input buffer had an unsupported format.
    #[error("One or more input buffers supplied to the feature had an unsupported format.")]
    UnsupportedInputFormat,
    /// An output buffer lacked read/write (UAV) access.
    #[error(
        "The feature requires read/write access to output buffers, but one or more provided buffers did not have the correct access flags (UAV in D3D11/D3D12)."
    )]
    RWFlagMissing,
    /// A required input was not provided (e.g. a null `ID3D12Resource`).
    #[error("A required input parameter was not provided.")]
    MissingInput,
    /// The requested feature's library could not be found / initialized.
    #[error(
        "The requested feature could not be initialized, likely because the library for that feature could not be found."
    )]
    UnableToInitializeFeature,
    /// A newer NVIDIA driver or feature library is required.
    #[error(
        "A function was used which requires a newer version of the NVIDIA Display Driver or feature library than is currently installed."
    )]
    OutOfDate,
    /// The system lacked sufficient GPU memory.
    #[error("An operation could not be completed because the system lacked sufficient GPU memory.")]
    OutOfGPUMemory,
    /// A provided buffer had an unsupported format.
    #[error("One or more buffers provided to the feature had an unsupported format.")]
    UnsupportedFormat,
    /// The SDK lacked write permission for `InApplicationDataPath`.
    #[error(
        "The SDK does not have the necessary write permissions for the path specified in InApplicationDataPath."
    )]
    UnableToWriteToAppDataPath,
    /// A parameter is unsupported by the current version or has an unsupported value.
    #[error(
        "A parameter supplied to the feature is either unsupported by the current version or has an unsupported value."
    )]
    UnsupportedParameter,
    /// NVIDIA has restricted use of this feature in the current application.
    #[error(
        "NVIDIA has restricted the use of this feature in the current application. Contact NVIDIA for further information."
    )]
    Denied,
    /// The requested functionality is not implemented in the current SDK / driver / feature library.
    #[error(
        "The requested feature or functionality has not been implemented in the current version of the NGX SDK, display driver, or feature library."
    )]
    NotImplemented,
    /// An NGX result code not covered above. Carried instead of panicking across the FFI boundary.
    #[error("Unhandled NGX result code: {0:#x}")]
    Other(NVSDK_NGX_Result),
}

pub fn check_ngx_result(result: NVSDK_NGX_Result) -> Result<(), DlssError> {
    use DlssError::*;

    // CAUTION: `NVSDK_NGX_Result` is a `c_int` type alias, not a Rust enum, and bindgen emits the
    // result constants DOUBLE-prefixed (`NVSDK_NGX_Result_NVSDK_NGX_Result_*`). A bare-identifier
    // match arm whose name does not resolve to a constant silently becomes an irrefutable variable
    // binding — which would make the first arm swallow every code and return `Ok`. We therefore use
    // `r if r == CONST` guards: a misspelled constant is an unresolved-name compile error, not a
    // silent catch-all. (Do not rewrite these as bare patterns.)
    Err(match result {
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_Success => return Ok(()),
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureNotSupported => FeatureNotSupported,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_PlatformError => PlatformError,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureAlreadyExists => FeatureAlreadyExists,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureNotFound => FeatureNotFound,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_InvalidParameter => InvalidParameters,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_ScratchBufferTooSmall => ScratchBufferTooSmall,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_NotInitialized => NotInitialized,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedInputFormat => UnsupportedInputFormat,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_RWFlagMissing => RWFlagMissing,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_MissingInput => MissingInput,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnableToInitializeFeature => UnableToInitializeFeature,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_OutOfDate => OutOfDate,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_OutOfGPUMemory => OutOfGPUMemory,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedFormat => UnsupportedFormat,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnableToWriteToAppDataPath => UnableToWriteToAppDataPath,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedParameter => UnsupportedParameter,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_Denied => Denied,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_NotImplemented => NotImplemented,
        // Never `unreachable!()` across FFI — surface the raw code.
        other => Other(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_ngx_result_maps_known_and_unknown_codes() {
        // Success → Ok.
        assert!(check_ngx_result(NVSDK_NGX_Result_NVSDK_NGX_Result_Success).is_ok());
        // A couple of distinct failures map to distinct variants (guards against a catch-all
        // collapse — the double-prefix footgun this fn's comment warns about).
        assert!(matches!(
            check_ngx_result(NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureNotSupported),
            Err(DlssError::FeatureNotSupported)
        ));
        assert!(matches!(
            check_ngx_result(NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_InvalidParameter),
            Err(DlssError::InvalidParameters)
        ));
        // An unknown code is carried as Other(code), never panics.
        let unknown: NVSDK_NGX_Result = 0xDEAD;
        assert!(matches!(
            check_ngx_result(unknown),
            Err(DlssError::Other(c)) if c == unknown
        ));
    }

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
        assert_eq!(with_subrect.as_flags(), DlssFeatureFlags::AutoExposure.as_flags());
        // ...but it is still observable on the original flags (it drives InEnableOutputSubrects).
        assert!(with_subrect.contains(DlssFeatureFlags::OutputSubrect));
        // And bit 256 never leaks through as_flags for any combination.
        assert_eq!(with_subrect.as_flags() & 256, 0);
    }
}
