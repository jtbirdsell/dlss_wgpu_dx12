# Changelog

All notable changes to `dlss_wgpu_dx12` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

These changes landed after the `0.1.0` tag, driven by a multi-agent codebase audit
([`docs/BACKLOG.md`](docs/BACKLOG.md), all 39 items merged across PRs #9–#22). The crate is not yet
published to crates.io (`publish = false` — a git `wgpu` dependency precludes it), so this is not a
release.

### Changed
- `DlssContext::render_resolution()` now returns the **optimal** render resolution (the
  `InWidth`/`InHeight` the NGX feature was created with) rather than the minimum, matching the Ray
  Reconstruction contract — render at this resolution to avoid a per-frame feature recreate /
  suboptimal reconstruction. The min..=max range is still available via `render_resolution_range()`.
- Declared and CI-enforced a minimum supported Rust version: **1.87**.

### Added
- A crate-level umbrella `Error` enum (`Error::Dlss(DlssError)` /
  `Error::Streamline(StreamlineError)`, the latter behind `frame-generation`) with `From` impls, so
  a combined SR + FG application can use a single `?`-able error type. The domain error types are
  unchanged.

### Removed
- `unsafe impl Sync for FrameGenerationContext` — the type remains `Send` but is no longer `Sync`
  (its `&self` methods call into Streamline's process-global state). Affects the experimental
  `frame-generation` feature only.

### Fixed
- Frame Generation now rejects Streamline `dxgi.dll` / `d3d12.dll` loader-shim copies staged beside
  the executable; this crate drives DLSS-G through the interposer **proxy** path, which conflicts
  with those shims (`slInit` recursion). Stage only the SL plugin DLLs — see `docs/SETUP.md`.
- SR/RR `Drop` impls are poison-tolerant, preventing a double-panic → process abort if an NGX FFI
  call panicked while the SDK mutex was held.

### Security
- All sibling Streamline plugin DLLs (`sl.common` / `sl.dlss_g` / `sl.reflex` / `sl.pcl`) are now
  Authenticode-verified before load, not just `sl.interposer.dll`, and the verify→load TOCTOU window
  is narrowed.
- The NVIDIA signer pin now fails closed when the PKCS#7 signer cannot be parsed.

### Internal
- SR and RR share an `NgxFeature` core (~150 lines of duplication removed); the SDK mutex is held
  only across the NGX evaluate; CI gained `cargo-deny`, rustdoc-warning, and `rustfmt --check` gates;
  both wgpu fork patches are vendored under `patches/` with a recovery procedure.

## [0.1.0] - 2026-05-31

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
- **SR + FG interop bridge**: `FgConstants::from_render_parameters` (and
  `from_ray_reconstruction_parameters`) derive the Frame Generation per-frame constants from the
  *same* `DlssRenderParameters` used for the NGX evaluate, giving one source of jitter, history
  reset, and motion-vector scale across SR and FG — and converting the motion-vector convention
  (NGX render-resolution pixels ↔ Streamline normalized `[-1, 1]`).
- NGX diagnostics are forwarded to the [`log`](https://docs.rs/log) crate (target
  `dlss_wgpu_dx12::ngx`); the minimum level tracks the active `log` filter (silent by default).
- Examples: headless Super Resolution + Ray Reconstruction; an interactive animated Frame
  Generation example (`examples/frame_generation.rs`, with `--ui` for DLSS-G UI recomposition); a
  combined SR + FG pipeline (`examples/sr_plus_fg.rs`); dynamic-resolution scaling
  (`examples/dynamic_resolution.rs`); a DLAA toggle (`super_resolution --dlaa`); and a runtime
  SR↔RR toggle (`examples/sr_rr_toggle.rs`).
- Headless integration test, unit tests (the SR→FG motion-vector-scale conversion, the Halton
  jitter sequence, and the NGX result-code / perf-quality mapping), and Windows CI
  (check / clippy `-D warnings` / build-tests across the feature matrix, including
  `frame-generation`).

### Notes
- **Frame Generation** (`frame-generation` feature) is **experimental**: it requires NVIDIA
  Streamline, an RTX 40-series (Ada) or newer GPU, a visible/composited window, a non-vsync present
  mode, the SL DLLs staged beside the exe, and `STREAMLINE_SDK` set. It compiles in CI but can only
  be verified on RTX hardware + a display. See `docs/SETUP.md` §5 for the build/run setup.
- Builds require a patched wgpu fork (`jtbirdsell/wgpu`) carrying two additive dx12 patches:
  `dx12::CommandEncoder::raw_command_list` (for SR/RR/FG — gfx-rs/wgpu#8888, PR
  [gfx-rs/wgpu#9613](https://github.com/gfx-rs/wgpu/pull/9613)) and a Streamline factory-upgrade in
  `Instance::init` (for FG — design issue [gfx-rs/wgpu#9614](https://github.com/gfx-rs/wgpu/issues/9614)).
  See `patches/`, `docs/SETUP.md`, and [`docs/upstreaming.md`](docs/upstreaming.md) for the path off
  the fork.
