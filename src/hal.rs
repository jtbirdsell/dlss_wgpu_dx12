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
