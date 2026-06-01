# Vendored wgpu patch (gfx-rs/wgpu#8888)

`wgpu-29.0.3-dx12-raw-command-list.patch` adds a `pub unsafe fn raw_command_list(&self) ->
Option<&ID3D12GraphicsCommandList>` accessor to `wgpu_hal::dx12::CommandEncoder`, exposing the
recording command list that NVIDIA NGX (DLSS) `CreateFeature`/`EvaluateFeature` require. wgpu 29
keeps this field private — see <https://github.com/gfx-rs/wgpu/issues/8888>.

(`Device::raw_device()` / `raw_queue()` already exist in stock wgpu 29, so the patch does **not**
touch them.)

## Status — not a build step

This file is the canonical record of the patch for the upstream PR; it is **not** applied at build
time. `Cargo.toml` resolves `wgpu` from the pinned fork `jtbirdsell/wgpu` @ `d81d755`, which already
carries this accessor (plus the Streamline factory-upgrade). Neither this crate's build nor CI
clones `gfx-rs/wgpu` or runs `git apply`. Consumers who pin the same fork/rev (see
[docs/SETUP.md](../docs/SETUP.md) §3) inherit the accessor automatically — no manual patching.

## Regenerating / re-verifying against `v29.0.3`

When forward-porting the accessor or refreshing this `.patch` for the upstream PR (e.g. after
line-ending drift), regenerate it from a clean `v29.0.3` tag:

```sh
git clone --depth 1 --branch v29.0.3 https://github.com/gfx-rs/wgpu.git ../wgpu
git -C ../wgpu apply patches/wgpu-29.0.3-dx12-raw-command-list.patch          # verify it still applies
git -C ../wgpu diff -- wgpu-hal/src/dx12/mod.rs > patches/wgpu-29.0.3-dx12-raw-command-list.patch
```
