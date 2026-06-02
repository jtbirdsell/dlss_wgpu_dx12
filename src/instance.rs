//! Opt-in helper for building a DXC-configured [`wgpu::InstanceDescriptor`].
//!
//! ## DLSS does not use DXC
//!
//! This helper has **nothing to do with DLSS itself**. NVIDIA ships DLSS as a precompiled
//! native library (`nvngx_dlss.dll`, and `nvngx_dlssd.dll` for Ray Reconstruction); its shaders
//! are baked into those DLLs by NVIDIA and are never compiled by your application or by `wgpu`.
//! You can use every part of this crate with the default FXC compiler — or with no DXC at all.
//!
//! ## What this helper *is* for
//!
//! It is purely a convenience for the **host application's own HLSL pipelines**. If your
//! renderer authors compute or graphics shaders that require Shader Model 6.0 or newer
//! (wave intrinsics, 16-bit types, mesh/amplification shaders, ray tracing, etc.), the legacy
//! FXC compiler that `wgpu` defaults to cannot compile them — you need DXC. This function returns
//! an [`wgpu::InstanceDescriptor`] preconfigured for the DX12 backend with the dynamically-loaded
//! DXC compiler, so you can pass it straight to [`wgpu::Instance::new`].
//!
//! Using it is entirely **opt-in**: this crate never forces a shader compiler on you. If you do
//! not call this helper, your `wgpu::Instance` keeps whatever compiler you configured (FXC by
//! default), and DLSS still works.
//!
//! ## DLLs to ship
//!
//! The dynamic DXC path requires you to ship **`dxcompiler.dll`** next to your executable (or at
//! the path you pass to [`dxc_instance_descriptor_at`]). Pre-built binaries are available from the
//! [DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases);
//! `wgpu` 29 requires at least `v1.8.2502`.
//!
//! Older DXC toolchains also needed `dxil.dll` (the signing validator) alongside `dxcompiler.dll`.
//! The `wgpu` 29 [`Dx12Compiler::DynamicDxc`] variant takes **only** a `dxcompiler.dll` path and
//! does not reference `dxil.dll`, so `wgpu` does not require you to ship it. Shipping `dxil.dll`
//! anyway is still recommended in practice: without it, DXC produces *unsigned* DXIL, which some
//! drivers and tools (and the D3D12 debug layer) reject. Ship both `dxcompiler.dll` and `dxil.dll`
//! from a matching DXC release to be safe.

use wgpu::{Backends, BackendOptions, Dx12BackendOptions, Dx12Compiler, InstanceDescriptor};

/// Default file name of the dynamic DXC compiler DLL.
///
/// When you pass this (the default used by [`dxc_instance_descriptor`]), `wgpu` loads
/// `dxcompiler.dll` from the standard Windows DLL search path — typically the directory containing
/// your executable.
pub const DEFAULT_DXC_PATH: &str = "dxcompiler.dll";

/// Builds a [`wgpu::InstanceDescriptor`] that targets the **DX12** backend using the dynamically
/// loaded **DXC** shader compiler (`dxcompiler.dll` on the default DLL search path).
///
/// This is a convenience for host applications that author Shader Model 6+ HLSL (DLSS itself never
/// needs DXC). Pass the returned descriptor to [`wgpu::Instance::new`].
///
/// The descriptor restricts backends to [`Backends::DX12`] (this crate is DX12-only) and leaves all
/// other instance options at their `wgpu` defaults (instance flags, memory budget thresholds, no
/// display handle). If you need to customize those, build on top of
/// [`dxc_instance_descriptor_at`] or set the fields yourself.
///
/// Requires `dxcompiler.dll` to be shippable next to your executable — see the crate README for the
/// list of DLLs.
///
/// # Example
///
/// ```no_run
/// let descriptor = dlss_wgpu_dx12::dxc_instance_descriptor();
/// let instance = wgpu::Instance::new(descriptor);
/// ```
#[must_use]
pub fn dxc_instance_descriptor() -> InstanceDescriptor {
    dxc_instance_descriptor_at(DEFAULT_DXC_PATH)
}

/// Like [`dxc_instance_descriptor`], but lets you specify the path to `dxcompiler.dll`.
///
/// Use this when `dxcompiler.dll` does not live on the default Windows DLL search path — for
/// example when you ship it in a subdirectory. `dxc_path` may be a bare file name (resolved via the
/// DLL search path) or an absolute/relative path to the DLL.
///
/// # Example
///
/// ```no_run
/// let descriptor = dlss_wgpu_dx12::dxc_instance_descriptor_at("bin/dxcompiler.dll");
/// let instance = wgpu::Instance::new(descriptor);
/// ```
#[must_use]
pub fn dxc_instance_descriptor_at(dxc_path: impl Into<String>) -> InstanceDescriptor {
    // `InstanceDescriptor` does not implement `Default`; build from the canonical constructor so
    // that fields we don't touch (flags, memory budget, display handle) track wgpu's defaults.
    let mut descriptor = InstanceDescriptor::new_without_display_handle();
    descriptor.backends = Backends::DX12;
    descriptor.backend_options = BackendOptions {
        dx12: Dx12BackendOptions {
            // wgpu 29's `DynamicDxc` carries only the `dxcompiler.dll` path (a `String`); it has no
            // `dxil_path` or `max_shader_model` field. `dxil.dll`, if shipped, is picked up
            // implicitly by `dxcompiler.dll` for signing.
            shader_compiler: Dx12Compiler::DynamicDxc {
                dxc_path: dxc_path.into(),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    descriptor
}
