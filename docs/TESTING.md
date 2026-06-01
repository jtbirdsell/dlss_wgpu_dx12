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

## Widening the matrix: GPU CI (not built here)

`.github/workflows/ci.yml` runs on GitHub-hosted runners with **no GPU**, so the real-evaluate tier
is not exercised in CI today. Two ways to add it (the tests are already CI-shaped — they self-stage
DLLs and skip when unsupported):

- **Self-hosted runner** — your own RTX box, or a cloud Ada/Ampere instance you register. A
  `workflow_dispatch` job with `runs-on: [self-hosted, windows, gpu]` that sets `DLSS_SDK`
  (and `STREAMLINE_SDK` for FG), then runs
  `cargo test --features ray-reconstruction --test headless -- --ignored`. Covers SR + RR + the FG
  contract on whatever GPU the runner has. No premium billing; you maintain the runner.
- **GitHub-hosted GPU larger runner** (NVIDIA **T4**, Turing) — available on Team/Enterprise plans,
  premium per-minute. A T4 is a **different architecture** than the 4090, so it takes SR/RR to n=2
  automatically. It **cannot** validate Frame Generation: DLSS-G needs an Ada (RTX 40-series)+ GPU
  *and* a composited display, while a T4 is Turing and headless — the FG tests will skip
  (`FeatureNotSupported`).

For FG on either runner you must also stage the Streamline `sl.*` plugins + the `dxgi.dll`/`d3d12.dll`
loader-shim copies next to the test binary (see [SETUP.md §5.3](SETUP.md)).

## The ceiling: FG real generation is manual

Real Frame Generation — `numFramesActuallyPresented` flipping 1 → 2 — fundamentally requires a
**visible, composited, foreground window**; DLSS-G silently declines to present generated frames to a
surface that is not actually being composited to the screen (see `src/streamline/frame_gen.rs`). No
headless or datacenter-GPU runner can prove it. `examples/frame_generation.rs` and
`examples/sr_plus_fg.rs` remain the gold standard — they print the final `numFramesActuallyPresented`,
validated on an RTX 4090.
