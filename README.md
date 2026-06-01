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
  feature. Generates interpolated frames inside wgpu's own swapchain `Present` via NVIDIA
  Streamline. Hardware-validated on an RTX 4090; has strict ordering and runtime requirements.
  See the [Frame Generation (experimental)](#frame-generation-experimental) section below.

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

## Frame Generation (experimental)

> **Experimental, behind the `frame-generation` cargo feature** (`--features frame-generation`,
> off by default). DLSS Frame Generation (DLSS-G) interpolates frames *inside* the swapchain
> `Present` call. Unlike SR/RR — which call NGX directly on a command list — FG is driven by
> NVIDIA **Streamline**: the patched wgpu fork upgrades its DXGI factory to a Streamline proxy in
> `Instance::init` (only when `sl.interposer.dll` is already loaded), so wgpu's own swapchain
> becomes the SL proxy that drives DLSS-G. This feature has strict ordering and runtime
> requirements; read all of them before integrating. Validated on an RTX 4090
> (`numFramesActuallyPresented == 2`). The wgpu fork is **transitional** — see
> [`docs/upstream-pr-8888.md`](docs/upstream-pr-8888.md) for the upstreaming status and the path to a
> fork-free dependency.

### API flow

The public surface is `Streamline`, `FrameGenerationContext`, `Frame` (plus the per-frame input
types `FgConstants`, `FgResources`, `FgResource`, `FgUi`, and `FrameGenerationOptions` /
`FrameGenerationState`).

- **`Streamline::init()` — before your `wgpu::Instance`.** This loads (after signature
  verification) and initializes `sl.interposer.dll` for DLSS-G + Reflex + PCL. It must run *before*
  `wgpu::Instance::new`, because the fork upgrades its DXGI factory to a Streamline proxy only if
  the interposer is already loaded. Create the instance first and DLSS-G can never bind to wgpu's
  swapchain. This is the single most important rule.
- **`FrameGenerationContext::new(&mut streamline, &device, &adapter, &options)` — after the device
  but *before* `surface.configure()`.** It runs `slSetD3DDevice` (which must precede swapchain
  creation), checks feature support against the adapter's real LUID, enables Reflex, and applies
  `slDLSSGSetOptions`. On success it *moves* the Streamline core API into the context (the
  `Streamline` handle then becomes inert); on failure the handle stays intact and reusable.
- **Per frame**, drive a `Frame` in this exact order (it enforces the sequence at runtime):
  `begin_frame(frame_index)` → `set_constants(&consts)` → `acquire(&surface)` → record your scene →
  `tag(&mut tag_encoder, &resources)` → submit `[render, tag]` → `end_render()` → `present(tex)`.
  `acquire` returns `(SurfaceTexture, back_buffer_index)` and performs the mandatory per-frame
  `GetCurrentBackBufferIndex` call for you. The tag must be recorded on a **dedicated, raw-only**
  command encoder (no wgpu passes), and `[render, tag]` must be submitted in that order.
- **`query_state()`** decodes `slDLSSGGetState` into a `FrameGenerationState`; poll it periodically
  (not every frame) to confirm DLSS-G is generating — a healthy result is `is_ok == true` with
  `num_frames_actually_presented > 1`.

```rust
use dlss_wgpu_dx12::{
    FgConstants, FgResource, FgResources, FrameGenerationContext, FrameGenerationOptions, Streamline,
};
use glam::UVec2;

// 1. BEFORE creating the wgpu Instance.
let mut streamline = Streamline::init()?;

// 2. Create the wgpu Instance / surface / adapter / device as usual, THEN — after the device but
//    BEFORE surface.configure() — bind the context.
let options = FrameGenerationOptions::enabled().with_color_format(surface_format);
let mut fg = FrameGenerationContext::new(&mut streamline, &device, &adapter, &options)?;
surface.configure(&device, &config); // swapchain created here, with SL's device registration in place

// 3. Per frame, in this exact order:
let frame = fg.begin_frame(frame_index)?;
let mut consts = FgConstants::new().with_pixel_motion(UVec2::new(width, height));
consts.camera_motion_included = true;
frame.set_constants(&consts)?;
let (surface_tex, _bbi) = frame.acquire(&surface)?;
// ... record your scene into surface_tex's view on a render encoder ...
let mut tag_encoder = device.create_command_encoder(&Default::default()); // raw-only: no wgpu passes
frame.tag(&mut tag_encoder, &FgResources {
    depth: FgResource::new(&depth_tex),
    motion_vectors: FgResource::new(&mvec_tex),
    hudless_color: Some(FgResource::new(&hudless_tex)),
    ui: None,
})?;
queue.submit([render_encoder.finish(), tag_encoder.finish()]); // [render, tag] in order
frame.end_render();
frame.present(surface_tex);
drop(frame);

// Periodically:
let state = fg.query_state()?; // expect is_ok == true && num_frames_actually_presented > 1
```

### Runtime requirements

DLSS-G is silent about most failures — it simply declines to generate frames. The following are
**load-bearing**; each was proven to gate whether `numFramesActuallyPresented` flips from 1 to 2:

1. **The window must be visible + focused / composited.** DLSS-G silently declines to present
   generated frames to a window that is not actually being composited to the screen — do not
   minimize it or cover it. A `Suboptimal` surface from `acquire` usually means exactly this.
2. **Use a non-vsync present mode** (`Mailbox` or `Immediate`, not `Fifo`). Reflex/DLSS-G own the
   frame pacing; hard vsync throttles the app's present rate and can suppress interpolation.
3. **Stage the Streamline DLLs beside your executable:** `sl.interposer.dll`, `sl.common.dll`,
   `sl.dlss_g.dll`, `sl.reflex.dll`, `sl.pcl.dll`, and `nvngx_dlssg.dll`. Without them `slInit` /
   DLSS-G load fails and no frames are generated. (See [docs/SETUP.md](docs/SETUP.md) for exact
   sources and the loader-shim copies.)
4. **Set the `STREAMLINE_SDK` environment variable** to the Streamline SDK root so the crate can
   locate and signature-verify the interposer at runtime (`$STREAMLINE_SDK/bin/x64/sl.interposer.dll`).
5. **Teardown order matters.** Disable Frame Generation and idle the GPU *before* destroying the
   swapchain. Dropping the `FrameGenerationContext` does this for you (it runs
   `slDLSSGSetOptions(Off)`, polls the device to idle, then `slShutdown`), so drop the context
   before you tear down the surface/device. Note that the crate **leaks the interposer** — it is
   never `FreeLibrary`'d — because NVIDIA's interposer installs process-wide DXGI/D3D hooks and is
   not designed to be unloaded (`FreeLibrary`ing it access-violates). `slShutdown` performs the real
   cleanup; the DLL staying resident until process exit is the expected, supported behavior.

### Hardware

Frame Generation needs an **NVIDIA RTX 40-series (Ada Lovelace) or newer** GPU for single-frame
generation (classic 2x). It was hardware-validated on an RTX 4090, where the example reported
`numFramesActuallyPresented == 2` (one generated frame between each pair of rendered frames). On
unsupported hardware `FrameGenerationContext::new` returns `StreamlineError::FeatureNotSupported`.

See **[examples/frame_generation.rs](examples/frame_generation.rs)** for a complete, runnable
example: an animated window that drives the full per-frame sequence and prints the observed
`numFramesActuallyPresented`.

### Combining with Super Resolution

Super Resolution (raw NGX) and Frame Generation (Streamline) compose: render low-res, upscale with
[`DlssContext`], then let DLSS-G interpolate the upscaled frames. Two things make this clean:

- **One source of per-frame constants.** Build the FG constants from the *same*
  [`DlssRenderParameters`] you hand to the SR evaluate via
  `FgConstants::from_render_parameters(&sr_params, render_resolution)` (and
  `from_ray_reconstruction_parameters` for RR). This keeps jitter, history reset, and motion-vector
  scale identical across SR and FG, and it converts the motion-vector convention for you (NGX reads
  render-resolution *pixels*; FG *normalizes* to `[-1, 1]`, so `fg.mvec_scale =
  sr.motion_vector_scale / render_resolution`).
- **Resolutions.** Depth and motion vectors stay at **render resolution** (shared with SR);
  hud-less color and the UI buffer are at **output resolution** (they must match the back buffer).
  After the SR upscale, blit the UAV output into the hud-less color and copy it to the back buffer,
  then tag the hud-less color for FG.

This combination was hardware-validated on an RTX 4090 (960×540 → 1920×1080 upscale **and**
`numFramesActuallyPresented == 2`). See **[examples/sr_plus_fg.rs](examples/sr_plus_fg.rs)** for the
full combined pipeline. (DLSS-G runs at output resolution regardless; the same depth/motion-vector
buffers feed both features.)

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
