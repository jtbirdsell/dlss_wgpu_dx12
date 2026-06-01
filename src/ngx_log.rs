//! Bridges NVIDIA NGX's diagnostic logging into the [`log`] crate.
//!
//! NGX can emit messages through an `NVSDK_NGX_AppLogCallback` registered in
//! `NVSDK_NGX_LoggingInfo`. We forward them to the `log` facade so they surface wherever the host's
//! logger sends output (e.g. `env_logger` via `RUST_LOG`). NGX invokes the callback from its own
//! threads, so it must never unwind across the FFI boundary.

use crate::nvsdk_ngx::*;
use std::ffi::CStr;

/// The NGX `MinimumLoggingLevel` to request, derived from the active `log` max level so NGX only
/// produces messages the logger would actually emit. `Off → OFF`, `Trace`/`Debug → VERBOSE`,
/// otherwise `ON`. This makes NGX logging configurable through the standard `log` facade with no
/// extra public API.
pub(crate) fn min_logging_level() -> NVSDK_NGX_Logging_Level {
    match log::max_level() {
        log::LevelFilter::Off => NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_OFF,
        log::LevelFilter::Trace | log::LevelFilter::Debug => {
            NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_VERBOSE
        }
        _ => NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_ON,
    }
}

/// `NVSDK_NGX_AppLogCallback`: forwards one NGX log message to the `log` crate.
///
/// # Safety
/// Must match the `NVSDK_NGX_AppLogCallback` ABI (enforced at the registration site). Called by NGX,
/// possibly from its own threads; `message` is a NUL-terminated C string valid for the call only.
pub(crate) unsafe extern "C" fn ngx_log_callback(
    message: *const core::ffi::c_char,
    logging_level: NVSDK_NGX_Logging_Level,
    _source_component: NVSDK_NGX_Feature,
) {
    // A panic unwinding across the `extern "C"` boundary is UB; NGX calls this from foreign threads.
    let _ = std::panic::catch_unwind(|| {
        if message.is_null() {
            return;
        }
        // SAFETY: NGX passes a NUL-terminated C string valid for the duration of the call;
        // `to_string_lossy` avoids panicking on any non-UTF-8 byte.
        let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        let trimmed = msg.trim_end();
        if trimmed.is_empty() {
            return;
        }
        // Map NGX level → `log::Level` with `== CONST` guards (same discipline as `check_ngx_result`:
        // the bindgen constants are double-prefixed `c_int` values, not a Rust enum).
        let level = if logging_level == NVSDK_NGX_Logging_Level_NVSDK_NGX_LOGGING_LEVEL_VERBOSE {
            log::Level::Debug
        } else {
            // LEVEL_ON (and any other non-OFF value NGX delivers) → Info.
            log::Level::Info
        };
        log::log!(target: "dlss_wgpu_dx12::ngx", level, "{trimmed}");
    });
}
