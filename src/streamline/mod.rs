//! DLSS Frame Generation (DLSS-G) via NVIDIA Streamline — **experimental**, behind the
//! `frame-generation` feature.
//!
//! Unlike Super Resolution and Ray Reconstruction (which call NGX directly on a command list),
//! Frame Generation runs *inside* the swapchain `Present` call. Streamline owns that path: when
//! `sl.interposer.dll` is loaded before the `wgpu::Instance` is created, the patched wgpu-hal
//! upgrades its DXGI factory to a Streamline proxy (see the `dx12::streamline` patch on the wgpu
//! fork, rev `d81d755`), so wgpu's own swapchain becomes the SL proxy that drives DLSS-G.
//!
//! This module is the safe, enterprise wrapper around that flow. It was distilled from a hardware-
//! validated spike (RTX 4090: `numFramesActuallyPresented == 2`).
//!
//! Security note (sibling-DLL surface): before `slInit`, the loader Authenticode-verifies and
//! NVIDIA-pins the interposer AND every known sibling SL plugin present where the loader resolves
//! them — the exe directory (searched first) and the interposer's own dir (`sl.common`, `sl.dlss_g`,
//! `sl.reflex`, `sl.pcl`, `nvngx_dlssg`). This does NOT fully close the DLL-hijack surface: the
//! interposer must locate its plugins via the DEFAULT search order (constraining it makes `slInit`
//! fail with `eErrorNoPlugins`), so an attacker-renamed/unknown DLL on the search path is NOT
//! covered. (The `dxgi.dll`/`d3d12.dll` SL loader-shims are a separate matter: this crate uses the
//! interposer PROXY path, not loader-shims, and now rejects a renamed-interposer `dxgi.dll`/`d3d12.dll`
//! beside the exe at load time — a `LoaderShimConflict` error.) Deployments MUST keep
//! `$STREAMLINE_SDK/bin/x64` and the exe directory on ACL-restricted storage writable only by an
//! administrator. See [`ffi::SlApi::load`].
//!
//! The non-obvious runtime requirements that the spike proved are load-bearing and are enforced /
//! documented here:
//!   1. The presented window MUST be visible + foreground/composited, or DLSS-G silently declines
//!      to present generated frames.
//!   2. The host MUST call `IDXGISwapChain3::GetCurrentBackBufferIndex()` every frame (wgpu never
//!      does); see [`crate::hal`].
//!   3. Motion vectors are normalized to `[-1,1]` via `mvecScale` (pixel mvecs => `(1/w, 1/h)`).
//!   4. A non-vsync present mode lets Reflex/DLSS-G own pacing.

// Substrate (filled by the FG substrate phase): hand-written #[repr(C)] SL ABI + the runtime
// loader and signature verification. `types` is pure data; `ffi` and `security` are unsafe glue.
pub(crate) mod ffi;
pub(crate) mod security;
pub(crate) mod types;

// FG core (this phase): the safe wrapper around the proven DLSS-G flow.
//   * `api`       — the StreamlineApi trait seam over the sl* calls (testability boundary).
//   * `tagging`   — public per-frame input types: FgResources / FgResource / FgConstants.
//   * `frame_gen` — the Streamline handle + FrameGenerationContext + per-frame Frame driver.
//
// The Reflex + PCL marker helpers that used to live in a `reflex` module are now methods on the
// `StreamlineApi` trait (folded into `SlApi`'s impl), so the per-frame marker cadence is exercised
// through the same seam the mock implements.
pub(crate) mod api;
mod frame_gen;
mod tagging;

// Re-export the public surface for the crate root to `pub use`.
pub use frame_gen::{
    DEFAULT_ENGINE_VERSION, DEFAULT_PROJECT_ID, Frame, FrameGenerationContext, FrameGenerationMode,
    FrameGenerationOptions, FrameGenerationState, Streamline,
};
pub use tagging::{
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, FgConstants, FgResource, FgResources, FgUi,
};
// The substrate error type is the FG error type (mirrors how DlssError surfaces for SR/RR).
pub use types::StreamlineError;
