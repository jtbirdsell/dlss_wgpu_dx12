//! The DLSS Frame Generation (DLSS-G) orchestration: the [`Streamline`] handle, the
//! [`FrameGenerationContext`], and the per-frame [`Frame`] driver.
//!
//! This is the safe, enterprise wrapper around the hardware-validated spike flow (RTX 4090:
//! `numFramesActuallyPresented == 2`). It reproduces the spike's exact per-frame Streamline call
//! order; see [`Frame`] for the ordering the caller must preserve.
//!
//! ## Ordering requirements (read before integrating)
//!
//! 1. [`Streamline::init`] MUST be called **before** you create your [`wgpu::Instance`]. The wgpu
//!    fork upgrades its DXGI factory to a Streamline proxy inside `Instance::init` *only if*
//!    `sl.interposer.dll` is already loaded into the process. If you create the instance first,
//!    DLSS-G can never bind to wgpu's swapchain. This is the single most important rule.
//! 2. [`FrameGenerationContext::new`] MUST be called **after** the [`wgpu::Device`] is created but
//!    **before** `surface.configure()`. `slSetD3DDevice` has to run before the swapchain is created
//!    (configure triggers SL's interposed swapchain-creation hooks); if the device is not yet
//!    registered, SL logs "API hook activated without device being created" and DLSS-G never binds.
//!
//! ## Proven runtime REQUIREMENTS (the spike proved these flip `numFramesActuallyPresented` 1→2)
//!
//! 1. The presented **window must be visible + foreground/composited**, or DLSS-G silently declines
//!    to present generated frames.
//! 2. `IDXGISwapChain3::GetCurrentBackBufferIndex()` MUST be called **every frame** — wgpu never
//!    calls it itself, and DLSS-G needs it (`eFailGetCurrentBackBufferIndexNotCalled`). This is now
//!    bound into [`Frame::acquire`] so it cannot be skipped; the manual escape hatch is
//!    [`FrameGenerationContext::current_back_buffer_index`].
//! 3. Motion vectors must be normalized to `[-1, 1]`: pixel-space mvecs use `mvec_scale = (1/w, 1/h)`
//!    (see [`super::tagging::FgConstants::with_pixel_motion`]).
//! 4. Set `camera_motion_included = true` when the mvec buffer carries full motion.
//! 5. Use a **non-vsync present mode** (`Mailbox`/`Immediate`) so Reflex/DLSS-G own frame pacing.

use super::api::StreamlineApi;
use super::ffi::SlApi;
use super::tagging::{dxgi_format_of, FgConstants, FgResources};
use super::types::*;
use crate::hal;
use core::ffi::c_void;
use glam::UVec2;
use std::cell::Cell;
use std::ffi::CString;

/// SL 2.11.1 `kSDKVersion`: `(2<<48)|(11<<32)|(1<<16)|0xfedc` (sl_version.h).
const SL_SDK_VERSION: u64 = (2u64 << 48) | (11u64 << 32) | (1u64 << 16) | 0xfedc;

/// Default Streamline application identity (the spike's proven values).
///
/// `DEFAULT_PROJECT_ID` is an arbitrary GUID; whether NGX accepts a self-generated project id for
/// experimental DLSS-G use is an OPEN QUESTION. A real integration may need an NVIDIA-issued project
/// id — override it via [`Streamline::init_with_identity`].
pub const DEFAULT_PROJECT_ID: &str = "a0f57b54-1daf-4934-90ae-c4035c19df04";
/// Default Streamline engine version string (the spike's proven value).
pub const DEFAULT_ENGINE_VERSION: &str = "0.1.0";

/// The process-wide Streamline handle: a loaded, signature-verified `sl.interposer.dll` plus a
/// completed `slInit` for the DLSS-G + Reflex + PCL features.
///
/// **Create this before your [`wgpu::Instance`].** See the [module docs](self) for why.
///
/// `slShutdown` runs on `Drop` **while this handle still owns the API**. A successful
/// [`FrameGenerationContext::new`] *moves* the API into the context, so this handle's `Drop` then
/// becomes a no-op (the context's `Drop` runs `slShutdown` instead). A *failed*
/// `FrameGenerationContext::new` leaves this handle intact and reusable.
pub struct Streamline {
    api: Option<Box<dyn StreamlineApi>>,
}

impl Streamline {
    /// Loads the verified interposer and runs `slInit` for DLSS-G, Reflex, and PCL, with the proven
    /// default app identity ([`DEFAULT_PROJECT_ID`] / [`DEFAULT_ENGINE_VERSION`]).
    ///
    /// MUST be called before [`wgpu::Instance`] creation (see the [module docs](self)).
    pub fn init() -> Result<Self, StreamlineError> {
        Self::init_with_identity(DEFAULT_PROJECT_ID, DEFAULT_ENGINE_VERSION)
    }

    /// Like [`Self::init`] but carries an explicit application identity into `slInit`.
    ///
    /// `project_id` and `engine_version` populate `Preferences::project_id` /
    /// `Preferences::engine_version` (`engine` stays [`EngineType::Custom`]). A real integration may
    /// need an NVIDIA-issued `project_id`. Both strings must be free of interior NUL bytes; they are
    /// NUL-terminated into [`CString`]s that are kept alive across the `slInit` call.
    ///
    /// Uses frame-based resource tagging (required by `slSetTagForFrame`) and disables SL's command
    /// list state tracking (so SL does not fight wgpu's barriers) — the proven flag combination.
    pub fn init_with_identity(
        project_id: &str,
        engine_version: &str,
    ) -> Result<Self, StreamlineError> {
        // SAFETY: `SlApi::load` locates, signature-verifies, and loads the NVIDIA interposer, then
        // resolves the exported core `sl*` functions whose declared ABIs match the headers. We make
        // no other use of the raw pointers here beyond the documented `slInit` call below.
        let api = unsafe { SlApi::load() }?;

        // App identity. The CStrings must outlive the slInit call (Preferences only borrows the
        // pointers), so they live in this scope through the call below. An interior NUL is a caller
        // bug; surface it as a typed error rather than panicking.
        let project_id_c = CString::new(project_id).map_err(|_| StreamlineError::SlCall {
            function: "slInit (project_id contained an interior NUL)".to_string(),
            result: SlResult::ErrorInvalidParameter,
        })?;
        let engine_version_c = CString::new(engine_version).map_err(|_| StreamlineError::SlCall {
            function: "slInit (engine_version contained an interior NUL)".to_string(),
            result: SlResult::ErrorInvalidParameter,
        })?;

        // DLSS-G needs all three features loaded: DLSS-G itself, Reflex (mandatory dependency), and
        // PCL (the latency markers). Order matches the spike.
        let features = [K_FEATURE_DLSS_G, K_FEATURE_REFLEX, K_FEATURE_PCL];
        let mut prefs = Preferences::new();
        prefs.features_to_load = features.as_ptr();
        prefs.num_features_to_load = features.len() as u32;
        prefs.render_api = RenderAPI::D3D12;
        // Frame-based tagging is required for slSetTagForFrame; CL-state-tracking stays disabled so
        // SL does not duplicate/conflict with wgpu's resource barriers (the proven combination).
        prefs.flags = preference_flags::USE_FRAME_BASED_RESOURCE_TAGGING
            | preference_flags::DISABLE_CL_STATE_TRACKING;
        prefs.log_level = LogLevel::Default;
        // App identity — the proven spike run set both (engine stays Custom). `application_id` is
        // left 0; engine + project id provide the identity NGX keys off.
        prefs.engine = EngineType::Custom;
        prefs.engine_version = engine_version_c.as_ptr().cast();
        prefs.project_id = project_id_c.as_ptr().cast();

        // SAFETY: `prefs` is a fully-initialized `#[repr(C)]` `sl::Preferences` that outlives the
        // call; the borrowed `features` array and the `project_id_c`/`engine_version_c` CStrings all
        // live in this scope through the call. `api.sl_init` is the resolved interposer export.
        let r = unsafe { (api.sl_init)(&prefs, SL_SDK_VERSION) };
        // Keep the identity CStrings alive until after the call returns.
        drop(project_id_c);
        drop(engine_version_c);
        if !r.is_ok() {
            // slInit is a resolved core export; a non-Ok result is an SlCall failure. Common causes:
            // non-NVIDIA GPU, missing SL plugins next to the exe, or hardware-scheduling disabled.
            return Err(StreamlineError::SlCall {
                function: "slInit".to_string(),
                result: r,
            });
        }
        log::info!("Streamline initialized (DLSS-G + Reflex + PCL)");
        Ok(Self {
            api: Some(Box::new(api)),
        })
    }
}

impl Drop for Streamline {
    fn drop(&mut self) {
        // If a context took the API (the success path of `FrameGenerationContext::new`), it is
        // responsible for slShutdown on its own drop. Only shut down here if we still own it.
        if let Some(api) = self.api.as_ref() {
            // SAFETY: `shutdown` forwards to the resolved interposer export; slInit succeeded in
            // `init`. Never panic across FFI in Drop — log and continue.
            let r = unsafe { api.shutdown() };
            if !r.is_ok() {
                log::error!("slShutdown returned {r:?}");
            }
        }
    }
}

/// Options selected once when creating a [`FrameGenerationContext`].
#[derive(Clone, Copy, Debug)]
pub struct FrameGenerationOptions {
    /// The DLSS-G mode. Use [`FrameGenerationMode::On`] to enable generation.
    pub mode: FrameGenerationMode,
    /// How many frames to generate between each pair of rendered frames (1 = classic 2x).
    pub num_frames_to_generate: u32,
    /// DXGI numeric format of the swapchain back buffer / HUD-less color. Set it explicitly via
    /// [`Self::with_color_format`] from your chosen surface format; if `None`, SL infers it (0 =
    /// `DXGI_FORMAT_UNKNOWN`).
    pub color_format: Option<u32>,
    /// DXGI numeric format of the motion-vector buffer (optional; SL infers it from the tag if 0).
    pub mvec_format: Option<u32>,
    /// DXGI numeric format of the depth buffer (optional).
    pub depth_format: Option<u32>,
    /// Opt into DLSS-G **UI recomposition**: when `true`, `slDLSSGSetOptions` sets
    /// `enableUserInterfaceRecomposition = eTrue` so DLSS-G recomposites a separately tagged UI
    /// layer over the generated frame (keeping the UI crisp). You must then tag a
    /// [`super::tagging::FgUi`] layer each frame. If left `false` (the default), UI tagging via
    /// [`super::tagging::FgUi`] is still *supported* (the tag is accepted) but recomposition is off.
    pub enable_ui_recomposition: bool,
    /// DXGI numeric format of the UI buffer, used only when [`Self::enable_ui_recomposition`] is
    /// `true`. Set it via [`Self::with_ui_format`]; if `None`, SL infers it (0).
    pub ui_format: Option<u32>,
}

impl FrameGenerationOptions {
    /// Enabled (mode = On), generating one frame (classic 2x), formats inferred from the tags, UI
    /// recomposition off.
    pub fn enabled() -> Self {
        Self {
            mode: FrameGenerationMode::On,
            num_frames_to_generate: 1,
            color_format: None,
            mvec_format: None,
            depth_format: None,
            enable_ui_recomposition: false,
            ui_format: None,
        }
    }

    /// Sets the back-buffer / HUD-less color DXGI format from a [`wgpu::TextureFormat`].
    pub fn with_color_format(mut self, format: wgpu::TextureFormat) -> Self {
        self.color_format = Some(dxgi_format_of(format));
        self
    }

    /// Enables UI recomposition and sets the UI buffer's DXGI format from a [`wgpu::TextureFormat`].
    ///
    /// Sets [`Self::enable_ui_recomposition`] `true` and [`Self::ui_format`] from `format`. Tag a
    /// [`super::tagging::FgUi`] layer each frame for DLSS-G to recomposite.
    pub fn with_ui_recomposition(mut self, format: wgpu::TextureFormat) -> Self {
        self.enable_ui_recomposition = true;
        self.ui_format = Some(dxgi_format_of(format));
        self
    }
}

impl Default for FrameGenerationOptions {
    fn default() -> Self {
        Self::enabled()
    }
}

/// DLSS Frame Generation mode (mirrors `sl::DLSSGMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameGenerationMode {
    /// DLSS-G off (no generation).
    Off,
    /// DLSS-G on, generating [`FrameGenerationOptions::num_frames_to_generate`] frame(s).
    On,
    /// DLSS-G decides automatically.
    Auto,
}

impl FrameGenerationMode {
    fn to_sl(self) -> DLSSGMode {
        match self {
            FrameGenerationMode::Off => DLSSGMode::Off,
            FrameGenerationMode::On => DLSSGMode::On,
            FrameGenerationMode::Auto => DLSSGMode::Auto,
        }
    }
}

/// Decoded snapshot of `sl::DLSSGState`, from [`FrameGenerationContext::query_state`].
#[derive(Clone, Debug)]
pub struct FrameGenerationState {
    /// Raw `DLSSGStatus` bitfield.
    pub status: u32,
    /// Human-readable decode of [`Self::status`] (e.g. `"eOk"`).
    pub status_text: String,
    /// `true` iff `status == eOk`.
    pub is_ok: bool,
    /// Frames the runtime actually presented for the last frame — `> 1` means DLSS-G is generating
    /// (the spike's success criterion was `== 2`).
    pub num_frames_actually_presented: u32,
    /// The maximum number of frames DLSS-G can generate on this hardware.
    pub num_frames_to_generate_max: u32,
    /// Estimated VRAM usage, in bytes.
    pub estimated_vram_usage_in_bytes: u64,
}

/// A per-camera DLSS Frame Generation feature, bound to a wgpu DX12 device + adapter.
///
/// Construct it after the device but **before** `surface.configure()` (see the [module docs](self)).
/// It owns the Streamline core API for its lifetime and runs `slShutdown` on `Drop`. Drive one
/// [`Frame`] per rendered frame via [`Self::begin_frame`].
pub struct FrameGenerationContext {
    api: Box<dyn StreamlineApi>,
    device: wgpu::Device,
    viewport: ViewportHandle,
    options: FrameGenerationOptions,
    /// DXGI format of the back buffer / color (resolved at construction).
    color_format: u32,
    /// The last mode that was successfully enabled (mode != Off), used by [`Self::query_state`] so
    /// a transient Off does not mask the real status. Defaults to [`FrameGenerationMode::On`].
    last_enabled_mode: FrameGenerationMode,
    /// Whether `slDLSSGSetOptions(eOn)` succeeded.
    dlssg_enabled: bool,
}

impl FrameGenerationContext {
    /// Binds Streamline to the wgpu DX12 device and enables DLSS-G.
    ///
    /// Performs, in the spike's proven order: `slSetD3DDevice(raw ID3D12Device)` → resolve the
    /// DLSS-G / Reflex / PCL feature functions → `slIsFeatureSupported(kFeatureDLSS_G)` with the
    /// adapter's real LUID → `slReflexSetOptions(eLowLatency)` → `slDLSSGSetOptions(eOn)`.
    ///
    /// MUST be called after [`wgpu::Device`] creation but **before** `surface.configure()`. See the
    /// [module docs](self) for why.
    ///
    /// On the **success path only**, moves the [`Streamline`] handle's core API into this context
    /// (the passed `streamline` then becomes inert — its `slShutdown` moves to this context's
    /// `Drop`). If `new` fails at any step, the `streamline` handle is left **intact and reusable**
    /// (it still owns the API and its `Drop` still runs `slShutdown`). Calling `new` twice on the
    /// same handle returns [`StreamlineError::ContextAlreadyCreated`].
    pub fn new(
        streamline: &mut Streamline,
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        options: &FrameGenerationOptions,
    ) -> Result<Self, StreamlineError> {
        // Borrow the API for all fallible setup; only MOVE it into the context on the success path.
        // A failure therefore leaves `streamline.api` owning the interposer (its Drop slShutdowns),
        // never orphaning the loaded+initialized interposer (finding B).
        let api = streamline
            .api
            .as_mut()
            .ok_or(StreamlineError::ContextAlreadyCreated)?;

        // --- slSetD3DDevice (before any swapchain is created) ---
        // SAFETY: `with_raw_device` (an unsafe fn) hands the closure the live `ID3D12Device*` of
        // this wgpu device, valid only for the duration of the closure; we only forward it to
        // `slSetD3DDevice`, the resolved interposer export.
        let set = unsafe {
            hal::with_raw_device(device, |raw_device| api.set_d3d_device(raw_device.cast()))
        };
        match set {
            Some(r) if r.is_ok() => log::info!("slSetD3DDevice -> eOk (before swapchain creation)"),
            Some(r) => {
                return Err(StreamlineError::SlCall {
                    function: "slSetD3DDevice".to_string(),
                    result: r,
                });
            }
            None => {
                return Err(StreamlineError::FeatureFunctionUnavailable {
                    feature: K_FEATURE_DLSS_G,
                    function: "slSetD3DDevice".to_string(),
                    detail: "the wgpu device is not a Dx12 device".to_string(),
                });
            }
        }

        // --- Resolve feature functions (after slSetD3DDevice, still before the swapchain) ---
        // SAFETY: `slSetD3DDevice` just succeeded, satisfying `resolve_feature_functions`'s contract.
        unsafe { api.resolve_feature_functions()? };
        log::info!("Resolved DLSS-G / Reflex / PCL feature functions");

        let viewport = ViewportHandle::new(0);
        let color_format = options.color_format.unwrap_or(0);

        // --- slIsFeatureSupported(kFeatureDLSS_G, adapterInfo) ---
        // A C++ reference (the interposer dereferences it), so the AdapterInfo must carry a real
        // LUID — passing a null one crashed the early spike. Fail fast (finding E) if unsupported.
        let mut luid = hal::adapter_luid(adapter);
        let mut adapter_info = AdapterInfo::new();
        if let Some(bytes) = luid.as_mut() {
            adapter_info.device_luid = bytes.as_mut_ptr();
            adapter_info.device_luid_size_in_bytes = bytes.len() as u32;
        } else {
            log::warn!(
                "slIsFeatureSupported: could not read the dx12 adapter LUID; passing a null LUID"
            );
        }
        // SAFETY: `adapter_info` is a fully-initialized `sl::AdapterInfo` whose LUID buffer (`luid`)
        // outlives the call; `is_feature_supported` forwards to the resolved interposer export.
        let supported = unsafe { api.is_feature_supported(K_FEATURE_DLSS_G, &adapter_info) };
        log::info!("slIsFeatureSupported(kFeatureDLSS_G) -> {supported:?}");
        if !supported.is_ok() {
            return Err(StreamlineError::FeatureNotSupported(supported));
        }

        // --- slReflexSetOptions(eLowLatency) — Reflex must be active or DLSS-G fails ---
        // The feature functions were resolved above; a non-Ok result is an SlCall failure.
        let reflex = api.reflex_set_options(ReflexMode::LowLatency)?;
        if !reflex.is_ok() {
            return Err(StreamlineError::SlCall {
                function: "slReflexSetOptions".to_string(),
                result: reflex,
            });
        }
        log::info!("slReflexSetOptions(eLowLatency) -> eOk");

        // --- slDLSSGSetOptions(mode) on the borrowed api (still fallible, before the move) ---
        apply_dlssg_options(&**api, &viewport, options, color_format)?;
        let dlssg_enabled = options.mode != FrameGenerationMode::Off;
        let last_enabled_mode = if dlssg_enabled {
            options.mode
        } else {
            FrameGenerationMode::On
        };

        // SUCCESS: now (and only now) move the API into the context. `take()` is infallible here
        // because `as_mut()` above already proved it was `Some`. From this point the `streamline`
        // handle is inert and our `Drop` owns slShutdown.
        let api = streamline
            .api
            .take()
            .expect("api was Some at the top of new()");

        Ok(Self {
            api,
            device: device.clone(),
            viewport,
            options: *options,
            color_format,
            last_enabled_mode,
            dlssg_enabled,
        })
    }

    /// Re-applies the current [`self.options`] via `slDLSSGSetOptions`. Used by [`Self::set_mode`].
    fn apply_dlssg_options(&mut self) -> Result<(), StreamlineError> {
        apply_dlssg_options(&*self.api, &self.viewport, &self.options, self.color_format)?;
        self.dlssg_enabled = self.options.mode != FrameGenerationMode::Off;
        if self.dlssg_enabled {
            self.last_enabled_mode = self.options.mode;
        }
        Ok(())
    }

    /// Switches the DLSS-G mode at runtime (e.g. to disable generation), re-applying options.
    pub fn set_mode(&mut self, mode: FrameGenerationMode) -> Result<(), StreamlineError> {
        self.options.mode = mode;
        self.apply_dlssg_options()
    }

    /// Whether `slDLSSGSetOptions` last enabled generation (mode != Off).
    pub fn is_enabled(&self) -> bool {
        self.dlssg_enabled
    }

    /// Calls `IDXGISwapChain3::GetCurrentBackBufferIndex()` on the surface's Streamline-proxied
    /// swapchain. **Advanced / manual path.**
    ///
    /// This is **REQUIRED every frame** (proven requirement 2): wgpu never calls it itself, and
    /// DLSS-G reports `eFailGetCurrentBackBufferIndexNotCalled` and declines to generate without it.
    /// The normal per-frame flow does this for you inside [`Frame::acquire`] (which is the
    /// anti-footgun path); use this method only if you are not driving acquisition through [`Frame`].
    /// Returns `None` if the surface is not a configured Dx12 surface.
    #[must_use]
    pub fn current_back_buffer_index(&self, surface: &wgpu::Surface) -> Option<u32> {
        hal::current_back_buffer_index(surface)
    }

    /// Begins a DLSS-G frame: `slGetNewFrameToken(frame_index)` → `slReflexSleep` → PCL
    /// `eSimulationStart` + `eSimulationEnd`.
    ///
    /// Returns a [`Frame`] that carries the frame token and the frame index, and that enforces the
    /// remaining per-frame steps in the proven order. The returned `Frame` MUST be fully driven and
    /// dropped within the same frame it was begun for (see [`Frame`] — the token is stale after
    /// present). `&mut self` makes the per-frame sequence single-threaded.
    pub fn begin_frame(&mut self, frame_index: u32) -> Result<Frame<'_>, StreamlineError> {
        begin_frame_with(&*self.api, self.viewport, frame_index)
    }

    /// Queries `slDLSSGGetState` and decodes it into a [`FrameGenerationState`].
    ///
    /// Poll this periodically (not every frame) to confirm DLSS-G is generating: a healthy result
    /// is `is_ok == true` with `num_frames_actually_presented > 1`.
    ///
    /// The query is built with the **last-enabled** mode (never `Off`), so a transient
    /// [`Self::set_mode`]`(Off)` does not mask the real runtime status (finding G).
    pub fn query_state(&self) -> Result<FrameGenerationState, StreamlineError> {
        query_state_with(
            &*self.api,
            &self.viewport,
            // Build with the last-enabled mode (defaults to On) so a transient Off does not suppress
            // the real status the runtime reports.
            self.last_enabled_mode.to_sl(),
            self.options.num_frames_to_generate,
        )
    }
}

/// Begins a DLSS-G frame against a borrowed [`StreamlineApi`]: `slGetNewFrameToken(frame_index)` →
/// `slReflexSleep` → PCL `eSimulationStart` + `eSimulationEnd`, returning a [`Frame`] at
/// [`Step::Begun`]. Factored out of [`FrameGenerationContext::begin_frame`] so the per-frame
/// sequence can be driven against a mock api (no `wgpu::Device` required).
fn begin_frame_with<'a>(
    api: &'a dyn StreamlineApi,
    viewport: ViewportHandle,
    frame_index: u32,
) -> Result<Frame<'a>, StreamlineError> {
    // SAFETY: `get_new_frame_token` is the resolved interposer export; the returned token is opaque
    // and only ever passed back to other sl* calls this frame.
    let (r, token) = unsafe { api.get_new_frame_token(frame_index) };
    if !r.is_ok() {
        return Err(StreamlineError::SlCall {
            function: "slGetNewFrameToken".to_string(),
            result: r,
        });
    }
    if token.is_null() {
        return Err(StreamlineError::FeatureFunctionUnavailable {
            feature: K_FEATURE_DLSS_G,
            function: "slGetNewFrameToken".to_string(),
            detail: "returned eOk but a null frame token".to_string(),
        });
    }

    // Reflex pacing point, then the simulation-phase markers (proven order).
    // SAFETY: `token` is the just-acquired live frame token; feature fns are resolved.
    unsafe {
        api.reflex_sleep(token);
        api.set_marker(PCLMarker::SimulationStart, token);
        api.set_marker(PCLMarker::SimulationEnd, token);
    }

    Ok(Frame::new_begun(api, viewport, token, frame_index))
}

/// Queries `slDLSSGGetState` against a borrowed [`StreamlineApi`] and decodes it into a
/// [`FrameGenerationState`]. Factored out of [`FrameGenerationContext::query_state`] so the decode +
/// query-mode logic can be tested against a mock api.
fn query_state_with(
    api: &dyn StreamlineApi,
    viewport: &ViewportHandle,
    mode: DLSSGMode,
    num_frames_to_generate: u32,
) -> Result<FrameGenerationState, StreamlineError> {
    let mut state = DLSSGState::new();
    let mut opts = DLSSGOptions::new();
    opts.mode = mode;
    opts.num_frames_to_generate = num_frames_to_generate;
    // SAFETY: `viewport` is valid; `&mut state` and `&opts` are valid in/out params; the trait impl
    // forwards to the resolved DLSS-G feature fn (or returns FeatureFunctionUnavailable).
    let r = unsafe { api.dlssg_get_state(viewport, &mut state, &opts) }?;
    if !r.is_ok() {
        return Err(StreamlineError::SlCall {
            function: "slDLSSGGetState".to_string(),
            result: r,
        });
    }
    Ok(FrameGenerationState {
        status: state.status,
        status_text: dlssg_status::decode(state.status),
        is_ok: state.status == dlssg_status::OK,
        num_frames_actually_presented: state.num_frames_actually_presented,
        num_frames_to_generate_max: state.num_frames_to_generate_max,
        estimated_vram_usage_in_bytes: state.estimated_vram_usage_in_bytes,
    })
}

/// Builds + applies the `sl::DLSSGOptions` from `options` against a borrowed `api`. Shared by
/// [`FrameGenerationContext::new`] (pre-move, on a borrowed `&SlApi`) and
/// [`FrameGenerationContext::apply_dlssg_options`].
fn apply_dlssg_options(
    api: &dyn StreamlineApi,
    viewport: &ViewportHandle,
    options: &FrameGenerationOptions,
    color_format: u32,
) -> Result<(), StreamlineError> {
    let mut opts = DLSSGOptions::new();
    opts.mode = options.mode.to_sl();
    opts.num_frames_to_generate = options.num_frames_to_generate;
    opts.color_buffer_format = color_format;
    opts.hud_less_buffer_format = color_format;
    opts.mvec_buffer_format = options.mvec_format.unwrap_or(0);
    opts.depth_buffer_format = options.depth_format.unwrap_or(0);
    // UI recomposition (finding F): opt in only when requested; otherwise leave it eFalse (UI
    // tagging is still accepted, just not recomposited).
    if options.enable_ui_recomposition {
        opts.enable_user_interface_recomposition = Boolean::True;
        opts.ui_buffer_format = options.ui_format.unwrap_or(0);
    }
    // SAFETY: `opts` is a fully-initialized `sl::DLSSGOptions` living on the stack through the call;
    // `viewport` is valid; the trait impl forwards to the resolved DLSS-G feature fn (or returns
    // FeatureFunctionUnavailable if it was never resolved).
    let r = unsafe { api.dlssg_set_options(viewport, &opts) }?;
    if !r.is_ok() {
        return Err(StreamlineError::SlCall {
            function: "slDLSSGSetOptions".to_string(),
            result: r,
        });
    }
    log::info!(
        "slDLSSGSetOptions(mode={:?}, numFramesToGenerate={}, uiRecomposition={}) -> eOk",
        options.mode,
        options.num_frames_to_generate,
        options.enable_ui_recomposition
    );
    Ok(())
}

impl Drop for FrameGenerationContext {
    fn drop(&mut self) {
        // Disable DLSS-G, idle the GPU, then shut Streamline down. Never panic across FFI in Drop.
        let mut opts = DLSSGOptions::new();
        opts.mode = DLSSGMode::Off;
        // SAFETY: `opts`/`&self.viewport` are valid; the trait impl forwards to the resolved feature
        // fn. `Err` (feature fn never resolved) means there is nothing to disable — swallow it.
        if let Ok(r) = unsafe { self.api.dlssg_set_options(&self.viewport, &opts) }
            && !r.is_ok()
        {
            log::error!("slDLSSGSetOptions(eOff) during drop returned {r:?}");
        }
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        // SAFETY: `shutdown` forwards to the resolved interposer export; slInit succeeded.
        let r = unsafe { self.api.shutdown() };
        if !r.is_ok() {
            log::error!("slShutdown during drop returned {r:?}");
        }
    }
}

// SAFETY: the raw interposer fn-pointers and the per-frame token are `Copy` and are only ever read
// (called) — never mutated through a shared alias — so moving the context (and its pointers) across
// threads does not create a data race on the pointers themselves. Streamline serializes its own
// process-global state internally, and this context holds the only owning handle to that state.
// The remaining obligation is on the caller: the per-frame sequence (`begin_frame` and the [`Frame`]
// methods) must NOT be driven from multiple threads concurrently. `begin_frame` takes `&mut self`,
// which prevents two `Frame`s from coexisting, but it does not (and these impls do not claim to)
// statically prevent moving a live `Frame` to another thread mid-sequence — that is the caller's
// contract.
unsafe impl Send for FrameGenerationContext {}
unsafe impl Sync for FrameGenerationContext {}

/// The per-frame step the [`Frame`] is currently at, used for lightweight runtime order enforcement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    /// After `begin_frame` (token + reflex sleep + sim markers done).
    Begun,
    /// After `set_constants`.
    Constants,
    /// After `acquire` (PCL render-submit-start emitted, back-buffer index queried).
    Acquired,
    /// After `tag`.
    Tagged,
    /// After `end_render` (PCL render-submit-end emitted).
    RenderEnded,
    /// After `present` (PCL present markers emitted around the present).
    Presented,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Begun => "begin_frame",
            Step::Constants => "set_constants",
            Step::Acquired => "acquire",
            Step::Tagged => "tag",
            Step::RenderEnded => "end_render",
            Step::Presented => "present",
        }
    }
}

/// One DLSS Frame Generation frame, in flight between [`FrameGenerationContext::begin_frame`] and
/// presentation. Holds the `slGetNewFrameToken` token plus the frame index, and exposes the
/// remaining per-frame steps with **runtime order enforcement**.
///
/// ## Lifetime requirement (read this)
///
/// A `Frame` must be **fully driven and dropped within the frame it was begun for**: its token is
/// only valid for that frame, and the per-frame Streamline state (constants, tags, markers) is keyed
/// off it. Do not stash a `Frame` across frames — the token goes stale and DLSS-G will reject the
/// late calls.
///
/// ## Per-frame call order (the spike's proven sequence — enforced at runtime)
///
/// 1. `let frame = ctx.begin_frame(idx)?;`  *(token + reflex sleep + sim markers — done)*
/// 2. [`frame.set_constants`](Self::set_constants)`(&consts)?`  *(reset is auto-OR'd on frame 0)*
/// 3. `let (tex, bbi) = `[`frame.acquire`](Self::acquire)`(&surface)?;`
///    *(PCL render-submit-start + `get_current_texture` + the REQUIRED back-buffer-index query)*
/// 4. record your scene render into `tex`'s view (on your own encoder)
/// 5. [`frame.tag`](Self::tag)`(&mut tag_encoder, &resources)?`  *(on a dedicated raw-only encoder)*
/// 6. submit `[render_encoder.finish(), tag_encoder.finish()]`
/// 7. [`frame.end_render`](Self::end_render)`()`  *(PCL render-submit-end)*
/// 8. [`frame.present`](Self::present)`(tex)`  *(PCL present-start → `present()` → present-end)*
/// 9. *(periodically)* [`ctx.query_state`](FrameGenerationContext::query_state)
///
/// Each method `debug_assert!`s (and `log::error!`s in release) if called out of order; a `Frame`
/// dropped before [`Self::present`] `log::error!`s (it indicates a skipped / aborted frame). This
/// catches sequencing mistakes without a heavy consuming-`self` typestate.
pub struct Frame<'a> {
    /// The Streamline call surface (the context's api, borrowed for this frame). Holding the trait
    /// object (not `&FrameGenerationContext`) is what lets a test drive the per-frame sequence
    /// against a mock with no `wgpu::Device`.
    api: &'a dyn StreamlineApi,
    /// The context's viewport handle (a `Copy` value, snapshotted at `begin_frame`).
    viewport: ViewportHandle,
    token: *mut FrameToken,
    /// The frame index this frame was begun for; drives the auto-reset on frame 0.
    frame_index: u32,
    /// The last completed step, for runtime order enforcement.
    step: Cell<Step>,
    /// Set once [`Self::present`] runs, so [`Drop`] can detect an aborted frame.
    presented: Cell<bool>,
    /// Test-only: set by [`Self::advance`] when a step is called out of order, so the mis-order can
    /// be asserted independent of build profile (the `debug_assert!` only fires in debug builds).
    #[cfg(test)]
    misordered: Cell<bool>,
}

impl<'a> Frame<'a> {
    /// Builds a freshly-begun frame (at [`Step::Begun`]). Used by [`begin_frame_with`] and, in
    /// tests, to construct a frame over a mock api.
    fn new_begun(
        api: &'a dyn StreamlineApi,
        viewport: ViewportHandle,
        token: *mut FrameToken,
        frame_index: u32,
    ) -> Self {
        Self {
            api,
            viewport,
            token,
            frame_index,
            step: Cell::new(Step::Begun),
            presented: Cell::new(false),
            #[cfg(test)]
            misordered: Cell::new(false),
        }
    }

    /// Records that we are advancing from `expected` to `to`, logging/asserting on a mis-order.
    fn advance(&self, expected: Step, to: Step) {
        let current = self.step.get();
        if current != expected {
            #[cfg(test)]
            self.misordered.set(true);
            debug_assert!(
                false,
                "DLSS-G Frame: {} called out of order (expected to be at {:?}, but was at {:?})",
                to.label(),
                expected,
                current
            );
            log::error!(
                "DLSS-G Frame: {} called out of order (expected previous step {:?}, found {:?}); \
                 the proven per-frame sequence was violated and DLSS-G may decline to generate",
                to.label(),
                expected,
                current
            );
        }
        self.step.set(to);
    }

    /// The frame index this frame was begun for (see [`FrameGenerationContext::begin_frame`]).
    pub fn frame_index(&self) -> u32 {
        self.frame_index
    }

    /// The orderable core of [`Self::acquire`]: advance the step and emit the PCL render-submit-start
    /// marker. Split out (no wgpu) so the per-frame ordering can be exercised against a mock api.
    fn acquire_core(&self) {
        self.advance(Step::Constants, Step::Acquired);
        // SAFETY: `self.token` is this frame's live token; PCL marker is best-effort.
        unsafe { self.api.set_marker(PCLMarker::RenderSubmitStart, self.token) };
    }

    /// The orderable core of [`Self::tag`]: advance the step and call `slSetTagForFrame` with an
    /// already-built tag list and the raw command list. Split out so a test can drive it with a
    /// hand-built tag slice + a dummy command-list pointer (no wgpu encoder).
    ///
    /// Note: `advance` runs here, *after* the public [`Self::tag`] shell extracts the raw resources.
    /// On the rare extraction-failure path (a tagged resource is not a Dx12 texture) the public
    /// method returns before reaching this core, so the step stays at `Acquired` rather than
    /// advancing — an inert difference, since that frame is already failing.
    fn tag_core(&self, tags: &[ResourceTag], cmd_list: *mut c_void) -> Result<(), StreamlineError> {
        self.advance(Step::Acquired, Step::Tagged);
        // SAFETY: `tags` points at `tags.len()` live `ResourceTag`s; `self.token`/`&self.viewport`
        // are valid; `cmd_list` is the encoder's open list (or a dummy the mock never derefs);
        // `set_tag_for_frame` forwards to the resolved export.
        let r = unsafe {
            self.api.set_tag_for_frame(
                self.token,
                &self.viewport,
                tags.as_ptr(),
                tags.len() as u32,
                cmd_list,
            )
        };
        if r.is_ok() {
            Ok(())
        } else {
            Err(StreamlineError::SlCall {
                function: "slSetTagForFrame".to_string(),
                result: r,
            })
        }
    }

    /// The orderable core of [`Self::present`]: advance the step, bracket `do_present` with the PCL
    /// present-start / present-end markers, and mark the frame presented. The public method passes
    /// `|| surface_texture.present()`; a test passes a no-op (or a recording) closure.
    fn present_core(&self, do_present: impl FnOnce()) {
        self.advance(Step::RenderEnded, Step::Presented);
        // SAFETY (both markers): `self.token` is this frame's live token; PCL markers are best-effort.
        unsafe { self.api.set_marker(PCLMarker::PresentStart, self.token) };
        do_present();
        unsafe { self.api.set_marker(PCLMarker::PresentEnd, self.token) };
        self.presented.set(true);
    }

    /// `slSetConstants` for this frame: camera matrices, jitter, mvec scale, reset, etc.
    ///
    /// **Auto-reset:** if this frame's index is `0`, `reset` is forced `true` (there is no valid
    /// previous frame yet to reproject from) regardless of `constants.reset`; on every later frame
    /// the caller's `constants.reset` is honored (set it `true` on camera cuts / discontinuities).
    ///
    /// Call after [`FrameGenerationContext::begin_frame`] and before [`Self::acquire`].
    pub fn set_constants(&self, constants: &FgConstants) -> Result<(), StreamlineError> {
        self.advance(Step::Begun, Step::Constants);
        let mut sl_consts = constants.to_sl();
        // Auto-OR reset on the first frame: the Frame knows its index, so the caller cannot forget.
        if self.frame_index == 0 {
            sl_consts.reset = Boolean::True;
        }
        // SAFETY: `sl_consts` is a fully-initialized `sl::Constants` on the stack through the call;
        // `self.token` is this frame's live token; `&self.viewport` is valid; `set_constants`
        // forwards to the resolved interposer export.
        let r = unsafe { self.api.set_constants(&sl_consts, self.token, &self.viewport) };
        if r.is_ok() {
            Ok(())
        } else {
            Err(StreamlineError::SlCall {
                function: "slSetConstants".to_string(),
                result: r,
            })
        }
    }

    /// Acquires the swapchain back buffer for this frame, binding the **two proven requirements that
    /// are easiest to forget** into one call:
    ///
    /// 1. emits the PCL `eRenderSubmitStart` marker (the render phase begins),
    /// 2. calls `surface.get_current_texture()`, and
    /// 3. calls `IDXGISwapChain3::GetCurrentBackBufferIndex()` — **REQUIRED every frame** (proven
    ///    requirement 2); wgpu never calls it, and without it DLSS-G reports
    ///    `eFailGetCurrentBackBufferIndexNotCalled`. Binding it here makes it impossible to skip.
    ///
    /// Returns `(surface_texture, back_buffer_index)`. A `Suboptimal` surface is passed through with
    /// a warning (it usually means the window is occluded / not composited, a state in which DLSS-G
    /// declines to present generated frames). `Outdated`/`Lost`/other statuses map to a typed
    /// [`StreamlineError`]; the caller should reconfigure the surface and retry next frame.
    ///
    /// Call after [`Self::set_constants`], then record your scene into the returned texture's view,
    /// then [`Self::tag`].
    pub fn acquire(
        &self,
        surface: &wgpu::Surface,
    ) -> Result<(wgpu::SurfaceTexture, u32), StreamlineError> {
        // (1) The orderable core (state advance + PCL render-submit-start marker). Runs first, so the
        // Suboptimal / error paths below still leave the frame at `Acquired`, exactly as before.
        self.acquire_core();

        // (2) Acquire the surface texture. The patched wgpu returns a `CurrentSurfaceTexture` enum.
        let texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                log::warn!(
                    "Frame::acquire: get_current_texture -> Suboptimal (window may be occluded / \
                     not composited; DLSS-G may decline to present generated frames)"
                );
                f
            }
            other => {
                return Err(StreamlineError::SurfaceUnavailable {
                    status: format!("{other:?}"),
                });
            }
        };

        // (3) The MANDATORY GetCurrentBackBufferIndex call — now impossible to skip (finding A,
        // proven requirement 2).
        let bbi = hal::current_back_buffer_index(surface).ok_or_else(|| {
            StreamlineError::FeatureFunctionUnavailable {
                feature: K_FEATURE_DLSS_G,
                function: "IDXGISwapChain3::GetCurrentBackBufferIndex".to_string(),
                detail: "the surface is not a configured Dx12 surface (cannot perform the \
                         per-frame GetCurrentBackBufferIndex call DLSS-G requires)"
                    .to_string(),
            }
        })?;

        Ok((texture, bbi))
    }

    /// Tags the DLSS-G input resources for this frame via `slSetTagForFrame`.
    ///
    /// `encoder` MUST be a **dedicated, raw-only** [`wgpu::CommandEncoder`] — one with no wgpu
    /// render/copy passes recorded on it — because the tag is recorded onto its raw
    /// `ID3D12GraphicsCommandList`, and wgpu 29 forbids mixing the wgpu and raw encoding APIs on a
    /// single encoder. Record your scene on a separate encoder, then submit `[scene, tag]` in order
    /// so the tagged content is produced before SL consumes the tags during `Present`.
    ///
    /// Tags depth, motion vectors, and (if provided) HUD-less color + UI, in the proven order. Call
    /// after [`Self::acquire`] and before [`Self::end_render`].
    pub fn tag(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        resources: &FgResources,
    ) -> Result<(), StreamlineError> {
        let pairs = resources.tags();

        // Build the `sl::Resource` payloads (one per tagged buffer); keep them alive until the call
        // returns since the `ResourceTag`s point at them.
        let mut sl_resources: Vec<Resource> = Vec::with_capacity(pairs.len());
        for (res, _) in &pairs {
            let native = match unsafe { hal::raw_resource(res.texture) } {
                Some(p) => p.cast::<c_void>(),
                None => {
                    return Err(StreamlineError::FeatureFunctionUnavailable {
                        feature: K_FEATURE_DLSS_G,
                        function: "slSetTagForFrame".to_string(),
                        detail: "a tagged FgResource is not a Dx12 texture".to_string(),
                    });
                }
            };
            let dims: UVec2 = res.dimensions();
            sl_resources.push(Resource::new_tex2d(
                native,
                res.resource_state,
                dims.x,
                dims.y,
                res.native_format(),
            ));
        }

        // Build the tags pointing at the resources (parallel to `pairs`).
        let mut tags: Vec<ResourceTag> = Vec::with_capacity(pairs.len());
        for (i, (_, buffer_type)) in pairs.iter().enumerate() {
            tags.push(ResourceTag::new(
                &mut sl_resources[i] as *mut Resource,
                *buffer_type,
                ResourceLifecycle::ValidUntilPresent,
            ));
        }

        // Extract the encoder's open raw command list and run the orderable core (advance +
        // slSetTagForFrame) inside the borrow.
        // SAFETY: `with_raw_command_list` hands us the encoder's open raw `ID3D12GraphicsCommandList`
        // for the duration of the closure only; `tags` (with `sl_resources`) outlives the call.
        let r = unsafe {
            hal::with_raw_command_list(encoder, |cmd_list| self.tag_core(&tags, cmd_list.cast()))
        };
        match r {
            Some(res) => res,
            None => Err(StreamlineError::FeatureFunctionUnavailable {
                feature: K_FEATURE_DLSS_G,
                function: "slSetTagForFrame".to_string(),
                detail: "the tagging encoder is not a Dx12 encoder with an open command list"
                    .to_string(),
            }),
        }
    }

    /// PCL `eRenderSubmitEnd` marker. Call just after submitting the frame's command buffers and
    /// before [`Self::present`].
    pub fn end_render(&self) {
        self.advance(Step::Tagged, Step::RenderEnded);
        // SAFETY: `self.token` is this frame's live token; PCL marker is best-effort.
        unsafe { self.api.set_marker(PCLMarker::RenderSubmitEnd, self.token) };
    }

    /// Presents the frame, bracketing `surface_texture.present()` with the PCL `ePresentStart` /
    /// `ePresentEnd` markers so the present markers can never be forgotten or mis-ordered.
    ///
    /// This is the final step: it consumes the [`wgpu::SurfaceTexture`] returned by
    /// [`Self::acquire`] and marks the frame as completed (so [`Drop`] does not flag it as aborted).
    pub fn present(&self, surface_texture: wgpu::SurfaceTexture) {
        self.present_core(|| surface_texture.present());
    }
}

impl<'a> Drop for Frame<'a> {
    fn drop(&mut self) {
        // A frame dropped before present() indicates a skipped / aborted frame. Surface it so the
        // sequencing mistake is visible; never panic in Drop.
        if !self.presented.get() {
            log::error!(
                "DLSS-G Frame (index {}) was dropped before present() (last step: {:?}); the frame \
                 was aborted and DLSS-G saw an incomplete per-frame sequence",
                self.frame_index,
                self.step.get()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    //! Headless unit tests for the DLSS-G per-frame call-order state machine.
    //!
    //! These drive the real [`Frame`] / [`begin_frame_with`] / [`query_state_with`] /
    //! [`apply_dlssg_options`] code paths against a recording [`MockApi`] that implements
    //! [`StreamlineApi`]. They never load `sl.interposer.dll`, create a `wgpu::Device`, or touch a
    //! GPU, so they run in CI under `cargo test --features frame-generation`. The opaque token /
    //! command-list pointers are dummy sentinels the mock never dereferences.
    //!
    //! What stays validated only on hardware (and is therefore exercised by the `#[ignore]`d
    //! integration test in `tests/headless.rs`, not here): the raw D3D12 reach-through in
    //! `crate::hal`, the real interposer load + signature verification, and that a real `slInit` /
    //! DLSS-G actually generates a frame.

    use super::*;
    use std::sync::Mutex;

    /// One recorded [`StreamlineApi`] call: its identity plus the salient scalar arguments the tests
    /// assert on (the PCL marker kind, the forced `reset` flag, the DLSS-G options, the tagged
    /// buffer-type order). Opaque pointers are never recorded or dereferenced.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        SetD3DDevice,
        ResolveFeatureFunctions,
        IsFeatureSupported(Feature),
        ReflexSetOptions(ReflexMode),
        DlssgSetOptions {
            mode: DLSSGMode,
            num_frames: u32,
            color_format: u32,
            ui_recomp: bool,
            ui_format: u32,
        },
        DlssgGetState {
            query_mode: DLSSGMode,
        },
        GetNewFrameToken(u32),
        ReflexSleep,
        SetMarker(PCLMarker),
        SetConstants {
            reset: bool,
        },
        SetTagForFrame {
            buffer_types: Vec<BufferType>,
        },
        Shutdown,
    }

    /// A recording, fully in-memory [`StreamlineApi`] for headless tests. It logs every call in
    /// order and returns canned [`SlResult`]s; set one of the `*_result` fields non-Ok (or
    /// `token_null` / `features_unresolved`) to exercise an error path. It reads only the documented
    /// plain-data fields of the ABI structs the tests keep alive across each call — never the opaque
    /// token / command-list / resource pointers.
    struct MockApi {
        calls: Mutex<Vec<Call>>,
        set_d3d_device_result: SlResult,
        is_feature_supported_result: SlResult,
        reflex_set_options_result: SlResult,
        dlssg_set_options_result: SlResult,
        dlssg_get_state_result: SlResult,
        get_new_frame_token_result: SlResult,
        set_constants_result: SlResult,
        set_tag_result: SlResult,
        shutdown_result: SlResult,
        /// When set, `get_new_frame_token` returns `Ok` but a null token.
        token_null: bool,
        /// When set, the feature-gated methods report `FeatureFunctionUnavailable`.
        features_unresolved: bool,
        /// Canned `slDLSSGGetState` out-params.
        state_status: u32,
        state_num_presented: u32,
    }

    impl Default for MockApi {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                set_d3d_device_result: SlResult::Ok,
                is_feature_supported_result: SlResult::Ok,
                reflex_set_options_result: SlResult::Ok,
                dlssg_set_options_result: SlResult::Ok,
                dlssg_get_state_result: SlResult::Ok,
                get_new_frame_token_result: SlResult::Ok,
                set_constants_result: SlResult::Ok,
                set_tag_result: SlResult::Ok,
                shutdown_result: SlResult::Ok,
                token_null: false,
                features_unresolved: false,
                state_status: dlssg_status::OK,
                state_num_presented: 0,
            }
        }
    }

    impl MockApi {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn push(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
        fn unavailable(&self, feature: Feature, function: &str) -> StreamlineError {
            StreamlineError::FeatureFunctionUnavailable {
                feature,
                function: function.to_string(),
                detail: "feature function not resolved (mock)".to_string(),
            }
        }
    }

    impl StreamlineApi for MockApi {
        unsafe fn set_d3d_device(&self, _device: *mut c_void) -> SlResult {
            self.push(Call::SetD3DDevice);
            self.set_d3d_device_result
        }

        unsafe fn resolve_feature_functions(&self) -> Result<(), StreamlineError> {
            self.push(Call::ResolveFeatureFunctions);
            Ok(())
        }

        unsafe fn is_feature_supported(
            &self,
            feature: Feature,
            _adapter_info: *const AdapterInfo,
        ) -> SlResult {
            self.push(Call::IsFeatureSupported(feature));
            self.is_feature_supported_result
        }

        fn reflex_set_options(&self, mode: ReflexMode) -> Result<SlResult, StreamlineError> {
            if self.features_unresolved {
                return Err(self.unavailable(K_FEATURE_REFLEX, "slReflexSetOptions"));
            }
            self.push(Call::ReflexSetOptions(mode));
            Ok(self.reflex_set_options_result)
        }

        unsafe fn dlssg_set_options(
            &self,
            _viewport: *const ViewportHandle,
            options: *const DLSSGOptions,
        ) -> Result<SlResult, StreamlineError> {
            if self.features_unresolved {
                return Err(self.unavailable(K_FEATURE_DLSS_G, "slDLSSGSetOptions"));
            }
            // SAFETY: the test keeps the `DLSSGOptions` alive across the call; we read only its plain
            // (Copy) fields, never any pointer.
            let o = unsafe { &*options };
            self.push(Call::DlssgSetOptions {
                mode: o.mode,
                num_frames: o.num_frames_to_generate,
                color_format: o.color_buffer_format,
                ui_recomp: o.enable_user_interface_recomposition == Boolean::True,
                ui_format: o.ui_buffer_format,
            });
            Ok(self.dlssg_set_options_result)
        }

        unsafe fn dlssg_get_state(
            &self,
            _viewport: *const ViewportHandle,
            state: *mut DLSSGState,
            options: *const DLSSGOptions,
        ) -> Result<SlResult, StreamlineError> {
            if self.features_unresolved {
                return Err(self.unavailable(K_FEATURE_DLSS_G, "slDLSSGGetState"));
            }
            // SAFETY: the test keeps both structs alive; read the plain query mode, write the plain
            // out-fields.
            let query_mode = unsafe { (*options).mode };
            self.push(Call::DlssgGetState { query_mode });
            unsafe {
                (*state).status = self.state_status;
                (*state).num_frames_actually_presented = self.state_num_presented;
            }
            Ok(self.dlssg_get_state_result)
        }

        unsafe fn get_new_frame_token(&self, frame_index: u32) -> (SlResult, *mut FrameToken) {
            self.push(Call::GetNewFrameToken(frame_index));
            let token = if self.token_null {
                core::ptr::null_mut()
            } else {
                dummy_token()
            };
            (self.get_new_frame_token_result, token)
        }

        unsafe fn reflex_sleep(&self, _token: *mut FrameToken) {
            self.push(Call::ReflexSleep);
        }

        unsafe fn set_marker(&self, marker: PCLMarker, _token: *mut FrameToken) {
            self.push(Call::SetMarker(marker));
        }

        unsafe fn set_constants(
            &self,
            values: *const Constants,
            _frame: *const FrameToken,
            _viewport: *const ViewportHandle,
        ) -> SlResult {
            // SAFETY: the test keeps the `Constants` alive across the call; read only the plain
            // `reset` field.
            let reset = unsafe { (*values).reset } == Boolean::True;
            self.push(Call::SetConstants { reset });
            self.set_constants_result
        }

        unsafe fn set_tag_for_frame(
            &self,
            _frame: *const FrameToken,
            _viewport: *const ViewportHandle,
            tags: *const ResourceTag,
            num_tags: u32,
            _cmd_buffer: *mut c_void,
        ) -> SlResult {
            // SAFETY: the test keeps the tag slice alive across the call; read only each plain
            // `buffer_type` (never the dangling `resource` pointers).
            let slice = unsafe { core::slice::from_raw_parts(tags, num_tags as usize) };
            let buffer_types = slice.iter().map(|t| t.buffer_type).collect();
            self.push(Call::SetTagForFrame { buffer_types });
            self.set_tag_result
        }

        unsafe fn shutdown(&self) -> SlResult {
            self.push(Call::Shutdown);
            self.shutdown_result
        }
    }

    /// A non-null opaque token sentinel — never dereferenced by the mock or the code under test.
    fn dummy_token() -> *mut FrameToken {
        core::ptr::NonNull::<FrameToken>::dangling().as_ptr()
    }

    /// A dummy command-list pointer — never dereferenced.
    fn dummy_cmd_list() -> *mut c_void {
        core::ptr::NonNull::<u8>::dangling().as_ptr().cast()
    }

    /// Build a `Vec<ResourceTag>` for the given buffer types (with null, never-deref'd resources),
    /// for driving [`Frame::tag_core`] headlessly.
    fn make_tags(buffer_types: &[BufferType]) -> Vec<ResourceTag> {
        buffer_types
            .iter()
            .map(|&bt| {
                ResourceTag::new(core::ptr::null_mut(), bt, ResourceLifecycle::ValidUntilPresent)
            })
            .collect()
    }

    #[test]
    fn begin_frame_emits_token_sleep_and_sim_markers_in_order() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 7).unwrap();
        assert_eq!(frame.frame_index(), 7);
        assert_eq!(frame.step.get(), Step::Begun);
        assert!(!frame.presented.get());
        assert_eq!(
            mock.calls(),
            vec![
                Call::GetNewFrameToken(7),
                Call::ReflexSleep,
                Call::SetMarker(PCLMarker::SimulationStart),
                Call::SetMarker(PCLMarker::SimulationEnd),
            ]
        );
    }

    #[test]
    fn full_happy_path_emits_the_proven_call_sequence() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 3).unwrap();
        frame.set_constants(&FgConstants::new()).unwrap();
        frame.acquire_core();
        let tags = make_tags(&[K_BUFFER_TYPE_DEPTH, K_BUFFER_TYPE_MOTION_VECTORS]);
        frame.tag_core(&tags, dummy_cmd_list()).unwrap();
        frame.end_render();
        frame.present_core(|| {});
        assert!(frame.presented.get());
        // The exact proven order AND completeness (length) of the per-frame Streamline call stream.
        assert_eq!(
            mock.calls(),
            vec![
                Call::GetNewFrameToken(3),
                Call::ReflexSleep,
                Call::SetMarker(PCLMarker::SimulationStart),
                Call::SetMarker(PCLMarker::SimulationEnd),
                Call::SetConstants { reset: false },
                Call::SetMarker(PCLMarker::RenderSubmitStart),
                Call::SetTagForFrame {
                    buffer_types: vec![K_BUFFER_TYPE_DEPTH, K_BUFFER_TYPE_MOTION_VECTORS],
                },
                Call::SetMarker(PCLMarker::RenderSubmitEnd),
                Call::SetMarker(PCLMarker::PresentStart),
                Call::SetMarker(PCLMarker::PresentEnd),
            ]
        );
    }

    #[test]
    fn auto_reset_is_forced_on_frame_zero() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 0).unwrap();
        // FgConstants::new() has reset = false; frame 0 must force it true.
        frame.set_constants(&FgConstants::new()).unwrap();
        assert!(mock.calls().contains(&Call::SetConstants { reset: true }));
    }

    #[test]
    fn reset_is_honored_after_frame_zero() {
        // reset = false is passed through unchanged on a later frame.
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 5).unwrap();
        let mut c = FgConstants::new();
        c.reset = false;
        frame.set_constants(&c).unwrap();
        assert!(mock.calls().contains(&Call::SetConstants { reset: false }));

        // reset = true is honored too.
        let mock2 = MockApi::default();
        let frame2 = begin_frame_with(&mock2, ViewportHandle::new(0), 5).unwrap();
        let mut c2 = FgConstants::new();
        c2.reset = true;
        frame2.set_constants(&c2).unwrap();
        assert!(mock2.calls().contains(&Call::SetConstants { reset: true }));
    }

    #[test]
    fn tag_core_passes_buffer_types_in_proven_order() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 1).unwrap();
        frame.set_constants(&FgConstants::new()).unwrap();
        frame.acquire_core();
        let order = [
            K_BUFFER_TYPE_DEPTH,
            K_BUFFER_TYPE_MOTION_VECTORS,
            K_BUFFER_TYPE_HUD_LESS_COLOR,
            K_BUFFER_TYPE_UI_COLOR_AND_ALPHA,
        ];
        frame.tag_core(&make_tags(&order), dummy_cmd_list()).unwrap();
        assert!(mock.calls().contains(&Call::SetTagForFrame {
            buffer_types: order.to_vec()
        }));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "out of order")]
    fn out_of_order_step_panics_in_debug() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 1).unwrap();
        // The frame is at `Begun`; `end_render` expects `Tagged` — the debug_assert! fires.
        frame.end_render();
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn out_of_order_step_is_flagged_in_release() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 1).unwrap();
        // In a release build the debug_assert! is compiled out, but the mis-order flag still flips.
        frame.end_render();
        assert!(frame.misordered.get());
    }

    #[test]
    fn correctly_ordered_frame_is_not_flagged() {
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 2).unwrap();
        frame.set_constants(&FgConstants::new()).unwrap();
        frame.acquire_core();
        frame
            .tag_core(&make_tags(&[K_BUFFER_TYPE_DEPTH]), dummy_cmd_list())
            .unwrap();
        frame.end_render();
        frame.present_core(|| {});
        assert!(!frame.misordered.get());
        assert!(frame.presented.get());
    }

    #[test]
    fn unpresented_frame_reads_as_aborted_before_drop() {
        // Drop logs an abort when `presented` is false; assert the observable signals it reads.
        let mock = MockApi::default();
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 1).unwrap();
        frame.set_constants(&FgConstants::new()).unwrap();
        assert!(!frame.presented.get());
        assert_eq!(frame.step.get(), Step::Constants);
    }

    #[test]
    fn get_new_frame_token_error_propagates() {
        let mock = MockApi {
            get_new_frame_token_result: SlResult::ErrorDeviceNotCreated,
            ..Default::default()
        };
        // `Frame` is not `Debug`, so match the `Result` directly rather than `unwrap_err()`.
        let result = begin_frame_with(&mock, ViewportHandle::new(0), 0);
        assert!(matches!(
            result,
            Err(StreamlineError::SlCall {
                result: SlResult::ErrorDeviceNotCreated,
                ..
            })
        ));
    }

    #[test]
    fn null_frame_token_is_reported() {
        let mock = MockApi {
            token_null: true,
            ..Default::default()
        };
        let result = begin_frame_with(&mock, ViewportHandle::new(0), 0);
        assert!(matches!(
            result,
            Err(StreamlineError::FeatureFunctionUnavailable { .. })
        ));
    }

    #[test]
    fn set_constants_error_propagates() {
        let mock = MockApi {
            set_constants_result: SlResult::ErrorMissingConstants,
            ..Default::default()
        };
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 5).unwrap();
        let err = frame.set_constants(&FgConstants::new()).unwrap_err();
        assert!(matches!(
            err,
            StreamlineError::SlCall {
                result: SlResult::ErrorMissingConstants,
                ..
            }
        ));
    }

    #[test]
    fn tag_core_error_propagates() {
        let mock = MockApi {
            set_tag_result: SlResult::ErrorMissingResourceState,
            ..Default::default()
        };
        let frame = begin_frame_with(&mock, ViewportHandle::new(0), 1).unwrap();
        frame.set_constants(&FgConstants::new()).unwrap();
        frame.acquire_core();
        let err = frame
            .tag_core(&make_tags(&[K_BUFFER_TYPE_DEPTH]), dummy_cmd_list())
            .unwrap_err();
        assert!(matches!(
            err,
            StreamlineError::SlCall {
                result: SlResult::ErrorMissingResourceState,
                ..
            }
        ));
    }

    #[test]
    fn query_state_decodes_status_and_uses_given_mode() {
        let mock = MockApi {
            state_status: dlssg_status::OK,
            state_num_presented: 2,
            ..Default::default()
        };
        let st = query_state_with(&mock, &ViewportHandle::new(0), DLSSGMode::On, 1).unwrap();
        assert!(st.is_ok);
        assert_eq!(st.num_frames_actually_presented, 2);
        assert_eq!(st.status_text, "eOk");
        // The query was built with the mode we passed (the "last-enabled mode" in production).
        assert!(mock.calls().contains(&Call::DlssgGetState {
            query_mode: DLSSGMode::On
        }));
    }

    #[test]
    fn query_state_reports_failure_status() {
        let mock = MockApi {
            state_status: dlssg_status::FAIL_GET_CURRENT_BACK_BUFFER_INDEX_NOT_CALLED,
            ..Default::default()
        };
        let st = query_state_with(&mock, &ViewportHandle::new(0), DLSSGMode::On, 1).unwrap();
        assert!(!st.is_ok);
        assert!(st.status_text.contains("eFailGetCurrentBackBufferIndexNotCalled"));
    }

    #[test]
    fn query_state_error_propagates() {
        let mock = MockApi {
            dlssg_get_state_result: SlResult::ErrorNotInitialized,
            ..Default::default()
        };
        let err = query_state_with(&mock, &ViewportHandle::new(0), DLSSGMode::On, 1).unwrap_err();
        assert!(matches!(
            err,
            StreamlineError::SlCall {
                result: SlResult::ErrorNotInitialized,
                ..
            }
        ));
    }

    #[test]
    fn apply_dlssg_options_maps_options_through() {
        let mock = MockApi::default();
        let opts = FrameGenerationOptions::enabled();
        apply_dlssg_options(&mock, &ViewportHandle::new(0), &opts, 87).unwrap();
        assert!(mock.calls().contains(&Call::DlssgSetOptions {
            mode: DLSSGMode::On,
            num_frames: 1,
            color_format: 87,
            ui_recomp: false,
            ui_format: 0,
        }));
    }

    #[test]
    fn apply_dlssg_options_enables_ui_recomposition() {
        let mock = MockApi::default();
        // with_ui_recomposition(R8Unorm) -> ui_format = dxgi_format_of(R8Unorm) = 61.
        let opts =
            FrameGenerationOptions::enabled().with_ui_recomposition(wgpu::TextureFormat::R8Unorm);
        apply_dlssg_options(&mock, &ViewportHandle::new(0), &opts, 0).unwrap();
        assert!(mock.calls().contains(&Call::DlssgSetOptions {
            mode: DLSSGMode::On,
            num_frames: 1,
            color_format: 0,
            ui_recomp: true,
            ui_format: 61,
        }));
    }

    #[test]
    fn apply_dlssg_options_unresolved_feature_is_reported() {
        let mock = MockApi {
            features_unresolved: true,
            ..Default::default()
        };
        let err = apply_dlssg_options(
            &mock,
            &ViewportHandle::new(0),
            &FrameGenerationOptions::enabled(),
            0,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            StreamlineError::FeatureFunctionUnavailable { .. }
        ));
    }

    #[test]
    fn apply_dlssg_options_call_error_propagates() {
        let mock = MockApi {
            dlssg_set_options_result: SlResult::ErrorFeatureNotSupported,
            ..Default::default()
        };
        let err = apply_dlssg_options(
            &mock,
            &ViewportHandle::new(0),
            &FrameGenerationOptions::enabled(),
            0,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            StreamlineError::SlCall {
                result: SlResult::ErrorFeatureNotSupported,
                ..
            }
        ));
    }

    #[test]
    fn reflex_set_options_records_the_mode() {
        let mock = MockApi::default();
        let r = mock.reflex_set_options(ReflexMode::LowLatency).unwrap();
        assert!(r.is_ok());
        assert!(mock
            .calls()
            .contains(&Call::ReflexSetOptions(ReflexMode::LowLatency)));
    }
}
