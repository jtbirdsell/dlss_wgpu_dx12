# Setup

`dlss_wgpu_dx12` is **Windows only** and depends on three things that are not fetched
automatically: the NVIDIA DLSS SDK, `libclang` (for bindgen at build time), and a patched copy of
wgpu. This document walks through building the crate and shipping a product that uses it.

All examples use PowerShell. Set the environment variables inline (in the same shell) **before**
running any `cargo` command, or persist them with `setx` / your system environment settings.

## 1. Clone the DLSS SDK and set `DLSS_SDK`

The build script generates NGX bindings from, and links against, the NVIDIA DLSS SDK. Clone it and
point the `DLSS_SDK` environment variable at the clone:

```powershell
git clone https://github.com/NVIDIA/DLSS C:/Users/<you>/dlss_sdk
$env:DLSS_SDK = 'C:/Users/<you>/dlss_sdk'
```

The build expects the standard SDK layout (headers under `include/`, import libraries and the
runtime DLLs under `lib/Windows_x86_64/`).

## 2. Install libclang (for bindgen)

`bindgen` needs `libclang` to parse the NGX headers. Either option works; pick one:

- **LLVM installer (needs admin):** install LLVM from <https://releases.llvm.org/> (or via
  `winget install LLVM.LLVM`). The installer requires administrator rights. This places
  `libclang.dll` somewhere bindgen can find it automatically.

- **pip, no admin:** install the prebuilt `libclang` wheel into your user site-packages and point
  `LIBCLANG_PATH` at the bundled native library:

  ```powershell
  pip install --user libclang
  $env:LIBCLANG_PATH = 'C:/Users/<you>/AppData/Roaming/Python/Python314/site-packages/clang/native'
  ```

  Adjust the path to match your Python version's site-packages directory. `LIBCLANG_PATH` must
  point at the directory containing `libclang.dll`.

With `DLSS_SDK` and (if needed) `LIBCLANG_PATH` set, `cargo build` should now configure bindgen and
link NGX.

## 3. Apply the vendored wgpu patch (gfx-rs/wgpu#8888)

This crate needs an accessor that stock wgpu does not yet expose: a dx12
`CommandEncoder::raw_command_list` accessor on wgpu-hal (tracked as
[gfx-rs/wgpu#8888](https://github.com/gfx-rs/wgpu/issues/8888)). NGX records its
initialization and evaluation work onto the *currently open* D3D12 command list, so the crate must
borrow that list out of a wgpu command encoder.

Until the patch lands upstream, **every consumer of this crate must build against the same patched
wgpu**, because the patch changes wgpu-hal's public surface and Cargo requires a single resolved
version of wgpu / wgpu-hal across the dependency graph.

Point your workspace at a patched copy of wgpu in one of two ways:

- **Path dependency** — depend on the patched checkout directly:

  ```toml
  [dependencies]
  wgpu = { path = "C:/Users/<you>/wgpu/wgpu", default-features = false, features = ["dx12"] }
  ```

- **`[patch.crates-io]`** — keep a normal `wgpu` dependency but redirect it (and wgpu-hal) to the
  patched checkout:

  ```toml
  [patch.crates-io]
  wgpu = { path = "C:/Users/<you>/wgpu/wgpu" }
  wgpu-hal = { path = "C:/Users/<you>/wgpu/wgpu-hal" }
  ```

This crate currently targets **wgpu 29.0.x**. Your patched checkout must be that same version so
the `windows` COM interface types stay ABI-compatible (this crate pins `windows` to the same major
version wgpu-hal uses).

Once the accessor is merged and released upstream, this step goes away and a plain
`wgpu = "29"` dependency will suffice.

## 4. Ship the release DLLs at runtime

At runtime, NGX loads the DLSS feature libraries from the directory next to your executable. Copy
the **release** DLLs from the SDK beside your built `.exe`:

| DLL | Source | When to ship |
| --- | --- | --- |
| `nvngx_dlss.dll` | `$DLSS_SDK/lib/Windows_x86_64/rel/` | Super Resolution (always). |
| `nvngx_dlssd.dll` | `$DLSS_SDK/lib/Windows_x86_64/rel/` | Ray Reconstruction (`ray-reconstruction` feature). |

```powershell
Copy-Item "$env:DLSS_SDK/lib/Windows_x86_64/rel/nvngx_dlss.dll"  -Destination .\target\release\
Copy-Item "$env:DLSS_SDK/lib/Windows_x86_64/rel/nvngx_dlssd.dll" -Destination .\target\release\
```

> **Never ship the development (`dev`) DLLs.** The `dev` builds under
> `$DLSS_SDK/lib/Windows_x86_64/dev/` render a visible watermark and are for development only.
> The `debug_overlay` cargo feature links against those dev libraries — do not enable it in a
> shipped build.

If you build with `--release` but DLSS reports `FeatureNotSupported` or fails to initialize at
runtime, confirm the correct release DLL is sitting next to the executable.

## 5. Reproduce the NVIDIA license / copyright notices

The DLSS SDK and its DLLs are governed by the NVIDIA software license, not by this crate's
MIT/Apache-2.0 license. When you redistribute a product built with this crate you must reproduce
the license and copyright notices specified in **section 9.5 ("License and Copyright Notices") of
the NVIDIA DLSS Programming Guide** (included with the SDK) in your shipped product — for example in
an about box, credits screen, or accompanying documentation.

Review the NVIDIA license you accepted when downloading the SDK for the authoritative and complete
redistribution requirements; the steps above summarize the parts relevant to this crate but are
not a substitute for that license.
