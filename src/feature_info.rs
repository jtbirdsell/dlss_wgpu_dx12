use crate::nvsdk_ngx::*;
use std::{
    env,
    ffi::{CString, OsStr, OsString},
    os::windows::ffi::OsStrExt,
    ptr,
};
use uuid::Uuid;

/// Builds an [`NVSDK_NGX_FeatureDiscoveryInfo`] (project id, engine version, app-data path, and the
/// shared-library search paths NGX uses to find `nvngx_dlss*.dll`) and runs `callback` with it.
///
/// The info and all backing allocations live only for the duration of `callback`.
pub(crate) fn with_feature_info<F, T>(project_id: Uuid, callback: F) -> T
where
    F: FnOnce(&NVSDK_NGX_FeatureDiscoveryInfo) -> T,
{
    let project_id = CString::new(project_id.to_string()).unwrap();
    let engine_version = CString::new(env!("CARGO_PKG_VERSION")).unwrap();
    let data_path = os_str_to_wchar(env::temp_dir().as_os_str());

    let shared_library_paths = shared_library_paths();
    let shared_library_path_pointers = shared_library_paths
        .iter()
        .map(Vec::as_ptr)
        .collect::<Vec<_>>();

    let feature_info_common = NVSDK_NGX_FeatureCommonInfo {
        PathListInfo: NVSDK_NGX_PathListInfo {
            Path: shared_library_path_pointers.as_ptr(),
            Length: shared_library_paths.len() as u32,
        },
        InternalData: ptr::null_mut(),
        // TODO(enterprise): wire LoggingCallback into the `log` crate behind a configurable level.
        LoggingInfo: NVSDK_NGX_LoggingInfo {
            LoggingCallback: None,
            MinimumLoggingLevel: NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_OFF,
            DisableOtherLoggingSinks: false,
        },
    };

    let feature_info = NVSDK_NGX_FeatureDiscoveryInfo {
        SDKVersion: NVSDK_NGX_Version_NVSDK_NGX_Version_API,
        FeatureID: NVSDK_NGX_Feature_NVSDK_NGX_Feature_SuperSampling,
        Identifier: NVSDK_NGX_Application_Identifier {
            IdentifierType:
                NVSDK_NGX_Application_Identifier_Type_NVSDK_NGX_Application_Identifier_Type_Project_Id,
            v: NVSDK_NGX_Application_Identifier_v {
                ProjectDesc: NVSDK_NGX_ProjectIdDescription {
                    ProjectId: project_id.as_ptr(),
                    EngineType: NVSDK_NGX_EngineType_NVSDK_NGX_ENGINE_TYPE_CUSTOM,
                    EngineVersion: engine_version.as_ptr(),
                },
            },
        },
        ApplicationDataPath: data_path.as_ptr(),
        FeatureInfo: &feature_info_common,
    };

    (callback)(&feature_info)
}

/// Where NGX should look for `nvngx_dlss*.dll`: the current directory (next to the exe) and, if it
/// was set at build time, `$DLSS_SDK/lib/Windows_x86_64/{rel|dev}`.
fn shared_library_paths() -> Vec<Vec<wchar_t>> {
    let mut paths = vec![os_str_to_wchar(OsStr::new("."))];

    #[cfg(feature = "debug_overlay")]
    let profile = "dev";
    #[cfg(not(feature = "debug_overlay"))]
    let profile = "rel";

    if let Some(sdk) = option_env!("DLSS_SDK") {
        let sdk_path = format!("{sdk}/lib/Windows_x86_64/{profile}");
        paths.push(os_str_to_wchar(&OsString::from(sdk_path)));
    }

    paths
}

fn os_str_to_wchar(s: &OsStr) -> Vec<wchar_t> {
    s.encode_wide().chain([0]).map(|c| c as wchar_t).collect()
}
