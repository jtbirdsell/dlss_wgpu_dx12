# Vendored wgpu patches

This crate depends on a **fork** of wgpu (`jtbirdsell/wgpu` @ `d81d755`, see `Cargo.toml`) that
carries two small *additive* dx12 patches on top of the stock `v29.0.3` tag. Both patches are
vendored here so the fork can be **reconstructed from scratch** if the fork repository is ever
deleted, renamed, made private, or its rev garbage-collected (the audit's "fork single point of
failure", M6). `Cargo.lock` pins the rev but not the *source*, so these files are the durable record.

The fork's two commits, in order, on top of `v29.0.3` (`4cbe623`):

| # | Fork commit | Patch file | Upstream tracker |
| - | ----------- | ---------- | ---------------- |
| 1 | `549f758` | `wgpu-29.0.3-dx12-raw-command-list.patch` | PR [gfx-rs/wgpu#9613](https://github.com/gfx-rs/wgpu/pull/9613) (origin: issue #8888) |
| 2 | `d81d755` | `wgpu-29.0.3-dx12-streamline-factory-upgrade.patch` | issue [gfx-rs/wgpu#9614](https://github.com/gfx-rs/wgpu/issues/9614) (design) |

**Patch 1 — `raw_command_list()` accessor.** Adds `pub unsafe fn raw_command_list(&self) ->
Option<&ID3D12GraphicsCommandList>` to `wgpu_hal::dx12::CommandEncoder`, exposing the recording
command list that NVIDIA NGX (DLSS) `CreateFeature`/`EvaluateFeature` require. wgpu 29 keeps this
field private — see <https://github.com/gfx-rs/wgpu/issues/8888>. (`Device::raw_device()` /
`raw_queue()` already exist in stock wgpu 29, so the patch does **not** touch them.) Needed by
SR/RR *and* FG.

**Patch 2 — Streamline factory-upgrade.** In `dx12::Instance::init`, right after `create_factory`,
calls `super::streamline::upgrade_factory(&mut factory)` to `slUpgradeInterface` the DXGI factory
into an NVIDIA Streamline proxy when (and only when) `sl.interposer.dll` is already loaded in the
process. This makes the swapchains wgpu creates — and their `Present` — Streamline proxies, which is
what drives DLSS **Frame Generation** over wgpu's surface. It is a **runtime-guarded no-op** (gated
on `GetModuleHandleW("sl.interposer.dll")`), so stock wgpu use is completely unaffected and no cargo
feature is involved. Adds `wgpu-hal/src/dx12/streamline.rs` plus the
`windows/Win32_System_LibraryLoader` feature. This one is **NVIDIA-specific and not upstreamable
as-is**; #9614 proposes a vendor-neutral factory-customization hook that would let it live in this
crate's code instead of the fork. See [docs/upstreaming.md](../docs/upstreaming.md).

## Status — not a build step

These `.patch` files are the canonical record + recovery kit; they are **not** applied at build
time. `Cargo.toml` resolves `wgpu` from the pinned fork `jtbirdsell/wgpu` @ `d81d755`, which already
carries both, and neither this crate's build nor CI clones `gfx-rs/wgpu` or runs `git apply`.
Consumers who pin the same fork/rev (see [docs/SETUP.md](../docs/SETUP.md) §3) inherit both patches
automatically — no manual patching.

## Reconstructing the fork from these patches (recovery procedure)

If the `jtbirdsell/wgpu` fork is ever unavailable, rebuild an equivalent tree from the upstream
`v29.0.3` tag and the two patches, then point `Cargo.toml` at the local clone (`wgpu = { path =
"../wgpu/wgpu", … }`) or push it to a fresh remote and pin that:

```sh
git clone --branch v29.0.3 https://github.com/gfx-rs/wgpu.git ../wgpu
cd ../wgpu
git apply ../dlss_wgpu_dx12/patches/wgpu-29.0.3-dx12-raw-command-list.patch
git apply ../dlss_wgpu_dx12/patches/wgpu-29.0.3-dx12-streamline-factory-upgrade.patch
git add -A && git commit -m "dlss_wgpu_dx12 dx12 patches (raw_command_list + streamline factory)"
```

Apply them **in order** (patch 2's `instance.rs`/`mod.rs` hunks sit alongside, but do not depend on,
patch 1). The result is byte-equivalent to fork rev `d81d755`. To additionally harden against the
SPOF, mirror the fork under a durable/org account or `cargo vendor` the wgpu crates and commit the
snapshot; protect the pinned rev with a tag or protected branch.

## Regenerating / re-verifying the patches against `v29.0.3`

When forward-porting or refreshing a `.patch` for the upstream work (e.g. after line-ending drift),
regenerate from a clean checkout:

```sh
git clone --depth 1 --branch v29.0.3 https://github.com/gfx-rs/wgpu.git ../wgpu
git -C ../wgpu apply patches/wgpu-29.0.3-dx12-raw-command-list.patch          # verify it still applies
git -C ../wgpu diff -- wgpu-hal/src/dx12/mod.rs > patches/wgpu-29.0.3-dx12-raw-command-list.patch
```

Patch 2 spans four files (`wgpu-hal/Cargo.toml`, `wgpu-hal/src/dx12/{instance.rs,mod.rs}`, and the
new `wgpu-hal/src/dx12/streamline.rs`); regenerate it from the two fork commits with
`git -C ../wgpu diff 549f758..d81d755 > patches/wgpu-29.0.3-dx12-streamline-factory-upgrade.patch`.
