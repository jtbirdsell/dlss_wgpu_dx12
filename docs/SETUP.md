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
# Pin to the SDK version this crate targets (DLSS 4 / 310.x); CI pins the same tag.
git clone --depth 1 --branch v310.6.0 https://github.com/NVIDIA/DLSS C:/Users/<you>/dlss_sdk
$env:DLSS_SDK = 'C:/Users/<you>/dlss_sdk'
```

The build expects the standard SDK layout: headers under `include/`, the import libraries under
`lib/Windows_x86_64/x64/` (what `build.rs` links against), and the runtime DLLs under
`lib/Windows_x86_64/rel/` (and `dev/`).

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

## 3. Depend on the patched wgpu (gfx-rs/wgpu#8888)

This crate needs an accessor that stock wgpu does not yet expose: a dx12
`CommandEncoder::raw_command_list` accessor on wgpu-hal (tracked as
[gfx-rs/wgpu#8888](https://github.com/gfx-rs/wgpu/issues/8888)). NGX records its
initialization and evaluation work onto the *currently open* D3D12 command list, so the crate must
borrow that list out of a wgpu command encoder.

Until the patch lands upstream, **every consumer of this crate must build against the same patched
wgpu**, because the patch changes wgpu-hal's public surface and Cargo requires a single resolved
version of wgpu / wgpu-hal across the dependency graph.

Point your workspace at the same patched wgpu. The first option is what this crate's own
`Cargo.toml` uses and is the simplest:

- **Fork-direct (recommended)** — depend on the pre-patched fork by git `rev`. The fork already
  carries *both* additive patches (the accessor *and* the Streamline factory-upgrade from §5.1), so
  there is nothing to clone or apply yourself:

  ```toml
  [dependencies]
  wgpu = { git = "https://github.com/jtbirdsell/wgpu", rev = "d81d7552cf201d47359e993fbebc9c088142bc38", default-features = false, features = ["dx12"] }
  ```

  If you also pull in `wgpu` transitively through other crates, redirect those to the same fork/rev
  so the graph resolves to one wgpu:

  ```toml
  [patch.crates-io]
  wgpu     = { git = "https://github.com/jtbirdsell/wgpu", rev = "d81d7552cf201d47359e993fbebc9c088142bc38" }
  wgpu-hal = { git = "https://github.com/jtbirdsell/wgpu", rev = "d81d7552cf201d47359e993fbebc9c088142bc38" }
  ```

- **Local checkout** — if you maintain your own wgpu clone, clone `gfx-rs/wgpu` at the `v29.0.3`
  tag, apply `patches/wgpu-29.0.3-dx12-raw-command-list.patch` from this repo, and depend on it by
  path (directly or via `[patch.crates-io]` with `path = ...`):

  ```toml
  [dependencies]
  wgpu = { path = "C:/Users/<you>/wgpu/wgpu", default-features = false, features = ["dx12"] }
  ```

  ```toml
  [patch.crates-io]
  wgpu     = { path = "C:/Users/<you>/wgpu/wgpu" }
  wgpu-hal = { path = "C:/Users/<you>/wgpu/wgpu-hal" }
  ```

  Note: the committed patch contains only the `raw_command_list` accessor (SR/RR). The Streamline
  factory-upgrade that **Frame Generation** needs lives only in the fork (§5.1), so a local checkout
  is sufficient for SR/RR but not for FG — for FG, use the fork-direct option above.

This crate currently targets **wgpu 29.0.x**. Your patched checkout must be that same version so
the `windows` COM interface types stay ABI-compatible (this crate pins `windows` to the same major
version wgpu-hal uses).

Once the accessor lands upstream (tracked as [gfx-rs/wgpu#9613](https://github.com/gfx-rs/wgpu/pull/9613)),
**Super Resolution and Ray Reconstruction** can move to a plain `wgpu = "29"` dependency. **Frame
Generation** still needs the fork's Streamline factory-upgrade (gfx-rs/wgpu#9614, NVIDIA-specific and
not upstreamable as-is), so an FG consumer keeps the fork via `[patch.crates-io]`. See
[`docs/upstreaming.md`](upstreaming.md) for the current status.

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

## 5. Frame Generation (experimental): build + run setup

DLSS Frame Generation (DLSS-G) is behind the `frame-generation` cargo feature and is **off by
default**. It is driven by NVIDIA **Streamline**, not raw NGX, so it needs some setup beyond the
SR/RR steps above. (FG is also EXPERIMENTAL and hardware-gated — see the README's
[Frame Generation](../README.md#frame-generation-experimental) section for the ordering and runtime
requirements.)

### 5.1 The wgpu fork carries the Streamline factory-upgrade patch

The vendored wgpu fork (`jtbirdsell/wgpu`, rev `d81d755`) carries **two** additive dx12 patches: the
`raw_command_list` / `raw_device` / `raw_queue` accessors (gfx-rs/wgpu#8888, used by SR/RR/FG) *and*
a Streamline factory-upgrade in `Instance::init`. The latter upgrades wgpu's DXGI factory to a
Streamline proxy so that wgpu's own swapchain becomes the SL proxy that drives DLSS-G. It is a
runtime-guarded no-op unless `sl.interposer.dll` is already loaded into the process, so it is safe
for non-FG builds. Section 3 already pins this same fork/rev; no extra patch step is needed for FG.

### 5.2 Obtain Streamline and set `STREAMLINE_SDK`

Obtain the NVIDIA Streamline SDK (the version this crate targets is SL 2.11.1) and point the
`STREAMLINE_SDK` environment variable at its root. At runtime the crate locates the interposer at
`$STREAMLINE_SDK/bin/x64/sl.interposer.dll`, verifies its signature, and resolves the exported
`sl*` entry points from it.

```powershell
$env:STREAMLINE_SDK = 'C:/Users/<you>/streamline'
```

`STREAMLINE_SDK` is required **at runtime** for any FG build. (`DLSS_SDK` and, if needed,
`LIBCLANG_PATH` are still required to *build* the crate, as in sections 1-2.)

### 5.3 Stage the Streamline DLLs

DLSS-G needs the Streamline interposer, the SL plugin DLLs, and the DLSS-G NGX model staged next to
your executable. Copy these beside the built `.exe` (e.g. `target/debug/examples/` for the example):

| DLL | Purpose |
| --- | --- |
| `sl.interposer.dll` | The Streamline interposer (loaded + signature-verified at runtime). |
| `sl.common.dll` | Streamline common plugin. |
| `sl.dlss_g.dll` | The DLSS-G (Frame Generation) plugin. |
| `sl.reflex.dll` | Reflex plugin (a mandatory DLSS-G dependency). |
| `sl.pcl.dll` | PCL plugin (latency markers DLSS-G requires). |
| `nvngx_dlssg.dll` | The DLSS-G NGX model library. |

Without these, `slInit` / DLSS-G load fails and no frames are generated.

In addition, Streamline expects its interposer to sit in front of the system DXGI/D3D12 so it can
hook the swapchain path: stage a copy of `sl.interposer.dll` next to the exe **named `dxgi.dll` and
`d3d12.dll`** (the loader-shim copies). These are distinct from the `$STREAMLINE_SDK/bin/x64`
interposer the crate loads to resolve the `sl*` exports.

```powershell
$sl = "$env:STREAMLINE_SDK/bin/x64"
$dst = ".\target\debug\examples"
foreach ($dll in 'sl.interposer.dll','sl.common.dll','sl.dlss_g.dll','sl.reflex.dll','sl.pcl.dll','nvngx_dlssg.dll') {
    Copy-Item "$sl/$dll" -Destination $dst
}
# Loader-shim copies so SL fronts the system DXGI/D3D12 swapchain path:
Copy-Item "$sl/sl.interposer.dll" -Destination "$dst/dxgi.dll"
Copy-Item "$sl/sl.interposer.dll" -Destination "$dst/d3d12.dll"
```

### 5.4 Signature verification of the interposer

Before it is ever `LoadLibrary`'d, `sl.interposer.dll` is hard-gated on two checks (see
`src/streamline/security.rs`):

1. **Trust (hard gate):** `WinVerifyTrust` validates the embedded Authenticode signature and
   confirms its certificate chain terminates at a trusted root — the same check Windows applies when
   you double-click a signed binary. Unsigned, tampered, expired-chain, or untrusted binaries are
   refused, and the crate will not load the DLL.
2. **Signer identity (NVIDIA pinning):** the crate then cracks the embedded PKCS#7, pulls the
   signer's leaf certificate, and requires its subject common name to contain "NVIDIA". A
   successfully parsed **non-NVIDIA** subject is a hard failure (`StreamlineError::UntrustedSigner`).
   This step is best-effort only in one direction: if the subject *cannot be parsed* after the trust
   gate has already passed, it is logged (a loud `SIGNER-PIN-SKIPPED` audit line) and treated as a
   soft pass (the `WinVerifyTrust` trust gate is the load-bearing requirement). For high-assurance
   deployments, set `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1` to promote that parse-failure soft pass to a
   hard failure, so the DLL loads only when the NVIDIA signer is positively confirmed.

The trust gate also checks **revocation** across the whole certificate chain
(`WTD_REVOKE_WHOLECHAIN`); if the revocation servers are unreachable the check degrades to offline
with a loud `REVOCATION-CHECK-SKIPPED` warning, but a genuinely *revoked* certificate is always a
hard failure.

If `Streamline::init` fails with a signature/trust error, confirm you staged the genuine
NVIDIA-signed `sl.interposer.dll` from the Streamline SDK (and that it was not replaced or
corrupted).

## 6. Reproduce the NVIDIA license / copyright notices

The DLSS SDK and its DLLs are governed by the NVIDIA software license, not by this crate's
MIT/Apache-2.0 license. When you redistribute a product built with this crate you must reproduce
the license and copyright notices specified in **section 9.5 ("License and Copyright Notices") of
the NVIDIA DLSS Programming Guide** (included with the SDK) in your shipped product — for example in
an about box, credits screen, or accompanying documentation.

Review the NVIDIA license you accepted when downloading the SDK for the authoritative and complete
redistribution requirements; the steps above summarize the parts relevant to this crate but are
not a substitute for that license.
