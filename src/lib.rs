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

#[cfg(not(windows))]
compile_error!(
    "dlss_wgpu_dx12 supports Windows + the wgpu Dx12 backend only; use dlss_wgpu (Vulkan) on other platforms."
);

mod context;
mod feature_info;
mod hal;
mod instance;
mod jitter;
mod ngx_log;
mod nvsdk_ngx;
#[cfg(feature = "ray-reconstruction")]
mod ray_reconstruction;
mod render_parameters;
mod sdk;
#[cfg(feature = "frame-generation")]
mod streamline;

pub use context::DlssContext;
pub use instance::{DEFAULT_DXC_PATH, dxc_instance_descriptor, dxc_instance_descriptor_at};
pub use nvsdk_ngx::{DlssError, DlssFeatureFlags, DlssPerfQualityMode};
#[cfg(feature = "ray-reconstruction")]
pub use ray_reconstruction::{
    DepthType, DlssRayReconstructionContext, DlssRayReconstructionParameters, RoughnessMode,
};
pub use render_parameters::{DlssExposure, DlssRenderParameters, DlssTexture};
pub use sdk::DlssSdk;
#[cfg(feature = "frame-generation")]
pub use streamline::{
    DEFAULT_ENGINE_VERSION, DEFAULT_PROJECT_ID, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    FgConstants, FgResource, FgResources, FgUi, Frame, FrameGenerationContext, FrameGenerationMode,
    FrameGenerationOptions, FrameGenerationState, Streamline, StreamlineError,
};
