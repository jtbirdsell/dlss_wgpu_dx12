# Upstreaming the wgpu DX12 `raw_command_list` accessor (gfx-rs/wgpu#8888)

This crate vendors a one-accessor patch to `wgpu-hal` (see `patches/`). To remove the vendoring,
upstream it. **This requires your GitHub account** — the steps below are ready to run; nothing here
pushes on your behalf.

## The change

`wgpu_hal::dx12::CommandEncoder` keeps its recording `ID3D12GraphicsCommandList` in a private field
with no accessor, so external libraries (NVIDIA NGX/DLSS, AMD FidelityFX, …) cannot record onto the
in-flight list. Vulkan's `CommandEncoder` already exposes `raw_handle()`; this adds the DX12
equivalent:

```rust
// wgpu-hal/src/dx12/mod.rs — inherent `impl CommandEncoder`
/// Returns the raw D3D12 graphics command list currently being recorded, if any.
/// `Some` between `begin_encoding` and `end_encoding`/`discard_encoding`.
/// # Safety
/// The reference must not outlive the current encoding; the caller must not Close/Reset the list.
pub unsafe fn raw_command_list(&self) -> Option<&Direct3D12::ID3D12GraphicsCommandList> {
    self.list.as_ref()
}
```

(`Device::raw_device()` / `raw_queue()` already exist upstream, so no change is needed there.)

## PR metadata

- **Title:** `[hal/dx12] Add CommandEncoder::raw_command_list() accessor (#8888)`
- **Body:**
  > Exposes the recording `ID3D12GraphicsCommandList` from `dx12::CommandEncoder`, mirroring the
  > Vulkan backend's `CommandEncoder::raw_handle()`. This unblocks `as_hal_mut`-based interop with
  > native libraries that must record onto wgpu's in-flight command list (NVIDIA NGX/DLSS, AMD
  > FidelityFX, etc.). The accessor is `unsafe` and documented with the encoding-lifetime contract.
  > Fixes #8888.
- Add a `CHANGELOG.md` entry under the unreleased section.

## Steps (run yourself)

The change is already in your working tree at `../wgpu`. Upstream targets **trunk**, not the
`v29.0.3` tag, so rebase the edit onto a fresh trunk checkout:

```sh
gh repo fork gfx-rs/wgpu --clone   # or fork in the UI and clone your fork
cd wgpu
git checkout -b dx12-raw-command-list
# Re-apply the accessor to wgpu-hal/src/dx12/mod.rs (inherent impl CommandEncoder).
# The exact text is in ../dlss_wgpu_dx12/patches/wgpu-29.0.3-dx12-raw-command-list.patch
# (the surrounding code is unchanged on trunk as of 29.0.3, so `git apply` usually works:)
git apply ../dlss_wgpu_dx12/patches/wgpu-29.0.3-dx12-raw-command-list.patch
# add a CHANGELOG.md entry, then:
git commit -am "[hal/dx12] Add CommandEncoder::raw_command_list() accessor (#8888)"
git push -u origin dx12-raw-command-list
gh pr create --repo gfx-rs/wgpu --title "[hal/dx12] Add CommandEncoder::raw_command_list() accessor (#8888)" --body-file -  # paste the body above
```

Once merged and released, drop the `patches/` vendoring and point `wgpu` in `Cargo.toml` back at
crates.io (the accessor will be available on stock wgpu).
