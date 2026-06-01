# Testing & validation matrix

This crate wraps native, GPU-only NVIDIA SDKs — NGX (Super Resolution, Ray Reconstruction) and
Streamline (Frame Generation). Different parts are validated at different levels. This document names
**what is proven where**, so the thin spots against the risk surface are explicit rather than hidden.

## Validation tiers

1. **Pure-logic unit tests** — run in CI on a GPU-less runner. Deterministic conversions and mappings
   (no FFI side effects).
2. **Headless mock + loader/signature** — run in CI, no GPU. The Frame-Generation call-order state
   machine driven against a recording mock, plus the interposer loader-path and Authenticode
   signature **error** paths (`WinVerifyTrust` against crafted inputs). See
   `src/streamline/frame_gen.rs`, `ffi.rs`, `security.rs` test modules.
3. **Real NGX evaluate** — `#[ignore]`d hardware tests (`tests/headless.rs`) that run an actual NGX
   evaluate end to end and **assert on the output**. Not run in CI (no GPU); run locally / on a GPU
   runner.
4. **Manual on-display** — Frame Generation *generation* (`numFramesActuallyPresented` 1 → 2)
   fundamentally needs a visible composited window; proven only by running the interactive examples.

## Matrix

| Capability | Pure-logic (CI) | Mock / loader (CI) | Real evaluate (hardware, automated) | On-display (manual) |
| --- | --- | --- | --- | --- |
| **Super Resolution** | NGX result-code mapping; perf/quality megapixel ladder; Halton jitter | — | `dlss_super_resolution_evaluates_and_writes_output` — real `DlssContext::render` (NGX evaluate), output sentinel-checked | n/a |
| **Ray Reconstruction** | mvec-scale SR→FG conversion | — | `dlss_ray_reconstruction_evaluates_and_writes_output` — real `DlssRayReconstructionContext::render`, output sentinel-checked | n/a |
| **Frame Generation** | tag-buffer ordering; `dxgi_format_of`; `FgConstants` translation | per-frame call-order state machine (mock); loader-path (`SdkPathNotSet`/`InterposerNotFound`) + signature (`SignatureVerificationFailed`/`UntrustedSigner`) error paths | `frame_generation_context_and_frame_contract` — real interposer load + signature verify + `hal` reach-through + `slSetD3DDevice`/feature-resolution/Reflex/per-frame plumbing. **Not generation.** | `numFramesActuallyPresented == 2` via `examples/frame_generation.rs` and `examples/sr_plus_fg.rs` |

How the real-evaluate tests prove NGX actually ran: each pre-fills the output texture with a white
sentinel, runs an 8-frame evaluate loop on non-black synthetic inputs, reads the output back, and
asserts (a) every `render()` returned `Ok` (NGX returned `Success` on real hardware) and (b) NGX
**overwrote the sentinel** (so the proof does not depend on DLSS image quality, and is immune to
wgpu's zero-initialization of textures it doesn't know were written through the raw command list).

## The n=1 caveat (read this)

The real-evaluate tests are output-asserted and reproducible, **but they have only ever run on one
GPU: an RTX 4090 (Ada), one driver.** A green "real evaluate" cell means *verified on that one
machine*, not *verified across architectures and drivers* — DLSS behavior can vary by GPU generation
and driver. Widen the matrix by running the `#[ignore]`d tests on more GPUs (below).

## Running the hardware tests locally

Set `DLSS_SDK` (a clone of <https://github.com/NVIDIA/DLSS>) and `LIBCLANG_PATH` (build-time bindgen).
The tests **auto-stage** their NGX DLL next to the test binary and **skip gracefully** (never fail)
when no NVIDIA Dx12 GPU / SDK is present. They serialize within the process, because NGX and
Streamline are process-global singletons.

```powershell
# Super Resolution
cargo test --test headless -- --ignored --nocapture
# + Ray Reconstruction
cargo test --features ray-reconstruction --test headless -- --ignored --nocapture
# + Frame Generation contract (also needs STREAMLINE_SDK and the SL DLLs staged — see SETUP.md §5.3)
cargo test --features frame-generation --test headless -- --ignored --nocapture
```

## Why there is no GPU CI

`.github/workflows/ci.yml` runs on GitHub-hosted runners with **no GPU**, so the real-evaluate tier
is not exercised in CI — and deliberately stays that way. The two ways to add GPU CI both fail for
this repository:

- **GitHub-hosted GPU runners** (NVIDIA T4 larger runners) require a paid **Team/Enterprise** plan.
- **Self-hosted runners** are a security risk on a **public** repository: a pull request from a fork
  can run arbitrary code on the runner machine. GitHub explicitly recommends against using
  self-hosted runners with public repos for exactly this reason. A `workflow_dispatch`-only job
  reduces but does not eliminate that posture, and it is not worth the hardening burden here.

So the real-evaluate **proof lives in the test code, not in CI infrastructure.** The `#[ignore]`d
hardware tests are output-asserted and skip-safe, so anyone with an RTX GPU reproduces them on
demand. CI covers everything that does **not** need a GPU: builds, clippy, the Frame-Generation mock
state machine, the loader-path + signature error paths, and all the pure-logic tests.

## Widening the matrix (n > 1)

Breadth — validating on more than one GPU generation / driver — is a **manual, on-hardware** step,
not a CI job. A maintainer or contributor runs the `#[ignore]`d tests on their own RTX hardware and
reports the result; the tests skip cleanly on non-NVIDIA / unsupported devices, so this is safe to
ask of anyone:

```powershell
cargo test --features ray-reconstruction --test headless -- --ignored --nocapture
```

A passing run on a different architecture (Turing / Ampere / Ada / Blackwell) or a newer driver is a
fresh data point — note notable results in the PR that adds them. (If GPU validation ever moves to a
**private** fork, a self-hosted runner there is fine — the public-repo fork-PR risk does not apply.)

## The ceiling: FG real generation is manual

Real Frame Generation — `numFramesActuallyPresented` flipping 1 → 2 — fundamentally requires a
**visible, composited, foreground window**; DLSS-G silently declines to present generated frames to a
surface that is not actually being composited to the screen (see `src/streamline/frame_gen.rs`). No
headless or datacenter-GPU runner can prove it. `examples/frame_generation.rs` and
`examples/sr_plus_fg.rs` remain the gold standard — they print the final `numFramesActuallyPresented`,
validated on an RTX 4090.
