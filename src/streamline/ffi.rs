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

use super::api::StreamlineApi;
use super::security::{verify_interposer_signature, verify_signed_dll};
use super::types::*;
use core::ffi::c_void;
use libloading::os::windows as ll;
use std::ffi::{CString, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Environment variable that points at the Streamline SDK root.
const STREAMLINE_SDK_ENV: &str = "STREAMLINE_SDK";

/// The sibling SL plugin DLLs the interposer pulls in (via the DEFAULT search order) once it is
/// active. We Authenticode + NVIDIA-pin each present one BEFORE `slInit` (M8), because
/// `verify_interposer_signature` covers only the interposer — an attacker who drops a malicious
/// sibling where the loader resolves it would otherwise get code execution despite a pristine
/// interposer. All are genuinely NVIDIA-signed, so the happy path still loads. Verified only if
/// PRESENT: `sl.common.dll` is effectively mandatory, but the DLSS-G-only plugins and the NGX runtime
/// may be absent in a non-FG staging, so a missing sibling is skipped, not failed.
const SIBLING_SL_PLUGINS: &[&str] = &[
    "sl.common.dll",
    "sl.dlss_g.dll",
    "sl.reflex.dll",
    "sl.pcl.dll",
    "nvngx_dlssg.dll",
];

/// Authenticode + NVIDIA-pin every known sibling SL plugin (see [`SIBLING_SL_PLUGINS`]) that is
/// present, BEFORE the interposer is loaded, reusing the interposer's trust gate via
/// [`verify_signed_dll`]. This does NOT change the DLL search order (a constrained search or a
/// `\\?\` path makes `slInit` fail with `eErrorNoPlugins`); it is a pre-load verification pass only.
///
/// **Which directories (M8 — corrected):** the Windows default search order resolves the siblings
/// from the **executable's directory FIRST**, and the validated deployment stages the SL plugins
/// *beside the exe* (see `docs/SETUP.md`); the interposer's own `$STREAMLINE_SDK/bin/x64` dir is an
/// alternate staging. So we verify the present siblings in BOTH the exe directory (where the loaded
/// copies live) and `interposer_dir`, de-duplicated.
///
/// **Residual surface (NOT closed):** because the search order is (correctly) preserved, this covers
/// only a KNOWN, ENUMERATED set in those two dirs — it cannot cover an attacker-renamed/unknown DLL
/// the default search might still resolve. (The `dxgi.dll`/`d3d12.dll` loader-shims are handled
/// separately: this crate uses the proxy path, not loader-shims, and rejects a renamed-interposer
/// `dxgi.dll`/`d3d12.dll` beside the exe at load time — see [`reject_loader_shims`].) Those dirs MUST
/// therefore be on ACL-restricted storage writable only by an administrator.
fn verify_sibling_plugins(interposer_dir: &std::path::Path) -> Result<(), StreamlineError> {
    // Build the de-duplicated directory set: the exe dir (searched first / validated staging) then
    // the interposer's own dir. A failure to locate the exe dir is non-fatal (we still cover the
    // interposer dir); the residual-surface doc above already requires ACL-restricted storage.
    let mut dirs: Vec<PathBuf> = Vec::new();
    // Combinator (not an `if let ... && ...` let-chain) so this stays on the crate's 1.87 MSRV —
    // let-chains only stabilized in Rust 1.88.
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
    {
        dirs.push(exe_dir);
    }
    if !dirs.iter().any(|d| d == interposer_dir) {
        dirs.push(interposer_dir.to_path_buf());
    }

    for dir in &dirs {
        for name in SIBLING_SL_PLUGINS {
            let plugin = dir.join(name);
            if !plugin.exists() {
                continue;
            }
            verify_signed_dll(&plugin)?;
            log::info!("verified sibling SL plugin signature: {}", plugin.display());
        }
    }
    Ok(())
}

/// The loader-shim filenames SL uses for its alternative interposition mode: `sl.interposer.dll`
/// copied beside the host exe as `dxgi.dll`/`d3d12.dll` so it fronts the *system* DXGI/D3D12. This
/// crate does NOT use that mode (see [`reject_loader_shims`]).
const LOADER_SHIM_NAMES: &[&str] = &["dxgi.dll", "d3d12.dll"];

/// True iff `a` and `b` are byte-for-byte identical files. A read failure on either is treated as
/// "not identical" — the caller then falls through to the normal interposer load, which surfaces any
/// genuine problem with its own typed error.
fn files_have_equal_bytes(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Core (testable) loader-shim detector: return the first `dxgi.dll`/`d3d12.dll` in `exe_dir` for
/// which `is_shim` is true. The predicate is injected so this is unit-testable without staging real
/// DLLs; production tests **byte-identity to the interposer about to be loaded** (see
/// [`reject_loader_shims`]). That is precise: an unrelated `dxgi.dll` (a system forwarder, or a non-SL
/// wrapper such as ReShade) is never an exact copy of the interposer, so it is NOT flagged.
fn find_loader_shim(exe_dir: &Path, is_shim: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    for name in LOADER_SHIM_NAMES {
        let candidate = exe_dir.join(name);
        if candidate.is_file() && is_shim(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Fail fast on the SL loader-shim ⨯ wgpu-fork-proxy conflict, BEFORE the interposer is touched.
///
/// SL supports two mutually-exclusive interposition modes. This crate uses the **proxy** path: it
/// loads `sl.interposer.dll` explicitly and the wgpu fork upgrades its DXGI factory to an SL proxy.
/// The **loader-shim** mode instead stages `sl.interposer.dll` beside the exe as `dxgi.dll`/`d3d12.dll`
/// so SL fronts the *system* DXGI/D3D12. If a loader-shim is ALSO present, `slInit`'s `getSystemCaps`
/// enumerates adapters through the shim'd DXGI/D3D12, re-enters the interposer, and recurses until the
/// stack overflows — a hard crash SL only reports as the opaque `eErrorExceptionHandler`. We detect
/// the shim as a `dxgi.dll`/`d3d12.dll` beside the exe that is **byte-identical to `interposer`** (the
/// one we are about to load): the documented mistake copies `sl.interposer.dll` to those names from the
/// same SDK, so they match exactly. This is precise (an unrelated `dxgi.dll` never matches, so there is
/// no false positive — including under the `STREAMLINE_ALLOW_UNVERIFIED_SIGNER` opt-out, which this
/// path does not consult) and we return an actionable [`StreamlineError::LoaderShimConflict`] instead
/// of crashing. Files are read only for a `dxgi.dll`/`d3d12.dll` that actually exists beside the exe,
/// so the happy path pays nothing. Not being able to locate the exe dir is non-fatal.
fn reject_loader_shims(interposer: &Path) -> Result<(), StreamlineError> {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return Ok(());
    };
    if let Some(shim) = find_loader_shim(&exe_dir, |candidate| {
        files_have_equal_bytes(candidate, interposer)
    }) {
        return Err(StreamlineError::LoaderShimConflict(shim));
    }
    Ok(())
}

/// Build the absolute path to the interposer (`<sdk>/bin/x64/sl.interposer.dll`) from an explicit
/// SDK root, or [`StreamlineError::SdkPathNotSet`] if `sdk` is `None`.
///
/// Split from [`interposer_path`] (which reads the env var) so the path-building + missing-SDK logic
/// is unit-testable without mutating the process environment.
///
/// THIS path is only used to obtain the exported `sl*` entry points. (This crate does NOT use SL's
/// loader-shim mode: a `dxgi.dll`/`d3d12.dll` copy of the interposer beside the exe is rejected at
/// load time — see [`reject_loader_shims`].)
fn interposer_path_from(sdk: Option<OsString>) -> Result<PathBuf, StreamlineError> {
    let sdk = sdk.ok_or(StreamlineError::SdkPathNotSet)?;
    let mut path = PathBuf::from(sdk);
    path.push("bin");
    path.push("x64");
    path.push("sl.interposer.dll");
    Ok(path)
}

/// Resolve the interposer path from an explicit SDK root and confirm the file exists on disk,
/// mapping the two pre-load failure modes ([`StreamlineError::SdkPathNotSet`] /
/// [`StreamlineError::InterposerNotFound`]). Split from [`SlApi::load`] so those branches are
/// unit-testable without loading a DLL.
fn resolve_existing_interposer(sdk: Option<OsString>) -> Result<PathBuf, StreamlineError> {
    let path = interposer_path_from(sdk)?;
    if !path.exists() {
        return Err(StreamlineError::InterposerNotFound(path));
    }
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

    // Feature-level functions, resolved exactly once (after slSetD3DDevice) via slGetFeatureFunction.
    // `OnceLock` gives interior mutability with `Send + Sync` and a set-once invariant, so the
    // `StreamlineApi` trait can resolve them through `&self` (the trait is `&self` everywhere).
    feature_fns: OnceLock<FeatureFns>,
}

/// The DLSS-G / Reflex / PCL feature functions, resolved together (all-or-nothing) via
/// `slGetFeatureFunction` after `slSetD3DDevice`. Held behind a [`OnceLock`] in [`SlApi`].
struct FeatureFns {
    dlssg_set_options: PfnSlDLSSGSetOptions,
    dlssg_get_state: PfnSlDLSSGGetState,
    reflex_set_options: PfnSlReflexSetOptions,
    reflex_sleep: PfnSlReflexSleep,
    pcl_set_marker: PfnSlPCLSetMarker,
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
            symbol: String::from_utf8_lossy(name)
                .trim_end_matches('\0')
                .to_string(),
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
    /// DLL is loaded. We ALSO verify, with the identical gate, every known sibling SL plugin present
    /// in the directories the loader will resolve them from — the **executable's directory** (the
    /// default search order's first hit, and where the validated deployment stages them) and the
    /// interposer's own dir — because the interposer pulls those in via the default search order and
    /// they would otherwise be unverified (see [`verify_sibling_plugins`]). The interposer is then
    /// loaded with the **default** Windows search order: constraining the search (e.g.
    /// `LOAD_LIBRARY_SEARCH_*`) or passing a verbatim `\\?\` path makes `slInit` fail with
    /// `eErrorNoPlugins`, so we deliberately keep the plain load the hardware-validated path uses.
    ///
    /// SECURITY POSTURE (not a closed surface): because the default search order is preserved, this
    /// verifies only a KNOWN, ENUMERATED set of siblings. It does NOT cover an attacker-renamed or
    /// otherwise-unknown DLL the search might still resolve. (The `dxgi.dll`/`d3d12.dll` loader-shims
    /// are not part of this crate's deployment — it uses the proxy path — and a renamed-interposer
    /// `dxgi.dll`/`d3d12.dll` beside the exe is rejected before load; see [`reject_loader_shims`].) The
    /// `$STREAMLINE_SDK/bin/x64` directory and the exe directory MUST therefore be on ACL-restricted
    /// storage writable only by an administrator. Feature functions are left `None` until
    /// [`SlApi::resolve_feature_functions`] runs (after `slSetD3DDevice`).
    ///
    /// # Safety
    /// Loads a native DLL and resolves raw C function pointers whose declared signatures are
    /// asserted (not checked) to match the interposer's ABI. The returned `SlApi` exposes
    /// `unsafe extern "C"` pointers; calling them is `unsafe`. Must be called on Windows with the
    /// Streamline SDK installed.
    pub unsafe fn load() -> Result<Self, StreamlineError> {
        let path = resolve_existing_interposer(std::env::var_os(STREAMLINE_SDK_ENV))?;

        // Fail fast on the SL loader-shim conflict BEFORE we touch the interposer: a dxgi.dll/d3d12.dll
        // byte-copy of the interposer beside the exe would otherwise make slInit's getSystemCaps recurse
        // and overflow the stack. This crate interposes via the wgpu-fork proxy path, not loader-shims.
        reject_loader_shims(&path)?;

        // Hard gate: refuse to load an interposer that is not a trusted, NVIDIA-signed binary. We
        // verify and load the SAME `path` value (no canonicalization: a `\\?\` verbatim path breaks
        // the interposer's relative plugin discovery -> slInit eErrorNoPlugins).
        //
        // L10: the returned guard holds an open handle to the interposer file with FILE_SHARE_READ
        // ONLY (no write/delete sharing). We keep it alive across `Library::new` below so the exact
        // bytes that just passed WinVerifyTrust are far harder to swap before LoadLibrary maps them
        // (NARROWS — does not absolutely close — the verify->load TOCTOU). Dropped after the image is
        // resident + symbols resolved.
        let _verified = verify_interposer_signature(&path)?;

        // M8: the interposer pulls in its sibling SL plugins via the default search order, NONE of
        // which `verify_interposer_signature` covers. Authenticode + NVIDIA-pin each present sibling
        // (in the exe dir — searched first — and the interposer dir) BEFORE we map the interposer, so
        // a malicious sibling cannot ride in. We do NOT alter the search order (that breaks slInit);
        // this is a pre-load verification pass only and cannot cover an unknown/renamed DLL — the
        // staging dirs must additionally be ACL-restricted (see `verify_sibling_plugins`).
        if let Some(plugin_dir) = path.parent() {
            verify_sibling_plugins(plugin_dir)?;
        }
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

        // The interposer image is now resident (mapped by Library::new); the share-locked handle has
        // served its purpose of keeping the verified bytes immutable across verify+load, so release
        // it. Dropping HERE (not earlier) is what kept the window narrow. `VerifiedDll::Drop` closes
        // the FILE HANDLE only — never the leaked `Library` (no FreeLibrary; constraint preserved).
        drop(_verified);

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
            feature_fns: OnceLock::new(),
        })
    }

    /// Resolve a single feature function via `slGetFeatureFunction`. Returns the raw pointer cast
    /// to the requested fn-pointer type.
    ///
    /// # Safety
    /// `T` must match the C++ ABI of the feature function named `name` under `feature`. Must be
    /// called only after `slSetD3DDevice` has succeeded.
    unsafe fn feature_fn<T: Copy>(
        &self,
        feature: Feature,
        name: &str,
    ) -> Result<T, StreamlineError> {
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

    /// The resolved feature functions, or [`StreamlineError::FeatureFunctionUnavailable`] (tagged
    /// with `feature`/`function`) until [`StreamlineApi::resolve_feature_functions`] has run. Used by
    /// the trait impl's feature-level methods to map "not resolved" to the typed error.
    fn resolved_feature_fns(
        &self,
        feature: Feature,
        function: &str,
    ) -> Result<&FeatureFns, StreamlineError> {
        self.feature_fns
            .get()
            .ok_or_else(|| StreamlineError::FeatureFunctionUnavailable {
                feature,
                function: function.to_string(),
                detail: "feature function not resolved (resolve_feature_functions not called)"
                    .to_string(),
            })
    }
}

/// The production [`StreamlineApi`]: each method forwards to the resolved interposer fn-pointer.
/// The best-effort Reflex/PCL methods (`reflex_sleep`/`set_marker`) log-and-swallow a non-Ok result
/// exactly as the old `reflex` helpers did, so a dropped latency signal never aborts a frame.
impl StreamlineApi for SlApi {
    unsafe fn set_d3d_device(&self, device: *mut c_void) -> SlResult {
        // SAFETY: `device` is a live ID3D12Device* per the caller's contract; `sl_set_d3d_device`
        // is the resolved core export.
        unsafe { (self.sl_set_d3d_device)(device) }
    }

    unsafe fn resolve_feature_functions(&self) -> Result<(), StreamlineError> {
        // SAFETY: each `feature_fn::<Pfn*>` uses a fn type matching the named feature function's
        // C++ ABI, and we are (by this fn's contract) past `slSetD3DDevice`.
        let fns = FeatureFns {
            dlssg_set_options: unsafe { self.feature_fn(K_FEATURE_DLSS_G, "slDLSSGSetOptions")? },
            dlssg_get_state: unsafe { self.feature_fn(K_FEATURE_DLSS_G, "slDLSSGGetState")? },
            reflex_set_options: unsafe { self.feature_fn(K_FEATURE_REFLEX, "slReflexSetOptions")? },
            reflex_sleep: unsafe { self.feature_fn(K_FEATURE_REFLEX, "slReflexSleep")? },
            // PCL marker function lives in the PCL feature (kFeaturePCL). The header name is
            // `slPCLSetMarker` (there is no `slReflexSetMarker` in 2.11.1 — marker setting moved to
            // PCL).
            pcl_set_marker: unsafe { self.feature_fn(K_FEATURE_PCL, "slPCLSetMarker")? },
        };
        // Resolution runs exactly once during context setup; a second call is a no-op.
        let _ = self.feature_fns.set(fns);
        Ok(())
    }

    unsafe fn is_feature_supported(
        &self,
        feature: Feature,
        adapter_info: *const AdapterInfo,
    ) -> SlResult {
        // SAFETY: `adapter_info` is a live `sl::AdapterInfo` per the caller's contract;
        // `sl_is_feature_supported` is the resolved core export.
        unsafe { (self.sl_is_feature_supported)(feature, adapter_info) }
    }

    fn reflex_set_options(&self, mode: ReflexMode) -> Result<SlResult, StreamlineError> {
        let set = self
            .resolved_feature_fns(K_FEATURE_REFLEX, "slReflexSetOptions")?
            .reflex_set_options;
        let opts = ReflexOptions::new(mode);
        // SAFETY: `opts` is a fully-initialized `sl::ReflexOptions` living on the stack through the
        // call; `set` is the resolved Reflex feature fn.
        Ok(unsafe { set(&opts) })
    }

    unsafe fn dlssg_set_options(
        &self,
        viewport: *const ViewportHandle,
        options: *const DLSSGOptions,
    ) -> Result<SlResult, StreamlineError> {
        let set = self
            .resolved_feature_fns(K_FEATURE_DLSS_G, "slDLSSGSetOptions")?
            .dlssg_set_options;
        // SAFETY: `viewport`/`options` are live per the caller's contract; `set` is the resolved
        // DLSS-G feature fn.
        Ok(unsafe { set(viewport, options) })
    }

    unsafe fn dlssg_get_state(
        &self,
        viewport: *const ViewportHandle,
        state: *mut DLSSGState,
        options: *const DLSSGOptions,
    ) -> Result<SlResult, StreamlineError> {
        let get = self
            .resolved_feature_fns(K_FEATURE_DLSS_G, "slDLSSGGetState")?
            .dlssg_get_state;
        // SAFETY: in/out params are live per the caller's contract; `get` is the resolved DLSS-G
        // feature fn.
        Ok(unsafe { get(viewport, state, options) })
    }

    unsafe fn get_new_frame_token(&self, frame_index: u32) -> (SlResult, *mut FrameToken) {
        let mut token: *mut FrameToken = core::ptr::null_mut();
        // SAFETY: `&frame_index` is a valid `*const u32` in-param; `&mut token` is a valid out-param;
        // `sl_get_new_frame_token` is the resolved core export.
        let r = unsafe { (self.sl_get_new_frame_token)(&mut token, &frame_index) };
        (r, token)
    }

    unsafe fn reflex_sleep(&self, token: *mut FrameToken) {
        // Best-effort: a missing feature function or non-Ok result is logged and swallowed.
        if let Some(fns) = self.feature_fns.get() {
            // SAFETY: `token` is this frame's live token per the caller's contract; `reflex_sleep`
            // is the resolved Reflex feature fn.
            let r = unsafe { (fns.reflex_sleep)(token) };
            if !r.is_ok() {
                log::debug!("slReflexSleep returned {r:?}");
            }
        }
    }

    unsafe fn set_marker(&self, marker: PCLMarker, token: *mut FrameToken) {
        // Best-effort: a missing feature function or non-Ok result is logged and swallowed.
        if let Some(fns) = self.feature_fns.get() {
            // SAFETY: `token` is this frame's live token per the caller's contract; `pcl_set_marker`
            // is the resolved PCL feature fn; `marker` is a valid `#[repr(u32)]` enum value.
            let r = unsafe { (fns.pcl_set_marker)(marker, token) };
            if !r.is_ok() {
                log::trace!("slPCLSetMarker({marker:?}) returned {r:?}");
            }
        }
    }

    unsafe fn set_constants(
        &self,
        values: *const Constants,
        frame: *const FrameToken,
        viewport: *const ViewportHandle,
    ) -> SlResult {
        // SAFETY: all three pointers are live per the caller's contract; `sl_set_constants` is the
        // resolved core export.
        unsafe { (self.sl_set_constants)(values, frame, viewport) }
    }

    unsafe fn set_tag_for_frame(
        &self,
        frame: *const FrameToken,
        viewport: *const ViewportHandle,
        tags: *const ResourceTag,
        num_tags: u32,
        cmd_buffer: *mut c_void,
    ) -> SlResult {
        // SAFETY: `frame`/`viewport`/`tags` are live per the caller's contract; `cmd_buffer` is a
        // live open command list; `sl_set_tag_for_frame` is the resolved core export.
        unsafe { (self.sl_set_tag_for_frame)(frame, viewport, tags, num_tags, cmd_buffer) }
    }

    unsafe fn shutdown(&self) -> SlResult {
        // SAFETY: slInit succeeded and shutdown is called once per the caller's contract;
        // `sl_shutdown` is the resolved core export.
        unsafe { (self.sl_shutdown)() }
    }
}

#[cfg(test)]
mod tests {
    //! Headless tests for the interposer-path resolution (the two pre-load failure modes). They
    //! never load a DLL and inject the SDK root explicitly, so they do not touch the process
    //! environment.

    use super::StreamlineError;
    use super::{
        files_have_equal_bytes, find_loader_shim, interposer_path_from, ll, resolve,
        resolve_existing_interposer,
    };
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn missing_sdk_env_is_reported() {
        assert!(matches!(
            interposer_path_from(None),
            Err(StreamlineError::SdkPathNotSet)
        ));
        assert!(matches!(
            resolve_existing_interposer(None),
            Err(StreamlineError::SdkPathNotSet)
        ));
    }

    #[test]
    fn path_is_built_under_bin_x64() {
        let p = interposer_path_from(Some(OsString::from("C:/some/sdk"))).unwrap();
        assert_eq!(p.file_name().unwrap(), "sl.interposer.dll");
        assert!(p.components().any(|c| c.as_os_str() == "bin"));
        assert!(p.components().any(|c| c.as_os_str() == "x64"));
    }

    #[test]
    fn missing_interposer_file_is_reported() {
        // An SDK root with no `bin/x64/sl.interposer.dll` under it must map to InterposerNotFound
        // (not a panic / not SdkPathNotSet). A nonexistent root suffices — the check is on the file.
        let sdk = std::env::temp_dir().join("dlss_wgpu_dx12_nonexistent_sdk_root_for_test");
        let result = resolve_existing_interposer(Some(sdk.into_os_string()));
        assert!(matches!(
            result,
            Err(StreamlineError::InterposerNotFound(_))
        ));
    }

    // The two tests below exercise the `resolve::<T>` helper directly, with NO signature gate, NO
    // Streamline SDK, and NO GPU. `resolve` is independent of the Authenticode check
    // (`verify_interposer_signature` is only called inside `SlApi::load`, never inside `resolve`),
    // so we drive it against `kernel32.dll` — always mapped into every Win32 process, so the load
    // cannot fail and needs no NVIDIA signature. This is the low-cost option from the L6 backlog: it
    // covers the `MissingExport` branch and the success (`transmute_copy`) branch that were
    // previously reachable only by loading a real signed interposer.

    /// Resolving a symbol guaranteed NOT to exist must map to `MissingExport` carrying the requested
    /// symbol name (NUL trimmed) — the `lib.get(name).map_err(...)` arm of `resolve`, with no signed
    /// DLL or GPU.
    #[test]
    fn resolve_absent_symbol_maps_to_missing_export() {
        // SAFETY: `kernel32.dll` is a real, already-mapped system DLL; loading it by bare name is
        // safe and runs no untrusted entry point.
        let lib = unsafe { ll::Library::new("kernel32.dll") }
            .expect("kernel32.dll is always loadable in a Win32 process");
        // SAFETY: the symbol does not exist, so `resolve` returns `Err` before any transmute; `T` is
        // a pointer-sized `unsafe extern "C" fn` (Copy), satisfying `resolve`'s contract even on the
        // (unreached) success path.
        let result = unsafe {
            resolve::<unsafe extern "C" fn()>(
                &lib,
                b"dlss_wgpu_dx12_definitely_not_a_real_export\0",
            )
        };
        match result {
            Err(StreamlineError::MissingExport { symbol, .. }) => {
                assert_eq!(symbol, "dlss_wgpu_dx12_definitely_not_a_real_export");
            }
            other => panic!("expected MissingExport, got {other:?}"),
        }
    }

    /// Resolving a symbol that DOES exist must succeed and yield a non-null fn-pointer — the
    /// `Ok(transmute_copy(...))` arm. `GetCurrentProcessId` is a stable kernel32 export; we only
    /// confirm the resolved pointer is non-null (we never CALL it through the deliberately-wrong
    /// `fn()` signature, whose real ABI is `extern "system" fn() -> u32`).
    #[test]
    fn resolve_present_symbol_yields_non_null_pointer() {
        // SAFETY: see above — loading kernel32.dll by bare name is safe.
        let lib = unsafe { ll::Library::new("kernel32.dll") }
            .expect("kernel32.dll is always loadable in a Win32 process");
        // SAFETY: `GetCurrentProcessId` is exported by kernel32; `T` is pointer-sized. We do not
        // invoke the returned pointer, only check that resolution produced a non-null address.
        let resolved = unsafe { resolve::<unsafe extern "C" fn()>(&lib, b"GetCurrentProcessId\0") };
        let f = resolved.expect("GetCurrentProcessId is a stable kernel32 export");
        assert_ne!(
            f as usize, 0,
            "resolved a present export to a null fn-pointer"
        );
    }

    /// The detector flags a `dxgi.dll`/`d3d12.dll` beside the exe that is byte-identical to the
    /// interposer (the SL interposer renamed) but leaves a differing/unrelated `dxgi.dll` and non-shim
    /// names alone. Exercises the REAL byte-identity predicate (`files_have_equal_bytes`) — no signed
    /// fixture, no GPU/SDK.
    #[test]
    fn loader_shim_detector_flags_only_interposer_copies() {
        use std::fs;
        const IMG: &[u8] = b"INTERPOSER-IMAGE-BYTES";
        let dir = std::env::temp_dir().join("dlss_wgpu_dx12_loader_shim_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp exe dir");
        let interposer = dir.join("sl.interposer.dll");
        fs::write(&interposer, IMG).expect("write interposer");
        // dxgi.dll is an exact copy (the shim); d3d12.dll is unrelated; sl.common.dll is an exact copy
        // but is NOT a loader-shim name, so it must be ignored.
        fs::write(dir.join("dxgi.dll"), IMG).expect("write dxgi");
        fs::write(dir.join("d3d12.dll"), b"some-other-dll").expect("write d3d12");
        fs::write(dir.join("sl.common.dll"), IMG).expect("write sibling");

        let is_copy = |p: &Path| files_have_equal_bytes(p, &interposer);
        assert_eq!(
            find_loader_shim(&dir, is_copy).as_deref(),
            Some(dir.join("dxgi.dll").as_path()),
            "an exact interposer copy named dxgi.dll must be flagged"
        );

        // Now dxgi.dll differs and d3d12.dll is the exact copy -> d3d12.dll is flagged.
        fs::write(dir.join("dxgi.dll"), b"now-unrelated").expect("rewrite dxgi");
        fs::write(dir.join("d3d12.dll"), IMG).expect("rewrite d3d12");
        assert_eq!(
            find_loader_shim(&dir, |p: &Path| files_have_equal_bytes(p, &interposer)).as_deref(),
            Some(dir.join("d3d12.dll").as_path())
        );

        // Neither shim is a copy of the interposer -> None (a legitimate unrelated dxgi.dll/d3d12.dll).
        fs::write(dir.join("dxgi.dll"), b"x").expect("rewrite dxgi");
        fs::write(dir.join("d3d12.dll"), b"y").expect("rewrite d3d12");
        assert!(
            find_loader_shim(&dir, |p: &Path| files_have_equal_bytes(p, &interposer)).is_none(),
            "non-copy dxgi/d3d12 must NOT be flagged"
        );

        // Absent shims -> None even if the predicate would say yes.
        let empty = std::env::temp_dir().join("dlss_wgpu_dx12_loader_shim_empty_test");
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).expect("create empty temp dir");
        assert!(find_loader_shim(&empty, |_p: &Path| true).is_none());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&empty);
    }
}
