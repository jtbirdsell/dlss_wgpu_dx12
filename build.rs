use std::{env, path::PathBuf};

fn main() {
    // The NVIDIA DLSS SDK (a clone of github.com/NVIDIA/DLSS) supplies the NGX headers + import libs.
    let dlss_sdk = env::var("DLSS_SDK").expect(
        "DLSS_SDK environment variable not set. Point it at a clone of github.com/NVIDIA/DLSS \
         (so that $DLSS_SDK/include/nvsdk_ngx.h exists). See the crate README.",
    );
    let out_dir =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set; build.rs must be run by Cargo"));
    let include = format!("{dlss_sdk}/include");
    // Fail early with the same actionable DLSS_SDK guidance if the headers are missing, rather than
    // letting bindgen surface an opaque libclang "file not found" further down.
    assert!(
        PathBuf::from(&include).join("nvsdk_ngx.h").exists(),
        "NGX headers not found at {include}/nvsdk_ngx.h. Point DLSS_SDK at a complete NVIDIA/DLSS \
         clone (so that $DLSS_SDK/include/nvsdk_ngx.h exists). See the crate README.",
    );

    // --- Link the NGX import library --------------------------------------------------------
    // One library serves all graphics backends. Selection is by CRT linkage:
    //   nvsdk_ngx_d.lib -> dynamic CRT (/MD, the default), nvsdk_ngx_s.lib -> static CRT (/MT).
    // (debug_overlay only swaps the *runtime* DLL search path, not the linked lib — see feature_info.rs.)
    let lib_dir = format!("{dlss_sdk}/lib/Windows_x86_64/x64");
    println!("cargo:rustc-link-search=native={lib_dir}");
    let crt_static = env::var("CARGO_CFG_TARGET_FEATURE")
        .map(|f| f.split(',').any(|x| x == "crt-static"))
        .unwrap_or(false);
    // nvsdk_ngx_d.lib expects the dynamic CRT (/MD, the default); nvsdk_ngx_s.lib the static CRT
    // (/MT). A mismatch is an opaque LNK2038, so select by the crt-static target feature.
    let ngx_lib = if crt_static { "nvsdk_ngx_s" } else { "nvsdk_ngx_d" };
    let ngx_lib_path = PathBuf::from(&lib_dir).join(format!("{ngx_lib}.lib"));
    assert!(
        ngx_lib_path.exists(),
        "NGX import library not found at {}. Ensure DLSS_SDK points at a complete NVIDIA/DLSS clone.",
        ngx_lib_path.display()
    );
    println!("cargo:rustc-link-lib=static={ngx_lib}");

    // nvsdk_ngx's Windows static lib references system APIs (registry lookups for the driver/NGX
    // paths, COM, shell, version info). rlibs aren't linked, so these only surface when an
    // executable (test/example/bin) links the crate. Link them explicitly — e.g. NGX's
    // RegOpenKeyExW/RegCloseKey/RegQueryValueExW resolve from advapi32.
    for system_lib in ["advapi32", "user32", "shell32", "ole32", "oleaut32", "version"] {
        println!("cargo:rustc-link-lib=dylib={system_lib}");
    }

    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-env-changed=DLSS_SDK");

    // --- Generate Rust bindings for the NGX headers -----------------------------------------
    // The NGX headers forward-declare the D3D12/DXGI COM interfaces as opaque structs and the
    // inline helpers call no COM methods, so libclang needs ONLY the DLSS include dir — no
    // Windows SDK / d3d12.h. We let the opaque COM types through (bindgen emits zero-sized
    // structs) and convert windows-rs handles to those pointer types via `.as_raw()` at call sites.
    bindgen::Builder::default()
        .header(format!("{}/src/wrapper.h", env!("CARGO_MANIFEST_DIR")))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // The create/evaluate/get-optimal-settings helpers are `static inline`; emit C thunks.
        .wrap_static_fns(true)
        .wrap_static_fns_path(out_dir.join("wrap_static_fns"))
        .clang_arg(format!("-I{include}"))
        .allowlist_item(".*NGX.*")
        // We target D3D12 + Vulkan-free; drop the D3D11 and CUDA helper surfaces entirely so we
        // never need <d3d11.h>/<cuda.h> at thunk-compile time.
        .blocklist_item(".*D3D11.*")
        // NGX spells the per-API parameter setters with a lowercase 'd' (SetD3d11Resource), so the
        // uppercase D3D11 filter above misses the PFN_*_SetD3d11Resource typedefs. Drop them too —
        // without catching the D3D12 setters (D3d12), which we need.
        .blocklist_item(".*D3d11.*")
        .blocklist_item(".*Cuda.*")
        .blocklist_item(".*CUDA.*")
        .generate()
        .expect("Failed to generate NGX bindings")
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Failed to write bindings.rs");

    // --- Compile the generated thunks for the static-inline NGX helpers ----------------------
    cc::Build::new()
        .file(out_dir.join("wrap_static_fns.c"))
        .include(&include)
        // Match the CRT the NGX import lib expects so the thunk objects link cleanly.
        .static_crt(crt_static)
        .compile("wrap_static_fns");
}
