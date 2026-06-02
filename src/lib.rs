//! # dlss_wgpu_dx12
//!
//! NVIDIA DLSS integration for the [`wgpu`] **DX12** backend: Super Resolution (SR),
//! Ray Reconstruction (RR), and — experimentally, behind the `frame-generation` feature —
//! Frame Generation (FG) via NVIDIA Streamline.
//!
//! This is the DX12 sibling of [`dlss_wgpu`](https://github.com/JMS55/dlss_wgpu) (Vulkan).
//! It is **Windows-only** and reaches through `wgpu`'s HAL to feed raw `ID3D12*` handles to
//! NVIDIA's NGX SDK.
//!
//! ## Setup
//! Set the `DLSS_SDK` environment variable to a clone of <https://github.com/NVIDIA/DLSS> before
//! building, and ship the appropriate `nvngx_dlss*.dll` next to your executable at runtime.
//! See the README for full instructions.

#![deny(missing_docs)]
// Public-item docs occasionally link to internal (`pub(crate)`/private-module) items via
// convenience paths like `self`/`super::...` — handy for `cargo doc --document-private-items`
// locally. The crate is not published to docs.rs (its NGX bindings derive from non-redistributable
// NVIDIA headers, and docs.rs has no SDK), so these never render to a public page; allow them while
// still denying genuinely-broken links (the default under `-D warnings`) via the CI doc gate.
#![allow(rustdoc::private_intra_doc_links)]

#[cfg(not(windows))]
compile_error!(
    "dlss_wgpu_dx12 supports Windows + the wgpu Dx12 backend only; use dlss_wgpu (Vulkan) on other platforms."
);

mod config;
mod context;
mod feature_info;
mod hal;
mod instance;
mod jitter;
mod ngx_feature;
mod ngx_log;
mod nvsdk_ngx;
#[cfg(feature = "ray-reconstruction")]
mod ray_reconstruction;
mod render_parameters;
mod sdk;
#[cfg(feature = "frame-generation")]
mod streamline;

#[cfg(feature = "ray-reconstruction")]
pub use config::{DepthType, RoughnessMode};
pub use config::{DlssFeatureFlags, DlssPerfQualityMode};
pub use context::DlssContext;
pub use instance::{DEFAULT_DXC_PATH, dxc_instance_descriptor, dxc_instance_descriptor_at};
pub use nvsdk_ngx::DlssError;
#[cfg(feature = "ray-reconstruction")]
pub use ray_reconstruction::{DlssRayReconstructionContext, DlssRayReconstructionParameters};
pub use render_parameters::{DlssExposure, DlssRenderParameters, DlssTexture};
pub use sdk::DlssSdk;
#[cfg(feature = "frame-generation")]
pub use streamline::{
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, DEFAULT_ENGINE_VERSION, DEFAULT_PROJECT_ID,
    FgConstants, FgResource, FgResources, FgUi, Frame, FrameGenerationContext, FrameGenerationMode,
    FrameGenerationOptions, FrameGenerationState, Streamline, StreamlineError,
};

/// A crate-level umbrella error for applications that drive both the NGX (Super Resolution /
/// Ray Reconstruction) and Streamline (Frame Generation) paths and want a single `?`-able error.
///
/// The domain errors (`DlssError`, `StreamlineError`) remain available and unchanged; this is purely
/// additive and converts from each via `?`/`From`. SR/RR and FG are independent NGX entry points, so
/// the two domains are kept as separate variants rather than merged.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error from the NGX (Super Resolution / Ray Reconstruction) path.
    #[error(transparent)]
    Dlss(#[from] DlssError),
    /// An error from the Streamline (Frame Generation) path.
    #[cfg(feature = "frame-generation")]
    #[error(transparent)]
    Streamline(#[from] StreamlineError),
}
