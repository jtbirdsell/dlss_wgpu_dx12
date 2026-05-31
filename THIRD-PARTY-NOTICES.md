# Third-Party Notices

`dlss_wgpu_dx12` itself is licensed under **MIT OR Apache-2.0** (see `LICENSE-MIT` /
`LICENSE-APACHE`). It bundles **no** NVIDIA code or binaries. However, building and shipping it
requires NVIDIA components governed by NVIDIA's own license terms, summarized here.

## NVIDIA DLSS / NGX SDK

This crate builds against — and applications that use it ship — components of the NVIDIA DLSS (NGX)
SDK:

- **Build time:** the NGX headers and import libraries (`nvsdk_ngx_d.lib` / `nvsdk_ngx_s.lib`) from
  a clone of <https://github.com/NVIDIA/DLSS> (located via the `DLSS_SDK` environment variable).
- **Runtime (shipped beside your executable):** `nvngx_dlss.dll` (Super Resolution),
  `nvngx_dlssd.dll` (Ray Reconstruction), and — if Frame Generation is ever enabled — the NVIDIA
  Streamline runtime DLLs.

These are licensed under the **NVIDIA DLSS SDK license**, *not* under this crate's MIT/Apache terms.
See `LICENSE.txt` in your `DLSS_SDK` checkout and the **DLSS Programming Guide** that ships with it.

### Redistribution obligations (your responsibility when shipping a product)

1. **Ship only the release DLLs** (`lib/Windows_x86_64/rel/...`). The `dev` DLLs are watermarked and
   carry a debug overlay — they must not be shipped. (The `debug_overlay` cargo feature only changes
   the runtime search path to the `dev` DLLs for local debugging.)
2. **Do not modify, repackage, or re-sign** NVIDIA's signed DLLs. If you Authenticode-sign your
   application payload, *append* to — never strip — NVIDIA's signature.
3. **Reproduce the copyright and license notices from §9.5 of the DLSS Programming Guide** (the
   NVIDIA proprietary notice plus the bundled third-party notices) in your product's documentation
   or about screen. Extract them from the exact SDK version you ship — the text can change between
   versions.
4. Review and comply with the full NVIDIA DLSS SDK license before distributing.

This file is informational and is **not legal advice**; consult the actual NVIDIA license terms.

## wgpu

This crate currently depends on a locally patched build of [`wgpu`](https://github.com/gfx-rs/wgpu)
(MIT OR Apache-2.0) that adds a `dx12::CommandEncoder::raw_command_list()` accessor; see
`patches/` and `docs/SETUP.md`. No wgpu source is redistributed by this crate.
