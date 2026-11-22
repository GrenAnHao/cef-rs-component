//! Windows D3D11 half of the shared-texture path.
//!
//! CEF hands us a shared `HANDLE` for the frame texture. We open it on our own
//! `ID3D11Device`, copy it into a `STAGING` texture and map that to read pixels
//! back. That is a real GPU→CPU copy, but it is *not* the same as the software
//! path: CEF never allocates or transfers a BGRA buffer, and the copy is a single
//! `CopyResource` the driver can schedule, not a per-frame `memcpy` from the
//! browser process.
//!
//! When gpui gains a Windows surface primitive this module's `open` becomes the
//! only piece needed for a genuinely zero-copy blit; `read_back` then disappears.

use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use super::shared_texture::{FrameFormat, copy_bgra_aperture, plane_size};

/// Describes the texture CEF produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureInfo {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
}

/// A D3D11 device owned by this process, used only to receive CEF's textures.
///
/// Kept separate from gpui's device on purpose: opening another process's shared
/// handle is only allowed for handles created with `SHARED_NTHANDLE`, and mixing
/// resource lifetimes with the renderer's device would couple shutdown order.
pub struct TextureReceiver {
    /// `ID3D11Device1` because `OpenSharedResource1` (NT handles, which is what
    /// CEF produces) lives on it, not on the base `ID3D11Device`.
    device: ID3D11Device1,
    base_device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Staging texture reused across frames. Creating one per paint allocated a
    /// multi-megabyte GPU resource 60 times a second; it only has to be rebuilt
    /// when the frame size or format changes.
    staging: Mutex<Option<(TextureInfo, ID3D11Texture2D)>>,
}

impl TextureInfo {
    /// Identity used to decide whether a cached staging texture still fits.
    fn matches(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height && self.format == other.format
    }
}

impl TextureReceiver {
    /// Creates the receiving device. Returns `None` when D3D11 is unavailable, in
    /// which case the caller must stay on the software OSR path.
    pub fn new() -> Option<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let feature_levels = [
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1,
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0,
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1,
        ];
        // SAFETY: plain COM creation; BGRA_SUPPORT is required because CEF
        // shares B8G8R8A8 textures.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if result.is_err() {
            return None;
        }
        let base_device = device?;
        let context = context?;
        let device: ID3D11Device1 = base_device.cast().ok()?;
        Some(Self {
            device,
            base_device,
            context,
            staging: Mutex::new(None),
        })
    }

    /// Opens the handle CEF reported.
    ///
    /// Uses `OpenSharedResource1`, which takes an NT handle name/handle pair and
    /// is what a producer created with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`
    /// requires — CEF sets that flag for accelerated OSR. The older
    /// `OpenSharedResource` only accepts legacy shared handles and fails here.
    ///
    /// A failure is normal (adapter or WARP mismatch), so this returns `None` and
    /// the caller falls back to the software path.
    pub fn open(&self, handle: *mut core::ffi::c_void) -> Option<ID3D11Texture2D> {
        // CEF types the field as a raw pointer; D3D11 wants an opaque HANDLE.
        let handle = HANDLE(handle);
        if handle.is_invalid() {
            return None;
        }
        let texture: ID3D11Texture2D = unsafe { self.device.OpenSharedResource1(handle) }.ok()?;
        Some(texture)
    }

    /// Copies the shared texture into a staging texture and returns tightly packed
    /// BGRA bytes ready for `RenderImage`.
    ///
    /// `dst` must be `width * height * 4` bytes; returns `false` on any mismatch
    /// so a paint callback never panics.
    /// Copies the shared texture into our own staging texture.
    ///
    /// Must happen inside `OnAcceleratedPaint`: CEF returns the handle to its
    /// pool when the callback returns, so the texture cannot be kept. Doing the
    /// copy here (rather than the map) keeps the callback short.
    pub fn copy_to_staging(&self, texture: &ID3D11Texture2D, info: TextureInfo) -> bool {
        let Ok(mut cached) = self.staging.lock() else {
            return false;
        };
        if !self.ensure_staging(&mut cached, info) {
            return false;
        }
        let Some((_, staging)) = cached.as_ref() else {
            return false;
        };
        unsafe { self.context.CopyResource(staging, texture) };
        true
    }

    /// Maps the staging texture and copies it into `dst`.
    ///
    /// This is the blocking half (it waits for the GPU), so callers run it after
    /// CEF's message dispatch has finished instead of inside the paint callback.
    pub fn read_staging(&self, info: TextureInfo, dst: &mut [u8]) -> bool {
        let Ok(mut cached) = self.staging.lock() else {
            return false;
        };
        if !self.ensure_staging(&mut cached, info) {
            return false;
        }
        let Some((_, staging)) = cached.as_ref() else {
            return false;
        };
        self.map_staging(staging, info, dst)
    }

    fn ensure_staging(
        &self,
        cached: &mut Option<(TextureInfo, ID3D11Texture2D)>,
        info: TextureInfo,
    ) -> bool {
        let need_new = match cached.as_ref() {
            Some((cached_info, _)) => !cached_info.matches(&info),
            None => true,
        };
        if !need_new {
            return true;
        }
        let Some(desc) = self.describe_staging(info) else {
            return false;
        };
        let mut created: Option<ID3D11Texture2D> = None;
        if unsafe {
            self.base_device
                .CreateTexture2D(&desc, None, Some(&mut created))
        }
        .is_err()
        {
            return false;
        }
        let Some(created) = created else { return false };
        *cached = Some((info, created));
        true
    }

    /// One-shot convenience: copy then read.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn read_back(&self, texture: &ID3D11Texture2D, info: TextureInfo, dst: &mut [u8]) -> bool {
        self.copy_to_staging(texture, info) && self.read_staging(info, dst)
    }

    fn map_staging(&self, staging: &ID3D11Texture2D, info: TextureInfo, dst: &mut [u8]) -> bool {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging textures are created with CPU_ACCESS_READ and cannot be
        // bound to the pipeline, which is what MAP_READ requires.
        // `D3D11_MAP_READ` blocks until the GPU has finished writing the shared
        // texture: measured 1.7-2.1 ms per frame on the pump thread.
        //
        // `D3D11_MAP_FLAG_DO_NOT_WAIT` was tried and is unusable here: it returns
        // `DXGI_ERROR_WAS_STILL_DRAWING` for every frame of a cross-process shared
        // texture (measured 0 frames delivered over 6 s), because the copy is
        // never complete by the time the immediate context is queried. Dropping
        // frames would blank the view, so the copy stays synchronous.
        if unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .is_err()
        {
            return false;
        }

        let pitch = mapped.RowPitch;
        let Some(needed) = plane_size(info.width, info.height, pitch, info.format) else {
            unsafe { self.context.Unmap(staging, 0) };
            return false;
        };
        let (Ok(width), Ok(height)) = (i32::try_from(info.width), i32::try_from(info.height))
        else {
            unsafe { self.context.Unmap(staging, 0) };
            return false;
        };
        if mapped.pData.is_null() {
            unsafe { self.context.Unmap(staging, 0) };
            return false;
        }
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, needed) };
        // The pre-reserved destination is the only CPU allocation: RGBA is
        // converted per row, without the legacy unbudgeted packed Vec.
        let ok = copy_bgra_aperture(
            src,
            pitch,
            info.width,
            info.height,
            (0, 0, width, height),
            info.format,
            dst,
        );

        unsafe { self.context.Unmap(staging, 0) };
        ok
    }

    fn describe_staging(&self, info: TextureInfo) -> Option<D3D11_TEXTURE2D_DESC> {
        if info.width == 0 || info.height == 0 {
            return None;
        }
        let format = match info.format {
            FrameFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            FrameFormat::Rgba8 => DXGI_FORMAT_R8G8B8A8_UNORM,
        };
        Some(D3D11_TEXTURE2D_DESC {
            Width: info.width,
            Height: info.height,
            MipLevels: 1,
            ArraySize: 1,
            // Staging must match the source's format and sample count.
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        })
    }
}

/// Keeps the constants referenced so their intent is visible even when the
/// description above is tuned later.
#[allow(dead_code)]
const SHARED_FLAGS: (u32, u32) = (
    D3D11_RESOURCE_MISC_SHARED.0 as u32,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32,
);
#[allow(dead_code)]
const UNUSED_BIND: u32 = D3D11_BIND_SHADER_RESOURCE.0 as u32;
#[allow(dead_code)]
fn _fmt_is_dxgi_format(f: DXGI_FORMAT) -> DXGI_FORMAT {
    f
}
