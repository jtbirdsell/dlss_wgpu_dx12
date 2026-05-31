//! Runtime loading of `sl.interposer.dll` + typed function pointers.
//!
//! The SL headers are C++, so we do NOT `#[link]` or declare `extern "C"` prototypes. Instead we
//! `libloading::Library::new` the interposer and `GetProcAddress` the exported core `sl*` symbols
//! (confirmed undecorated `extern "C"` exports). The feature-level functions (`slDLSSGSetOptions`,
//! `slReflexSleep`, `slPCLSetMarker`, ...) are NOT exported — they are resolved at runtime via
//! `slGetFeatureFunction` AFTER `slSetD3DDevice`.
//!
//! Production hardening over the spike:
//!   * The interposer is located via the `STREAMLINE_SDK` environment variable at runtime
//!     (`$STREAMLINE_SDK/bin/x64/sl.interposer.dll`), not a hardcoded path.
//!   * Its Authenticode signature is verified (see [`super::security`]) BEFORE `LoadLibrary`; an
//!     untrusted/unsigned binary is refused.
//!   * Failures surface as the typed [`StreamlineError`] rather than `String`.

use super::security::verify_interposer_signature;
use super::types::*;
use core::ffi::c_void;
use libloading::os::windows as ll;
use std::ffi::CString;
use std::path::PathBuf;

/// Environment variable that points at the Streamline SDK root.
const STREAMLINE_SDK_ENV: &str = "STREAMLINE_SDK";

/// Resolve the absolute path to the production interposer from `$STREAMLINE_SDK/bin/x64`.
///
/// The runner places the loader shim (sl.interposer.dll copied as dxgi.dll + d3d12.dll) next to the
/// exe separately; THIS path is only used to obtain the exported `sl*` entry points.
fn interposer_path() -> Result<PathBuf, StreamlineError> {
    let sdk = std::env::var_os(STREAMLINE_SDK_ENV).ok_or(StreamlineError::SdkPathNotSet)?;
    let mut path = PathBuf::from(sdk);
    path.push("bin");
    path.push("x64");
    path.push("sl.interposer.dll");
    Ok(path)
}

// --- Core exported function-pointer signatures (sl_core_api.h) ----------------------------------
// All take/return the SL ABI types. `&` reference params in C++ are passed as pointers in the ABI,
// so we model them as `*const`/`*mut`.

pub type PfnSlInit = unsafe extern "C" fn(pref: *const Preferences, sdk_version: u64) -> SlResult;
pub type PfnSlShutdown = unsafe extern "C" fn() -> SlResult;
pub type PfnSlSetD3DDevice = unsafe extern "C" fn(d3d_device: *mut c_void) -> SlResult;
pub type PfnSlIsFeatureSupported =
    unsafe extern "C" fn(feature: Feature, adapter_info: *const AdapterInfo) -> SlResult;
pub type PfnSlGetFeatureRequirements =
    unsafe extern "C" fn(feature: Feature, requirements: *mut c_void) -> SlResult;
pub type PfnSlGetNewFrameToken =
    unsafe extern "C" fn(token: *mut *mut FrameToken, frame_index: *const u32) -> SlResult;
pub type PfnSlSetConstants = unsafe extern "C" fn(
    values: *const Constants,
    frame: *const FrameToken,
    viewport: *const ViewportHandle,
) -> SlResult;
pub type PfnSlSetTagForFrame = unsafe extern "C" fn(
    frame: *const FrameToken,
    viewport: *const ViewportHandle,
    tags: *const ResourceTag,
    num_tags: u32,
    cmd_buffer: *mut c_void,
) -> SlResult;
// Deprecated non-frame-based tagging. Kept for reference (we use slSetTagForFrame).
#[allow(dead_code)]
pub type PfnSlSetTag = unsafe extern "C" fn(
    viewport: *const ViewportHandle,
    tags: *const ResourceTag,
    num_tags: u32,
    cmd_buffer: *mut c_void,
) -> SlResult;
pub type PfnSlGetFeatureFunction = unsafe extern "C" fn(
    feature: Feature,
    function_name: *const u8,
    function: *mut *mut c_void,
) -> SlResult;

// --- Feature-level function-pointer signatures (sl_dlss_g.h / sl_reflex.h / sl_pcl.h) -----------
// Resolved via slGetFeatureFunction.

pub type PfnSlDLSSGSetOptions =
    unsafe extern "C" fn(viewport: *const ViewportHandle, options: *const DLSSGOptions) -> SlResult;
pub type PfnSlDLSSGGetState = unsafe extern "C" fn(
    viewport: *const ViewportHandle,
    state: *mut DLSSGState,
    options: *const DLSSGOptions,
) -> SlResult;
pub type PfnSlReflexSetOptions = unsafe extern "C" fn(options: *const ReflexOptions) -> SlResult;
pub type PfnSlReflexSleep = unsafe extern "C" fn(frame: *const FrameToken) -> SlResult;
pub type PfnSlPCLSetMarker =
    unsafe extern "C" fn(marker: PCLMarker, frame: *const FrameToken) -> SlResult;

/// Holds the resolved core + feature function pointers for the loaded interposer.
///
/// The function pointers are plain (transmute-copied) addresses, not borrowing `libloading::Symbol`s,
/// so they stay valid for as long as `sl.interposer.dll` remains mapped. [`SlApi::load`] deliberately
/// **leaks** the `libloading::Library` (never calling `FreeLibrary`): NVIDIA's interposer installs
/// process-wide DXGI/D3D hooks and is not designed to be unloaded — `FreeLibrary`ing it
/// access-violates. `slShutdown` performs the real cleanup; the DLL staying resident until the OS
/// reclaims it at process exit is the expected, supported behavior.
pub struct SlApi {
    // Core (exported)
    pub sl_init: PfnSlInit,
    pub sl_shutdown: PfnSlShutdown,
    pub sl_set_d3d_device: PfnSlSetD3DDevice,
    pub sl_is_feature_supported: PfnSlIsFeatureSupported,
    // Resolved but unused by the probe; callers can use slGetFeatureRequirements(kFeatureDLSS_G)
    // to confirm the exact required plugin/tag set at runtime.
    #[allow(dead_code)]
    pub sl_get_feature_requirements: PfnSlGetFeatureRequirements,
    pub sl_get_new_frame_token: PfnSlGetNewFrameToken,
    pub sl_set_constants: PfnSlSetConstants,
    pub sl_set_tag_for_frame: PfnSlSetTagForFrame,
    pub sl_get_feature_function: PfnSlGetFeatureFunction,

    // Feature-level (resolved after slSetD3DDevice via slGetFeatureFunction)
    pub sl_dlssg_set_options: Option<PfnSlDLSSGSetOptions>,
    pub sl_dlssg_get_state: Option<PfnSlDLSSGGetState>,
    pub sl_reflex_set_options: Option<PfnSlReflexSetOptions>,
    pub sl_reflex_sleep: Option<PfnSlReflexSleep>,
    pub sl_pcl_set_marker: Option<PfnSlPCLSetMarker>,
}

/// Resolve an exported symbol to a typed fn-pointer.
///
/// # Safety
/// `T` must be a `unsafe extern "C" fn(...)` type that exactly matches the C++ ABI of the export
/// named `name`. `name` must be a NUL-terminated byte string (e.g. `b"slInit\0"`).
unsafe fn resolve<T: Copy>(lib: &ll::Library, name: &[u8]) -> Result<T, StreamlineError> {
    // The export symbol is a NUL-terminated &[u8] (e.g. b"slInit\0"). We resolve it as an opaque
    // function pointer and transmute_copy it into the requested `unsafe extern "C" fn(...)` type.
    // `T` is always a fn-pointer (pointer-sized), matching `*mut c_void`.
    //
    // SAFETY: delegated to the caller's contract on `T` and `name` (see fn docs). `lib.get` returns
    // a symbol borrowing from `lib`, which `SlApi` keeps alive for the lifetime of the fn pointers.
    let sym: ll::Symbol<*mut c_void> =
        unsafe { lib.get(name) }.map_err(|source| StreamlineError::MissingExport {
            symbol: String::from_utf8_lossy(name).trim_end_matches('\0').to_string(),
            source,
        })?;
    let raw: *mut c_void = *sym;
    // SAFETY: `raw` is a pointer-sized export address; `T` is a pointer-sized fn type per contract.
    Ok(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&raw) })
}

impl SlApi {
    /// Locate, verify, load `sl.interposer.dll`, and resolve the exported core functions.
    ///
    /// The interposer is found via `$STREAMLINE_SDK/bin/x64/sl.interposer.dll`; its Authenticode
    /// signature is verified (trusted chain + NVIDIA signer pin, see [`super::security`]) before the
    /// DLL is loaded. It is then loaded with the **default** Windows search order: NVIDIA's
    /// interposer locates its sibling plugins (`sl.common`, `sl.dlss_g`, ...) relative to the loading
    /// context, and constraining the search (e.g. `LOAD_LIBRARY_SEARCH_*`) or passing a verbatim
    /// `\\?\` path makes `slInit` fail with `eErrorNoPlugins`, so we deliberately keep the plain
    /// load that the hardware-validated path uses. Feature functions are left `None` until
    /// [`SlApi::resolve_feature_functions`] runs (after `slSetD3DDevice`).
    ///
    /// # Safety
    /// Loads a native DLL and resolves raw C function pointers whose declared signatures are
    /// asserted (not checked) to match the interposer's ABI. The returned `SlApi` exposes
    /// `unsafe extern "C"` pointers; calling them is `unsafe`. Must be called on Windows with the
    /// Streamline SDK installed.
    pub unsafe fn load() -> Result<Self, StreamlineError> {
        let path = interposer_path()?;
        if !path.exists() {
            return Err(StreamlineError::InterposerNotFound(path));
        }

        // Hard gate: refuse to load an interposer that is not a trusted, NVIDIA-signed binary. We
        // verify and load the SAME `path` value (no canonicalization: a `\\?\` verbatim path breaks
        // the interposer's relative plugin discovery -> slInit eErrorNoPlugins).
        verify_interposer_signature(&path)?;
        log::info!("loading verified sl.interposer.dll from {}", path.display());

        // SAFETY: `path` was just signature-verified and confirmed to exist. Loading a DLL runs its
        // entry point; we accept that for the (now trusted) NVIDIA interposer. We use the default
        // search order (`Library::new`) because the interposer must find its sibling SL plugins via
        // the standard search; restricting it makes slInit report eErrorNoPlugins.
        let lib = unsafe { ll::Library::new(&path) }.map_err(|source| {
            StreamlineError::LibraryLoadFailed {
                path: path.clone(),
                source,
            }
        })?;

        // SAFETY for each resolve: the named export exists in the interposer and its declared
        // `Pfn*` type matches the C++ `sl*` ABI (validated by the spike against dumpbin + a live
        // RTX 4090 run). `lib` is kept alive by the returned `SlApi`.
        let sl_init = unsafe { resolve::<PfnSlInit>(&lib, b"slInit\0")? };
        let sl_shutdown = unsafe { resolve::<PfnSlShutdown>(&lib, b"slShutdown\0")? };
        let sl_set_d3d_device = unsafe { resolve::<PfnSlSetD3DDevice>(&lib, b"slSetD3DDevice\0")? };
        let sl_is_feature_supported =
            unsafe { resolve::<PfnSlIsFeatureSupported>(&lib, b"slIsFeatureSupported\0")? };
        let sl_get_feature_requirements =
            unsafe { resolve::<PfnSlGetFeatureRequirements>(&lib, b"slGetFeatureRequirements\0")? };
        let sl_get_new_frame_token =
            unsafe { resolve::<PfnSlGetNewFrameToken>(&lib, b"slGetNewFrameToken\0")? };
        let sl_set_constants = unsafe { resolve::<PfnSlSetConstants>(&lib, b"slSetConstants\0")? };
        let sl_set_tag_for_frame =
            unsafe { resolve::<PfnSlSetTagForFrame>(&lib, b"slSetTagForFrame\0")? };
        let sl_get_feature_function =
            unsafe { resolve::<PfnSlGetFeatureFunction>(&lib, b"slGetFeatureFunction\0")? };

        // All symbols are now resolved into plain fn pointers (they do NOT borrow `lib`). Leak the
        // `Library` so its `Drop` never calls `FreeLibrary`: the NVIDIA interposer installs
        // process-wide hooks and is not safe to unload (FreeLibrary access-violates); it must stay
        // mapped for the process lifetime. The OS reclaims it at exit. `slShutdown` (called on
        // teardown) does the real cleanup. See the `SlApi` doc comment.
        core::mem::forget(lib);

        Ok(Self {
            sl_init,
            sl_shutdown,
            sl_set_d3d_device,
            sl_is_feature_supported,
            sl_get_feature_requirements,
            sl_get_new_frame_token,
            sl_set_constants,
            sl_set_tag_for_frame,
            sl_get_feature_function,
            sl_dlssg_set_options: None,
            sl_dlssg_get_state: None,
            sl_reflex_set_options: None,
            sl_reflex_sleep: None,
            sl_pcl_set_marker: None,
        })
    }

    /// Resolve a single feature function via `slGetFeatureFunction`. Returns the raw pointer cast
    /// to the requested fn-pointer type.
    ///
    /// # Safety
    /// `T` must match the C++ ABI of the feature function named `name` under `feature`. Must be
    /// called only after `slSetD3DDevice` has succeeded.
    unsafe fn feature_fn<T: Copy>(&self, feature: Feature, name: &str) -> Result<T, StreamlineError> {
        let cname = CString::new(name).expect("feature function name contained an interior NUL");
        let mut ptr: *mut c_void = core::ptr::null_mut();
        // SAFETY: `cname` is a valid NUL-terminated C string that outlives the call; `&mut ptr` is
        // a valid out-param. `self.sl_get_feature_function` is the resolved interposer export.
        let r = unsafe { (self.sl_get_feature_function)(feature, cname.as_ptr().cast(), &mut ptr) };
        if !r.is_ok() {
            return Err(StreamlineError::FeatureFunctionUnavailable {
                feature,
                function: name.to_string(),
                detail: format!("slGetFeatureFunction returned {r:?}"),
            });
        }
        if ptr.is_null() {
            return Err(StreamlineError::FeatureFunctionUnavailable {
                feature,
                function: name.to_string(),
                detail: "slGetFeatureFunction returned a null function pointer".to_string(),
            });
        }
        // SAFETY: `ptr` is a non-null fn address the size of `*mut c_void`; `T` is a pointer-sized
        // fn type per the caller's contract.
        Ok(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&ptr) })
    }

    /// Resolve all feature functions. MUST be called only after `slSetD3DDevice` succeeded.
    ///
    /// # Safety
    /// Requires that `slSetD3DDevice` was already called successfully on this interposer; otherwise
    /// `slGetFeatureFunction` has no device context to resolve against.
    pub unsafe fn resolve_feature_functions(&mut self) -> Result<(), StreamlineError> {
        // SAFETY: each `feature_fn::<Pfn*>` uses a fn type matching the named feature function's
        // C++ ABI, and we are (by this fn's contract) past `slSetD3DDevice`.
        self.sl_dlssg_set_options =
            Some(unsafe { self.feature_fn(K_FEATURE_DLSS_G, "slDLSSGSetOptions")? });
        self.sl_dlssg_get_state =
            Some(unsafe { self.feature_fn(K_FEATURE_DLSS_G, "slDLSSGGetState")? });
        self.sl_reflex_set_options =
            Some(unsafe { self.feature_fn(K_FEATURE_REFLEX, "slReflexSetOptions")? });
        self.sl_reflex_sleep = Some(unsafe { self.feature_fn(K_FEATURE_REFLEX, "slReflexSleep")? });
        // PCL marker function lives in the PCL feature (kFeaturePCL). The header name is
        // `slPCLSetMarker` (there is no `slReflexSetMarker` in 2.11.1 — marker setting moved to PCL).
        self.sl_pcl_set_marker = Some(unsafe { self.feature_fn(K_FEATURE_PCL, "slPCLSetMarker")? });
        Ok(())
    }
}
