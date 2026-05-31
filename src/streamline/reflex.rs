//! Thin, internal wrappers over the Reflex and PCL (PC-Latency) feature functions.
//!
//! Reflex and PCL are not user-facing features in their own right here — they exist because DLSS
//! Frame Generation (DLSS-G) *requires* them: Reflex must be active (the runtime otherwise reports
//! `eFailReflexNotDetectedAtRuntime`), and the PCL simulation/render/present markers drive the
//! latency model that DLSS-G uses to pace the generated frame. The spike proved this exact pairing
//! on an RTX 4090.
//!
//! These helpers are `pub(crate)`: [`super::frame_gen`] calls them at the right points. They are
//! intentionally minimal — the public ergonomics live on [`super::frame_gen::Frame`].

use super::ffi::SlApi;
use super::types::{FrameToken, PCLMarker, ReflexMode, ReflexOptions, StreamlineError};

/// Sets the Reflex mode once during context setup.
///
/// DLSS-G mandates `eLowLatency` (or `eLowLatencyWithBoost`); `eOff` makes DLSS-G decline to
/// generate. Returns an error only if Reflex's feature function was not resolved or the call fails.
///
/// # Safety
/// `api.sl_reflex_set_options` must be the resolved interposer feature function (i.e.
/// [`SlApi::resolve_feature_functions`] succeeded) and the interposer must be initialized.
pub(crate) unsafe fn set_reflex_mode(api: &SlApi, mode: ReflexMode) -> Result<(), StreamlineError> {
    let set = api
        .sl_reflex_set_options
        .ok_or_else(|| StreamlineError::FeatureFunctionUnavailable {
            feature: super::types::K_FEATURE_REFLEX,
            function: "slReflexSetOptions".to_string(),
            detail: "feature function was not resolved (call resolve_feature_functions first)"
                .to_string(),
        })?;
    let opts = ReflexOptions::new(mode);
    // SAFETY: `opts` is a fully-initialized `#[repr(C)]` `sl::ReflexOptions` living on the stack for
    // the duration of the call; `set` is the resolved Reflex feature fn per this fn's contract.
    let r = unsafe { set(&opts) };
    if r.is_ok() {
        Ok(())
    } else {
        // The function resolved but the call failed: surface the typed SlResult via SlCall (reserve
        // FeatureFunctionUnavailable for genuine resolution failures, handled above).
        Err(StreamlineError::SlCall {
            function: "slReflexSetOptions".to_string(),
            result: r,
        })
    }
}

/// Runs `slReflexSleep(token)` — the Reflex-driven pacing point at the top of a frame.
///
/// Best-effort: a missing feature function or a non-OK result is logged and swallowed (a single
/// dropped sleep must not abort the frame loop). The spike treated this the same way.
///
/// # Safety
/// `token` must be a live `*mut FrameToken` returned by `slGetNewFrameToken` this frame, and
/// `api.sl_reflex_sleep` must be the resolved feature function.
pub(crate) unsafe fn reflex_sleep(api: &SlApi, token: *mut FrameToken) {
    if let Some(sleep) = api.sl_reflex_sleep {
        // SAFETY: `token` is a valid frame token per this fn's contract; `sleep` is the resolved
        // Reflex feature fn.
        let r = unsafe { sleep(token) };
        if !r.is_ok() {
            log::debug!("slReflexSleep returned {r:?}");
        }
    }
}

/// Sets a single PCL latency marker for the current frame.
///
/// Best-effort, for the same reason as [`reflex_sleep`]: markers are a latency-optimization signal,
/// and dropping one must never abort presentation.
///
/// # Safety
/// `token` must be the live frame token for this frame, and `api.sl_pcl_set_marker` must be the
/// resolved PCL feature function.
pub(crate) unsafe fn set_marker(api: &SlApi, marker: PCLMarker, token: *mut FrameToken) {
    if let Some(set) = api.sl_pcl_set_marker {
        // SAFETY: `token` is valid per contract; `set` is the resolved PCL feature fn. `marker` is a
        // valid `#[repr(u32)]` enum value.
        let r = unsafe { set(marker, token) };
        if !r.is_ok() {
            log::trace!("slPCLSetMarker({marker:?}) returned {r:?}");
        }
    }
}
