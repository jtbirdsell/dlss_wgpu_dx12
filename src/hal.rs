//! Extraction of raw D3D12 handles from `wgpu` objects via the HAL "escape hatch".
//!
//! wgpu 29's `Device`/`Texture::as_hal` return a guard (`Option<impl Deref>`), while
//! `CommandEncoder::as_hal_mut` takes a closure. We convert the `windows-rs` COM interfaces to
//! raw `*mut c_void` (via [`windows::core::Interface::as_raw`], a non-AddRef borrow) and cast them
//! to the opaque bindgen pointer types. The underlying COM objects are owned by the `wgpu`
//! objects, so the raw pointers stay valid as long as those objects live.
//!
//! `raw_device`/`raw_queue`/`raw_command_list` rely on accessors added to our vendored wgpu-hal
//! (gfx-rs/wgpu#8888); the rest use stock wgpu 29 accessors.

use crate::nvsdk_ngx::{ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource};
use wgpu::hal::api::Dx12;
use windows::core::Interface;

/// Runs `f` with the raw `ID3D12Device` of `device`'s Dx12 backend.
///
/// Returns `None` if `device` is not a Dx12 device.
///
/// # Safety
/// The raw pointer passed to `f` is valid only for the duration of the call.
pub(crate) unsafe fn with_raw_device<R>(
    device: &wgpu::Device,
    f: impl FnOnce(*mut ID3D12Device) -> R,
) -> Option<R> {
    let hal_device = unsafe { device.as_hal::<Dx12>() }?;
    // `raw_device()` is a safe accessor in stock wgpu 29 (dx12/device.rs).
    let raw = hal_device.raw_device();
    Some(f(raw.as_raw().cast()))
}

/// Returns the raw `ID3D12Resource` backing a wgpu texture, or `None` if it is not a Dx12 texture.
///
/// # Safety
/// The pointer is valid as long as `texture` is alive and undropped.
pub(crate) unsafe fn raw_resource(texture: &wgpu::Texture) -> Option<*mut ID3D12Resource> {
    let hal_texture = unsafe { texture.as_hal::<Dx12>() }?;
    let raw = unsafe { hal_texture.raw_resource() };
    Some(raw.as_raw().cast())
}

/// Returns the DXGI adapter LUID (8 raw bytes, native endianness) of `adapter`'s Dx12 backend, or
/// `None` if it is not a Dx12 adapter.
///
/// `sl::AdapterInfo` wants the adapter's LUID as a raw byte blob; passing a real LUID is what lets
/// `slIsFeatureSupported` do the adapter-specific check (a null `AdapterInfo` is a C++ reference the
/// interposer dereferences, which crashes). We read it from the wgpu-hal dx12 `Adapter` via its
/// `raw_adapter()` accessor (`IDXGIAdapter3`) and `GetDesc().AdapterLuid` (a Win32 `LUID =
/// {u32, i32}`, 8 bytes).
///
/// Only compiled with the `frame-generation` feature (the FG context is the sole caller).
#[cfg(feature = "frame-generation")]
pub(crate) fn adapter_luid(adapter: &wgpu::Adapter) -> Option<[u8; 8]> {
    // SAFETY: `as_hal::<Dx12>` yields a guard borrowing the adapter's HAL state; we only read
    // through it in this scope. `GetDesc` is a const COM method.
    let hal_adapter = unsafe { adapter.as_hal::<Dx12>() }?;
    let dxgi_adapter = hal_adapter.raw_adapter();
    let desc = unsafe { dxgi_adapter.GetDesc() }.ok()?;
    let luid = desc.AdapterLuid;
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&luid.LowPart.to_ne_bytes());
    bytes[4..8].copy_from_slice(&luid.HighPart.to_ne_bytes());
    Some(bytes)
}

/// Calls `IDXGISwapChain3::GetCurrentBackBufferIndex()` on `surface`'s (Streamline-proxied) Dx12
/// swapchain, or returns `None` if `surface` is not a Dx12 surface or has not been configured yet.
///
/// This is **required every frame** by DLSS Frame Generation on D3D12. wgpu never calls it (it
/// tracks the acquired back-buffer index internally and presents a waitable swapchain), so without
/// this call DLSS-G reports `eFailGetCurrentBackBufferIndexNotCalled` and silently passes the real
/// frame straight through Present without generating. Because the wgpu fork upgraded its swapchain
/// to a Streamline proxy in `Instance::init` (rev `d81d755`), this query routes through SL's
/// `slHookGetCurrentBackBufferIndex` hook and is how SL learns the per-frame back-buffer cadence to
/// insert the generated frame into the present sequence. It is a read-only query and does not
/// perturb wgpu's own index tracking.
///
/// Only compiled with the `frame-generation` feature (it is the sole caller).
#[cfg(feature = "frame-generation")]
pub(crate) fn current_back_buffer_index(surface: &wgpu::Surface) -> Option<u32> {
    // SAFETY: `as_hal::<Dx12>` hands back a guard borrowing the surface's HAL state; we only read
    // through it within this scope. `GetCurrentBackBufferIndex` is a const COM method with no
    // arguments and no side effects on wgpu's tracking.
    let hal_surface = unsafe { surface.as_hal::<Dx12>() }?;
    let swap_chain = hal_surface.swap_chain()?;
    Some(unsafe { swap_chain.GetCurrentBackBufferIndex() })
}

/// Runs `f` with the raw recording `ID3D12GraphicsCommandList` of `encoder`'s Dx12 backend.
///
/// The list is only open (and the pointer valid) for the duration of `f` — i.e. before the encoder
/// is `finish`ed. Returns `None` if `encoder` is not a Dx12 encoder or has no open list.
///
/// # Safety
/// `f` must not close/reset the list or otherwise leave it in a state wgpu does not expect.
pub(crate) unsafe fn with_raw_command_list<R>(
    encoder: &mut wgpu::CommandEncoder,
    f: impl FnOnce(*mut ID3D12GraphicsCommandList) -> R,
) -> Option<R> {
    unsafe {
        encoder.as_hal_mut::<Dx12, _, _>(|hal_encoder| {
            let list = hal_encoder?.raw_command_list()?;
            Some(f(list.as_raw().cast()))
        })
    }
}
