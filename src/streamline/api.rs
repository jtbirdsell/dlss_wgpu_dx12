//! The [`StreamlineApi`] seam — the Streamline call surface the DLSS-G per-frame state machine
//! depends on, abstracted behind a trait so it can be driven by a mock in headless unit tests.
//!
//! Every method is the safe-Rust shape of exactly one `sl*` FFI call (or, for `reflex_set_options`,
//! one call plus its tiny option-struct build). Raw handles (`*mut FrameToken`, the `*mut c_void`
//! device / command list, the `*const`/`*mut` ABI structs) stay **opaque**: the trait neither
//! constructs nor dereferences them, so a mock can hand back and accept dummy addresses without ever
//! touching memory. The production implementation is [`super::ffi::SlApi`]; the test mock lives in
//! [`super::frame_gen`]'s test module.
//!
//! ## Error-mapping split (why the return types differ)
//!
//! * **Core exported calls** (`set_d3d_device`, `is_feature_supported`, `get_new_frame_token`,
//!   `set_constants`, `set_tag_for_frame`, `shutdown`) are always resolved at load time, so they
//!   return the raw [`SlResult`]; the caller in [`super::frame_gen`] maps a non-Ok result to the
//!   appropriate [`StreamlineError`] (this keeps that mapping in one place where the mock exercises
//!   it).
//! * **Feature-level calls** (`reflex_set_options`, `dlssg_set_options`, `dlssg_get_state`) are
//!   resolved lazily via `slGetFeatureFunction`, so they return `Result<SlResult, StreamlineError>`:
//!   the `Err` arm is *only* [`StreamlineError::FeatureFunctionUnavailable`] (the function was never
//!   resolved); an `Ok(non_ok)` is mapped to [`StreamlineError::SlCall`] by the caller.
//! * **Best-effort calls** (`reflex_sleep`, `set_marker`) return `()` and swallow any error inside
//!   the implementation (a dropped Reflex/PCL signal must never abort the frame).

use super::types::{
    AdapterInfo, Constants, DLSSGOptions, DLSSGState, Feature, FrameToken, PCLMarker, ReflexMode,
    ResourceTag, SlResult, StreamlineError, ViewportHandle,
};
use core::ffi::c_void;

/// The Streamline call surface used by the DLSS-G context + per-frame [`super::frame_gen::Frame`].
///
/// Implemented for real by [`super::ffi::SlApi`] (forwarding to the resolved interposer
/// fn-pointers) and, in tests, by a recording mock. `Send + Sync` so the `Box<dyn StreamlineApi>`
/// the context owns inherits those bounds.
pub(crate) trait StreamlineApi: Send + Sync {
    // --- context setup (FrameGenerationContext::new) -------------------------------------------

    /// `slSetD3DDevice`. `device` is an opaque `ID3D12Device*`, valid only for the call.
    ///
    /// # Safety
    /// `device` must be a live `ID3D12Device*` for the duration of the call.
    unsafe fn set_d3d_device(&self, device: *mut c_void) -> SlResult;

    /// Resolve the DLSS-G / Reflex / PCL feature functions (via `slGetFeatureFunction`).
    ///
    /// # Safety
    /// Must be called only after [`Self::set_d3d_device`] succeeded (the interposer needs a device
    /// context to resolve feature functions against).
    unsafe fn resolve_feature_functions(&self) -> Result<(), StreamlineError>;

    /// `slIsFeatureSupported(feature, adapter_info)`.
    ///
    /// # Safety
    /// `adapter_info` must point at a live `sl::AdapterInfo` whose LUID buffer outlives the call.
    unsafe fn is_feature_supported(
        &self,
        feature: Feature,
        adapter_info: *const AdapterInfo,
    ) -> SlResult;

    // --- options (new + set_mode + Drop + query_state) -----------------------------------------

    /// `slReflexSetOptions(mode)` — builds the `sl::ReflexOptions` internally. `Err` only when the
    /// Reflex feature function was not resolved.
    fn reflex_set_options(&self, mode: ReflexMode) -> Result<SlResult, StreamlineError>;

    /// `slDLSSGSetOptions(viewport, options)`. `Err` only when the DLSS-G feature function was not
    /// resolved.
    ///
    /// # Safety
    /// `viewport` and `options` must point at live, fully-initialized ABI structs for the call.
    unsafe fn dlssg_set_options(
        &self,
        viewport: *const ViewportHandle,
        options: *const DLSSGOptions,
    ) -> Result<SlResult, StreamlineError>;

    /// `slDLSSGGetState(viewport, state, options)` — writes `state`. `Err` only when the DLSS-G
    /// feature function was not resolved.
    ///
    /// # Safety
    /// `viewport`/`options` must be live for the call; `state` must be a live, writable
    /// `sl::DLSSGState`.
    unsafe fn dlssg_get_state(
        &self,
        viewport: *const ViewportHandle,
        state: *mut DLSSGState,
        options: *const DLSSGOptions,
    ) -> Result<SlResult, StreamlineError>;

    // --- per-frame (begin_frame + Frame methods) -----------------------------------------------

    /// `slGetNewFrameToken(&mut token, &frame_index)`. Returns the raw result and the opaque token
    /// (the caller checks the result and the null-token case).
    ///
    /// # Safety
    /// The returned `*mut FrameToken` is opaque and must not be dereferenced; it is only ever passed
    /// back to other `sl*` calls this frame.
    unsafe fn get_new_frame_token(&self, frame_index: u32) -> (SlResult, *mut FrameToken);

    /// `slReflexSleep(token)` — best-effort (a non-Ok result is logged and swallowed).
    ///
    /// # Safety
    /// `token` must be this frame's live token from [`Self::get_new_frame_token`].
    unsafe fn reflex_sleep(&self, token: *mut FrameToken);

    /// `slPCLSetMarker(marker, token)` — best-effort (a non-Ok result is logged and swallowed).
    ///
    /// # Safety
    /// `token` must be this frame's live token from [`Self::get_new_frame_token`].
    unsafe fn set_marker(&self, marker: PCLMarker, token: *mut FrameToken);

    /// `slSetConstants(values, frame, viewport)`.
    ///
    /// # Safety
    /// All three pointers must point at live ABI structs for the duration of the call.
    unsafe fn set_constants(
        &self,
        values: *const Constants,
        frame: *const FrameToken,
        viewport: *const ViewportHandle,
    ) -> SlResult;

    /// `slSetTagForFrame(frame, viewport, tags, num_tags, cmd_buffer)`. `cmd_buffer` is an opaque
    /// `ID3D12GraphicsCommandList*`.
    ///
    /// # Safety
    /// `frame`/`viewport`/`tags` must be live for the call (`tags` covering `num_tags` entries);
    /// `cmd_buffer` must be a live, open command list.
    unsafe fn set_tag_for_frame(
        &self,
        frame: *const FrameToken,
        viewport: *const ViewportHandle,
        tags: *const ResourceTag,
        num_tags: u32,
        cmd_buffer: *mut c_void,
    ) -> SlResult;

    // --- teardown (Drop) -----------------------------------------------------------------------

    /// `slShutdown`.
    ///
    /// # Safety
    /// `slInit` must have succeeded for this interposer, and `shutdown` must not be called twice.
    unsafe fn shutdown(&self) -> SlResult;
}
