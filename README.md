# dlss_wgpu_dx12

NVIDIA DLSS for the [`wgpu`](https://github.com/gfx-rs/wgpu) **DX12** backend.

This crate integrates NVIDIA's NGX SDK with wgpu by reaching through wgpu's HAL to feed
raw `ID3D12*` handles to NGX. It is the DX12 sibling of
[`dlss_wgpu`](https://github.com/JMS55/dlss_wgpu) (Vulkan), and is intended for applications
that render with the wgpu DX12 backend and compile their own shaders with DXC.

It currently provides:

- **DLSS Super Resolution (SR)** — temporal upscaling of a render-resolution color buffer.
- **DLSS Ray Reconstruction (RR / DLSS-D)** — combined denoising + upscaling for path-traced
  input, behind the `ray-reconstruction` feature.
- **DLSS Frame Generation (FG / DLSS-G)** — *experimental*, behind the `frame-generation`
  feature. Not yet complete; requires owning the swapchain and is gated at runtime.

> **Windows only.** This crate targets the wgpu Dx12 backend and the NVIDIA NGX SDK, both of
> which are Windows-only. Building on any other platform fails with a `compile_error!`. Use
> [`dlss_wgpu`](https://github.com/JMS55/dlss_wgpu) (Vulkan) on other platforms.

## Features

- Super Resolution upscaling with the standard quality presets (Auto, DLAA, Quality, Balanced,
  Performance, Ultra Performance) and dynamic-resolution support.
- Ray Reconstruction for path-traced renderers, with selectable roughness packing and depth type.
- Suggested camera jitter (Halton sequence) and texture mip bias helpers per frame.
- Manual or automatic exposure, HDR color, inverted depth, jittered / low-resolution motion
  vectors, alpha upscaling, and output-subrect rendering via feature flags.
- Resource state transitions routed through wgpu's tracker so they stay consistent with NGX.

## Requirements

- **OS:** Windows.
- **GPU:** an NVIDIA RTX GPU.
  - **Super Resolution / Ray Reconstruction:** RTX 20-series (Turing) or newer.
  - **Frame Generation:** Ada Lovelace / RTX 40-series or newer.
- A reasonably recent NVIDIA Display Driver. On unsupported hardware or drivers, DLSS
  initialization returns `DlssError::FeatureNotSupported` so you can fall back to a plain device.
- The wgpu **Dx12** backend. A non-Dx12 device/adapter also yields `DlssError::FeatureNotSupported`.

## Setup

Building and shipping this crate requires the NVIDIA DLSS SDK, `libclang` (for bindgen), and a
patched copy of wgpu. See **[docs/SETUP.md](docs/SETUP.md)** for full, step-by-step instructions,
including which DLLs to ship at runtime.

In short, before any `cargo` command, set the `DLSS_SDK` environment variable to a clone of
<https://github.com/NVIDIA/DLSS>, and make `libclang` discoverable (via `LIBCLANG_PATH`).

## Usage

Create one `DlssSdk` per application, then a `DlssContext` per camera, and call `render` each
frame (after you have submitted the scene rendering that produced the inputs).

```rust
use std::sync::{Arc, Mutex};
use dlss_wgpu_dx12::{
    DlssContext, DlssExposure, DlssFeatureFlags, DlssPerfQualityMode, DlssRenderParameters,
    DlssSdk, DlssTexture,
};
use glam::{UVec2, Vec2};

// Once per application. Returns Err(DlssError::FeatureNotSupported) on non-RTX / non-Dx12 devices.
let project_id = uuid::Uuid::new_v4();
let sdk: Arc<Mutex<DlssSdk>> = DlssSdk::new(project_id, device.clone())?;

// Once per camera (recreate only when output resolution, quality mode, or flags change).
let mut context = DlssContext::new(
    UVec2::new(2560, 1440),          // upscaled (output) resolution
    DlssPerfQualityMode::Auto,
    DlssFeatureFlags::empty(),
    Arc::clone(&sdk),
    &device,
    &queue,
)?;

// Render your scene at `context.render_resolution()`, applying the suggested camera jitter:
let render_resolution = context.render_resolution();
let jitter = context.suggested_jitter(frame_number, render_resolution);
let mip_bias = context.suggested_mip_bias(render_resolution);

// Each frame: render your scene into the input textures and submit that work, THEN evaluate DLSS.
// `render` records the resource transitions and the NGX evaluate on its own command encoders and
// submits them on `queue`, so submit your scene rendering before calling it.
context.render(
    DlssRenderParameters {
        color: DlssTexture { texture: &color },
        depth: DlssTexture { texture: &depth },
        motion_vectors: DlssTexture { texture: &motion_vectors },
        exposure: DlssExposure::Automatic,
        bias: None,
        dlss_output: DlssTexture { texture: &upscaled_output },
        reset: false,
        jitter_offset: jitter,
        partial_texture_size: None,
        motion_vector_scale: None,
    },
    &queue,
)?;
```

For Ray Reconstruction, enable the `ray-reconstruction` feature and use
`DlssRayReconstructionContext` / `DlssRayReconstructionParameters` instead. RR is mutually
exclusive with SR for a given upscale pass — create *either* a `DlssContext` *or* a
`DlssRayReconstructionContext`, never both.

## Cargo features

| Feature | Default | Description |
| --- | --- | --- |
| `super-resolution` | yes | DLSS Super Resolution (raw NGX). The baseline upscaler. |
| `ray-reconstruction` | no | DLSS Ray Reconstruction / DLSS-D. Mutually exclusive with SR at runtime. |
| `frame-generation` | no | DLSS Frame Generation / DLSS-G via NVIDIA Streamline. **Experimental** and incomplete; requires owning the swapchain and is runtime capability-gated. |
| `debug_overlay` | no | Links the NGX dev/debug feature library and dev DLL search path for the on-screen debug overlay. **Development only — never ship a build with this enabled.** |

## License and redistribution

This crate is licensed under either of MIT or Apache-2.0, at your option.

**The NVIDIA DLSS SDK, the NGX runtime, and the `nvngx_dlss*.dll` libraries are NOT covered by
this crate's license.** They are governed by the NVIDIA software license you accept when you
download the DLSS SDK. If you redistribute an application built with this crate you must:

- Ship only the **release** DLLs (`nvngx_dlss.dll`, and `nvngx_dlssd.dll` for Ray
  Reconstruction). **Never ship the watermarked development DLLs.**
- Reproduce the license and copyright notices required by **section 9.5 of the NVIDIA DLSS
  Programming Guide** in your shipped product.

See **[docs/SETUP.md](docs/SETUP.md)** for the exact files to ship and the redistribution steps.
