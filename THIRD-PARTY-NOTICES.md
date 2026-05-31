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
  `nvngx_dlssd.dll` (Ray Reconstruction), and — if Frame Generation is enabled — the NVIDIA
  Streamline runtime DLLs (see **NVIDIA Streamline / DLSS Frame Generation** below).

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

## NVIDIA Streamline / DLSS Frame Generation

The optional `frame-generation` cargo feature drives DLSS Frame Generation (DLSS-G) through NVIDIA's
**Streamline (SL)** framework. Streamline is an *interposer*: this crate links nothing from it at
build time — the SL runtime is loaded dynamically at runtime — so the build only needs the same NGX
SDK and `libclang` environment as the Super Resolution / Ray Reconstruction features (see above).

- **Build time:** nothing additional. Frame Generation reuses the NGX headers/import libs located via
  `DLSS_SDK`; it does **not** require the Streamline SDK to be present to compile.
- **Runtime (shipped beside your executable):** the Streamline runtime DLLs that the feature loads
  and the DLSS-G NGX snippet they bring in —
  - `sl.interposer.dll` — the Streamline interposer entry point
  - `sl.common.dll` — shared Streamline plugin support
  - `sl.dlss_g.dll` — the DLSS Frame Generation plugin
  - `sl.reflex.dll` — NVIDIA Reflex (a hard dependency of Frame Generation)
  - `sl.pcl.dll` — PC Latency markers support
  - `nvngx_dlssg.dll` — the DLSS Frame Generation NGX runtime

These are licensed under the **NVIDIA Streamline license**, *not* under this crate's MIT/Apache terms.
See `license.txt` in your Streamline SDK checkout, and the SDK's `3rd-party-licenses.md` for the
third-party components Streamline itself redistributes (e.g. Premake, nlohmann/json, ImGui, slang,
and others; note that `sl_nvperf.h` / `sl_nvperf.dll` are covered separately by the NVIDIA Nsight
Perf SDK License referenced in that `license.txt`).

### Redistribution obligations (your responsibility when shipping a product)

1. **These NVIDIA-signed binaries are NOT bundled in this crate.** They must be obtained from NVIDIA
   — specifically the **Streamline SDK** — and staged beside your application by you. This crate ships
   no Streamline code or binaries.
2. **Do not modify, repackage, or re-sign** NVIDIA's signed SL DLLs (or `nvngx_dlssg.dll`). Streamline
   validates the signatures of the plugins it loads; tampering will break Frame Generation in addition
   to violating the license. If you Authenticode-sign your application payload, *append* to — never
   strip — NVIDIA's signature.
3. **Ship release binaries, not development ones.** The development/profiling SL DLLs are watermarked
   and must not be shipped in a product.
4. **Reproduce NVIDIA's Streamline copyright and license notices** (the contents of `license.txt`)
   together with the bundled third-party notices (`3rd-party-licenses.md`) in your product's
   documentation or about screen. Extract them from the exact Streamline SDK version you ship — the
   text can change between versions.
5. Review and comply with the full NVIDIA Streamline license before distributing.

This file is informational and is **not legal advice**; consult the actual NVIDIA license terms.

## wgpu

This crate currently depends on a locally patched build of [`wgpu`](https://github.com/gfx-rs/wgpu)
(MIT OR Apache-2.0) that adds a `dx12::CommandEncoder::raw_command_list()` accessor; see
`patches/` and `docs/SETUP.md`. No wgpu source is redistributed by this crate.
