//! Windows D3D11 accelerated-capture primitive.
//!
//! CEF hands `OnAcceleratedPaint` a borrowed NT shared handle that is valid
//! only for the callback. This module opens it on a private receive device,
//! copies it into a client-owned STAGING texture, checks the device-removed
//! state, maps, and converts the visible aperture straight into the caller's
//! budget-owned slice. The imported texture is released before `copy` returns;
//! the borrowed handle is never closed and never cached.
//!
//! Ownership rules:
//! - every native call happens on the OS thread passed to [`D3d11Capture::new`]
//!   and inside a single `&mut self` section;
//! - no native handle, texture or mapped pointer leaves this module;
//! - the only output is compact premultiplied BGRA in the caller's slice;
//! - a failure never falls back to another backend or silently to WARP;
//! - unmapping is RAII: every successful `Map` is followed by exactly one
//!   `Unmap`, including injected/format/geometry failures;
//! - a non-owner `Drop` forgets native references instead of releasing them
//!   (the owner may still be inside a CEF callback); a normal owner `Drop`
//!   releases them synchronously.
//!
//! The parent capture endpoint additionally proves `currently_on(UI)`; this
//! primitive only checks the OS thread it was constructed on, so it can be
//! exercised by native fixtures without initialising CEF.

#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::slice;

use cef::AcceleratedPaintInfo;
use rust_cef_core::{
    frame_layout::{FrameFormat, copy_bgra_aperture},
    ports::{CaptureFailureKind, SurfaceKind},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HMODULE},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_11_0,
            },
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
                ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SAMPLE_DESC,
                },
                DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
                DXGI_ERROR_DRIVER_INTERNAL_ERROR, IDXGIAdapter,
            },
        },
    },
    core::{HRESULT, Interface},
};

use super::CaptureFault;

#[cfg(feature = "test-support")]
fn readback_adapter_luid(device: &ID3D11Device) -> Option<(i32, u32)> {
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    let dxgi = device.cast::<IDXGIDevice>().ok()?;
    // SAFETY: called on the receiver's creation thread on a live device.
    // Both temporary interfaces are released here, outside the registry lock.
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    let luid = (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart);
    (luid != (0, 0)).then_some(luid)
}

/// Only successful native stages are counted. `staging_bytes`/`staging_peak`
/// describe this receiver's own staging GPU capacity (two surface slots), not
/// the Browser CPU frame budget. `live_imports` is 1 only while a CEF texture
/// is open; it is back to 0 when `copy` returns, success or failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct D3dMetrics {
    pub(crate) opens: u64,
    pub(crate) copies: u64,
    pub(crate) maps: u64,
    pub(crate) unmaps: u64,
    pub(crate) staging_bytes: u64,
    pub(crate) staging_peak: u64,
    pub(crate) live_imports: u64,
}

// One-shot fault stages are plain codes so production builds carry no faulting
// API and no stage enum; only the cfg'd setter below can ever make them nonzero.
const FAULT_NONE: u8 = 0;
const FAULT_DEVICE_CREATION: u8 = 1;
const FAULT_OPEN: u8 = 2;
const FAULT_DESCRIBE: u8 = 3;
const FAULT_STAGING: u8 = 4;
const FAULT_COPY: u8 = 5;
const FAULT_MAP: u8 = 6;
const FAULT_AFTER_MAP: u8 = 7;
const FAULT_DEVICE_REMOVED: u8 = 8;

/// Test/probe-only, exactly one-shot for the next `copy`, plus construction.
/// Set through [`D3d11Capture::set_next_fault`] / [`D3d11Capture::new_with_fault`];
/// there is no environment or generic native-injection entry point.
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum D3dFaultPoint {
    DeviceCreation,
    Open,
    Describe,
    Staging,
    Copy,
    Map,
    AfterMap,
    DeviceRemoved,
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
fn fault_stage(point: D3dFaultPoint) -> u8 {
    match point {
        D3dFaultPoint::DeviceCreation => FAULT_DEVICE_CREATION,
        D3dFaultPoint::Open => FAULT_OPEN,
        D3dFaultPoint::Describe => FAULT_DESCRIBE,
        D3dFaultPoint::Staging => FAULT_STAGING,
        D3dFaultPoint::Copy => FAULT_COPY,
        D3dFaultPoint::Map => FAULT_MAP,
        D3dFaultPoint::AfterMap => FAULT_AFTER_MAP,
        D3dFaultPoint::DeviceRemoved => FAULT_DEVICE_REMOVED,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StagingKey {
    width: u32,
    height: u32,
    format: FrameFormat,
}

struct Staging {
    key: StagingKey,
    texture: ID3D11Texture2D,
    bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct FrameLayout {
    coded_width: u32,
    coded_height: u32,
    visible: (i32, i32, i32, i32),
    format: FrameFormat,
    dxgi: DXGI_FORMAT,
}

/// Per-Browser receive device. All fields are native COM objects: create, use
/// and normal-drop only on `owner`; the wrong-thread `Drop` forgets them.
pub(crate) struct D3d11Capture {
    owner: u64,
    device: Option<ID3D11Device>,
    device1: Option<ID3D11Device1>,
    context: Option<ID3D11DeviceContext>,
    /// One slot per `SurfaceKind`; rebuilt only when coded size/format changes.
    slots: [Option<Staging>; 2],
    metrics: D3dMetrics,
    #[cfg(feature = "test-support")]
    readback_adapter_luid: Option<(i32, u32)>,
    retired: bool,
    next_fault: u8,
}

impl D3d11Capture {
    /// Creates the hardware receive device. There is deliberately no WARP or
    /// software fallback: a `D3d11Required` endpoint fails instead.
    pub(crate) fn new(owner_thread: u64) -> Result<Self, CaptureFault> {
        Self::build(owner_thread)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub(crate) fn new_with_fault(
        owner_thread: u64,
        fault: Option<D3dFaultPoint>,
    ) -> Result<Self, CaptureFault> {
        let stage = fault.map_or(FAULT_NONE, fault_stage);
        if stage == FAULT_DEVICE_CREATION {
            if crate::executor::current_thread_id() != owner_thread {
                return Err(CaptureFault::new(CaptureFailureKind::WrongThread));
            }
            return Err(CaptureFault::new(CaptureFailureKind::DeviceCreation));
        }
        let mut capture = Self::build(owner_thread)?;
        capture.next_fault = stage;
        Ok(capture)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub(crate) fn set_next_fault(&mut self, fault: Option<D3dFaultPoint>) {
        self.next_fault = fault.map_or(FAULT_NONE, fault_stage);
    }

    pub(crate) fn metrics(&self) -> D3dMetrics {
        self.metrics
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn readback_adapter_luid(&self) -> Option<(i32, u32)> {
        self.readback_adapter_luid
    }

    /// Abnormal teardown: forget every native reference instead of releasing
    /// it, because an unfinished cohort may still be owned by CEF's pool.
    /// The parent uses this only when it cannot prove callback quiescence.
    #[allow(dead_code)]
    pub(crate) fn abandon(&mut self) {
        self.retired = true;
        self.forget_native();
    }

    fn build(owner_thread: u64) -> Result<Self, CaptureFault> {
        if crate::executor::current_thread_id() != owner_thread {
            return Err(CaptureFault::new(CaptureFailureKind::WrongThread));
        }
        let levels = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: plain hardware device creation; BGRA_SUPPORT is required for
        // CEF's shared B8G8R8A8 textures. No adapter/software module is passed.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(levels.as_slice()),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|error| native_fault(CaptureFailureKind::DeviceCreation, error.code()))?;
        let device = device.ok_or_else(|| CaptureFault::new(CaptureFailureKind::DeviceCreation))?;
        let context =
            context.ok_or_else(|| CaptureFault::new(CaptureFailureKind::DeviceCreation))?;
        // A device that was removed before first use must not be handed out.
        // SAFETY: both interfaces are live and were just created.
        unsafe { device.GetDeviceRemovedReason() }
            .map_err(|error| native_fault(CaptureFailureKind::DeviceCreation, error.code()))?;
        // SAFETY: QueryInterface on a live device; ID3D11Device1 is needed for
        // NT-handle `OpenSharedResource1`.
        let device1: ID3D11Device1 = device
            .cast()
            .map_err(|error| native_fault(CaptureFailureKind::DeviceCreation, error.code()))?;
        #[cfg(feature = "test-support")]
        let readback_adapter_luid = readback_adapter_luid(&device);
        Ok(Self {
            owner: owner_thread,
            device: Some(device),
            device1: Some(device1),
            context: Some(context),
            slots: [None, None],
            metrics: D3dMetrics::default(),
            #[cfg(feature = "test-support")]
            readback_adapter_luid,
            retired: false,
            next_fault: FAULT_NONE,
        })
    }

    /// Opens, copies, maps, converts and unmaps in one serial section.
    ///
    /// `dst` must be exactly `visible.width * visible.height * 4` bytes and is
    /// already budget-reserved by the caller: this method allocates no CPU
    /// pixels and calls no host callback.
    pub(crate) fn copy(
        &mut self,
        surface: SurfaceKind,
        info: &AcceleratedPaintInfo,
        dst: &mut [u8],
    ) -> Result<(), CaptureFault> {
        self.copy_with(surface, info, dst, None)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn copy_after_map_probe(
        &mut self,
        surface: SurfaceKind,
        info: &AcceleratedPaintInfo,
        dst: &mut [u8],
    ) -> Result<(), CaptureFault> {
        self.copy_with(surface, info, dst, Some(FAULT_AFTER_MAP))
    }

    fn copy_with(
        &mut self,
        surface: SurfaceKind,
        info: &AcceleratedPaintInfo,
        dst: &mut [u8],
        fault_override: Option<u8>,
    ) -> Result<(), CaptureFault> {
        if crate::executor::current_thread_id() != self.owner {
            return Err(CaptureFault::new(CaptureFailureKind::WrongThread));
        }
        if self.retired {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        }
        // Metadata validation precedes fault consumption: a malformed frame is
        // rejected on its own merits and does not swallow a pending injection
        // that the next well-formed callback still has to observe.
        let layout = layout_for(info, dst.len())?;
        let fault =
            fault_override.unwrap_or_else(|| std::mem::replace(&mut self.next_fault, FAULT_NONE));

        if fault == FAULT_DEVICE_CREATION {
            // Only reachable if a caller asks for this stage after construction;
            // there is no device to rebuild, so the stage stays an explicit
            // failure instead of being silently ignored.
            return Err(CaptureFault::new(CaptureFailureKind::DeviceCreation));
        }
        if fault == FAULT_OPEN {
            return Err(CaptureFault::new(CaptureFailureKind::OpenSharedResource));
        }
        let Some(device1) = self.device1.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        // SAFETY: the handle is CEF's borrowed NT handle for this callback and
        // is only opened for the duration of this copy. It is never closed or
        // cached; the imported COM texture is dropped before returning.
        let imported: ID3D11Texture2D =
            unsafe { device1.OpenSharedResource1(HANDLE(info.shared_texture_handle)) }.map_err(
                |error| native_fault(CaptureFailureKind::OpenSharedResource, error.code()),
            )?;
        self.metrics.opens += 1;
        self.metrics.live_imports += 1;

        let result = self.copy_imported(surface, &imported, &layout, dst, fault);

        drop(imported);
        self.metrics.live_imports = self.metrics.live_imports.saturating_sub(1);
        result
    }

    fn copy_imported(
        &mut self,
        surface: SurfaceKind,
        imported: &ID3D11Texture2D,
        layout: &FrameLayout,
        dst: &mut [u8],
        fault: u8,
    ) -> Result<(), CaptureFault> {
        if fault == FAULT_DESCRIBE {
            return Err(CaptureFault::new(CaptureFailureKind::TextureMismatch));
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a writable out-parameter of the right type.
        unsafe { imported.GetDesc(&mut desc) };
        if desc.Width != layout.coded_width
            || desc.Height != layout.coded_height
            || desc.Format != layout.dxgi
            || desc.MipLevels != 1
            || desc.ArraySize != 1
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
        {
            return Err(CaptureFault::new(CaptureFailureKind::TextureMismatch));
        }

        let slot = match surface {
            SurfaceKind::View => 0,
            SurfaceKind::Popup => 1,
        };
        self.ensure_staging(slot, layout, fault)?;
        let Some(staging) = self.slots[slot].as_ref().map(|staging| &staging.texture) else {
            return Err(CaptureFault::new(CaptureFailureKind::StagingCreation));
        };
        let Some(context) = self.context.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let Some(device) = self.device.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };

        if fault == FAULT_COPY {
            return Err(CaptureFault::new(CaptureFailureKind::CopyResource));
        }
        // SAFETY: both textures live here, have identical descs, and the
        // immediate context is only used on the owner thread.
        unsafe { context.CopyResource(staging, imported) };
        // CopyResource reports no HRESULT; device loss surfaces here.
        // SAFETY: live device on the owner thread.
        unsafe { device.GetDeviceRemovedReason() }
            .map_err(|error| native_fault(CaptureFailureKind::DeviceRemoved, error.code()))?;
        if fault == FAULT_DEVICE_REMOVED {
            return Err(CaptureFault::native(
                CaptureFailureKind::DeviceRemoved,
                DXGI_ERROR_DEVICE_REMOVED.0,
            ));
        }
        self.metrics.copies += 1;

        if fault == FAULT_MAP {
            return Err(CaptureFault::new(CaptureFailureKind::Map));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: STAGING with CPU_ACCESS_READ is what MAP_READ requires, and
        // `mapped` is a correctly typed out-parameter.
        unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .map_err(|error| native_fault(CaptureFailureKind::Map, error.code()))?;
        self.metrics.maps += 1;
        // Every path after a successful Map goes through this guard: the
        // injected AfterMap failure, pointer/pitch anomalies, aperture failure
        // and success all Unmap exactly once.
        let _unmap = MapGuard {
            context,
            staging,
            metrics: &mut self.metrics,
        };

        if fault == FAULT_AFTER_MAP {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        }
        if mapped.pData.is_null() {
            return Err(CaptureFault::new(CaptureFailureKind::Map));
        }
        let pitch = mapped.RowPitch;
        let Some(min_pitch) = layout.coded_width.checked_mul(4) else {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        };
        if pitch < min_pitch {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        }
        let Some(len) = (pitch as usize).checked_mul(layout.coded_height as usize) else {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        };
        if len > isize::MAX as usize {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        }
        // SAFETY: D3D11 guarantees a mapped staging subresource exposes
        // RowPitch * height bytes at pData; `len` is checked for slice bounds.
        let src = unsafe { slice::from_raw_parts(mapped.pData as *const u8, len) };
        if !copy_bgra_aperture(
            src,
            pitch,
            layout.coded_width,
            layout.coded_height,
            layout.visible,
            layout.format,
            dst,
        ) {
            return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
        }
        Ok(())
    }

    /// Reuses a staging texture only when the coded allocation identity matches;
    /// staging is never shared between surfaces. Old staging drops on the owner
    /// thread before the new one is recorded.
    fn ensure_staging(
        &mut self,
        slot: usize,
        layout: &FrameLayout,
        fault: u8,
    ) -> Result<(), CaptureFault> {
        if fault == FAULT_STAGING {
            return Err(CaptureFault::new(CaptureFailureKind::StagingCreation));
        }
        let key = StagingKey {
            width: layout.coded_width,
            height: layout.coded_height,
            format: layout.format,
        };
        if self.slots[slot]
            .as_ref()
            .is_some_and(|staging| staging.key == key)
        {
            return Ok(());
        }
        let Some(device) = self.device.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let desc = staging_desc(layout);
        let mut created: Option<ID3D11Texture2D> = None;
        // SAFETY: staging desc matches the source desc exactly; no initial data.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut created)) }
            .map_err(|error| native_fault(CaptureFailureKind::StagingCreation, error.code()))?;
        let Some(texture) = created else {
            return Err(CaptureFault::new(CaptureFailureKind::StagingCreation));
        };
        let bytes = u64::from(layout.coded_width) * u64::from(layout.coded_height) * 4;
        // Include the transient overlap: new texture allocation succeeds before
        // the old slot is dropped. This is logical owned capacity, not GPU RSS.
        self.metrics.staging_peak = self
            .metrics
            .staging_peak
            .max(self.metrics.staging_bytes.saturating_add(bytes));
        let old = self.slots[slot].replace(Staging {
            key,
            texture,
            bytes,
        });
        let previous = old.map_or(0, |staging| staging.bytes);
        self.metrics.staging_bytes = self
            .metrics
            .staging_bytes
            .saturating_sub(previous)
            .saturating_add(bytes);
        self.metrics.staging_peak = self.metrics.staging_peak.max(self.metrics.staging_bytes);
        Ok(())
    }

    fn forget_native(&mut self) {
        if let Some(device) = self.device.take() {
            std::mem::forget(device);
        }
        if let Some(device1) = self.device1.take() {
            std::mem::forget(device1);
        }
        if let Some(context) = self.context.take() {
            std::mem::forget(context);
        }
        for slot in &mut self.slots {
            if let Some(staging) = slot.take() {
                std::mem::forget(staging.texture);
            }
        }
        self.metrics.live_imports = 0;
    }
}

impl Drop for D3d11Capture {
    fn drop(&mut self) {
        self.retired = true;
        if crate::executor::current_thread_id() != self.owner {
            // Releasing a pooled CEF texture from an arbitrary thread is not
            // allowed, and a non-owner Drop also cannot prove the CEF callback
            // finished. Forget every native reference rather than Release.
            self.forget_native();
        }
    }
}

struct MapGuard<'a> {
    context: &'a ID3D11DeviceContext,
    staging: &'a ID3D11Texture2D,
    metrics: &'a mut D3dMetrics,
}

impl Drop for MapGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: pairs with the successful Map; same staging/context, owner
        // thread, subresource 0.
        unsafe { self.context.Unmap(self.staging, 0) };
        self.metrics.unmaps += 1;
    }
}

fn invalid_dimensions() -> CaptureFault {
    CaptureFault::new(CaptureFailureKind::InvalidDimensions)
}

fn format_for(info: &AcceleratedPaintInfo) -> Option<(FrameFormat, DXGI_FORMAT)> {
    match info.format.get_raw() {
        FrameFormat::CEF_BGRA_8888 => Some((FrameFormat::Bgra8, DXGI_FORMAT_B8G8R8A8_UNORM)),
        FrameFormat::CEF_RGBA_8888 => Some((FrameFormat::Rgba8, DXGI_FORMAT_R8G8B8A8_UNORM)),
        _ => None,
    }
}

/// Validates everything derivable from CEF metadata before any native call:
/// format, coded allocation, visible clean aperture, content letterboxing, and
/// output length. `content_rect` must equal `visible_rect` (the old
/// `is_full_frame` rule): an intermediate letterboxed frame is not published.
fn layout_for(info: &AcceleratedPaintInfo, dst_len: usize) -> Result<FrameLayout, CaptureFault> {
    let Some((format, dxgi)) = format_for(info) else {
        return Err(CaptureFault::new(CaptureFailureKind::UnsupportedFormat));
    };
    let expected = super::accelerated_frame_bytes(&info.extra)?;
    let coded_width = info.extra.coded_size.width as u32;
    let coded_height = info.extra.coded_size.height as u32;
    let r = &info.extra.visible_rect;
    let visible = (r.x, r.y, r.width, r.height);
    if dst_len != expected {
        return Err(invalid_dimensions());
    }
    if info.shared_texture_handle.is_null() {
        return Err(CaptureFault::new(CaptureFailureKind::InvalidHandle));
    }
    Ok(FrameLayout {
        coded_width,
        coded_height,
        visible,
        format,
        dxgi,
    })
}

fn staging_desc(layout: &FrameLayout) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: layout.coded_width,
        Height: layout.coded_height,
        MipLevels: 1,
        ArraySize: 1,
        Format: layout.dxgi,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    }
}

fn is_device_lost(hr: HRESULT) -> bool {
    hr == DXGI_ERROR_DEVICE_REMOVED
        || hr == DXGI_ERROR_DEVICE_RESET
        || hr == DXGI_ERROR_DEVICE_HUNG
        || hr == DXGI_ERROR_DRIVER_INTERNAL_ERROR
}

/// Real HRESULTs are preserved; device-loss codes are re-categorised so the
/// endpoint reports `DeviceRemoved` with the actual native code. Never rebuilds
/// the device or reloads the Browser: that decision stays with the host.
fn native_fault(kind: CaptureFailureKind, hr: HRESULT) -> CaptureFault {
    if is_device_lost(hr) {
        CaptureFault::native(CaptureFailureKind::DeviceRemoved, hr.0)
    } else {
        CaptureFault::native(kind, hr.0)
    }
}

// ---------------------------------------------------------------------------
// Native fixture (no CEF initialisation) for real shared-texture readback tests.
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
use cef::{ColorType as CefColorType, Rect as CefRect, Size as CefSize};
#[cfg(any(test, feature = "test-support"))]
use windows::{
    Win32::{
        Foundation::CloseHandle,
        Graphics::{
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT,
            },
            Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1},
        },
    },
    core::PCWSTR,
};

/// Producer-side fixture: a real hardware D3D11 device owns a 3x3 coded texture
/// whose visible/content aperture is the inset 2x2 rect `(1, 0, 2, 2)`. The NT
/// handle is created by this fixture and is therefore its own to close; it is
/// NOT CEF's borrowed handle. The contained device/context/texture keep the
/// resource alive while `info` is borrowed by the receiver.
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub(super) struct TestTexture {
    pub(super) info: AcceleratedPaintInfo,
    handle: HANDLE,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    texture: Option<ID3D11Texture2D>,
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
impl TestTexture {
    /// Compact BGRA of the visible aperture, generated independently of the
    /// copy implementation (it does not call `copy_bgra_aperture`).
    pub(super) fn expected_bgra(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        for row in 0..2u32 {
            for col in 0..2u32 {
                let (x, y) = (1 + col, row);
                let base = (x * 8 + y * 4) as u8;
                out.extend_from_slice(&[base + 3, base + 2, base + 1, 0xff]);
            }
        }
        out
    }

    pub(super) fn coded_size(&self) -> (u32, u32) {
        (3, 3)
    }

    pub(super) fn visible_rect(&self) -> (i32, i32, i32, i32) {
        (1, 0, 2, 2)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestTexture {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: this handle was created by `CreateSharedHandle` in
            // `test_texture` and closed exactly once here.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = HANDLE(std::ptr::null_mut());
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
fn source_pixel(format: FrameFormat, x: u32, y: u32) -> [u8; 4] {
    let base = (x * 8 + y * 4) as u8;
    match format {
        FrameFormat::Bgra8 => [base + 3, base + 2, base + 1, 0xff],
        FrameFormat::Rgba8 => [base + 1, base + 2, base + 3, 0xff],
    }
}

/// Builds a real producer texture and shared handle using hardware D3D11.
/// Panics with an explicit message when no hardware device is present; WARP is
/// never substituted, so the accelerated tests cannot silently pass on a
/// software rasteriser.
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub(super) fn test_texture(format: FrameFormat) -> TestTexture {
    let (frame_format, dxgi) = match format {
        FrameFormat::Bgra8 => (FrameFormat::Bgra8, DXGI_FORMAT_B8G8R8A8_UNORM),
        FrameFormat::Rgba8 => (FrameFormat::Rgba8, DXGI_FORMAT_R8G8B8A8_UNORM),
    };
    let levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: plain hardware creation; this device is fixture-owned and freed
    // by the fixture, never by the receiver under test.
    if let Err(error) = unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(levels.as_slice()),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    } {
        panic!(
            "shared-texture fixture requires a hardware D3D11 device \
             (no WARP fallback): {error:?}"
        );
    }
    let device = device.expect("D3D11CreateDevice returned no device");
    let context = context.expect("D3D11CreateDevice returned no context");

    let mut coded = vec![0u8; 3 * 3 * 4];
    for y in 0..3u32 {
        for x in 0..3u32 {
            let pixel = source_pixel(frame_format, x, y);
            let offset = ((y * 3 + x) * 4) as usize;
            coded[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 3,
        Height: 3,
        MipLevels: 1,
        ArraySize: 1,
        Format: dxgi,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let initial_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: coded.as_ptr().cast(),
        SysMemPitch: 3 * 4,
        SysMemSlicePitch: 3 * 3 * 4,
    };
    let initial: *const D3D11_SUBRESOURCE_DATA = &initial_data;
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: desc matches the initial data (3 rows of 12 bytes) and the
    // out-parameter is correctly typed.
    unsafe { device.CreateTexture2D(&desc, Some(initial), Some(&mut texture)) }
        .expect("fixture CreateTexture2D");
    let texture = texture.expect("fixture CreateTexture2D returned no texture");
    // Resource initialization can be queued on the producer's immediate
    // context. Shared-resource consumers on a different device must not race
    // an unsubmitted upload. This fixture owns both context and texture.
    unsafe { context.Flush() };

    // SAFETY: QueryInterface on a live shared texture.
    let resource: IDXGIResource1 = texture
        .cast()
        .expect("fixture texture supports IDXGIResource1");
    // SAFETY: a fixture-owned resource; READ|WRITE access so the receiver may
    // open it for readback. No name, no security attributes.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )
    }
    .expect("fixture CreateSharedHandle");

    let mut info = AcceleratedPaintInfo::default();
    info.shared_texture_handle = handle.0;
    info.format = match frame_format {
        FrameFormat::Bgra8 => CefColorType::BGRA_8888,
        FrameFormat::Rgba8 => CefColorType::RGBA_8888,
    };
    let visible = CefRect {
        x: 1,
        y: 0,
        width: 2,
        height: 2,
    };
    info.extra.coded_size = CefSize {
        width: 3,
        height: 3,
    };
    info.extra.visible_rect = visible.clone();
    info.extra.content_rect = visible;
    TestTexture {
        info,
        handle,
        device: Some(device),
        context: Some(context),
        texture: Some(texture),
    }
}

#[cfg(test)]
mod tests {
    #![allow(dead_code, unused_imports)]

    use super::*;

    fn owner() -> u64 {
        crate::executor::current_thread_id()
    }

    #[test]
    fn real_shared_texture_copy_is_compact_bgra_and_reuses_staging() {
        for format in [FrameFormat::Bgra8, FrameFormat::Rgba8] {
            let fixture = test_texture(format);
            let mut capture =
                D3d11Capture::new(owner()).expect("hardware D3D11 device for real readback");
            let mut dst = vec![0u8; 16];
            capture
                .copy(SurfaceKind::View, &fixture.info, &mut dst)
                .unwrap_or_else(|fault| panic!("{format:?} copy failed: {fault:?}"));
            assert_eq!(dst, fixture.expected_bgra(), "{format:?} pixels");
            let metrics = capture.metrics();
            assert_eq!(
                (
                    metrics.opens,
                    metrics.copies,
                    metrics.maps,
                    metrics.unmaps,
                    metrics.live_imports
                ),
                (1, 1, 1, 1, 0),
                "{format:?}"
            );
            assert_eq!(metrics.staging_bytes, 36, "coded 3x3 staging capacity");
            assert_eq!(metrics.staging_peak, 36);

            // Same coded identity on the same surface slot reuses staging and
            // still opens a fresh import each callback.
            dst.fill(0);
            capture
                .copy(SurfaceKind::View, &fixture.info, &mut dst)
                .expect("second copy");
            assert_eq!(dst, fixture.expected_bgra());
            let metrics = capture.metrics();
            assert_eq!(
                (metrics.opens, metrics.copies, metrics.maps, metrics.unmaps),
                (2, 2, 2, 2)
            );
            assert_eq!(metrics.staging_bytes, 36);
            assert_eq!(metrics.live_imports, 0);
        }
    }

    #[test]
    fn owner_thread_is_enforced_for_new_and_copy() {
        let owner = owner();
        let creation_fault =
            D3d11Capture::new_with_fault(owner, Some(D3dFaultPoint::DeviceCreation))
                .err()
                .expect("explicit device creation injection must fail without fallback");
        assert_eq!(creation_fault.kind, CaptureFailureKind::DeviceCreation);
        assert_eq!(creation_fault.code, None);
        let capture =
            D3d11Capture::new(owner).expect("hardware D3D11 device for wrong-thread test");
        let capture = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let mut capture = capture;
                    let refused = match D3d11Capture::new(owner) {
                        Ok(_) => panic!("device construction was accepted off the owner thread"),
                        Err(fault) => fault,
                    };
                    assert_eq!(refused.kind, CaptureFailureKind::WrongThread);
                    let info = AcceleratedPaintInfo::default();
                    let mut dst = [0u8; 16];
                    let refused = capture
                        .copy(SurfaceKind::View, &info, &mut dst)
                        .expect_err("copy was accepted off the owner thread");
                    assert_eq!(refused.kind, CaptureFailureKind::WrongThread);
                    capture
                })
                .join()
                .expect("wrong-thread probe must not panic")
        });
        drop(capture); // back on the owner: normal synchronous release.
    }

    #[test]
    fn injected_failures_release_imports_and_always_unmap() {
        let fixture = test_texture(FrameFormat::Bgra8);
        let mut capture = D3d11Capture::new(owner()).expect("hardware D3D11 device");
        let mut dst = vec![0u8; 16];
        let cases = [
            (D3dFaultPoint::Open, CaptureFailureKind::OpenSharedResource),
            (D3dFaultPoint::Describe, CaptureFailureKind::TextureMismatch),
            (D3dFaultPoint::Staging, CaptureFailureKind::StagingCreation),
            (D3dFaultPoint::Copy, CaptureFailureKind::CopyResource),
            (D3dFaultPoint::Map, CaptureFailureKind::Map),
            (D3dFaultPoint::AfterMap, CaptureFailureKind::IncompleteFrame),
            (
                D3dFaultPoint::DeviceRemoved,
                CaptureFailureKind::DeviceRemoved,
            ),
        ];
        for (point, kind) in cases {
            let before = capture.metrics();
            capture.set_next_fault(Some(point));
            let fault = capture
                .copy(SurfaceKind::View, &fixture.info, &mut dst)
                .expect_err("injected fault must fail");
            let after = capture.metrics();
            assert_eq!(fault.kind, kind, "injected {point:?}");
            assert_eq!(after.live_imports, 0, "import released after {point:?}");
            assert_eq!(after.maps, after.unmaps, "map/unmap balance at {point:?}");
            match point {
                D3dFaultPoint::AfterMap => {
                    assert_eq!(after.maps, before.maps + 1);
                    assert_eq!(after.unmaps, before.unmaps + 1);
                }
                D3dFaultPoint::DeviceRemoved => {
                    assert_eq!(fault.code, Some(DXGI_ERROR_DEVICE_REMOVED.0));
                    assert_eq!(after.copies, before.copies);
                }
                _ => assert_eq!(after.maps, before.maps),
            }
        }
        // The injected removal must not have rebuilt anything: the same
        // receive device still completes a clean copy.
        capture.set_next_fault(None);
        dst.fill(0);
        capture
            .copy(SurfaceKind::View, &fixture.info, &mut dst)
            .expect("clean copy after injected device-removed");
        assert_eq!(dst, fixture.expected_bgra());
        assert_eq!(capture.metrics().live_imports, 0);
    }

    #[test]
    fn validation_rejects_bad_geometry_before_opening_shared_resource() {
        let mut info = AcceleratedPaintInfo::default();
        info.extra.coded_size = CefSize {
            width: 3,
            height: 3,
        };
        info.extra.visible_rect = CefRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        info.extra.content_rect = CefRect {
            x: 0,
            y: 0,
            width: 1,
            height: 2,
        };
        let fault = layout_for(&info, 16).expect_err("content != visible");
        assert_eq!(fault.kind, CaptureFailureKind::IncompleteFrame);

        info.extra.content_rect = info.extra.visible_rect.clone();
        let out_of_coded = AcceleratedPaintInfo {
            extra: cef::AcceleratedPaintInfoCommon {
                visible_rect: CefRect {
                    x: 2,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                ..info.extra.clone()
            },
            ..info.clone()
        };
        let fault = layout_for(&out_of_coded, 16).expect_err("visible outside coded");
        assert_eq!(fault.kind, CaptureFailureKind::InvalidDimensions);

        let fault = layout_for(&info, 15).expect_err("dst length must match visible");
        assert_eq!(fault.kind, CaptureFailureKind::InvalidDimensions);

        // Geometry is fine but the handle is null: still refused before any
        // OpenSharedResource1 call.
        let fault = layout_for(&info, 16).expect_err("null borrowed handle");
        assert_eq!(fault.kind, CaptureFailureKind::InvalidHandle);
    }
}
