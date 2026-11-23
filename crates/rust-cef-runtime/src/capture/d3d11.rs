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

use std::{
    slice,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use cef::AcceleratedPaintInfo;
use rust_cef_core::{
    frame_layout::{FrameFormat, copy_bgra_aperture},
    ports::{
        CaptureFailureKind, GpuSharedTextureFrame, SHARED_TEXTURE_SLOT_COUNT,
        SharedTextureSlotTracker, SurfaceKind, TextureFormat,
    },
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HMODULE},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0,
                D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
            },
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
                ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
                    DXGI_SAMPLE_DESC,
                },
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_DEVICE_HUNG,
                DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
                DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_SHARED_RESOURCE_READ,
                DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter, IDXGIDevice1, IDXGIFactory1,
                IDXGIKeyedMutex, IDXGIResource, IDXGIResource1,
            },
        },
    },
    core::{HRESULT, Interface, PCWSTR},
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

static NEXT_SHARED_POOL_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenHandleMode {
    Unknown,
    NtHandle,
    LegacyHandle,
}

#[derive(Debug)]
struct OwnedSharedHandle {
    raw: isize,
    is_nt_handle: bool,
    ring_counter: Arc<AtomicUsize>,
}

impl OwnedSharedHandle {
    fn new_nt(handle: HANDLE, ring_counter: Arc<AtomicUsize>) -> Self {
        ring_counter.fetch_add(1, Ordering::AcqRel);
        Self {
            raw: handle.0 as isize,
            is_nt_handle: true,
            ring_counter,
        }
    }

    fn new_legacy(handle: HANDLE, ring_counter: Arc<AtomicUsize>) -> Self {
        ring_counter.fetch_add(1, Ordering::AcqRel);
        Self {
            raw: handle.0 as isize,
            is_nt_handle: false,
            ring_counter,
        }
    }
}

impl Drop for OwnedSharedHandle {
    fn drop(&mut self) {
        if self.raw != 0 {
            self.ring_counter.fetch_sub(1, Ordering::AcqRel);
            if self.is_nt_handle {
                unsafe {
                    let _ = CloseHandle(HANDLE(self.raw as *mut core::ffi::c_void));
                }
            }
            self.raw = 0;
        }
    }
}

struct SharedSlot {
    width: u32,
    height: u32,
    generation: u64,
    texture: ID3D11Texture2D,
    keyed_mutex: Option<IDXGIKeyedMutex>,
    handle: Arc<OwnedSharedHandle>,
    keepalive: Arc<()>,
    bytes: u64,
}

struct SharedTextureRing {
    pool_id: u64,
    tracker: Arc<SharedTextureSlotTracker>,
    slots: [Option<SharedSlot>; SHARED_TEXTURE_SLOT_COUNT],
    next_generation: u64,
    live_handles: Arc<AtomicUsize>,
}

impl SharedTextureRing {
    fn new() -> Self {
        Self {
            pool_id: NEXT_SHARED_POOL_ID.fetch_add(1, Ordering::Relaxed),
            tracker: Arc::new(SharedTextureSlotTracker::new()),
            slots: [None, None, None, None],
            next_generation: 0,
            live_handles: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct KeyedMutexGuard<'a> {
    mutex: &'a IDXGIKeyedMutex,
    key: u64,
}

impl<'a> KeyedMutexGuard<'a> {
    fn acquire(mutex: &'a IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> Result<Self, CaptureFault> {
        let hr = unsafe { (Interface::vtable(mutex).AcquireSync)(mutex.as_raw(), key, timeout_ms) };
        if hr.0 != 0 {
            return Err(native_fault(CaptureFailureKind::CopyResource, hr));
        }
        Ok(Self { mutex, key })
    }
}

impl Drop for KeyedMutexGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mutex.ReleaseSync(self.key);
        }
    }
}

/// Per-Browser receive device. All fields are native COM objects: create, use
/// and normal-drop only on `owner`; the wrong-thread `Drop` forgets them.
pub(crate) struct D3d11Capture {
    owner: u64,
    device: Option<ID3D11Device>,
    device1: Option<ID3D11Device1>,
    context: Option<ID3D11DeviceContext>,
    open_mode: OpenHandleMode,
    /// One slot per `SurfaceKind`; rebuilt only when coded size/format changes.
    slots: [Option<Staging>; 2],
    /// Triple-buffered shared GPU textures exported to GPUI without CPU readback.
    shared_rings: [SharedTextureRing; 2],
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

        // Enumerate DXGI hardware adapters first (matching GPUI's primary hardware
        // adapter LUID). When a non-null IDXGIAdapter is supplied, DriverType MUST
        // be D3D_DRIVER_TYPE_UNKNOWN.
        let mut created_via_adapter = false;
        if let Ok(factory) = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
            let mut idx = 0u32;
            while let Ok(adapter1) = unsafe { factory.EnumAdapters1(idx) } {
                idx = idx.saturating_add(1);
                let Ok(desc) = (unsafe { adapter1.GetDesc1() }) else {
                    continue;
                };
                if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                    continue;
                }
                let Ok(adapter) = adapter1.cast::<IDXGIAdapter>() else {
                    continue;
                };
                let mut dev = None;
                let mut ctx = None;
                if unsafe {
                    D3D11CreateDevice(
                        Some(&adapter),
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(levels.as_slice()),
                        D3D11_SDK_VERSION,
                        Some(&mut dev),
                        None,
                        Some(&mut ctx),
                    )
                }
                .is_ok()
                    && dev.is_some()
                    && ctx.is_some()
                {
                    device = dev;
                    context = ctx;
                    created_via_adapter = true;
                    break;
                }
            }
        }

        if !created_via_adapter {
            // Fallback to default hardware driver selection; still never WARP.
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
        }

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
        if let Ok(dxgi_device1) = device.cast::<IDXGIDevice1>() {
            let _ = unsafe { dxgi_device1.SetMaximumFrameLatency(1) };
        }
        #[cfg(feature = "test-support")]
        let readback_adapter_luid = readback_adapter_luid(&device);
        Ok(Self {
            owner: owner_thread,
            device: Some(device),
            device1: Some(device1),
            context: Some(context),
            open_mode: OpenHandleMode::Unknown,
            slots: [None, None],
            shared_rings: [SharedTextureRing::new(), SharedTextureRing::new()],
            metrics: D3dMetrics::default(),
            #[cfg(feature = "test-support")]
            readback_adapter_luid,
            retired: false,
            next_fault: FAULT_NONE,
        })
    }

    fn open_cef_shared_texture(
        &mut self,
        raw_handle: *mut core::ffi::c_void,
    ) -> Result<ID3D11Texture2D, CaptureFault> {
        let Some(device1) = self.device1.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let Some(device) = self.device.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let handle = HANDLE(raw_handle);
        match self.open_mode {
            OpenHandleMode::NtHandle => unsafe { device1.OpenSharedResource1(handle) }
                .map_err(|error| native_fault(CaptureFailureKind::OpenSharedResource, error.code())),
            OpenHandleMode::LegacyHandle => {
                let mut opened: Option<ID3D11Texture2D> = None;
                unsafe { device.OpenSharedResource(handle, &mut opened) }.map_err(|error| {
                    native_fault(CaptureFailureKind::OpenSharedResource, error.code())
                })?;
                opened.ok_or_else(|| CaptureFault::new(CaptureFailureKind::OpenSharedResource))
            }
            OpenHandleMode::Unknown => {
                match unsafe { device1.OpenSharedResource1::<ID3D11Texture2D>(handle) } {
                    Ok(tex) => {
                        self.open_mode = OpenHandleMode::NtHandle;
                        Ok(tex)
                    }
                    Err(nt_err) => {
                        let mut opened: Option<ID3D11Texture2D> = None;
                        if unsafe { device.OpenSharedResource(handle, &mut opened) }.is_ok()
                            && let Some(tex) = opened
                        {
                            self.open_mode = OpenHandleMode::LegacyHandle;
                            Ok(tex)
                        } else {
                            Err(native_fault(
                                CaptureFailureKind::OpenSharedResource,
                                nt_err.code(),
                            ))
                        }
                    }
                }
            }
        }
    }

    /// Zero-CPU-copy capture into the triple-buffered D3D11 shared texture pool.
    pub(crate) fn copy_to_shared_pool(
        &mut self,
        surface: SurfaceKind,
        info: &AcceleratedPaintInfo,
        mut try_reclaim_unconsumed: impl FnMut() -> Option<usize>,
    ) -> Result<(GpuSharedTextureFrame, u64), CaptureFault> {
        if crate::executor::current_thread_id() != self.owner {
            return Err(CaptureFault::new(CaptureFailureKind::WrongThread));
        }
        if self.retired {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        }

        if info.format.get_raw() != FrameFormat::CEF_BGRA_8888 {
            return Err(CaptureFault::new(CaptureFailureKind::UnsupportedFormat));
        }
        let expected_bytes = super::accelerated_frame_bytes(&info.extra)?;
        let coded_width = info.extra.coded_size.width as u32;
        let coded_height = info.extra.coded_size.height as u32;
        let r = &info.extra.visible_rect;
        let visible_x = r.x as u32;
        let visible_y = r.y as u32;
        let visible_width = r.width as u32;
        let visible_height = r.height as u32;
        let _ = expected_bytes;
        if info.shared_texture_handle.is_null() {
            return Err(CaptureFault::new(CaptureFailureKind::InvalidHandle));
        }

        let ring_idx = match surface {
            SurfaceKind::View => 0,
            SurfaceKind::Popup => 1,
        };
        let slot_index = match self.shared_rings[ring_idx].tracker.acquire_write_slot() {
            Some(idx) => idx,
            None => {
                let _ = try_reclaim_unconsumed();
                match self.shared_rings[ring_idx].tracker.acquire_write_slot() {
                    Some(idx) => idx,
                    None => return Err(CaptureFault::new(CaptureFailureKind::Busy)),
                }
            }
        };

        let imported = self.open_cef_shared_texture(info.shared_texture_handle)?;
        self.metrics.opens += 1;
        self.metrics.live_imports += 1;

        let result = self.copy_imported_to_shared_slot(
            ring_idx,
            slot_index,
            &imported,
            coded_width,
            coded_height,
            visible_x,
            visible_y,
            visible_width,
            visible_height,
        );

        drop(imported);
        self.metrics.live_imports = self.metrics.live_imports.saturating_sub(1);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_imported_to_shared_slot(
        &mut self,
        ring_idx: usize,
        slot_index: usize,
        imported: &ID3D11Texture2D,
        coded_width: u32,
        coded_height: u32,
        visible_x: u32,
        visible_y: u32,
        visible_width: u32,
        visible_height: u32,
    ) -> Result<(GpuSharedTextureFrame, u64), CaptureFault> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { imported.GetDesc(&mut desc) };
        if desc.Width != coded_width
            || desc.Height != coded_height
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.MipLevels != 1
            || desc.ArraySize != 1
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
        {
            return Err(CaptureFault::new(CaptureFailureKind::TextureMismatch));
        }

        self.ensure_shared_slot(ring_idx, slot_index, visible_width, visible_height)?;

        let Some(context) = self.context.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let Some(device) = self.device.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };
        let ring = &self.shared_rings[ring_idx];
        let Some(slot) = ring.slots[slot_index].as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::StagingCreation));
        };

        let started = Instant::now();
        {
            let _guard = match &slot.keyed_mutex {
                Some(mutex) => Some(KeyedMutexGuard::acquire(mutex, 0, 100)?),
                None => None,
            };
            if visible_x == 0
                && visible_y == 0
                && visible_width == coded_width
                && visible_height == coded_height
            {
                unsafe { context.CopyResource(&slot.texture, imported) };
            } else {
                let src_box = D3D11_BOX {
                    left: visible_x,
                    top: visible_y,
                    front: 0,
                    right: visible_x + visible_width,
                    bottom: visible_y + visible_height,
                    back: 1,
                };
                unsafe {
                    context.CopySubresourceRegion(&slot.texture, 0, 0, 0, 0, imported, 0, Some(&src_box));
                }
            }
            // `KeyedMutexGuard::drop` calls `ReleaseSync(0)`, which implicitly flushes the
            // immediate context; only flush explicitly when no KeyedMutex is attached.
            if _guard.is_none() {
                unsafe { context.Flush() };
            }
        }
        unsafe { device.GetDeviceRemovedReason() }
            .map_err(|error| native_fault(CaptureFailureKind::DeviceRemoved, error.code()))?;
        let copy_us = started.elapsed().as_micros() as u64;
        self.metrics.copies += 1;

        let frame = GpuSharedTextureFrame {
            pool_id: ring.pool_id,
            shared_handle: slot.handle.raw,
            is_nt_handle: slot.handle.is_nt_handle,
            keyed_mutex_key: slot.keyed_mutex.as_ref().map(|_| 0),
            width: visible_width,
            height: visible_height,
            format: TextureFormat::Bgra8888,
            frame_seq: 0,
            slot_index,
            generation: slot.generation,
            resource_keepalive: Arc::downgrade(&slot.keepalive),
            handle_owner: slot.handle.clone(),
            lease: ring.tracker.lease(slot_index),
        };
        Ok((frame, copy_us))
    }

    fn ensure_shared_slot(
        &mut self,
        ring_idx: usize,
        slot_index: usize,
        width: u32,
        height: u32,
    ) -> Result<(), CaptureFault> {
        if self.shared_rings[ring_idx].slots[slot_index]
            .as_ref()
            .is_some_and(|slot| slot.width == width && slot.height == height)
        {
            return Ok(());
        }

        let Some(device) = self.device.as_ref() else {
            return Err(CaptureFault::new(CaptureFailureKind::Retired));
        };

        let make_desc = |misc_flags: u32| D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags,
        };

        let nt_keyed_flags =
            (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32;
        let nt_plain_flags =
            (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32;
        let legacy_flags = D3D11_RESOURCE_MISC_SHARED.0 as u32;

        let mut created: Option<ID3D11Texture2D> = None;
        let mut try_nt = true;
        let mut with_keyed_mutex = false;

        if unsafe { device.CreateTexture2D(&make_desc(nt_keyed_flags), None, Some(&mut created)) }
            .is_ok()
            && created.is_some()
        {
            with_keyed_mutex = true;
        } else {
            created = None;
            if unsafe {
                device.CreateTexture2D(&make_desc(nt_plain_flags), None, Some(&mut created))
            }
            .is_err()
                || created.is_none()
            {
                try_nt = false;
                created = None;
                unsafe {
                    device.CreateTexture2D(&make_desc(legacy_flags), None, Some(&mut created))
                }
                .map_err(|error| {
                    native_fault(CaptureFailureKind::StagingCreation, error.code())
                })?;
            }
        }

        let texture =
            created.ok_or_else(|| CaptureFault::new(CaptureFailureKind::StagingCreation))?;

        let ring_counter = Arc::clone(&self.shared_rings[ring_idx].live_handles);
        let (handle, keyed_mutex) = if try_nt {
            let resource1: IDXGIResource1 = texture
                .cast()
                .map_err(|error| native_fault(CaptureFailureKind::StagingCreation, error.code()))?;
            let raw = unsafe {
                resource1.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR::null(),
                )
            }
            .map_err(|error| native_fault(CaptureFailureKind::StagingCreation, error.code()))?;
            let keyed = if with_keyed_mutex {
                texture.cast::<IDXGIKeyedMutex>().ok()
            } else {
                None
            };
            (Arc::new(OwnedSharedHandle::new_nt(raw, ring_counter)), keyed)
        } else {
            let resource: IDXGIResource = texture
                .cast()
                .map_err(|error| native_fault(CaptureFailureKind::StagingCreation, error.code()))?;
            let raw = unsafe { resource.GetSharedHandle() }.map_err(|error| {
                native_fault(CaptureFailureKind::StagingCreation, error.code())
            })?;
            (
                Arc::new(OwnedSharedHandle::new_legacy(raw, ring_counter)),
                None,
            )
        };

        let bytes = u64::from(width) * u64::from(height) * 4;
        self.metrics.staging_peak = self
            .metrics
            .staging_peak
            .max(self.metrics.staging_bytes.saturating_add(bytes));

        let ring = &mut self.shared_rings[ring_idx];
        ring.next_generation = ring.next_generation.saturating_add(1);
        let generation = ring.next_generation;
        let old = ring.slots[slot_index].replace(SharedSlot {
            width,
            height,
            generation,
            texture,
            keyed_mutex,
            handle,
            keepalive: Arc::new(()),
            bytes,
        });
        let previous = old.map_or(0, |slot| slot.bytes);
        self.metrics.staging_bytes = self
            .metrics
            .staging_bytes
            .saturating_sub(previous)
            .saturating_add(bytes);
        self.metrics.staging_peak = self.metrics.staging_peak.max(self.metrics.staging_bytes);
        Ok(())
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
        let imported = self.open_cef_shared_texture(info.shared_texture_handle)?;
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
        for ring in &mut self.shared_rings {
            for slot in &mut ring.slots {
                if let Some(shared) = slot.take() {
                    std::mem::forget(shared.texture);
                    std::mem::forget(shared.keyed_mutex);
                    std::mem::forget(shared.handle);
                }
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
use windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA;

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

    #[test]
    fn shared_pool_cross_device_srv_readback_matches_source_pixels() {
        use windows::Win32::Graphics::{
            Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D,
            Direct3D11::{
                D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
                D3D11_TEX2D_SRV, ID3D11ShaderResourceView,
            },
        };

        let fixture = test_texture(FrameFormat::Bgra8);
        let mut capture = D3d11Capture::new(owner()).expect("hardware D3D11 device");
        let (gpu_frame, copy_us) = capture
            .copy_to_shared_pool(SurfaceKind::View, &fixture.info, || None)
            .expect("copy_to_shared_pool");
        let _ = copy_us;
        assert_eq!(gpu_frame.width, 2);
        assert_eq!(gpu_frame.height, 2);
        assert_ne!(gpu_frame.shared_handle, 0);

        // Open the exported shared handle on an independent consumer D3D11 device,
        // create an SRV (verifying D3D11_BIND_SHADER_RESOURCE + BGRA compatibility),
        // acquire keyed mutex if present, and read back pixels to compare against source.
        let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1];
        let mut consumer_dev: Option<ID3D11Device> = None;
        let mut consumer_ctx: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut consumer_dev),
                None,
                Some(&mut consumer_ctx),
            )
        }
        .expect("consumer D3D11CreateDevice");
        let consumer_dev = consumer_dev.unwrap();
        let consumer_ctx = consumer_ctx.unwrap();
        let handle = HANDLE(gpu_frame.shared_handle as *mut core::ffi::c_void);
        let opened: ID3D11Texture2D = if gpu_frame.is_nt_handle {
            let dev1: ID3D11Device1 = consumer_dev.cast().unwrap();
            unsafe { dev1.OpenSharedResource1(handle) }.expect("consumer OpenSharedResource1")
        } else {
            let mut out = None;
            unsafe { consumer_dev.OpenSharedResource(handle, &mut out) }
                .expect("consumer OpenSharedResource");
            out.unwrap()
        };

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe { consumer_dev.CreateShaderResourceView(&opened, Some(&srv_desc), Some(&mut srv)) }
            .expect("consumer CreateShaderResourceView");
        assert!(srv.is_some());

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: 2,
            Height: 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { consumer_dev.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .expect("consumer staging");
        let staging: ID3D11Texture2D = staging.unwrap();

        {
            let km = gpu_frame.keyed_mutex_key.map(|key| {
                (
                    opened
                        .cast::<IDXGIKeyedMutex>()
                        .expect("opened IDXGIKeyedMutex"),
                    key,
                )
            });
            let _guard = km.as_ref().map(|(mutex, key)| {
                KeyedMutexGuard::acquire(mutex, *key, 100).expect("consumer AcquireSync")
            });
            unsafe { consumer_ctx.CopyResource(&staging, &opened) };
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { consumer_ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .expect("consumer Map");
        let mut got = vec![0u8; 2 * 2 * 4];
        for y in 0..2usize {
            let src_row = unsafe {
                slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    8,
                )
            };
            got[y * 8..y * 8 + 8].copy_from_slice(src_row);
        }
        unsafe { consumer_ctx.Unmap(&staging, 0) };
        assert_eq!(got, fixture.expected_bgra());
    }

    #[test]
    fn shared_pool_100_consecutive_resizes_leak_no_handles_or_vram() {
        let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1];
        let mut src_dev: Option<ID3D11Device> = None;
        let mut src_ctx: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut src_dev),
                None,
                Some(&mut src_ctx),
            )
        }
        .expect("producer D3D11CreateDevice");
        let src_dev = src_dev.unwrap();
        let src_ctx = src_ctx.unwrap();

        let mut capture = D3d11Capture::new(owner()).expect("hardware D3D11 device");
        let pool_handles = Arc::clone(&capture.shared_rings[0].live_handles);

        let mut presented: Option<GpuSharedTextureFrame> = None;
        for step in 0..100u32 {
            let w = 64 + step * 4;
            let h = 48 + step * 3;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                    as u32,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { src_dev.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap();
            let tex = tex.unwrap();
            unsafe { src_ctx.Flush() };
            let res: IDXGIResource1 = tex.cast().unwrap();
            let raw_handle = unsafe {
                res.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR::null(),
                )
            }
            .unwrap();

            let mut info = AcceleratedPaintInfo::default();
            info.shared_texture_handle = raw_handle.0;
            info.format = CefColorType::BGRA_8888;
            info.extra.coded_size = CefSize {
                width: w as i32,
                height: h as i32,
            };
            info.extra.visible_rect = CefRect {
                x: 0,
                y: 0,
                width: w as i32,
                height: h as i32,
            };
            info.extra.content_rect = info.extra.visible_rect.clone();

            let (frame, _) = capture
                .copy_to_shared_pool(SurfaceKind::View, &info, || None)
                .expect("resize pool copy");
            frame.lease.tracker().mark_presenting(frame.slot_index);
            presented = Some(frame);
            unsafe {
                let _ = CloseHandle(raw_handle);
            }

            let live_handles = pool_handles.load(Ordering::Acquire);
            assert!(
                live_handles <= SHARED_TEXTURE_SLOT_COUNT,
                "handle count grew to {live_handles} at step {step}"
            );
        }

        let metrics = capture.metrics();
        // Max size at step 99 is (460 x 345 * 4 = 634_800), across the quad-buffered slots.
        let upper_bound = (SHARED_TEXTURE_SLOT_COUNT as u64) * (64 + 99 * 4) * (48 + 99 * 3) * 4;
        assert!(
            metrics.staging_bytes <= upper_bound,
            "vram bytes {} exceeded bound {}",
            metrics.staging_bytes,
            upper_bound
        );

        drop(presented);
        drop(capture);
        assert_eq!(
            pool_handles.load(Ordering::Acquire),
            0,
            "all shared handles must close after capture and frames drop"
        );
    }
}
