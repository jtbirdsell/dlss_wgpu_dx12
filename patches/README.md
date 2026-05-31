# Vendored wgpu patch (gfx-rs/wgpu#8888)

`wgpu-29.0.3-dx12-raw-command-list.patch` adds a `pub unsafe fn raw_command_list(&self) ->
Option<&ID3D12GraphicsCommandList>` accessor to `wgpu_hal::dx12::CommandEncoder`, exposing the
recording command list that NVIDIA NGX (DLSS) `CreateFeature`/`EvaluateFeature` require. wgpu 29
keeps this field private — see <https://github.com/gfx-rs/wgpu/issues/8888>.

(`Device::raw_device()` / `raw_queue()` already exist in stock wgpu 29, so the patch does **not**
touch them.)

## Applying

This crate's `Cargo.toml` depends on `wgpu` via a path to a sibling `../wgpu` checkout:

```sh
git clone --depth 1 --branch v29.0.3 https://github.com/gfx-rs/wgpu.git ../wgpu
git -C ../wgpu apply patches/wgpu-29.0.3-dx12-raw-command-list.patch
```

Until the accessor lands upstream, **consumers of this crate must apply the same patch** in their
own workspace (via a path or `[patch.crates-io]` dependency on the patched `wgpu-hal`). If the patch
fails to apply due to line-ending drift, regenerate it against the `v29.0.3` tag:

```sh
git -C ../wgpu diff -- wgpu-hal/src/dx12/mod.rs > patches/wgpu-29.0.3-dx12-raw-command-list.patch
```
