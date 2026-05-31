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
- Opt-in **DXC** instance helpers: `dxc_instance_descriptor`, `dxc_instance_descriptor_at`.
- Headless integration test, Super Resolution + Ray Reconstruction examples, and Windows CI
  (check / clippy `-D warnings` / build-tests).

### Notes
- **Frame Generation** (`frame-generation` feature) is experimental and not yet implemented; it
  requires NVIDIA Streamline and owning the swapchain.
- Until [gfx-rs/wgpu#8888](https://github.com/gfx-rs/wgpu/issues/8888) lands upstream, builds require
  a patched wgpu (`dx12::CommandEncoder::raw_command_list`); see `patches/` and `docs/SETUP.md`.
