#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unused)]

// Generated NGX bindings. The opaque COM types (ID3D12Device, ID3D12Resource,
// ID3D12GraphicsCommandList, IDXGIAdapter, ...) are emitted by bindgen as zero-sized structs;
// at call sites we cast windows-rs handles to `*mut <opaque>` via `Interface::as_raw()`.
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

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
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureAlreadyExists => {
            FeatureAlreadyExists
        }
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_FeatureNotFound => FeatureNotFound,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_InvalidParameter => InvalidParameters,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_ScratchBufferTooSmall => {
            ScratchBufferTooSmall
        }
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_NotInitialized => NotInitialized,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedInputFormat => {
            UnsupportedInputFormat
        }
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_RWFlagMissing => RWFlagMissing,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_MissingInput => MissingInput,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnableToInitializeFeature => {
            UnableToInitializeFeature
        }
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_OutOfDate => OutOfDate,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_OutOfGPUMemory => OutOfGPUMemory,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedFormat => UnsupportedFormat,
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnableToWriteToAppDataPath => {
            UnableToWriteToAppDataPath
        }
        r if r == NVSDK_NGX_Result_NVSDK_NGX_Result_FAIL_UnsupportedParameter => {
            UnsupportedParameter
        }
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

    // `DlssPerfQualityMode`/`DlssFeatureFlags` and their unit tests moved to `crate::config`.

    #[test]
    fn bindgen_blocklist_excludes_d3d11_and_cuda() {
        // build.rs blocklists D3D11/D3d11/Cuda/CUDA so the static-inline thunks never pull in
        // <d3d11.h>/<cuda.h> at compile time. If a blocklist pattern silently stopped matching (e.g.
        // an NGX header rename), the thunks would fail to compile with an opaque libclang error far
        // from here. Guard it: read the generated bindings and assert the excluded surfaces stayed
        // out while the D3D12 surface we depend on stayed in.
        let bindings = include_str!(concat!(env!("OUT_DIR"), "/bindings.rs"));
        assert!(
            !bindings.contains("D3D11"),
            "a D3D11 symbol leaked past the build.rs blocklist"
        );
        assert!(
            !bindings.contains("D3d11"),
            "a D3d11 (lowercase-d setter) symbol leaked past the build.rs blocklist"
        );
        assert!(
            !bindings.contains("Cuda") && !bindings.contains("CUDA"),
            "a CUDA symbol leaked past the build.rs blocklist"
        );
        // Sanity: the D3D12 surface the crate actually needs is still generated.
        assert!(
            bindings.contains("D3D12"),
            "expected the D3D12 NGX bindings to be present"
        );
    }
}
