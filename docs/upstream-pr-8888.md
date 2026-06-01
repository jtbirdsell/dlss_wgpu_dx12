# Upstreaming status: removing the wgpu fork

`dlss_wgpu_dx12` currently depends on a small wgpu fork (`jtbirdsell/wgpu`, pinned by `rev` in
`Cargo.toml`) that carries two additive patches over stock wgpu 29.0.3. This document tracks the work
to upstream both so the crate can eventually depend on released wgpu with **no fork**.

## Patch 1 — `dx12::CommandEncoder::raw_command_list()` (gfx-rs/wgpu#8888)

A read accessor exposing the recording `ID3D12GraphicsCommandList`, needed by SR, RR, and FG to record
NGX/Streamline work onto wgpu's in-flight command list (via `as_hal_mut`).

- **Status: PR open — [gfx-rs/wgpu#9613](https://github.com/gfx-rs/wgpu/pull/9613).** Re-authored
  against trunk; mirrors the existing `Buffer::raw_resource` / `Fence::raw_fence` accessors. #8888 was
  filed independently (for AMD FidelityFX), so the accessor is broadly useful.
- **When it merges + ships:** Super Resolution and Ray Reconstruction no longer need the fork — point
  `Cargo.toml` at the released wgpu; FG-only consumers keep the fork via `[patch.crates-io]`.

## Patch 2 — Streamline factory-upgrade in `dx12::Instance::init` (DLSS Frame Generation)

`wgpu-hal/src/dx12/streamline.rs` swaps wgpu's DXGI factory for a Streamline proxy (`slUpgradeInterface`)
immediately after creation, when `sl.interposer.dll` is loaded — a runtime-guarded no-op otherwise.
This is what lets DLSS-G hook `Present`. It is **NVIDIA-specific, so not upstreamable as-is** (vendor
code does not belong in wgpu's generic Instance init).

- **Status: design-validation issue open — [gfx-rs/wgpu#9614](https://github.com/gfx-rs/wgpu/issues/9614).**
  It proposes a *vendor-neutral* dx12 factory-customization hook (mirroring the merged Vulkan
  `Instance::init_with_callback` / `Adapter::open_with_callback`, #7829) so the proxy install can live
  in this crate's code instead of a fork. Validating the design first, per wgpu's CONTRIBUTING.
- **If maintainers are receptive:** implement the agreed hook as a wgpu PR; then migrate the
  `slUpgradeInterface` call out of the fork into `src/streamline/` (invoked via the hook when the
  consumer builds the `wgpu::Instance`), delete `streamline.rs` from the fork, and repoint `Cargo.toml`
  at released wgpu.

## End state

No fork: SR/RR on released wgpu (patch 1 upstreamed), FG installing the Streamline proxy via the
upstream factory hook (patch 2 replaced by an upstream mechanism). This also unblocks crates.io
publishing, which the current git dependency precludes.

## Re-validation

After migrating off the fork, re-run `examples/frame_generation.rs` on an RTX 40-series (Ada) or newer
GPU and confirm DLSS-G still reaches `numFramesActuallyPresented == 2` with the proxy installed via the
upstream hook, and that crate CI stays green against released wgpu.
