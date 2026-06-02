# Engineering backlog

Produced by a multi-agent audit of the crate — 8 dimensional reviewers (security/unsafe,
supply-chain/licensing, architecture, correctness/FFI-ABI, tests, devops/CI/release, docs/API,
performance) → **adversarial per-finding verification** by independent agents → dedup + synthesis.
46 verified findings were deduped into the **39** items below. Each cites a concrete `file:line` a
reader can check. Severity: **Critical** (none) → **High** → **Medium** → **Low** → **Nit**.
Effort: **S** (≈ < 2h) · **M** · **L** (> 1 day).

## Summary

| Severity | Count |
| --- | ---: |
| Critical | 0 |
| High | 2 |
| Medium | 11 |
| Low | 24 |
| Nit | 2 |
| **Total** | **39** |

Raw findings by dimension (pre-dedup): security-unsafe 5 · supply-chain-licensing 6 ·
architecture-modularity 7 · correctness-ffi-abi 4 · test-coverage 7 · devops-build-ci-release 9 ·
docs-api-ergonomics 5 · performance-resources 3.

## Progress

**23 of 39 done.** Completed (with the PR that landed them):

- **High:** H1 ✅ #9 · H2 ✅ #10
- **Medium:** M1 ✅ #9 · M2 ✅ #9 · M3 ✅ (resolved by H1 + M5) · M4 ✅ #11 · M5 ✅ #13 · M7 ✅ #14 · M9 ✅ #12 · M11 ✅ #12
- **Low:** L2 ✅ #9 · L13 ✅ #9 · L1 ✅ #10 · L8 ✅ #10 · L14 ✅ #10 · L20 ✅ #10 · L21 ✅ #10 · L3 ✅ #11 · L4 ✅ #11 · L18 ✅ #11 · L19 ✅ #11 · L16 ✅ #12 · L17 ✅ #12
- **Partial:** L22 — `rustfmt.toml` + `cargo fmt --all --check` gate landed in #14 (with M7); `.editorconfig` in #10. Still open: `rust-version` (MSRV) + `rust-toolchain.toml` + an MSRV-pinned CI job.

Remaining work, grouped: policy — M6 (fork SPOF), L9 (docs.rs), L22 MSRV remainder; security depth — M8, L10, L15, L23; plus M10, L5–L7, L11, L12, L24, N1, N2. (Items below are not individually re-marked; cross-reference this list.)

---

## High

### H1 — `DlssContext::render_resolution()` returns min, but the feature was created at optimal
`correctness-ffi-abi` · **S** · `src/context.rs:254-256` (set at :107-108, created with optimal at :69-70); `README.md:87-88`
**Problem:** `DlssContext::new()` creates the NGX feature with `InWidth/InHeight = optimal` but stores only min/max; `render_resolution()` returns `min`. For every non-DLAA mode where optimal ≠ min, the host is told to render at min while NGX was created at optimal (e.g. ~800×450 vs ~1067×600 for Quality@1600×900), so the eval subrect/inputs disagree with the created feature → per-frame re-init / suboptimal reconstruction. The README tells callers to render at `render_resolution()`. This is the exact bug already fixed for RR in commit `3e79f06`, never applied to SR.
**Fix:** Mirror the RR fix — add an `optimal_render_resolution` field, store optimal in `new()`, return it from `render_resolution()`; keep `render_resolution_range()` as min..=max. Update the doc comment.

### H2 — CI builds against an unpinned NVIDIA/DLSS branch → non-reproducible, can break with no code change
`supply-chain-licensing` · **S** · `.github/workflows/ci.yml:34-35`; `build.rs:16,48-68`; `docs/SETUP.md:16`
**Problem:** CI runs `git clone --depth 1 https://github.com/NVIDIA/DLSS.git` with no branch/commit pin, so every run builds against whatever the default branch HEAD is. The crate is tightly coupled (DLSS 4 / SDK 310.x preset behavior, the whole NGX binding surface bindgen'd from these headers, hard-coded lib names/layout). An upstream SDK bump can break CI on an unrelated PR or silently alter the FFI ABI, with no record of which SDK commit was used — defeating the committed-`Cargo.lock` reproducibility rationale.
**Fix:** Pin the clone to a tag/commit (`--branch v310.x` or `clone` then `checkout <sha>`); record the pinned SDK version in docs the way wgpu / SL 2.11.1 are; re-pin deliberately on NGX bumps.

---

## Medium

### M1 — SR/RR `Drop` impls use `Mutex::lock().unwrap()` and can double-panic (process abort)
`correctness-ffi-abi` · **S** · `src/context.rs:268`; `src/ray_reconstruction.rs:342`
**Problem:** Both `Drop`s do `self.sdk.lock().unwrap()` on the same mutex locked during NGX FFI in `new()/render()`. If any FFI/HAL call panics while the lock is held, the mutex is poisoned; a subsequent context `Drop` then panics on `.unwrap()`, and during an unwind that's a double-panic → immediate `abort`. Violates the "never panic across FFI in Drop" rule.
**Fix:** `self.sdk.lock().unwrap_or_else(|e| e.into_inner())` in both Drops (poison-tolerant); a poisoned lock doesn't invalidate the parameter pointer, so `ReleaseFeature` still runs. Optionally apply to the `render()`/`new()` locks too.

### M2 — `Preferences` (the struct passed to `slInit`) has no size/offset const-assert, unlike every other ABI struct
`correctness-ffi-abi` · **S** · `src/streamline/types.rs:537-556` vs the assert block at `:912-934`
**Problem:** The const-assert block pins `size_of`/`offset_of` for every ABI struct except `Preferences` — the first and most ABI-sensitive struct passed to `slInit` (mixes a 1-byte bool, several u32 enums, a u64, and six pointers, exactly where a transcription slip silently corrupts later fields). Expected MSVC size is 144 bytes; nothing catches a regression.
**Fix:** Add `assert!(size_of::<Preferences>() == 144)` plus `offset_of` asserts at the pointer/scalar boundary, verified against MSVC (`paths_to_plugins = 40`, `flags = 88`, `render_api = 136`).

### M3 — `render_resolution()` means different things on SR (min) vs RR (optimal) — same name, divergent contract
`architecture-modularity` · **S** · `src/context.rs:254-256` vs `src/ray_reconstruction.rs:329-331`; `examples/sr_rr_toggle.rs:128-129`
**Problem:** Both contexts expose an identically-named `render_resolution()`, but SR returns min and RR returns optimal. The advertised SR↔RR toggle sizes inputs from the divergent results and silently gets a different resolution policy from the same method name. (Same root cause as H1, tracked from the API-consistency angle.)
**Fix:** Unify the contract (both return optimal, as in H1) or rename to make the difference explicit and document it on both; add a doc-test/assertion pinning the chosen semantics.

### M4 — `DlssRayReconstructionParameters` has no `validate()`; RR hands possibly-null `ID3D12Resource` pointers to NGX
`architecture-modularity` · **S** · `src/ray_reconstruction.rs:238-302` vs `src/render_parameters.rs:62-75`
**Problem:** SR's `render()` calls `validate()` to reject null required resources before NGX evaluate; the RR path has no equivalent and writes `color.raw()/depth.raw()/…` straight into the eval struct. RR has *more* required inputs, so the gap is wider — a copy-paste asymmetry.
**Fix:** Give `DlssRayReconstructionParameters` a `validate()` mirroring SR's (null-check all required inputs; `roughness` only when `Unpacked`) and call it at the top of `render()`. Better: fold the null-check into the shared `evaluate()` helper (M6) so neither twin can omit it.

### M5 — `DlssContext` and `DlssRayReconstructionContext` are near-copies; ~150 lines duplicated with no shared core
`architecture-modularity` · **M** · `src/context.rs:19-281` vs `src/ray_reconstruction.rs:98-354`
**Problem:** The two contexts are the same wrapper with NGX call names + param structs swapped; the optimal-settings/DLAA logic, the create tail, the dual-encoder render path, the four getters, the `Drop` body, and the two `unsafe impl Send+Sync` blocks are byte-for-byte identical. Every fix to encoder ordering, mutex discipline, Drop, or the Send/Sync argument must be made twice — and they've already drifted (H1/M3, M4).
**Fix:** Extract shared mechanics onto an inner `NgxFeature { feature, sdk, device, resolutions }` both contexts embed: a feature-create helper, an `evaluate(barrier_list, |cmd_list| …)` helper (fold M4's null-check here), and the shared getters/Drop/Send-Sync. Keep the public types distinct.

### M6 — Reproducibility hinges on a single personal wgpu fork with no mirror/vendor fallback
`supply-chain-licensing` · **M** · `Cargo.toml:19,65`; `Cargo.lock:2160-2264`
**Problem:** Every build fetches seven workspace crates from `github.com/jtbirdsell/wgpu@d81d755` — a personal account, not an org or vendored copy. If the repo is deleted/renamed/made private or the rev is GC'd, all builds break irrecoverably (the lock pins the rev but not the source). No `[source]` mirror, no `cargo vendor` snapshot, and `patches/` carries only patch #1 (not the Streamline factory-upgrade), so the fork can't be reconstructed for FG.
**Fix:** Commit both patches under `patches/`; `cargo vendor` the wgpu crates or mirror the fork under a durable/org account; protect the pinned rev (tag/protected branch). At minimum document the recovery procedure.

### M7 — No `cargo-audit` / `cargo-deny`, `fmt --check`, or `cargo doc` gate in CI
`supply-chain-licensing` · **M** · `.github/workflows/ci.yml` (whole pipeline); no `deny.toml`/`rustfmt.toml`
**Problem:** CI runs only check/clippy/build/test — no RUSTSEC scan, no license/banned-source policy, no `fmt --check`, no doc-warning gate (broken intra-doc links ship silently in a doc-heavy crate aiming for docs.rs). For a heavy-unsafe FFI crate pulling from a non-crates.io git source, `cargo-deny`'s `[sources]` allowlist would document/enforce that the only permitted git source is the pinned fork.
**Fix:** Add `cargo fmt --all --check`; `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`; and `cargo deny check` with a committed `deny.toml` (allowlist the `jtbirdsell/wgpu` source + the MIT/Apache/Unicode-3.0 licenses; fold `audit` in via `advisories`). The audit/deny leg can run on a cheap Linux job.

### M8 — Only the interposer is signature-verified; its sibling plugins + the dxgi/d3d12 shims load unchecked
`security-unsafe` · **L** · `src/streamline/ffi.rs:189-207`; `src/streamline/security.rs:1-31`
**Problem:** The gate verifies exactly one file (the interposer); `Library::new` then loads it with the default search order, pulling in `sl.common/dlss_g/reflex/pcl.dll` and the `dxgi.dll`/`d3d12.dll` shims next to the exe, none Authenticode-checked. The exe dir is searched first, so an attacker who can drop a malicious sibling next to the exe gets code execution despite a pristine interposer — the module doc overclaims the surface is closed. (Mitigated: NVIDIA's interposer self-validates its SL plugins, and exploitation needs write access to the ACL-protected exe/SDK dir.)
**Fix:** Either verify the whole DLL set before `slInit`, or constrain plugin discovery to the trusted SDK bin dir via `AddDllDirectory`/`SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_*)` while keeping the exe dir out of the shim search. At minimum, downgrade the doc claim and require the SDK/exe dirs to be on ACL-restricted storage.

### M9 — SR/RR context-creation edge cases (DLAA override, optimal-settings, RR preset pinning) are reachable only on hardware
`test-coverage` · **M** · `src/context.rs:61-65`; `src/ray_reconstruction.rs:151-155,183-195`
**Problem:** The DLAA branch (override optimal/min/max to upscaled), the RR DLSS-4 render-preset loop, and the RR `render_resolution()`-returns-optimal contract have zero automated coverage off an RTX GPU — and the n=1 ignored tests only ever pass `Quality`. No seam exists (NGX calls are made directly inside `new()`), so even a mock can't reach them.
**Fix:** (1) Cheap — add a DLAA case to the hardware tests asserting `render_resolution() == upscaled_resolution`. (2) Structural — extract the pure resolution-decision logic (DLAA override + optimal-vs-min) into a free function over the raw NGX-returned optimal/min/max and unit-test it without a device.

### M10 — Manual exposure + bias-mask render path is exercised by no test at all
`test-coverage` · **M** · `src/context.rs:134-145,166-169`; `src/render_parameters.rs:79-105`
**Problem:** The only `render()` call sites (the ignored hardware tests) always pass `Automatic` exposure and `bias: None`. Nothing ever takes the `Manual` branch (wires `pInExposureTexture`/scales) or the bias branch, including the corresponding `barrier_list()` arms — a regression dropping the manual-exposure or bias barrier (a real corruption risk on hardware) would pass CI silently.
**Fix:** Add a pure-logic test over `barrier_list()` asserting the transition set differs for Automatic vs Manual and that `Some(bias)` adds exactly one transition. Optionally extend a hardware test to drive Manual + a 1×1 exposure texture + a bias mask once.

### M11 — README usage snippet passes `partial_texture_size: None`, mismatching the rendered subrect
`docs-api-ergonomics` · **S** · `README.md:95-110`; cross-ref `src/context.rs:130-132`
**Problem:** Every example passes `Some(render_resolution)` and documents that pinning the subrect is load-bearing, but the README's headline Usage block passes `None`, which defaults to `max_render_resolution`. Combined with rendering at `render_resolution()` (currently min for SR), the README's own snippet tells NGX the eval subrect is max while inputs were produced smaller — exactly the mismatch the examples warn yields `InvalidParameters`. The primary onboarding example models the wrong usage.
**Fix:** Change the README snippet to `Some(render_resolution)` + a one-line note that the subrect must match the rendered size; document the `None`-means-max default on the field.

---

## Low

### L1 — SETUP.md §1 documents the wrong import-lib directory vs `build.rs`
`supply-chain-licensing` · **S** · `docs/SETUP.md:20` vs `build.rs:16`
**Problem:** SETUP.md says import libs live under `lib/Windows_x86_64/`, but `build.rs` links from `lib/Windows_x86_64/x64`. The first thing a new builder reads points at the wrong place. **Fix:** Correct SETUP.md to `…/x64/`.

### L2 — `as_perf_quality_value` multiplies `width*height` as u32, panicking in debug for pathological resolutions
`correctness-ffi-abi` · **S** · `src/nvsdk_ngx.rs:34-35`
**Problem:** `(x * y) as f32` multiplies in u32 before the cast; an oversized `UVec2` (>4.29B px, ~87k×49k — unreachable by real displays) overflows/panics or mis-selects the Auto tier. Marginal robustness nit. **Fix:** Compute in u64 (or f32) before dividing by 1e6.

### L3 — `DlssFeatureFlags::as_flags()` (strips the synthetic `OutputSubrect` bit) has no unit test
`test-coverage` · **S** · `src/nvsdk_ngx.rs:79-85,74-75`
**Problem:** `OutputSubrect = 256` is a crate-invented bit that must be stripped before NGX; if `as_flags()` stopped stripping it NGX would get an undefined flag, but nothing guards it. **Fix:** Unit-test that `(AutoExposure | OutputSubrect).as_flags() == AutoExposure.as_flags()` and `.contains(OutputSubrect)` still holds.

### L4 — `dlssg_status::decode` multi-bit and unknown-status branches are untested
`test-coverage` · **S** · `src/streamline/types.rs:405-432`
**Problem:** `decode()` is tested only for OK and a single bit; the `" | "`-joined multi-bit path and the `<unknown status …>` fallback are untested — and this string is the main signal for diagnosing why FG silently isn't generating on hardware you may not have. **Fix:** Add a test for a two-bit value (joined tokens) and a high/undocumented bit (unknown form).

### L5 — `DlssSdk::new`/`Drop` + NGX init success path proven on exactly one GPU; no CI smoke for the Send/Sync + Mutex contract
`test-coverage` · **M** · `src/sdk.rs:31-72,82-93`; `tests/headless.rs:22-81`
**Problem:** Everything past `DlssSdk::new` returning Ok (capability probe, the unsupported-but-initialized branch, the drop ordering) is exercised only by the n=1 ignored tests; the documented drop order and the Send/Sync justification have no test that even compiles a multi-context teardown. **Fix:** Add a hardware test that creates + drops two SR contexts (and an RR context) before the SDK, asserting no panic; factor the capability probe so the `supported==0` branch is unit-assertable.

### L6 — FFI happy-path symbol resolution is proven only on hardware, with no fault-injection seam
`test-coverage` · **M** · `src/streamline/ffi.rs:155-170,189-246,254-277`
**Problem:** CI ffi tests cover only the two pre-load failures; `resolve::<T>` (libloading + `transmute_copy`), `feature_fn` null handling, and `resolve_feature_functions` are reachable only by loading a real signed interposer. `MissingExport` and the feature-fn null-but-Ok branch have no automated coverage. **Fix:** Add a tiny test-only stub DLL exporting a couple of no-op `sl*` symbols so `resolve` succeeds and a missing symbol exercises `MissingExport`; at minimum assert `resolve` maps a known-absent symbol against any small system DLL.

### L7 — `build.rs` CRT-selection and bindgen-blocklist logic has no test and no negative-path guard
`test-coverage` · **M** · `build.rs:18-30,55-64`
**Problem:** The `crt-static → nvsdk_ngx_s` selection (opaque LNK2038 on mismatch) and the four bindgen blocklist patterns are easy to silently break; CI builds only the dynamic CRT, and nothing asserts the blocklist excluded D3D11/CUDA symbols. **Fix:** Add a CI leg building with `+crt-static`; optionally an integration test scanning `$OUT_DIR/bindings.rs` to assert no D3D11/Cuda symbol leaked.

### L8 — CI never builds the default (SR-only) or `--no-default-features` matrix; `debug_overlay` is unbuilt
`devops-build-ci-release` · **M** · `.github/workflows/ci.yml:37-96`
**Problem:** Every invocation passes `--features ray-reconstruction`/`frame-generation`; the default SR-only config (what a plain consumer gets), `--no-default-features`, and `debug_overlay` are never checked. **Fix:** Add those configurations to build/clippy via a small `strategy.matrix` over feature sets.

### L9 — docs.rs build will fail: no Windows target metadata, and `build.rs` hard-requires `DLSS_SDK`
`docs-api-ergonomics` · **M** · `Cargo.toml:1-9` (no `[package.metadata.docs.rs]`); `build.rs:5`; `src/lib.rs:16-19`
**Problem:** docs.rs builds on Linux; the crate `compile_error!`s off-Windows and `build.rs` `expect`s `DLSS_SDK` + needs libclang/NGX, so the build script panics before compilation. No `[package.metadata.docs.rs]` and no `cfg(docsrs)` shim → the future docs.rs page is a build-failure stub. (Forward-looking — can't publish yet anyway, see L14.) **Fix:** Add `[package.metadata.docs.rs]` (Windows default-target, feature selection, `--cfg docsrs`); make `build.rs` a no-op when `DOCS_RS` is set (stub `bindings.rs`); add `#![cfg_attr(docsrs, feature(doc_cfg))]`.

### L10 — TOCTOU window between `WinVerifyTrust` and `LoadLibrary` on the same path
`security-unsafe` · **M** · `src/streamline/ffi.rs:195-207`
**Problem:** The file is verified by path, then re-opened by name and mapped with no handle held across the check; an attacker-writable SDK bin dir could swap in an unsigned DLL between verify and load. Low — that precondition already enables the strictly-easier unverified-sibling-plugin attack (M8). **Fix:** Open once with deny-write/delete sharing, pass that handle to `WinVerifyTrust` (`WINTRUST_FILE_INFO.hFile`), and keep it open across `LoadLibrary`; or document the ACL-locked-storage requirement.

### L11 — Two command encoders created and submitted per SR/RR evaluate
`performance-resources` · **M** · `src/context.rs:198-227`; `src/ray_reconstruction.rs:272-300`
**Problem:** Each `render()` creates two fresh encoders (barrier + eval) per frame; encoder creation allocates a D3D12 allocator/list from wgpu's pool each frame. The split is required by wgpu 29, so it can't be collapsed today. **Fix:** Largely dictated by wgpu-29; if profiling shows it's hot, fold the barrier transitions into the caller's scene encoder (eval encoder must stay separate). At minimum document the accepted cost.

### L12 — SDK Mutex held across the entire NGX evaluate (CPU record), not just the NGX calls
`performance-resources` · **M** · `src/context.rs:128-228`; `src/ray_reconstruction.rs:243-301`
**Problem:** `render()` holds the SDK lock across `validate()`, eval-params build, both encoder creations, `transition_resources`, the NGX record, and `queue.submit` — but only the NGX evaluate + parameters access need it. Multiple cameras sharing one `Arc<Mutex<DlssSdk>>` serialize all per-frame CPU recording. **Fix:** Narrow the critical section to the NGX calls; move encoder creation/transitions/submit outside the guard.

### L13 — `CMSG_SIGNER_INFO` is reinterpreted from a 1-byte-aligned `Vec<u8>`, violating its 8-byte alignment
`security-unsafe` · **S** · `src/streamline/security.rs:355-372`
**Problem:** `&*(buffer.as_ptr() as *const CMSG_SIGNER_INFO)` over a `vec![0u8; …]` (align 1) creates a reference to a possibly-misaligned address — instant UB on the abstract machine; works today only because the Windows allocator over-aligns. The SAFETY comment falsely calls it "aligned". **Fix:** `core::ptr::read_unaligned` the header into a local, or back the blob with an over-aligned allocation; fix the SAFETY comment.

### L14 — crates.io publish is impossible while wgpu is a git dependency (no guard, no status note)
`supply-chain-licensing` · **S** · `Cargo.toml:19,65`; `docs/upstream-pr-8888.md:37-38`
**Problem:** `cargo publish` categorically rejects git/path deps, so the stated publishing goal is currently impossible — but nothing in README/Cargo.toml surfaces it, and nothing guards a premature publish. The path off the fork is gated on `gfx-rs/wgpu#9613` (SR/RR accessor PR) and `#9614` (FG factory hook, a design issue). **Fix:** Add `publish = false` now (fails an accidental publish loudly); add a `cargo publish --dry-run` CI step (continue-on-error) surfacing remaining blockers; document the blocker; when #9613 lands, move SR/RR to released `wgpu = "=29.x"` and keep the fork only for FG via consumer `[patch.crates-io]`.

### L15 — `unsafe impl Sync for FrameGenerationContext` rests on a caller contract the type doesn't enforce
`security-unsafe` · **S** · `src/streamline/frame_gen.rs:599-609`
**Problem:** `Sync` (shared `&ctx` across threads) lets `&self` methods like `query_state`/`is_enabled` make concurrent FFI into Streamline's process-global state, asserted-but-unproven to be internally synchronized. `Sync` is unused in-crate, so dropping it is a sound tightening. (`Send` is well-justified.) **Fix:** Drop `unsafe impl Sync` unless concurrent shared-ref use is actually required and proven; if it stays, tighten the SAFETY comment to name which interposer entry points are documented thread-safe for concurrent `&`-access.

### L16 — `DlssError` and `StreamlineError` are wholly separate with no bridge; a combined SR+FG app juggles two Result types
`architecture-modularity` · **S** · `src/nvsdk_ngx.rs:88-155`; `src/streamline/types.rs:35-111`
**Problem:** Two top-level error enums with no bridge and no umbrella; the flagship `sr_plus_fg` example hand-rolls its own wrapper + two `From` impls to get one `?`-able error. **Fix:** Add a small additive umbrella `pub enum Error { Dlss(DlssError), Streamline(StreamlineError) }` with `#[from]` impls, re-exported from the crate root. Optionally split StreamlineError's setup vs runtime variants.

### L17 — Frame Generation reaches NGX/Streamline through a parallel ownership model that never touches `DlssSdk`
`architecture-modularity` · **S** · `src/sdk.rs:8-21` vs `src/streamline/frame_gen.rs:63-66,269-281`
**Problem:** `DlssSdk` is documented as the app-wide NGX object SR/RR serialize behind, but FG uses an entirely separate substrate (`Streamline` + `Box<dyn StreamlineApi>`) with no shared serialization — so an app running SR+FG concurrently has two uncoordinated paths into native NGX/driver state. Partly unavoidable (SL interposes the swapchain). **Fix:** Make the boundary explicit: document on both `DlssSdk` and `Streamline` that SR/RR and FG are independent NGX entry points with independent lifecycles + the required init/drop ordering; optionally a debug-time note when both are live.

### L18 — `#![deny(missing_docs)]` not set; some public items are undocumented
`docs-api-ergonomics` · **S** · `src/lib.rs:1-19`; `src/nvsdk_ngx.rs:89-151`; `src/render_parameters.rs:13`
**Problem:** No `missing_docs` guard; `DlssError` variants carry only `#[error(...)]` strings (no prose) and `DlssTexture.texture` is undocumented — both publicly re-exported. **Fix:** Add `#![deny(missing_docs)]` (or warn during transition) and fix the gaps; this also completes the future docs.rs page.

### L19 — Fork/upstreaming docs are stale: #8888 framed as the live tracker; links/filenames say 8888 not 9613/9614
`docs-api-ergonomics` · **S** · `README.md:127`; `docs/SETUP.md:45-49,101-102`; `patches/README.md:1,6`; `docs/upstream-pr-8888.md` (filename)
**Problem:** The real status (correct in the doc body + CHANGELOG) is patch 1 = PR #9613, patch 2 = design-issue #9614 (not upstreamable as-is). But README/SETUP/patches still present #8888 as the single live tracker, and SETUP §3 claims "a plain `wgpu = "29"` will suffice once the accessor lands" — wrong, FG still needs the factory hook. **Fix:** Rename to `docs/upstreaming.md`, update the README link, lead with #9613 (#8888 as origin), and correct the "plain wgpu will suffice" claim.

### L20 — `build.rs` uses bare `.unwrap()` on `OUT_DIR` and gives no actionable message for a missing include dir
`devops-build-ci-release` · **S** · `build.rs:9-29`
**Problem:** A wrong `DLSS_SDK` surfaces the header path as an opaque libclang "file not found" rather than the README-aligned message the `DLSS_SDK` check promises; `OUT_DIR.unwrap()` is a bare panic (a pure nit — Cargo always sets it). **Fix:** Assert `…/include/nvsdk_ngx.h` exists before bindgen with the same `DLSS_SDK` guidance; add an `.expect` message to `OUT_DIR`.

### L21 — No dependency/build caching: every CI run recompiles the full wgpu+windows+bindgen tree
`devops-build-ci-release` · **S** · `.github/workflows/ci.yml:16-96`
**Problem:** No `Swatinem/rust-cache`/`actions/cache`; the heavy git-wgpu + windows + bindgen graph recompiles cold on every push/PR across many cargo invocations, costing minutes per run on Windows. **Fix:** Add `Swatinem/rust-cache@v2` after the toolchain install; optionally cache the choco LLVM install.

### L22 — CI uses a floating toolchain and the crate declares no MSRV / `rust-toolchain.toml` / `rustfmt.toml` / `.editorconfig`
`devops-build-ci-release` · **S** · `.github/workflows/ci.yml:20-23`; `Cargo.toml` (no `rust-version`, `edition="2024"`); repo root
**Problem:** `@stable` floats each run; `edition="2024"` needs Rust ≥ 1.85 but there's no `rust-version`/MSRV, so old-toolchain consumers get a late confusing error and `-D warnings` clippy can turn CI red on an unrelated PR via a new lint. No `rustfmt.toml`/`.editorconfig` either. **Fix:** Add `rust-version` + a CI job on that exact toolchain; commit `rust-toolchain.toml`, a minimal `rustfmt.toml` (+ `fmt --check`), and an `.editorconfig`.

### L23 — NVIDIA signer pin defaults to soft-pass on parse failure, weakening the "signed by NVIDIA" guarantee
`security-unsafe` · **S** · `src/streamline/security.rs:232-275,279-288`
**Problem:** A failure to extract the signer subject is a soft pass unless `STREAMLINE_REQUIRE_NVIDIA_SIGNER=1` (off by default), so the default posture is effectively "signed by anyone Windows trusts." Narrow: a parseable non-NVIDIA subject is always hard-rejected, so this only triggers when an embedded-signed PE passes `WinVerifyTrust` yet its PKCS#7 won't parse. **Fix:** Flip the default to fail-closed on parse failure with an env opt-out; at minimum document the default-posture gap prominently.

### L24 — `Frame::tag` builds five short-lived heap `Vec`s every frame
`performance-resources` · **S** · `src/streamline/frame_gen.rs:889-923`; `src/streamline/tagging.rs:97-114,137-150`
**Problem:** On the DLSS-G hot path each `tag()` allocates/frees up to five small `Vec`s per presented frame, though the count/order are statically bounded (≤ 4). Pure churn. **Fix:** Collect into stack-backed small buffers (`ArrayVec`/`SmallVec`<_, 4> or `[MaybeUninit; 4]` + len) for the resource/type pairs, `sl_resources`, and `tags`.

---

## Nit

### N1 — `DlssSdk::new` probes only SuperSampling availability, but is the shared SDK for RR and FG too
`architecture-modularity` · **S** · `src/sdk.rs:31-72`; `src/feature_info.rs:46`
**Problem:** `DlssSdk::new` hard-codes a SuperSampling-availability probe (and `FeatureDiscoveryInfo` pinned to SuperSampling), so a hypothetical RR-capable-but-SR-unavailable system would be rejected at construction; the SR probe is dead weight for RR-only callers. (The scenario doesn't exist on real NVIDIA hardware — RR implies SR.) **Fix:** Probe lazily per context (or accept a requested-feature arg) so the probe targets the right capability; at minimum document that `DlssSdk::new` currently requires SR support, and cfg-gate the SR probe behind `super-resolution`.

### N2 — `RoughnessMode`/`DepthType` live in `ray_reconstruction.rs` while other RR config lives in `nvsdk_ngx.rs`
`architecture-modularity` · **S** · `src/ray_reconstruction.rs:22-39` vs `src/nvsdk_ngx.rs:14-85`
**Problem:** Public construction knobs are split across two modules with no clear rule, and hand-authored enums share `nvsdk_ngx.rs` with the generated FFI, making the public API/re-export list read as ad hoc. Cosmetic. **Fix:** Group the hand-written config types (`DlssPerfQualityMode`, `DlssFeatureFlags`, `RoughnessMode`, `DepthType`) into a small `config` module; re-export from the root as today. Do it opportunistically alongside M5.

---

## Suggested sequencing

1. **Quick correctness/safety wins (all S):** H1, M1, M2, M4, L2, L13. These are small, high-confidence, and several are latent-UB / process-abort risks.
2. **DevOps hardening (mostly S):** H2, M7, L21, L22, L8, L14 (`publish = false`). Cheap, reduces non-reproducibility and supply-chain risk.
3. **Structural:** M5 (SR/RR shared core) — unblocks M3/M4 and stops future drift; M6 (fork SPOF); L9 (docs.rs) before any publish.
4. **Coverage + docs:** M9/M10/L3–L7 (tests), M11/L18/L19 (docs), L16/L17 (API ergonomics).
5. **Security depth (L):** M8, L10, L15, L23 — real hardening, larger effort or narrow exploitability.
