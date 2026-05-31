# Changelog

All notable changes to `dlss_wgpu_dx12` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- DLSS **Super Resolution** for the wgpu DX12 backend: `DlssSdk`, `DlssContext`,
  `DlssRenderParameters`, `DlssTexture`, `DlssExposure`, `DlssPerfQualityMode`, `DlssFeatureFlags`.
- DLSS **Ray Reconstruction** behind the `ray-reconstruction` feature:
  `DlssRayReconstructionContext`, `DlssRayReconstructionParameters`, `RoughnessMode`, `DepthType`
  (enforces the IsHDR + low-res-motion-vector flags NGX requires).
- DLSS **Frame Generation** (DLSS-G) behind the `frame-generation` feature (**experimental**): a
  safe wrapper over NVIDIA Streamline that generates interpolated frames inside wgpu's own DX12
  swapchain `Present`. Public API: `Streamline` (loads + signature-verifies `sl.interposer.dll` and
  runs `slInit` for DLSS-G + Reflex + PCL, before `wgpu::Instance`), `FrameGenerationContext`
  (binds `slSetD3DDevice` after the device but before `surface.configure`, enables Reflex + DLSS-G,
  and exposes `query_state` / `set_mode`), and a per-frame `Frame` driver
  (`begin_frame` → `set_constants` → `acquire` → `tag` → `end_render` → `present`) that enforces
  the proven Streamline call order at runtime and performs the mandatory per-frame
  `GetCurrentBackBufferIndex`. Per-frame input types: `FgConstants`, `FgResources`, `FgResource`,
  `FgUi`; plus `FrameGenerationOptions`, `FrameGenerationMode`, `FrameGenerationState`, and the
  `StreamlineError` error type. Hardware-validated on an RTX 4090
  (`numFramesActuallyPresented == 2`); requires an RTX 40-series (Ada) or newer GPU. Relies on the
  wgpu fork's `Instance::init` Streamline factory-upgrade (rev `d81d755`); the interposer is located
  via `STREAMLINE_SDK` and the SL plugin DLLs + `nvngx_dlssg.dll` must be staged beside the exe. See
  the README's Frame Generation section, `docs/SETUP.md` §5, and `examples/frame_generation.rs`.
- Opt-in **DXC** instance helpers: `dxc_instance_descriptor`, `dxc_instance_descriptor_at`.
- Headless integration test, Super Resolution + Ray Reconstruction examples, an interactive
  animated Frame Generation example (`examples/frame_generation.rs`), and Windows CI
  (check / clippy `-D warnings` / build-tests).

### Notes
- **Frame Generation** (`frame-generation` feature) is **experimental**: it requires NVIDIA
  Streamline, an RTX 40-series (Ada) or newer GPU, a visible/composited window, a non-vsync present
  mode, the SL DLLs staged beside the exe, and `STREAMLINE_SDK` set. It compiles in CI but can only
  be verified on RTX hardware + a display. See `docs/SETUP.md` §5 for the build/run setup.
- Until [gfx-rs/wgpu#8888](https://github.com/gfx-rs/wgpu/issues/8888) lands upstream, builds require
  a patched wgpu (`dx12::CommandEncoder::raw_command_list`); see `patches/` and `docs/SETUP.md`.
