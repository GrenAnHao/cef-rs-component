//! Per-browser CEF client. Callbacks copy pixels into owned storage and publish
//! owned events; they never borrow GPUI and never hold a lock across a call into
//! the host.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

#[cfg(feature = "test-support")]
use std::sync::atomic::AtomicU64;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

use cef::*;
use rust_cef_core::{
    contracts::{
        BrowserCursor, BrowserId, BrowserOptions, CefFeatureState, ConsoleSeverity, Viewport,
        progress_to_per_mille, truncate_utf8_bytes,
    },
    frame_budget::FrameError,
    frame_store::frame_len,
    ime::{ImeRect, ImeReportedBounds},
    input::Utf16Range,
    mailbox::{FrameMailbox, SurfaceBounds},
    outbound_drag::OutboundDragPolicy,
    ports::{BrowserEvent, FrameSink, PolicyAction, SurfaceKind, UiDispatcher},
};

use crate::{
    capture::{CaptureEndpoint, CaptureFrameSink},
    policy::ApprovedDataPages,
    policy_native::NativePolicy,
    registry::{UiRegistry, WritableCefString, wake_frames},
};

type MonitorRectPair = ((i32, i32, i32, i32), (i32, i32, i32, i32));

const MAX_OSR_TEXTURE_DIMENSION: i32 = rust_cef_core::contracts::MAX_NATIVE_BOUND as i32;

#[derive(Clone, Copy)]
struct ViewportInner {
    viewport: Viewport,
    screen_origin: Option<(i32, i32)>,
    last_monitor: Option<MonitorRectPair>,
}

pub(crate) struct ViewportState {
    inner: Mutex<ViewportInner>,
}

impl ViewportState {
    pub(crate) fn new(viewport: Viewport) -> Self {
        Self {
            inner: Mutex::new(ViewportInner {
                viewport,
                screen_origin: None,
                last_monitor: None,
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> (Viewport, Option<(i32, i32)>) {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (guard.viewport, guard.screen_origin)
    }

    pub(crate) fn get(&self) -> Viewport {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .viewport
    }

    pub(crate) fn set(&self, viewport: Viewport) {
        let (x_px, y_px) = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.viewport = viewport;
            let Some(origin) = guard.screen_origin else {
                return;
            };
            origin
        };
        if let Some((m, w)) = monitor_screen_rects(x_px, y_px, viewport.scale) {
            let next_rects = ((m.x, m.y, m.width, m.height), (w.x, w.y, w.width, w.height));
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.viewport == viewport && guard.screen_origin == Some((x_px, y_px)) {
                guard.last_monitor = Some(next_rects);
            }
        }
    }

    pub(crate) fn set_screen_origin(&self, x_px: i32, y_px: i32) -> (bool, bool) {
        let (origin_changed, scale) = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let next = Some((x_px, y_px));
            if guard.screen_origin == next {
                return (false, false);
            }
            guard.screen_origin = next;
            (true, guard.viewport.scale)
        };
        let next_monitor = monitor_screen_rects(x_px, y_px, scale)
            .map(|(m, w)| ((m.x, m.y, m.width, m.height), (w.x, w.y, w.width, w.height)));
        let monitor_changed = if let Some(next_rects) = next_monitor {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.screen_origin != Some((x_px, y_px)) {
                false
            } else {
                let changed = guard.last_monitor != Some(next_rects);
                if changed {
                    guard.last_monitor = Some(next_rects);
                }
                changed
            }
        } else {
            false
        };
        (origin_changed, monitor_changed)
    }

    pub(crate) fn screen_point(&self, view_x: i32, view_y: i32) -> Option<(i32, i32)> {
        let (viewport, origin) = self.snapshot();
        let (ox, oy) = origin?;
        let sx = ox.saturating_add((view_x as f32 * viewport.scale).round() as i32);
        let sy = oy.saturating_add((view_y as f32 * viewport.scale).round() as i32);
        Some((sx, sy))
    }
}

fn monitor_screen_rects(origin_x: i32, origin_y: i32, scale: f32) -> Option<(Rect, Rect)> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Win32Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Win32Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: Win32Rect,
        rc_work: Win32Rect,
        dw_flags: u32,
    }
    #[allow(clashing_extern_declarations)]
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromPoint(pt: Win32Point, flags: u32) -> *mut std::ffi::c_void;
        fn GetMonitorInfoW(hmonitor: *mut std::ffi::c_void, lpmi: *mut MonitorInfo) -> i32;
    }
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let hmon = unsafe {
        MonitorFromPoint(
            Win32Point {
                x: origin_x,
                y: origin_y,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    if hmon.is_null() {
        return None;
    }
    let mut info = MonitorInfo {
        cb_size: std::mem::size_of::<MonitorInfo>() as u32,
        rc_monitor: Win32Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rc_work: Win32Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dw_flags: 0,
    };
    if unsafe { GetMonitorInfoW(hmon, &mut info) } == 0 {
        return None;
    }
    let to_cef = |r: Win32Rect| -> Rect {
        let x = (r.left as f32 / scale).round() as i32;
        let y = (r.top as f32 / scale).round() as i32;
        let width = ((r.right - r.left) as f32 / scale).round().max(1.0) as i32;
        let height = ((r.bottom - r.top) as f32 / scale).round().max(1.0) as i32;
        Rect {
            x,
            y,
            width,
            height,
        }
    };
    Some((to_cef(info.rc_monitor), to_cef(info.rc_work)))
}

/// Callback sizes are device pixels, whereas CEF view/popup rectangles are DIP.
/// This is a local target-size guard, not a native callback epoch identifier.
pub(crate) fn pixel_extent(width: i32, height: i32, scale: f32) -> Result<(u32, u32), FrameError> {
    if width <= 0 || height <= 0 || !scale.is_finite() || scale <= 0.0 {
        return Err(FrameError::InvalidDimensions);
    }
    // Fixed Chromium SizeF scales in float before ClampCeil. Widening the
    // already-rounded scale to f64 first miscomputes e.g. 10 * 1.1 as 12.
    let scaled_width = width as f32 * scale;
    let scaled_height = height as f32 * scale;
    // SizeF clamps trivial dimensions to zero; do not accept a viewport for
    // which the fixed native implementation cannot produce a positive frame.
    if scaled_width <= 8.0 * f32::EPSILON || scaled_height <= 8.0 * f32::EPSILON {
        return Err(FrameError::InvalidDimensions);
    }
    let w = scaled_width.ceil();
    let h = scaled_height.ceil();
    if !w.is_finite() || !h.is_finite() || w >= u32::MAX as f32 || h >= u32::MAX as f32 {
        return Err(FrameError::InvalidDimensions);
    }
    let (w, h) = (w as u32, h as u32);
    frame_len(w, h).ok_or(FrameError::InvalidDimensions)?;
    Ok((w, h))
}

pub(crate) fn new_frame_mailbox(
    browser: BrowserId,
    viewport: Viewport,
) -> Result<Arc<FrameMailbox>, FrameError> {
    let (width, height) = pixel_extent(viewport.width, viewport.height, viewport.scale)?;
    let mailbox = Arc::new(FrameMailbox::new(browser));
    mailbox.configure_view_with_scale(
        SurfaceBounds {
            x: 0,
            y: 0,
            width: viewport.width,
            height: viewport.height,
        },
        width,
        height,
        viewport.scale,
    )?;
    // Initial configuration precedes exposure to any reader. Do not leave an
    // armed but never-dispatched wake that would swallow the first real paint.
    let _ = mailbox.take_update();
    Ok(mailbox)
}

pub(crate) struct BrowserClientState {
    pub(crate) browser: BrowserId,
    pub(crate) viewport: Arc<ViewportState>,
    pub(crate) store: Arc<FrameMailbox>,
    pub(crate) capture: Arc<CaptureEndpoint>,
    pub(crate) recovery: Arc<crate::resize::ResizeState>,
    #[cfg(feature = "test-support")]
    frame_probe: Mutex<crate::test_support::FrameProbeState>,
    frame_sink: CaptureFrameSink,
    pub(crate) ui: Arc<dyn UiDispatcher>,
    pub(crate) registry: Weak<UiRegistry>,
    pub(crate) policy: Arc<NativePolicy>,
    pub(crate) accessibility: Arc<crate::accessibility::RuntimeAccessibilityState>,
    pub(crate) active: AtomicBool,
    pub(crate) find_unmanaged: AtomicBool,
    /// Test-only: the unit tests run without a CEF UI sequence, so the real
    /// thread qualification is bypassed only when a fixture opts in.
    #[cfg(test)]
    ime_adapter_test: AtomicBool,
    approved_data: Mutex<ApprovedDataPages>,
    pub(crate) options: Arc<BrowserOptions>,
    cursor: Mutex<BrowserCursor>,
    pub(crate) cursor_slot: rust_cef_core::cursor::CursorSlot,
    last_tooltip: Mutex<String>,
    last_status: Mutex<String>,
    first_frame_logged: AtomicBool,
    frame_stats_count: std::sync::atomic::AtomicU64,
    frame_stats_copy_us_sum: std::sync::atomic::AtomicU64,
    #[cfg(feature = "test-support")]
    probe_console_kinds: AtomicU64,
    #[cfg(feature = "test-support")]
    pub(crate) menu_probe: Arc<crate::menu_probe::MenuProbe>,
}

fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GPUI_CEF_FRAME_STATS")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

impl BrowserClientState {
    pub(crate) fn new(
        browser: BrowserId,
        viewport: Arc<ViewportState>,
        store: Arc<FrameMailbox>,
        frames: Arc<dyn FrameSink>,
        ui: Arc<dyn UiDispatcher>,
        registry: Weak<UiRegistry>,
        policy: Arc<NativePolicy>,
        initial_url: &str,
    ) -> Self {
        let recovery = Arc::new(crate::resize::ResizeState::new());
        Self {
            browser,
            viewport,
            recovery: recovery.clone(),
            #[cfg(feature = "test-support")]
            frame_probe: Mutex::new(crate::test_support::FrameProbeState::default()),
            capture: Arc::new(CaptureEndpoint::new(store.budget())),
            frame_sink: CaptureFrameSink {
                mailbox: store.clone(),
                observer: frames,
                ui: ui.clone(),
                recovery: Some(recovery),
            },
            store,
            ui,
            registry,
            active: AtomicBool::new(true),
            find_unmanaged: AtomicBool::new(false),
            #[cfg(test)]
            ime_adapter_test: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            probe_console_kinds: AtomicU64::new(0),
            #[cfg(feature = "test-support")]
            menu_probe: Arc::new(crate::menu_probe::MenuProbe::new()),
            approved_data: Mutex::new({
                let mut approved = ApprovedDataPages::default();
                approved.approve_host(&policy.rules, initial_url);
                approved
            }),
            options: Arc::new(BrowserOptions::default()),
            cursor: Mutex::new(BrowserCursor::Arrow),
            cursor_slot: rust_cef_core::cursor::CursorSlot::new(),
            last_tooltip: Mutex::new(String::new()),
            last_status: Mutex::new(String::new()),
            first_frame_logged: AtomicBool::new(false),
            frame_stats_count: std::sync::atomic::AtomicU64::new(0),
            frame_stats_copy_us_sum: std::sync::atomic::AtomicU64::new(0),
            accessibility: crate::accessibility::RuntimeAccessibilityState::new(false),
            policy,
        }
    }

    pub(crate) fn with_options(mut self, options: BrowserOptions) -> Self {
        self.options = Arc::new(options);
        self
    }

    pub(crate) fn with_accessibility(
        mut self,
        accessibility: Arc<crate::accessibility::RuntimeAccessibilityState>,
    ) -> Self {
        self.accessibility = accessibility;
        self
    }

    pub(crate) fn with_cursor_slot(
        mut self,
        cursor_slot: rust_cef_core::cursor::CursorSlot,
    ) -> Self {
        self.cursor_slot = cursor_slot;
        self
    }

    pub(crate) fn set_viewport(&self, viewport: Viewport) -> Result<(), FrameError> {
        let old = self.viewport.get();
        if old == viewport {
            return Ok(());
        }
        let (width, height) = pixel_extent(viewport.width, viewport.height, viewport.scale)?;
        let wake = self.store.configure_view_with_scale(
            SurfaceBounds {
                x: 0,
                y: 0,
                width: viewport.width,
                height: viewport.height,
            },
            width,
            height,
            viewport.scale,
        )?;
        self.viewport.set(viewport);
        wake_frames(&self.store, &self.ui, wake);
        // Popup dimensions are independent DIP geometry; a DPI change must not
        // keep accepting its previous pixel layout under a fresh VIEW target.
        if old.scale != viewport.scale {
            // The mailbox atomically revoked both generations/old POPUP when
            // publishing the exact scale above. Only its DIP-to-pixel layout
            // needs adjusting here; there is no metadata/invalidation gap.
            let popup = self.store.snapshot().popup;
            if popup.bounds.width > 0 && popup.bounds.height > 0 {
                self.set_popup_bounds(popup.bounds)?;
            }
        }
        let view = self.store.snapshot().view;
        self.recovery.target(
            view.generation,
            viewport.width,
            viewport.height,
            viewport.scale,
            view.visible,
            std::time::Instant::now(),
        );
        if let Some(registry) = self.registry.upgrade() {
            registry.schedule_resize(self.browser, 0);
        }
        Ok(())
    }

    fn set_popup_bounds(&self, bounds: SurfaceBounds) -> Result<(), FrameError> {
        let scale = self.viewport.get().scale;
        let (width, height) = pixel_extent(bounds.width, bounds.height, scale)?;
        let wake = self.store.configure_popup(bounds, width, height)?;
        wake_frames(&self.store, &self.ui, wake);
        Ok(())
    }

    pub(crate) fn set_visible(&self, visible: bool) -> Result<(), FrameError> {
        let wake = self.store.set_visible(visible)?;
        wake_frames(&self.store, &self.ui, wake);
        if visible {
            let viewport = self.viewport.get();
            let view = self.store.snapshot().view;
            self.recovery.target(
                view.generation,
                viewport.width,
                viewport.height,
                viewport.scale,
                true,
                std::time::Instant::now(),
            );
            if let Some(registry) = self.registry.upgrade() {
                registry.schedule_resize(self.browser, 0);
            }
        } else {
            self.recovery.cancel(false);
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn frame_probe_control(
        &self,
        control: crate::test_support::FrameProbeControl,
    ) -> bool {
        use crate::test_support::FrameProbeControl;
        let mut state = self
            .frame_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match control {
            FrameProbeControl::DropView(drop) => {
                state.drop_view = drop;
                state.clear_incomplete();
            }
            FrameProbeControl::RejectIncompleteAndHold {
                generation,
                width,
                height,
            } => {
                let view = self.store.snapshot().view;
                if !view.visible
                    || !view
                        .frame
                        .is_some_and(|f| f.info.surface_generation == view.generation)
                    || !state.arm_incomplete(view.generation, (generation, width, height))
                {
                    return false;
                }
            }
            FrameProbeControl::RejectIncompleteUntilReleased => {
                let view = self.store.snapshot().view;
                if !view.visible || !state.arm_reject_until_released() {
                    return false;
                }
            }
            FrameProbeControl::ReleaseIncompleteRefusals => {
                if !state.release_refusals() {
                    return false;
                }
            }
            FrameProbeControl::RejectIncompleteFor { frames } => {
                let view = self.store.snapshot().view;
                if !view.visible || !state.arm_reject_for(frames) {
                    return false;
                }
            }
            FrameProbeControl::RejectNextIncomplete => {
                // Identity-free and non-terminal: the refused frame is dropped,
                // but drop_view stays false so later frames can publish.
                let view = self.store.snapshot().view;
                if !view.visible || !state.arm_reject_next_once() {
                    return false;
                }
            }
            FrameProbeControl::CaptureFaultAndInvalidate => {
                if state.capture_fault.is_some()
                    || state.capture_fault_armed
                    || state.drop_view
                    || state.incomplete_pending
                {
                    return false;
                }
                state.capture_fault_armed = true;
            }
            FrameProbeControl::ResumeCaptureAndInvalidate => {
                if state.capture_fault.is_none() || !state.drop_view {
                    return false;
                }
                state.drop_view = false;
            }
            FrameProbeControl::InvalidateView => {}
        }
        state.applied += 1;
        true
    }

    #[cfg(feature = "test-support")]
    fn capture_fault_probe(
        &self,
        surface: SurfaceKind,
        backend: &'static str,
        capture: impl FnOnce(
            bool,
        ) -> Result<
            rust_cef_core::ports::OwnedFrame,
            rust_cef_core::ports::EngineError,
        >,
    ) -> Result<rust_cef_core::ports::OwnedFrame, rust_cef_core::ports::EngineError> {
        use rust_cef_core::ports::{CaptureFailureKind, EngineError};
        let inject = if surface == SurfaceKind::View {
            let mut state = self
                .frame_probe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let armed = std::mem::take(&mut state.capture_fault_armed);
            if armed {
                state.drop_view = true;
            }
            armed
        } else {
            false
        };
        if !inject {
            return capture(false);
        }
        let before = self.store.snapshot().view.frame;
        let budget = self.store.budget();
        let usage = budget.stats().used;
        let native_before = self.capture.diagnostics();
        let result = capture(true);
        let snapshot = self.store.snapshot();
        let native_after = self.capture.diagnostics();
        let io = |d: &crate::capture::CaptureDiagnostics| (d.opens, d.copies, d.maps, d.unmaps);
        let kind = if backend == "D3d11" {
            CaptureFailureKind::IncompleteFrame
        } else {
            CaptureFailureKind::InvalidDimensions
        };
        let witness = crate::test_support::CallbackCaptureFault {
            browser: (self.browser.runtime().value(), self.browser.local()),
            thread: crate::executor::current_thread_id(),
            backend,
            expected_error_returned: result.as_ref().err()
                == Some(&EngineError::Capture { kind, code: None }),
            same_frame: before
                .as_ref()
                .zip(snapshot.view.frame.as_ref())
                .is_some_and(|(old, after)| Arc::ptr_eq(old, after)),
            good_frame: before
                .as_ref()
                .map(|f| (f.info.surface_generation, f.info.sequence)),
            generation_after: snapshot.view.generation,
            budget_used: (usage, budget.stats().used),
            native_io_before: io(&native_before),
            native_io_after: io(&native_after),
            live_imports_after: native_after.live_imports,
        };
        self.frame_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture_fault = Some(witness);
        result
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn frame_probe_state(&self) -> crate::test_support::FrameProbeState {
        self.frame_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(feature = "test-support")]
    fn frame_probe_gate(&self, generation: u64, width: u32, height: u32) -> bool {
        use crate::test_support::FrameProbeViewGate;
        let outcome = self
            .frame_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gate_view((generation, width, height));
        match outcome {
            FrameProbeViewGate::Pass => return true,
            FrameProbeViewGate::Dropped => self.recovery.note_probe_drop(),
            FrameProbeViewGate::RejectedIncomplete => self.recovery.note_incomplete(),
        }
        false
    }

    pub(crate) fn deny(&self, action: PolicyAction) {
        self.policy.deny(Some(self.browser), action);
    }

    /// Test-only hook: the real handler body still runs, but without CEF's UI
    /// sequence assertion, which no unit test can satisfy.
    #[cfg(test)]
    pub(crate) fn prepare_ime_adapter_test(&self) {
        self.ime_adapter_test.store(true, Ordering::Release);
    }

    pub(crate) fn audit_native(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.audit_native_entry();
        }
    }

    pub(crate) fn approve_host_navigation(&self, url: &str) {
        self.approved_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .approve_host(&self.policy.rules, url);
    }

    fn clear_data_approvals(&self) {
        self.approved_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn allows_navigation_for_script(&self, url: &str, main: bool) -> bool {
        self.allows_navigation(url, main)
    }

    pub(crate) fn handle_popup_request(
        &self,
        target_url: Option<&CefString>,
        target_frame_name: Option<&CefString>,
        target_disposition: WindowOpenDisposition,
        user_gesture: ::std::os::raw::c_int,
    ) {
        if let Some(svc) = self.policy.host_capabilities().popup {
            let target_url = target_url.map(ToString::to_string).unwrap_or_default();
            let target_frame_name = target_frame_name
                .map(ToString::to_string)
                .unwrap_or_default();
            let policy_allowed = self.allows_navigation(&target_url, true);
            let registry = self.registry.upgrade();
            let _guard = registry.as_ref().map(|r| r.host_requests.enter_callback());
            svc.on_popup_request(rust_cef_core::host_capabilities::PopupRequest {
                browser: self.browser,
                target_url,
                target_frame_name,
                disposition: map_window_disposition(target_disposition),
                user_gesture: user_gesture != 0,
                policy_allowed,
            });
        } else {
            self.deny(PolicyAction::Popup);
        }
    }

    fn allows_navigation(&self, url: &str, main: bool) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if self.policy.rules.allows_page_navigation(url) {
            return true;
        }
        main && self
            .approved_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(url)
    }

    pub(crate) fn allows_resource(&self, url: &str, main_navigation: bool) -> bool {
        self.active.load(Ordering::Acquire)
            && (self.policy.rules.allows_network(url)
                || (main_navigation
                    && self
                        .approved_data
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains(url)))
    }

    fn navigation_event(&self, event: BrowserEvent) {
        if let Some(registry) = self.registry.upgrade() {
            registry.navigation_event(self.browser, event);
        }
    }

    fn navigation_observation(&self, event: BrowserEvent, source_url: Option<String>) {
        if let Some(registry) = self.registry.upgrade() {
            registry.navigation_observation(self.browser, event, source_url);
        }
    }
}

wrap_render_handler! {
    struct RuntimeRenderHandler {
        state: Arc<BrowserClientState>,
    }

    impl RenderHandler {
        fn accessibility_handler(&self) -> Option<AccessibilityHandler> {
            if self.state.options.accessibility_state == CefFeatureState::Disabled {
                return None;
            }
            Some(crate::accessibility::new_handler(
                self.state.browser,
                self.state.accessibility.clone(),
                self.state.registry.clone(),
                self.state.ui.clone(),
            ))
        }

        fn start_dragging(
            &self,
            browser: Option<&mut Browser>,
            drag_data: Option<&mut DragData>,
            allowed_ops: DragOperationsMask,
            x: i32,
            y: i32,
        ) -> i32 {
            if self.state.options.outbound_drag_policy == OutboundDragPolicy::Deny {
                #[cfg(feature = "test-support")]
                {
                    // Record only presence and ownership metadata, never DragData.
                    let thread = crate::executor::current_thread_id();
                    let active = self.state.active.load(Ordering::Acquire);
                    let arguments_present = browser.is_some() && drag_data.is_some();
                    let mut probes = self
                        .state
                        .frame_probe
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let denial = probes.outbound_drag_denial.get_or_insert_default();
                    denial.callbacks = denial.callbacks.saturating_add(1);
                    denial.thread = thread;
                    denial.active = active;
                    denial.arguments_present = arguments_present;
                }
                // CEF aborts the drag when we return false. No system drag loop or
                // embedder DragSource*Ended* calls are owed on this path.
                return 0;
            }
            if !self.state.active.load(Ordering::Acquire) {
                return 0;
            }
            let (Some(_browser), Some(drag_data), Some(registry)) =
                (browser, drag_data, self.state.registry.upgrade())
            else {
                return 0;
            };
            let scale = self.state.viewport.get().scale;
            let payload = crate::outbound_drag::extract_drag_payload(drag_data, scale);
            if payload.is_empty() {
                return 0;
            }
            let mask = crate::outbound_drag::cef_mask_to_core(allowed_ops);
            i32::from(
                registry
                    .begin_outbound_drag(self.state.browser, payload, mask, x, y)
                    .is_ok(),
            )
        }

        fn update_drag_cursor(&self, _browser: Option<&mut Browser>, operation: DragOperationsMask) {
            if !self.state.active.load(Ordering::Acquire) {
                return;
            }
            let mask = crate::outbound_drag::cef_mask_to_core(operation);
            if let Some(registry) = self.state.registry.upgrade() {
                registry.update_drag_cursor(
                    self.state.browser,
                    mask,
                    self.state.options.outbound_drag_policy == OutboundDragPolicy::AllowAsync,
                );
            }
        }

        fn on_virtual_keyboard_requested(&self, _browser: Option<&mut Browser>, input_mode: TextInputMode) {
            if let Some(registry) = self.state.registry.upgrade() {
                registry.set_accepts_text(self.state.browser, input_mode != TextInputMode::NONE);
            }
        }

        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let viewport = self.state.viewport.get();
            *rect = Rect { x: 0, y: 0, width: viewport.width, height: viewport.height };
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            view_x: ::std::os::raw::c_int,
            view_y: ::std::os::raw::c_int,
            screen_x: Option<&mut ::std::os::raw::c_int>,
            screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if !self.state.active.load(Ordering::Acquire) {
                return 0;
            }
            let Some((sx, sy)) = self.state.viewport.screen_point(view_x, view_y) else {
                return 0;
            };
            if let Some(out_x) = screen_x {
                *out_x = sx;
            }
            if let Some(out_y) = screen_y {
                *out_y = sy;
            }
            1
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(screen_info) = screen_info else { return 0 };
            let (viewport, screen_origin) = self.state.viewport.snapshot();
            screen_info.device_scale_factor = viewport.scale;
            screen_info.depth = 32;
            screen_info.depth_per_component = 8;
            if let Some((ox, oy)) = screen_origin
                && let Some((monitor_rect, work_rect)) =
                    monitor_screen_rects(ox, oy, viewport.scale)
            {
                screen_info.rect = monitor_rect;
                screen_info.available_rect = work_rect;
            } else {
                let view = Rect { x: 0, y: 0, width: viewport.width, height: viewport.height };
                screen_info.rect = view.clone();
                screen_info.available_rect = view;
            }
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            if !self.state.active.load(Ordering::Acquire) { return; }
            match self.state.store.set_popup_visible(show != 0) {
                Ok(wake) => wake_frames(&self.state.store, &self.state.ui, wake),
                Err(error) => self.report_capture_failure(error),
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            if !self.state.active.load(Ordering::Acquire) { return; }
            let Some(rect) = rect else { return; };
            if let Err(error) = self.state.set_popup_bounds(SurfaceBounds {
                x: rect.x, y: rect.y, width: rect.width, height: rect.height,
            }) { self.report_capture_failure(error); }
        }

        /// Copies the borrowed range/rectangle list into bounded owned geometry.
        /// The last rectangle is not treated as the caret; the range is kept as
        /// reported. No text, GPUI object or window handle crosses this boundary,
        /// and the only notification is the original capacity-one wake.
        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            // Only the CEF UI sequence may publish geometry, and a callback that
            // lands after OnBeforeClose is dropped rather than reviving a closed
            // instance. Unit tests drive the same body without a CEF UI thread;
            // release builds always require the real qualification.
            if !ime_callback_qualified(&self.state) || !self.state.active.load(Ordering::Acquire) {
                return;
            }
            let Some(registry) = self.state.registry.upgrade() else { return };
            let range = selected_range.map(|range| Utf16Range {
                start: range.from,
                end: range.to,
            });
            let bounds = match character_bounds {
                None => ImeReportedBounds::Missing,
                Some(bounds) => {
                    let mut next = bounds.iter();
                    ImeReportedBounds::from_reported(bounds.len(), || {
                        next.next().map(|rect| ImeRect {
                            x: rect.x,
                            y: rect.y,
                            width: rect.width,
                            height: rect.height,
                        })
                    })
                }
            };
            let _ = registry.observe_ime(self.state.browser, range, bounds);
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            element_type: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if !self.state.active.load(Ordering::Acquire) { return; }
            let surface = if element_type == PaintElementType::VIEW {
                SurfaceKind::View
            } else if element_type == PaintElementType::POPUP {
                SurfaceKind::Popup
            } else { return; };
            if buffer.is_null() || width < 0 || height < 0 {
                self.report_capture_failure(FrameError::InvalidDimensions);
                return;
            }
            if width == 0
                || height == 0
                || width > MAX_OSR_TEXTURE_DIMENSION
                || height > MAX_OSR_TEXTURE_DIMENSION
            {
                return;
            }
            let (width, height) = (width as u32, height as u32);
            let Some(length) = frame_len(width, height) else {
                self.report_capture_failure(FrameError::InvalidDimensions);
                return;
            };
            let token = match self.state.store.capture_token(surface, width, height) {
                Ok(token) => token,
                Err(error) => { self.report_capture_failure(error); return; }
            };
            #[cfg(feature="test-support")]
            if surface==SurfaceKind::View && !self.state.frame_probe_gate(token.generation(),width,height) {return;}
            // SAFETY: fixed CEF guarantees the whole width*height*4 BGRA buffer
            // for this callback. Only the synchronous copy reads the borrowed
            // bytes; both mailbox and observer receive budget-owned pixels.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, length) };
            let copy_started = std::time::Instant::now();
            #[cfg(feature="test-support")]
            let captured = self.state.capture_fault_probe(surface, "Software", |inject| {
                // Keep the original CEF buffer borrow intact; the tested copy
                // sees one fewer byte and must reject without publishing.
                let input = if inject { &pixels[..pixels.len() - 1] } else { pixels };
                self.state.capture.software(token.info(), input)
            });
            #[cfg(not(feature="test-support"))]
            let captured = self.state.capture.software(token.info(), pixels);
            let copy_us = copy_started.elapsed().as_micros() as u64;
            let frame = match captured {
                Ok(frame) => frame,
                Err(error) => { self.report_engine_failure(error); return; }
            };
            if let Err(error) = self.state.frame_sink.publish_checked(frame) {
                self.report_capture_failure(error);
                return;
            }
            if !self.state.first_frame_logged.swap(true, Ordering::AcqRel) {
                eprintln!(
                    "[gpui-cef] First frame active path: CpuPaint (Software OSR) browser={:?} size={}x{} copy_us={}",
                    self.state.browser, width, height, copy_us
                );
            }
            if frame_stats_enabled() {
                let count = self.state.frame_stats_count.fetch_add(1, Ordering::AcqRel) + 1;
                let total_us =
                    self.state.frame_stats_copy_us_sum.fetch_add(copy_us, Ordering::AcqRel)
                        + copy_us;
                if count == 1 || count % 60 == 0 {
                    eprintln!(
                        "[gpui-cef] Frame stats (CpuPaint): browser={:?} frames={} last_copy_us={} avg_copy_us={}",
                        self.state.browser,
                        count,
                        copy_us,
                        total_us / count
                    );
                }
            }
            if let Some(registry) = self.state.registry.upgrade() {
                registry.note_software_frame();
            }
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            element_type: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if !self.state.active.load(Ordering::Acquire) { return; }
            let surface = if element_type == PaintElementType::VIEW { SurfaceKind::View }
                else if element_type == PaintElementType::POPUP { SurfaceKind::Popup }
                else { return; };
            let Some(info) = info else {
                self.report_capture_failure(FrameError::InvalidDimensions); return;
            };
            let coded = &info.extra.coded_size;
            let vis = &info.extra.visible_rect;
            if coded.width == 0
                || coded.height == 0
                || vis.width == 0
                || vis.height == 0
                || coded.width > MAX_OSR_TEXTURE_DIMENSION
                || coded.height > MAX_OSR_TEXTURE_DIMENSION
                || vis.width > MAX_OSR_TEXTURE_DIMENSION
                || vis.height > MAX_OSR_TEXTURE_DIMENSION
            {
                return;
            }
            if let Err(error) = crate::capture::validate_accelerated(info) {
                #[cfg(feature="test-support")]
                if matches!(&error,rust_cef_core::ports::EngineError::Capture {kind:rust_cef_core::ports::CaptureFailureKind::IncompleteFrame,..}) {
                    // The pure validator has not mutated Source. Observe only
                    // this refusal path, never prolong old pixels on success.
                    let before=(self.state.store.snapshot(),self.state.recovery.diagnostics(),self.state.capture.diagnostics());
                    let after=self.state.store.snapshot();
                    let recovery=self.state.recovery.diagnostics();
                    let capture=self.state.capture.diagnostics();
                    let identity=|frame:&Arc<rust_cef_core::ports::OwnedFrame>| {
                        (frame.info.surface_generation,frame.info.sequence,frame.info.width,frame.info.height)
                    };
                    let rect=|r:&cef::Rect| (r.x,r.y,r.width,r.height);
                    let witness=crate::test_support::NativeFrameRejection {
                        browser:format!("{:?}",self.state.browser),surface:if surface==SurfaceKind::View {"View"} else {"Popup"},
                        thread:crate::executor::current_thread_id(),
                        failure_ordinal:before.2.failures.saturating_add(1),
                        frame_sha256:before.0.view.frame.as_ref().and_then(|frame|{frame.cpu_pixels().map(|px|{use sha2::Digest as _;format!("{:x}",sha2::Sha256::digest(px.as_slice()))})}),
                        coded:(info.extra.coded_size.width,info.extra.coded_size.height),
                        visible:rect(&info.extra.visible_rect),content:rect(&info.extra.content_rect),
                        source_generation_before:before.0.view.generation,source_generation_after:after.view.generation,
                        frame_before:before.0.view.frame.as_ref().map(identity),frame_after:after.view.frame.as_ref().map(identity),
                        same_frame:match (&before.0.view.frame,&after.view.frame) {(Some(a),Some(b))=>Arc::ptr_eq(a,b),(None,None)=>true,_=>false},
                        recovery_status_before:before.1.status,recovery_status_after:recovery.status,
                        target_generation:before.1.generation,target:(before.1.expected_width,before.1.expected_height),
                        acks_before:before.1.acks,acks_after:recovery.acks,
                        native_io_before:(before.2.opens,before.2.copies,before.2.maps,before.2.unmaps),
                        native_io_after:(capture.opens,capture.copies,capture.maps,capture.unmaps),
                    };
                    let mut probe=self.state.frame_probe.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if probe.native_rejections.len()<32 {probe.native_rejections.push(witness);} else {probe.native_rejections_overflow=true;}
                }
                self.report_engine_failure(error); return;
            }
            let rect = &info.extra.visible_rect;
            let token = match self.state.store.capture_token(surface, rect.width as u32, rect.height as u32) {
                Ok(token) => token,
                Err(error) => { self.report_capture_failure(error); return; }
            };
            #[cfg(feature="test-support")]
            if surface==SurfaceKind::View && !self.state.frame_probe_gate(token.generation(),token.width(),token.height()) {return;}
            // Zero-CPU-copy capture copies on the CEF UI thread into the owned triple-buffered
            // shared texture pool; `Flush()` completes before `Slot` lease publication.
            let use_shared_pool = self.state.options.accelerated_osr;
            let copy_us = std::cell::Cell::new(0u64);
            #[cfg(feature="test-support")]
            let captured = self.state.capture_fault_probe(surface, "D3d11", |inject| {
                if inject {
                    self.state.capture.accelerated_after_map_probe(token.info(), info)
                } else if use_shared_pool {
                    self.state
                        .capture
                        .accelerated_shared(token.info(), info, || {
                            self.state.store.try_reclaim_unconsumed_gpu_slot(surface)
                        })
                        .map(|(frame, us)| {
                            copy_us.set(us);
                            frame
                        })
                } else {
                    self.state.capture.accelerated(token.info(), info)
                }
            });
            #[cfg(not(feature="test-support"))]
            let captured = if use_shared_pool {
                self.state
                    .capture
                    .accelerated_shared(token.info(), info, || {
                        self.state.store.try_reclaim_unconsumed_gpu_slot(surface)
                    })
                    .map(|(frame, us)| {
                        copy_us.set(us);
                        frame
                    })
            } else {
                self.state.capture.accelerated(token.info(), info)
            };
            let copy_us = copy_us.get();
            let frame = match captured {
                Ok(frame) => frame,
                Err(rust_cef_core::ports::EngineError::Capture {
                    kind: rust_cef_core::ports::CaptureFailureKind::Busy,
                    ..
                }) => {
                    // All pooled GPU slots are transiently busy between producer and UI;
                    // drop this intermediate frame rather than raising a fatal Browser fault.
                    return;
                }
                Err(error) => { self.report_engine_failure(error); return; }
            };
            let gpu_meta = frame.gpu_texture().map(|gpu| {
                (
                    gpu.slot_index,
                    gpu.generation,
                    gpu.is_nt_handle,
                    gpu.keyed_mutex_key.is_some(),
                    gpu.width,
                    gpu.height,
                )
            });
            if let Err(error) = self.state.frame_sink.publish_checked(frame) {
                self.report_capture_failure(error); return;
            }
            if let Some((slot_index, pool_gen, is_nt, has_km, width, height)) = gpu_meta {
                if !self.state.first_frame_logged.swap(true, Ordering::AcqRel) {
                    eprintln!(
                        "[gpui-cef] First frame active path: AcceleratedPaint (D3D11 Shared Texture) browser={:?} size={}x{} slot={} pool_gen={} nt_handle={} keyed_mutex={} copy_us={}",
                        self.state.browser, width, height, slot_index, pool_gen, is_nt, has_km, copy_us
                    );
                }
                if frame_stats_enabled() {
                    let count = self.state.frame_stats_count.fetch_add(1, Ordering::AcqRel) + 1;
                    let total_us =
                        self.state.frame_stats_copy_us_sum.fetch_add(copy_us, Ordering::AcqRel)
                            + copy_us;
                    if count == 1 || count % 60 == 0 {
                        eprintln!(
                            "[gpui-cef] Frame stats (AcceleratedPaint): browser={:?} frames={} size={}x{} pool_gen={} last_copy_us={} avg_copy_us={}",
                            self.state.browser,
                            count,
                            width,
                            height,
                            pool_gen,
                            copy_us,
                            total_us / count
                        );
                    }
                }
            }
            if let Some(registry) = self.state.registry.upgrade() { registry.note_d3d11_frame(); }
        }
    }
}

/// Real UI-sequence qualification for adapter callbacks. CEF invokes render
/// handlers on its UI thread, so anything else is a stale/foreign call and must
/// not publish geometry. Only unit fixtures opt out, through a test-only flag
/// that has no production constructor.
fn ime_callback_qualified(state: &BrowserClientState) -> bool {
    #[cfg(test)]
    if state.ime_adapter_test.load(Ordering::Acquire) {
        return true;
    }
    let _ = state;
    cef::currently_on(ThreadId::UI) != 0
}

impl RuntimeRenderHandler {
    fn report_engine_failure(&self, error: rust_cef_core::ports::EngineError) {
        // A callback already in flight can observe main-thread abandon. This
        // is terminal cancellation, not a new capture fault after Source close.
        if error == rust_cef_core::ports::EngineError::Closed {
            return;
        }
        self.state.capture.record_failure(&error);
        if let Some(registry) = self.state.registry.upgrade() {
            registry.capture_failed(self.state.browser, error);
        }
    }

    fn report_capture_failure(&self, error: FrameError) {
        // Expected stale/hidden/closed deliveries must not resurrect a terminal
        // Browser or turn cancellation into an unrelated Runtime failure.
        if matches!(
            error,
            FrameError::Hidden | FrameError::Closed | FrameError::StaleGeneration
        ) {
            return;
        }
        self.report_engine_failure(crate::capture::frame_error(error));
    }
}

wrap_life_span_handler! {
    struct RuntimeLifeSpanHandler {
        state: Arc<BrowserClientState>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            self.state.handle_popup_request(
                target_url,
                target_frame_name,
                target_disposition,
                user_gesture,
            );
            // Unmanaged popup windows are never created.
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            // Clone adds a CEF reference; the registry keeps it until
            // OnBeforeClose. The callback-scoped wrapper is not stored.
            let native = browser.clone();
            if let Some(registry) = self.state.registry.upgrade() {
                registry.after_created(self.state.browser, native);
            }
        }

        fn do_close(&self, browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            if self.state.options.surface_mode.is_native_child_hwnd() {
                let hwnd = browser
                    .and_then(|b| b.host())
                    .and_then(|host| crate::native_hwnd::hwnd_from_cef(host.window_handle()));
                if let Some(registry) = self.state.registry.upgrade()
                    && registry.schedule_native_child_destroy(self.state.browser, hwnd)
                {
                    return 1;
                }
            }
            // Windowless rendering (and fallback when child HWND is already gone):
            // returning false lets CEF proceed with teardown immediately.
            0
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let _ = browser;
            self.state.active.store(false, Ordering::Release);
            self.state.clear_data_approvals();
            if let Some(registry) = self.state.registry.upgrade() {
                registry.before_close(self.state.browser);
            }
        }
    }
}

pub(crate) fn map_window_disposition(
    disposition: WindowOpenDisposition,
) -> rust_cef_core::host_capabilities::PopupDisposition {
    use rust_cef_core::host_capabilities::PopupDisposition;
    match disposition {
        WindowOpenDisposition::CURRENT_TAB => PopupDisposition::CurrentTab,
        WindowOpenDisposition::SINGLETON_TAB => PopupDisposition::SingletonTab,
        WindowOpenDisposition::NEW_FOREGROUND_TAB => PopupDisposition::NewForegroundTab,
        WindowOpenDisposition::NEW_BACKGROUND_TAB => PopupDisposition::NewBackgroundTab,
        WindowOpenDisposition::NEW_POPUP => PopupDisposition::NewPopup,
        WindowOpenDisposition::NEW_WINDOW => PopupDisposition::NewWindow,
        WindowOpenDisposition::SAVE_TO_DISK => PopupDisposition::SaveToDisk,
        WindowOpenDisposition::OFF_THE_RECORD => PopupDisposition::OffTheRecord,
        WindowOpenDisposition::IGNORE_ACTION => PopupDisposition::IgnoreAction,
        other => PopupDisposition::Unknown(other.get_raw()),
    }
}

pub(crate) fn map_cef_cursor(type_: CursorType) -> BrowserCursor {
    match type_ {
        CursorType::POINTER => BrowserCursor::Arrow,
        CursorType::IBEAM => BrowserCursor::IBeam,
        CursorType::CROSS => BrowserCursor::Crosshair,
        CursorType::GRABBING => BrowserCursor::ClosedHand,
        CursorType::GRAB => BrowserCursor::OpenHand,
        CursorType::HAND => BrowserCursor::PointingHand,
        CursorType::WESTRESIZE => BrowserCursor::ResizeLeft,
        CursorType::EASTRESIZE => BrowserCursor::ResizeRight,
        CursorType::EASTWESTRESIZE => BrowserCursor::ResizeLeftRight,
        CursorType::NORTHRESIZE => BrowserCursor::ResizeUp,
        CursorType::SOUTHRESIZE => BrowserCursor::ResizeDown,
        CursorType::NORTHSOUTHRESIZE => BrowserCursor::ResizeUpDown,
        CursorType::NORTHWESTRESIZE
        | CursorType::SOUTHEASTRESIZE
        | CursorType::NORTHWESTSOUTHEASTRESIZE => BrowserCursor::ResizeUpLeftDownRight,
        CursorType::NORTHEASTRESIZE
        | CursorType::SOUTHWESTRESIZE
        | CursorType::NORTHEASTSOUTHWESTRESIZE => BrowserCursor::ResizeUpRightDownLeft,
        CursorType::COLUMNRESIZE => BrowserCursor::ResizeColumn,
        CursorType::ROWRESIZE => BrowserCursor::ResizeRow,
        CursorType::VERTICALTEXT => BrowserCursor::IBeamVertical,
        CursorType::NOTALLOWED | CursorType::NODROP | CursorType::DND_NONE => {
            BrowserCursor::OperationNotAllowed
        }
        CursorType::ALIAS | CursorType::DND_LINK => BrowserCursor::DragLink,
        CursorType::COPY | CursorType::DND_COPY => BrowserCursor::DragCopy,
        CursorType::CONTEXTMENU => BrowserCursor::ContextualMenu,
        CursorType::CUSTOM => BrowserCursor::CustomUnsupported,
        _ => BrowserCursor::Arrow,
    }
}

pub(crate) fn map_log_severity(level: LogSeverity) -> ConsoleSeverity {
    match level {
        LogSeverity::VERBOSE => ConsoleSeverity::Verbose,
        LogSeverity::INFO => ConsoleSeverity::Info,
        LogSeverity::WARNING => ConsoleSeverity::Warning,
        LogSeverity::ERROR => ConsoleSeverity::Error,
        LogSeverity::FATAL => ConsoleSeverity::Fatal,
        _ => ConsoleSeverity::Default,
    }
}

pub(crate) fn read_cef_string_list(
    list: &mut CefStringList,
    max_items: usize,
    max_item_bytes: usize,
) -> Vec<String> {
    let raw: *mut cef::sys::_cef_string_list_t = list.into();
    if raw.is_null() || max_items == 0 {
        return Vec::new();
    }
    let count = unsafe { cef::sys::cef_string_list_size(raw) }.min(max_items);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let mut writable = WritableCefString::new();
        let ok = unsafe { cef::sys::cef_string_list_value(raw, index, writable.as_mut_raw()) };
        if ok != 0 {
            out.push(truncate_utf8_bytes(
                &writable.to_string_lossy(),
                max_item_bytes,
            ));
        }
    }
    out
}

wrap_display_handler! {
    struct RuntimeDisplayHandler {
        state: Arc<BrowserClientState>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if frame.is_some_and(|frame| frame.is_main() != 0) {
                if let Some(url) = url {
                    self.state.navigation_event(BrowserEvent::AddressChanged(url.to_string()));
                }
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                self.state.navigation_event(BrowserEvent::TitleChanged(title.to_string()));
            }
        }

        fn on_favicon_urlchange(
            &self,
            _browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            let urls = icon_urls
                .map(|list| read_cef_string_list(list, 8, 512))
                .unwrap_or_default();
            self.state
                .navigation_event(BrowserEvent::FaviconUrlsChanged(urls));
        }

        fn on_fullscreen_mode_change(&self, _browser: Option<&mut Browser>, fullscreen: i32) {
            self.state
                .navigation_event(BrowserEvent::FullscreenModeChanged(fullscreen != 0));
        }

        fn on_tooltip(&self, _browser: Option<&mut Browser>, text: Option<&mut CefString>) -> i32 {
            if !self.state.options.enable_tooltip_events {
                return 0;
            }
            let raw = text.map(|t| t.to_string()).unwrap_or_default();
            let bounded = truncate_utf8_bytes(&raw, 512);
            let changed = {
                let mut last = self
                    .state
                    .last_tooltip
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if *last == bounded {
                    false
                } else {
                    *last = bounded.clone();
                    true
                }
            };
            if changed {
                self.state
                    .navigation_event(BrowserEvent::TooltipChanged(bounded));
            }
            1
        }

        fn on_status_message(&self, _browser: Option<&mut Browser>, value: Option<&CefString>) {
            let raw = value.map(ToString::to_string).unwrap_or_default();
            let bounded = truncate_utf8_bytes(&raw, 512);
            let changed = {
                let mut last = self
                    .state
                    .last_status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if *last == bounded {
                    false
                } else {
                    *last = bounded.clone();
                    true
                }
            };
            if changed {
                self.state
                    .navigation_event(BrowserEvent::StatusMessageChanged(bounded));
            }
        }

        fn on_loading_progress_change(&self, _browser: Option<&mut Browser>, progress: f64) {
            self.state
                .navigation_event(BrowserEvent::LoadingProgressChanged(progress_to_per_mille(
                    progress,
                )));
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            cursor: cef::sys::HCURSOR,
            type_: CursorType,
            custom_cursor_info: Option<&CursorInfo>,
        ) -> i32 {
            let mapped = map_cef_cursor(type_);
            let changed = {
                let mut current = self
                    .state
                    .cursor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if *current == mapped {
                    false
                } else {
                    *current = mapped;
                    true
                }
            };
            let native = crate::cursor_win::resolve_cef_cursor(cursor, type_, custom_cursor_info);
            let native_changed = self.state.cursor_slot.set(native);
            if changed {
                self.state
                    .navigation_event(BrowserEvent::CursorChanged(mapped));
            } else if native_changed {
                let _ = self.state.ui.wake();
            }
            // In windowed child HWND mode, return 0 so Chromium/Win32 updates the native child cursor.
            i32::from(!self.state.options.surface_mode.is_native_child_hwnd())
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            line: i32,
        ) -> i32 {
            let emit_event = self.state.options.console_policy.enabled;
            let raw_msg = if emit_event || cfg!(feature = "test-support") {
                message.map(ToString::to_string)
            } else {
                None
            };
            if emit_event {
                let max_bytes = self.state.options.console_policy.max_message_bytes;
                let bounded = truncate_utf8_bytes(raw_msg.as_deref().unwrap_or_default(), max_bytes);
                self.state.navigation_event(BrowserEvent::ConsoleMessage {
                    severity: map_log_severity(level),
                    message: bounded,
                    line,
                });
            }
            #[cfg(feature = "test-support")]
            {
                // Bounded diagnostic categories only: never log page source,
                // message text, data URI, URL/query, or native pointers.
                let text = raw_msg.unwrap_or_default().to_ascii_lowercase();
                let mut bits = 0u64;
                for (index, needle) in [
                    "syntaxerror",
                    "cors",
                    "local network",
                    "address space",
                    "mixed content",
                    "blocked_by_client",
                    "notallowed",
                    "failed to fetch",
                ]
                .iter()
                .enumerate()
                {
                    if text.contains(needle) {
                        bits |= 1 << (index + 1);
                    }
                }
                if bits == 0 {
                    bits = 1;
                }
                let previous = self
                    .state
                    .probe_console_kinds
                    .fetch_or(bits, Ordering::Relaxed);
                if bits & !previous != 0 {
                    eprintln!(
                        "probe-console runtime={} browser={} category_bits={}",
                        self.state.browser.runtime().value(),
                        self.state.browser.local(),
                        bits
                    );
                }
                return 1;
            }
            #[cfg(not(feature = "test-support"))]
            {
                i32::from(emit_event)
            }
        }
    }
}

wrap_focus_handler! {
    struct RuntimeFocusHandler {
        state: Arc<BrowserClientState>,
    }

    impl FocusHandler {
        fn on_take_focus(&self, _browser: Option<&mut Browser>, next: i32) {
            self.state
                .navigation_event(BrowserEvent::FocusTaken { next: next != 0 });
        }

        fn on_set_focus(&self, _browser: Option<&mut Browser>, _source: FocusSource) -> i32 {
            // Allow focus to be set by default (matches CefSharp ClientAdapter::OnSetFocus).
            0
        }

        fn on_got_focus(&self, _browser: Option<&mut Browser>) {
            self.state.navigation_event(BrowserEvent::GotFocus);
        }
    }
}

wrap_keyboard_handler! {
    struct RuntimeKeyboardHandler {
        state: Arc<BrowserClientState>,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: Option<&mut cef::sys::MSG>,
            _is_keyboard_shortcut: Option<&mut i32>,
        ) -> i32 {
            let Some(event) = event else {
                return 0;
            };
            if event.type_ != KeyEventType::RAWKEYDOWN && event.type_ != KeyEventType::KEYDOWN {
                return 0;
            }
            if let Some(id) = self
                .state
                .options
                .shortcut_filter
                .match_key(event.windows_key_code, event.modifiers)
            {
                self.state
                    .navigation_event(BrowserEvent::HostShortcutTriggered { id });
                return 1;
            }
            0
        }
    }
}

fn frame_load_error(is_main_frame: bool, error: Errorcode) -> BrowserEvent {
    let code = error.get_raw();
    if error == Errorcode::ABORTED {
        BrowserEvent::LoadCancelled {
            is_main_frame,
            code,
        }
    } else {
        BrowserEvent::LoadFailed {
            is_main_frame,
            code,
        }
    }
}

wrap_load_handler! {
    struct RuntimeLoadHandler {
        state: Arc<BrowserClientState>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            self.state.navigation_event(BrowserEvent::LoadingChanged {
                loading: is_loading != 0,
                can_go_back: can_go_back != 0,
                can_go_forward: can_go_forward != 0,
            });
        }

        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if let Some(frame) = frame {
                self.state.navigation_event(BrowserEvent::LoadStarted {
                    is_main_frame: frame.is_main() != 0,
                });
            }
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            if let Some(frame) = frame {
                self.state.navigation_observation(BrowserEvent::LoadFinished {
                    is_main_frame: frame.is_main() != 0,
                    http_status_code,
                }, Some(CefString::from(&frame.url()).to_string()));
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            if let Some(frame) = frame {
                self.state.navigation_observation(
                    frame_load_error(frame.is_main() != 0, error_code),
                    failed_url.map(ToString::to_string),
                );
            }
        }
    }
}

wrap_request_handler! {
    struct RuntimeRequestHandler {
        state: Arc<BrowserClientState>,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(frame) = frame else { self.state.deny(PolicyAction::Navigation); return 1; };
            let Some(request) = request else { self.state.deny(PolicyAction::Navigation); return 1; };
            let url = CefString::from(&request.url()).to_string();
            let main = frame.is_main() != 0;
            if !self.state.allows_navigation(&url, main) {
                let external = url::Url::parse(&url).is_ok_and(|parsed| {
                    !matches!(
                        parsed.scheme(),
                        "http" | "https" | "data" | "about" | "file" | "javascript" | "blob"
                    ) && !self.state.policy.rules.is_custom_scheme(parsed.scheme())
                });
                self.state.deny(if external { PolicyAction::ExternalProtocol } else { PolicyAction::Navigation });
                return 1;
            }
            self.state.navigation_observation(BrowserEvent::NavigationRequested {
                is_main_frame: main,
                is_redirect: is_redirect != 0,
            }, Some(url));
            0
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            self.state
                .handle_popup_request(target_url, None, target_disposition, user_gesture);
            1
        }

        fn resource_request_handler(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>,
            _request: Option<&mut Request>, is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int, _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>) -> Option<ResourceRequestHandler> {
            let main = frame.is_some_and(|frame| frame.is_main() != 0);
            Some(crate::resource_policy::browser_handler(self.state.clone(), is_navigation != 0 && main))
        }

        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            realm: Option<&CefString>,
            scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.policy.host_capabilities().auth {
                let (Some(registry), Some(callback)) = (self.state.registry.upgrade(), callback)
                else {
                    return 0;
                };
                let _guard = registry.host_requests.enter_callback();
                match registry
                    .host_requests
                    .register_auth(self.state.browser, callback.clone())
                {
                    Ok(id) => {
                        svc.on_auth_credentials(rust_cef_core::host_capabilities::AuthRequest {
                            id,
                            origin_url: origin_url.map(ToString::to_string).unwrap_or_default(),
                            is_proxy: is_proxy != 0,
                            host: host.map(ToString::to_string).unwrap_or_default(),
                            port,
                            realm: realm.map(ToString::to_string).unwrap_or_default(),
                            scheme: scheme.map(ToString::to_string).unwrap_or_default(),
                        });
                        1
                    }
                    Err((cb, _)) => {
                        drop(cb);
                        0
                    }
                }
            } else {
                self.state.deny(PolicyAction::Authentication);
                0
            }
        }

        fn on_render_view_ready(&self,_browser:Option<&mut Browser>){
            if let Some(registry)=self.state.registry.upgrade(){registry.renderer_event(self.state.browser,BrowserEvent::RenderViewReady);}
        }
        fn on_render_process_unresponsive(&self,_browser:Option<&mut Browser>,_callback:Option<&mut UnresponsiveProcessCallback>)->i32{
            if let Some(registry)=self.state.registry.upgrade(){registry.renderer_event(self.state.browser,BrowserEvent::Unresponsive);}
            // Fixed SDK explicitly supports true without executing callback:
            // keep waiting, no default Chrome modal, no automatic termination.
            1
        }
        fn on_render_process_responsive(&self,_browser:Option<&mut Browser>){
            if let Some(registry)=self.state.registry.upgrade(){registry.renderer_event(self.state.browser,BrowserEvent::Responsive);}
        }
        fn on_render_process_terminated(&self,_browser:Option<&mut Browser>,status:TerminationStatus,error_code:i32,error_string:Option<&CefString>){
            let reason = error_string.map(ToString::to_string).unwrap_or_default();
            eprintln!(
                "[gpui-cef] Renderer terminated: browser={:?} status={} code={} (0x{:08X}) reason={:?}",
                self.state.browser,
                status.get_raw(),
                error_code,
                error_code as u32,
                reason
            );
            if let Some(registry)=self.state.registry.upgrade(){
                registry.renderer_event(self.state.browser,BrowserEvent::RendererTerminated{native_status:status.get_raw(),code:Some(error_code)});
            }
        }

        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            cert_error: Errorcode,
            request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.policy.host_capabilities().certificate {
                let (Some(registry), Some(callback)) = (self.state.registry.upgrade(), callback)
                else {
                    return 0;
                };
                let _guard = registry.host_requests.enter_callback();
                match registry.host_requests.register_certificate(
                    self.state.browser,
                    crate::host_requests::CertificateReply::Server(callback.clone()),
                ) {
                    Ok(id) => {
                        svc.on_certificate_error(
                            rust_cef_core::host_capabilities::CertificateErrorRequest {
                                id,
                                cert_error: cert_error.get_raw(),
                                request_url: request_url.map(ToString::to_string).unwrap_or_default(),
                            },
                        );
                        1
                    }
                    Err((reply, _)) => {
                        if let crate::host_requests::CertificateReply::Server(cb) = reply {
                            drop(cb);
                        }
                        0
                    }
                }
            } else {
                self.state.deny(PolicyAction::Certificate);
                0
            }
        }

        fn on_select_client_certificate(
            &self,
            _browser: Option<&mut Browser>,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            certificates: Option<&[Option<X509Certificate>]>,
            callback: Option<&mut SelectClientCertificateCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.policy.host_capabilities().certificate {
                let Some(registry) = self.state.registry.upgrade() else {
                    if let Some(callback) = callback {
                        self.state.audit_native();
                        callback.select(None);
                    }
                    return 1;
                };
                let Some(callback) = callback else {
                    return 1;
                };
                let _guard = registry.host_requests.enter_callback();
                let mut owned_certs = Vec::new();
                let mut infos = Vec::new();
                for cert in certificates.unwrap_or(&[]).iter().flatten() {
                    let subject = cert
                        .subject()
                        .map(|p| CefString::from(&p.display_name()).to_string())
                        .unwrap_or_default();
                    let issuer = cert
                        .issuer()
                        .map(|p| CefString::from(&p.display_name()).to_string())
                        .unwrap_or_default();
                    infos.push(rust_cef_core::host_capabilities::ClientCertificateInfo {
                        index: owned_certs.len(),
                        subject,
                        issuer,
                    });
                    owned_certs.push(cert.clone());
                }
                match registry.host_requests.register_certificate(
                    self.state.browser,
                    crate::host_requests::CertificateReply::Client {
                        callback: callback.clone(),
                        certs: owned_certs,
                    },
                ) {
                    Ok(id) => {
                        svc.on_select_client_certificate(
                            rust_cef_core::host_capabilities::ClientCertificateRequest {
                                id,
                                is_proxy: is_proxy != 0,
                                host: host.map(ToString::to_string).unwrap_or_default(),
                                port,
                                certificates: infos,
                            },
                        );
                    }
                    Err((reply, _)) => {
                        if let crate::host_requests::CertificateReply::Client { callback, certs } =
                            reply
                        {
                            self.state.audit_native();
                            callback.select(None);
                            drop(certs);
                        }
                    }
                }
                1
            } else {
                self.state.deny(PolicyAction::ClientCertificate);
                if let Some(callback) = callback {
                    self.state.audit_native();
                    callback.select(None);
                }
                // Continue without a client certificate; this is not a claim that
                // every server will reject the resulting TLS connection.
                1
            }
        }
    }
}

pub(crate) fn context_resource_handler(policy: Arc<NativePolicy>) -> ResourceRequestHandler {
    crate::resource_policy::context_handler(policy)
}

// Managed print entry point: deny until a host-owned provider can supply a
// real printer, a confirmed job and positive result evidence. Returning 0 from
// the dialog/job callbacks cancels; CEF never falls back to a system dialog.
wrap_print_handler! {
    struct HostPrintHandler {
        state: Arc<BrowserClientState>,
    }
    impl PrintHandler {
        fn on_print_dialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            _has_selection: i32,
            callback: Option<&mut cef::PrintDialogCallback>,
        ) -> i32 {
            // 1 = handled asynchronously; the host must resolve it. Never let
            // CEF fall back to a system dialog when the handoff is refused.
            let stored = crate::print_provider::store(self.state.browser, callback);
            #[cfg(feature = "test-support")]
            eprintln!("print-handler dialog stored={stored}");
            if stored { 1 } else { 0 }
        }
        fn on_print_job(
            &self,
            _browser: Option<&mut cef::Browser>,
            document_name: Option<&cef::CefString>,
            _pdf_file_path: Option<&cef::CefString>,
            callback: Option<&mut cef::PrintJobCallback>,
        ) -> i32 {
            let document = document_name.map(|n| n.to_string()).unwrap_or_default();
            let epoch = self
                .state
                .registry
                .upgrade()
                .and_then(|registry| registry.document_epoch(self.state.browser))
                .unwrap_or_default();
            let job = crate::print_provider::take_job(self.state.browser, &document, epoch);
            #[cfg(feature = "test-support")]
            eprintln!("print-handler job document={document:?} epoch={epoch} matched={}", job.is_some());
            if job.is_some() {
                crate::print_provider::continue_job(callback);
                1
            } else {
                0
            }
        }
    }
}

wrap_client! {
    pub(crate) struct BrowserClientBuilder {
        render: Option<RenderHandler>,
        life_span: LifeSpanHandler,
        display: DisplayHandler,
        focus: FocusHandler,
        keyboard: KeyboardHandler,
        load: LoadHandler,
        request: RequestHandler,
        js: JsdialogHandler,
        file: DialogHandler,
        download: DownloadHandler,
        permission: PermissionHandler,
        menu: Option<ContextMenuHandler>,
        menu_state: Arc<BrowserClientState>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            self.render.clone()
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }

        fn focus_handler(&self) -> Option<FocusHandler> {
            Some(self.focus.clone())
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(self.keyboard.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> { Some(self.js.clone()) }
        fn dialog_handler(&self) -> Option<DialogHandler> { Some(self.file.clone()) }
        fn download_handler(&self) -> Option<DownloadHandler> { Some(self.download.clone()) }
        fn permission_handler(&self) -> Option<PermissionHandler> { Some(self.permission.clone()) }
        fn find_handler(&self)->Option<FindHandler>{Some(crate::find::RuntimeFindHandler::new(self.menu_state.clone()))}
        fn print_handler(&self) -> Option<PrintHandler> {
            #[cfg(feature = "test-support")]
            eprintln!("print-handler requested");
            Some(HostPrintHandler::new(self.menu_state.clone()))
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            if let Some(registry)=self.menu_state.registry.upgrade(){
                let binding=registry.menu_bridge.get(self.menu_state.browser);
                registry.note_menu_handler_request(binding.is_some());
                if let Some(binding)=binding{
                    return Some(crate::menu_bridge::ProductionMenuHandler::new(self.menu_state.clone(),binding));
                }
            }
            self.menu.clone()
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            if source_process != ProcessId::RENDERER {
                return 0;
            }
            let Some(message) = message else {
                return 0;
            };
            if let Some(registry) = self.menu_state.registry.upgrade() {
                registry.handle_process_message(
                    self.menu_state.browser,
                    &self.menu_state,
                    frame,
                    message,
                )
            } else {
                0
            }
        }
    }
}

impl BrowserClientBuilder {
    /// Builds a fresh handler set per browser, as CEF requires.
    #[allow(dead_code)]
    pub(crate) fn build(state: Arc<BrowserClientState>) -> Client {
        Self::build_for_surface(state, false)
    }

    /// Builds a fresh handler set, omitting `RenderHandler` for Alloy native child HWND hosts.
    pub(crate) fn build_for_surface(
        state: Arc<BrowserClientState>,
        is_native_child: bool,
    ) -> Client {
        let render = (!is_native_child).then(|| RuntimeRenderHandler::new(state.clone()));
        let life_span = RuntimeLifeSpanHandler::new(state.clone());
        let display = RuntimeDisplayHandler::new(state.clone());
        let focus = RuntimeFocusHandler::new(state.clone());
        let keyboard = RuntimeKeyboardHandler::new(state.clone());
        let load = RuntimeLoadHandler::new(state.clone());
        let capabilities = crate::policy_handlers::PolicyHandlers::new(state.clone());
        #[cfg(feature = "test-support")]
        let menu = Some(crate::menu_probe::MenuProbeHandler::new(state.clone()));
        #[cfg(not(feature = "test-support"))]
        let menu = None;
        let request = RuntimeRequestHandler::new(state.clone());
        BrowserClientBuilder::new(
            render,
            life_span,
            display,
            focus,
            keyboard,
            load,
            request,
            capabilities.js,
            capabilities.file,
            capabilities.download,
            capabilities.permission,
            menu,
            state,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FrameObservers {
        source: Mutex<Option<Arc<dyn rust_cef_core::ports::FrameSource>>>,
        wakes: std::sync::atomic::AtomicUsize,
        frames: std::sync::atomic::AtomicUsize,
        failures: std::sync::atomic::AtomicUsize,
    }
    impl rust_cef_core::ports::BrowserEventSink for FrameObservers {
        fn publish(
            &self,
            event: rust_cef_core::ports::BrowserEventEnvelope,
        ) -> rust_cef_core::ports::PublishResult {
            if matches!(event.event, BrowserEvent::CaptureFailed(_)) {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
            rust_cef_core::ports::PublishResult::Published
        }
    }
    impl FrameSink for FrameObservers {
        fn publish(
            &self,
            frame: rust_cef_core::ports::OwnedFrame,
        ) -> rust_cef_core::ports::PublishResult {
            // Re-enter read-only consumption from the actual observer routing:
            // no mailbox/registry lock may span this callback.
            let source = self.source.lock().unwrap();
            let latest = source.as_ref().unwrap().latest(frame.info.surface).unwrap();
            assert_eq!(latest.info, frame.info);
            assert!(Arc::ptr_eq(
                latest.cpu_pixels().unwrap(),
                frame.cpu_pixels().unwrap()
            ));
            self.frames.fetch_add(1, Ordering::Relaxed);
            // A refused observation tap cannot discard the Source's good slot.
            rust_cef_core::ports::PublishResult::Rejected
        }
    }
    impl UiDispatcher for FrameObservers {
        fn wake(&self) -> Result<(), rust_cef_core::ports::EngineError> {
            if let Some(source) = self.source.lock().unwrap().as_ref() {
                let _ = source.snapshot();
            }
            self.wakes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    fn frame_fixture() -> (
        Arc<UiRegistry>,
        Arc<BrowserClientState>,
        Arc<FrameObservers>,
    ) {
        use rust_cef_core::contracts::*;
        let observers = Arc::new(FrameObservers {
            source: Mutex::new(None),
            wakes: std::sync::atomic::AtomicUsize::new(0),
            frames: std::sync::atomic::AtomicUsize::new(0),
            failures: std::sync::atomic::AtomicUsize::new(0),
        });
        let generation = RuntimeGeneration::new(1);
        let id = BrowserId::new(generation, 1);
        let viewport = Viewport {
            width: 2,
            height: 2,
            scale: 1.0,
        };
        let registry = UiRegistry::new(
            generation,
            observers.clone(),
            observers.clone(),
            observers.clone(),
            crate::executor::NativeAudit::new(),
        );
        registry
            .begin_create(
                id,
                BrowserConfig {
                    initial_url: "data:text/html,fixture".into(),
                    profile: None,
                    render_preference: RenderPreference::SoftwareRequired,
                    viewport,
                    options: Default::default(),
                },
            )
            .unwrap();
        let policy = NativePolicy::new(
            Arc::new(
                crate::policy::CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec![],
                    allow_embedded_data: true,
                })
                .unwrap(),
            ),
            Arc::downgrade(&registry),
        );
        let state = Arc::new(BrowserClientState::new(
            id,
            Arc::new(ViewportState::new(viewport)),
            registry.mailbox(id).unwrap(),
            observers.clone(),
            observers.clone(),
            Arc::downgrade(&registry),
            policy,
            "data:text/html,fixture",
        ));
        state.capture.prepare_adapter_test();
        *observers.source.lock().unwrap() = Some(registry.frame_source(id).unwrap());
        (registry, state, observers)
    }

    #[test]
    fn paint_callbacks_use_both_runtime_slots_and_one_pending_frame_wake() {
        let (registry, state, observers) = frame_fixture();
        let source = registry.frame_source(state.browser).unwrap();
        let handler = RuntimeRenderHandler::new(state.clone());
        let first = [1, 2, 3, 255].repeat(4);
        handler.on_paint(None, PaintElementType::VIEW, None, first.as_ptr(), 2, 2);
        let detached_pixels = source
            .latest(SurfaceKind::View)
            .unwrap()
            .cpu_pixels()
            .unwrap()
            .clone();
        let next = [4, 5, 6, 127].repeat(4);
        handler.on_paint(None, PaintElementType::VIEW, None, next.as_ptr(), 2, 2);
        assert_eq!(observers.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(observers.frames.load(Ordering::Relaxed), 2);
        assert_eq!(registry.frame_budget_stats(state.browser).unwrap().used, 32);
        assert_eq!(
            source
                .latest(SurfaceKind::View)
                .unwrap()
                .cpu_pixels()
                .unwrap()
                .as_slice(),
            next.as_slice()
        );
        assert_eq!(detached_pixels.as_slice(), first.as_slice());
        assert!(source.take_update().is_some());
        assert!(source.take_update().is_none());

        handler.on_popup_show(None, 1);
        handler.on_popup_size(
            None,
            Some(&Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            }),
        );
        let popup = [9, 8, 7, 127];
        handler.on_paint(None, PaintElementType::POPUP, None, popup.as_ptr(), 1, 1);
        assert_eq!(observers.wakes.load(Ordering::Relaxed), 2);
        assert_eq!(
            source
                .latest(SurfaceKind::Popup)
                .unwrap()
                .cpu_pixels()
                .unwrap()
                .as_slice(),
            &popup
        );
        assert_eq!(registry.frame_budget_stats(state.browser).unwrap().used, 36);
        assert!(source.take_update().is_some());
        let view_generation = source.snapshot().view.generation;
        handler.on_popup_show(None, 0);
        assert!(source.latest(SurfaceKind::Popup).is_none());
        assert_eq!(source.snapshot().view.generation, view_generation);
        assert!(source.latest(SurfaceKind::View).is_some());

        // Actual life-span adapter revokes the same Source and captured handler.
        RuntimeLifeSpanHandler::new(state.clone()).on_before_close(None);
        assert!(source.snapshot().closed);
        assert!(source.latest(SurfaceKind::View).is_none());
        handler.on_paint(None, PaintElementType::VIEW, None, next.as_ptr(), 2, 2);
        assert_eq!(observers.frames.load(Ordering::Relaxed), 3);
        assert_eq!(registry.frame_budget_stats(state.browser).unwrap().used, 16);
        drop(detached_pixels);
        assert_eq!(registry.frame_budget_stats(state.browser).unwrap().used, 0);
        assert_eq!(observers.failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn viewport_change_revokes_capture_tokens_and_discards_old_callback_sizes() {
        let (_registry, state, observers) = frame_fixture();
        let token = state.store.capture_token(SurfaceKind::View, 2, 2).unwrap();
        let old = [1, 2, 3, 255].repeat(4);
        let old_frame = state.store.copy_frame(&token, &old).unwrap();
        state
            .set_viewport(Viewport {
                width: 3,
                height: 2,
                scale: 1.0,
            })
            .unwrap();
        assert_eq!(
            state.store.publish(old_frame).unwrap_err(),
            FrameError::StaleGeneration
        );
        let handler = RuntimeRenderHandler::new(state.clone());
        handler.on_paint(None, PaintElementType::VIEW, None, old.as_ptr(), 2, 2);
        assert_eq!(observers.frames.load(Ordering::Relaxed), 0);
        assert_eq!(observers.failures.load(Ordering::Relaxed), 0);
        assert_eq!(
            pixel_extent(1, 1, f32::NAN),
            Err(FrameError::InvalidDimensions)
        );
        assert_eq!(
            pixel_extent(i32::MAX, i32::MAX, 1.),
            Err(FrameError::InvalidDimensions)
        );
        assert_eq!(
            pixel_extent(8193, 8192, 1.),
            Err(FrameError::InvalidDimensions)
        );
        assert_eq!(pixel_extent(3, 2, 1.5).unwrap(), (5, 3));
        assert_eq!(pixel_extent(10, 10, 1.1).unwrap(), (11, 11));
        assert_eq!(
            pixel_extent(1, 1, f32::EPSILON),
            Err(FrameError::InvalidDimensions)
        );

        state
            .set_viewport(Viewport {
                width: 2,
                height: 2,
                scale: 1.1,
            })
            .unwrap();
        handler.on_popup_show(None, 1);
        handler.on_popup_size(
            None,
            Some(&Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }),
        );
        assert_eq!(
            state.store.snapshot().viewport,
            Some(Viewport {
                width: 2,
                height: 2,
                scale: 1.1
            })
        );
        let view = state.store.capture_token(SurfaceKind::View, 3, 3).unwrap();
        let popup = state.store.capture_token(SurfaceKind::Popup, 3, 3).unwrap();
        let old_view = state.store.copy_frame(&view, &[255; 36]).unwrap();
        let old_popup = state.store.copy_frame(&popup, &[255; 36]).unwrap();
        state
            .set_viewport(Viewport {
                width: 2,
                height: 2,
                scale: 1.2,
            })
            .unwrap();
        assert_eq!(
            pixel_extent(2, 2, 1.1).unwrap(),
            pixel_extent(2, 2, 1.2).unwrap()
        );
        assert_eq!(
            state.store.snapshot().viewport,
            Some(Viewport {
                width: 2,
                height: 2,
                scale: 1.2
            })
        );
        assert_eq!(
            state.store.publish(old_view).unwrap_err(),
            FrameError::StaleGeneration
        );
        assert_eq!(
            state.store.publish(old_popup).unwrap_err(),
            FrameError::StaleGeneration
        );
        assert!(state.store.snapshot().view.generation > view.generation());
        assert!(state.store.snapshot().popup.generation > popup.generation());
        assert!(
            state.store.snapshot().popup.visible,
            "DPI does not synthesize PopupHide"
        );
        handler.on_paint(
            None,
            PaintElementType::POPUP,
            None,
            [255; 36].as_ptr(),
            3,
            3,
        );
        assert!(state.store.latest(SurfaceKind::Popup).is_some());
        assert_eq!(
            observers.frames.load(Ordering::Relaxed),
            1,
            "no second PopupShow needed"
        );
    }

    #[test]
    fn renderer_callback_routes_status_and_suppresses_default_hang_ui() {
        use rust_cef_core::{contracts::*, ports::*};
        struct Sinks;
        impl BrowserEventSink for Sinks {
            fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
                PublishResult::Published
            }
        }
        impl FrameSink for Sinks {
            fn publish(&self, _: OwnedFrame) -> PublishResult {
                PublishResult::Published
            }
        }
        impl UiDispatcher for Sinks {
            fn wake(&self) -> Result<(), EngineError> {
                Ok(())
            }
        }
        let sinks = Arc::new(Sinks);
        let generation = RuntimeGeneration::new(1);
        let id = BrowserId::new(generation, 1);
        let registry = UiRegistry::new(
            generation,
            sinks.clone(),
            sinks.clone(),
            sinks.clone(),
            crate::executor::NativeAudit::new(),
        );
        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 1.,
        };
        registry
            .begin_create(
                id,
                BrowserConfig {
                    initial_url: "data:text/html,fixture".into(),
                    profile: None,
                    render_preference: RenderPreference::SoftwareRequired,
                    viewport,
                    options: Default::default(),
                },
            )
            .unwrap();
        let policy = NativePolicy::new(
            Arc::new(
                crate::policy::CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec![],
                    allow_embedded_data: true,
                })
                .unwrap(),
            ),
            Arc::downgrade(&registry),
        );
        let state = Arc::new(BrowserClientState::new(
            id,
            Arc::new(ViewportState::new(viewport)),
            new_frame_mailbox(id, viewport).unwrap(),
            sinks.clone(),
            sinks,
            Arc::downgrade(&registry),
            policy,
            "data:text/html,fixture",
        ));
        let handler = RuntimeRequestHandler::new(state);
        handler.on_render_view_ready(None);
        assert_eq!(registry.snapshot(id).renderer.state, RendererState::Running);
        assert_eq!(handler.on_render_process_unresponsive(None, None), 1);
        assert_eq!(
            registry.snapshot(id).renderer.state,
            RendererState::Unresponsive
        );
        handler.on_render_process_responsive(None);
        assert_eq!(registry.snapshot(id).renderer.state, RendererState::Running);
        handler.on_render_process_terminated(None, TerminationStatus::PROCESS_CRASHED, -987, None);
        let snapshot = registry.snapshot(id);
        assert_eq!(
            snapshot.state,
            SessionState::Creating,
            "fault does not synthesize lifecycle"
        );
        assert_eq!(
            snapshot.renderer.last_termination,
            Some(RendererTermination {
                native_status: TerminationStatus::PROCESS_CRASHED.get_raw(),
                code: Some(-987)
            })
        );
    }

    #[test]
    fn aborted_load_is_observable_cancellation_not_network_failure() {
        assert_eq!(
            frame_load_error(true, Errorcode::ABORTED),
            BrowserEvent::LoadCancelled {
                is_main_frame: true,
                code: -3
            }
        );
        assert_eq!(
            frame_load_error(false, Errorcode::ABORTED),
            BrowserEvent::LoadCancelled {
                is_main_frame: false,
                code: -3
            }
        );
        assert_eq!(
            frame_load_error(false, Errorcode::FAILED),
            BrowserEvent::LoadFailed {
                is_main_frame: false,
                code: Errorcode::FAILED.get_raw()
            }
        );
    }

    #[test]
    fn ime_callback_copies_bounded_owned_geometry_and_wakes_without_a_frame() {
        use rust_cef_core::ime::{ImeRejection, ImeStatus};
        let (registry, state, observers) = frame_fixture();
        state.prepare_ime_adapter_test();
        registry.prepare_ime_test_entry(state.browser);
        let handler = RuntimeRenderHandler::new(state.clone());

        let rects = [
            Rect {
                x: 10,
                y: 20,
                width: 12,
                height: 18,
            },
            Rect {
                x: 22,
                y: 20,
                width: 12,
                height: 18,
            },
        ];
        let selected = Range { from: 7, to: 9 };
        handler.on_ime_composition_range_changed(None, Some(&selected), Some(rects.as_slice()));
        let snapshot = registry.ime_snapshot(state.browser).unwrap();
        assert_eq!(snapshot.status, ImeStatus::Ready);
        assert_eq!(snapshot.revision, 1);
        assert_eq!(
            snapshot.range,
            Some(Utf16Range { start: 7, end: 9 }),
            "the reported range is kept, never replaced by a caret guess"
        );
        assert_eq!(snapshot.character_bounds.len(), 2);
        assert_eq!(snapshot.character_bounds[1].x, 22);
        assert_eq!(snapshot.observed_input_epoch, 1);
        assert_eq!(observers.frames.load(Ordering::Relaxed), 0);
        assert_eq!(observers.wakes.load(Ordering::Relaxed), 1);

        // A missing array is a distinct state, not an empty composition.
        handler.on_ime_composition_range_changed(None, Some(&selected), None);
        assert_eq!(
            registry.ime_snapshot(state.browser).unwrap().status,
            ImeStatus::Rejected(ImeRejection::MissingGeometry)
        );
        // An over-cap array is refused before collection.
        let oversized = vec![
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            };
            rust_cef_core::ime::MAX_IME_CHARACTER_BOUNDS + 1
        ];
        handler.on_ime_composition_range_changed(None, Some(&selected), Some(oversized.as_slice()));
        let overflow = registry.ime_snapshot(state.browser).unwrap();
        assert_eq!(
            overflow.status,
            ImeStatus::Rejected(ImeRejection::OverflowAllocation)
        );
        assert!(overflow.character_bounds.is_empty());

        // Late geometry after OnBeforeClose is dropped, not resurrected.
        RuntimeLifeSpanHandler::new(state.clone()).on_before_close(None);
        let wakes = observers.wakes.load(Ordering::Relaxed);
        let frames = observers.frames.load(Ordering::Relaxed);
        handler.on_ime_composition_range_changed(None, Some(&selected), Some(rects.as_slice()));
        assert_eq!(
            registry.ime_snapshot(state.browser),
            Err(rust_cef_core::ports::EngineError::Closed)
        );
        assert_eq!(observers.wakes.load(Ordering::Relaxed), wakes);
        assert_eq!(observers.frames.load(Ordering::Relaxed), frames);
    }

    #[test]
    fn cursor_and_severity_mappings_cover_all_standard_types() {
        let cases = [
            (CursorType::POINTER, BrowserCursor::Arrow),
            (CursorType::IBEAM, BrowserCursor::IBeam),
            (CursorType::CROSS, BrowserCursor::Crosshair),
            (CursorType::GRABBING, BrowserCursor::ClosedHand),
            (CursorType::GRAB, BrowserCursor::OpenHand),
            (CursorType::HAND, BrowserCursor::PointingHand),
            (CursorType::WESTRESIZE, BrowserCursor::ResizeLeft),
            (CursorType::EASTRESIZE, BrowserCursor::ResizeRight),
            (CursorType::EASTWESTRESIZE, BrowserCursor::ResizeLeftRight),
            (CursorType::NORTHRESIZE, BrowserCursor::ResizeUp),
            (CursorType::SOUTHRESIZE, BrowserCursor::ResizeDown),
            (CursorType::NORTHSOUTHRESIZE, BrowserCursor::ResizeUpDown),
            (
                CursorType::NORTHWESTSOUTHEASTRESIZE,
                BrowserCursor::ResizeUpLeftDownRight,
            ),
            (
                CursorType::NORTHEASTSOUTHWESTRESIZE,
                BrowserCursor::ResizeUpRightDownLeft,
            ),
            (CursorType::COLUMNRESIZE, BrowserCursor::ResizeColumn),
            (CursorType::ROWRESIZE, BrowserCursor::ResizeRow),
            (CursorType::VERTICALTEXT, BrowserCursor::IBeamVertical),
            (CursorType::NOTALLOWED, BrowserCursor::OperationNotAllowed),
            (CursorType::ALIAS, BrowserCursor::DragLink),
            (CursorType::COPY, BrowserCursor::DragCopy),
            (CursorType::CONTEXTMENU, BrowserCursor::ContextualMenu),
            (CursorType::CUSTOM, BrowserCursor::CustomUnsupported),
            (CursorType::WAIT, BrowserCursor::Arrow),
        ];
        for (cef_cursor, expected) in cases {
            assert_eq!(map_cef_cursor(cef_cursor), expected);
        }

        assert_eq!(
            map_log_severity(LogSeverity::VERBOSE),
            ConsoleSeverity::Verbose
        );
        assert_eq!(map_log_severity(LogSeverity::INFO), ConsoleSeverity::Info);
        assert_eq!(
            map_log_severity(LogSeverity::WARNING),
            ConsoleSeverity::Warning
        );
        assert_eq!(map_log_severity(LogSeverity::ERROR), ConsoleSeverity::Error);
        assert_eq!(map_log_severity(LogSeverity::FATAL), ConsoleSeverity::Fatal);
        assert_eq!(
            map_log_severity(LogSeverity::DEFAULT),
            ConsoleSeverity::Default
        );
    }

    #[test]
    fn read_cef_string_list_enforces_item_and_utf8_byte_bounds() {
        let mut list = CefStringList::new();
        for index in 0..10 {
            let entry = format!("https://example.com/{index}/你好世界");
            assert!(list.append(&entry));
        }
        let read = read_cef_string_list(&mut list, 8, 28);
        assert_eq!(read.len(), 8);
        for item in &read {
            assert!(item.len() <= 28);
            assert!(std::str::from_utf8(item.as_bytes()).is_ok());
        }
    }

    #[test]
    fn shortcut_filter_pre_key_consumes_only_matched_keydown() {
        use rust_cef_core::{
            contracts::{ShortcutFilter, ShortcutId, ShortcutRule},
            keymap::EVENTFLAG_CONTROL_DOWN,
        };
        let filter = ShortcutFilter::new(vec![ShortcutRule {
            id: ShortcutId(7),
            windows_key_code: 0x4D,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        }])
        .unwrap();
        assert_eq!(
            filter.match_key(0x4D, EVENTFLAG_CONTROL_DOWN),
            Some(ShortcutId(7))
        );
        assert_eq!(filter.match_key(0x4D, 0), None);
    }

    #[test]
    fn popup_and_auth_preserve_default_deny_and_block_native_popups_on_opt_in() {
        use rust_cef_core::{
            contracts::{RuntimeGeneration, TrustedContentPolicy},
            host_capabilities::{HostCapabilities, PopupRequest, PopupService},
            ports::*,
        };
        struct Sinks;
        impl BrowserEventSink for Sinks {
            fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
                PublishResult::Published
            }
        }
        impl FrameSink for Sinks {
            fn publish(&self, _: OwnedFrame) -> PublishResult {
                PublishResult::Published
            }
        }
        impl UiDispatcher for Sinks {
            fn wake(&self) -> Result<(), EngineError> {
                Ok(())
            }
        }

        struct Recorder(std::sync::Mutex<Vec<PopupRequest>>);
        impl PopupService for Recorder {
            fn on_popup_request(&self, request: PopupRequest) {
                self.0.lock().unwrap().push(request);
            }
        }

        let viewport = Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        };
        let sinks = Arc::new(Sinks);
        let id = BrowserId::new(RuntimeGeneration::new(1), 42);
        let policy = NativePolicy::new(
            Arc::new(
                crate::policy::CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec!["https://allowed.local".into()],
                    allow_embedded_data: true,
                })
                .unwrap(),
            ),
            Weak::new(),
        );
        let state = Arc::new(BrowserClientState::new(
            id,
            Arc::new(ViewportState::new(viewport)),
            new_frame_mailbox(id, viewport).unwrap(),
            sinks.clone(),
            sinks,
            Weak::new(),
            policy.clone(),
            "https://allowed.local/index.html",
        ));
        let lifespan = RuntimeLifeSpanHandler::new(state.clone());
        let request = RuntimeRequestHandler::new(state);
        let url = CefString::from("https://allowed.local/next");

        // Default deny: blocks popup, denies auth and cert.
        assert_eq!(
            lifespan.on_before_popup(
                None,
                None,
                0,
                Some(&url),
                None,
                WindowOpenDisposition::NEW_POPUP,
                1,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            1
        );
        assert_eq!(
            request.auth_credentials(None, None, 0, None, 0, None, None, None),
            0
        );
        assert_eq!(
            request.on_certificate_error(None, Errorcode::FAILED, None, None, None),
            0
        );
        let (counts, _) = policy.snapshot();
        assert_eq!(counts[PolicyAction::Popup as usize], 1);
        assert_eq!(counts[PolicyAction::Authentication as usize], 1);
        assert_eq!(counts[PolicyAction::Certificate as usize], 1);

        // Opt-in popup still returns 1 so zero unmanaged native popups spawn.
        let rec = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        policy.set_host_capabilities(HostCapabilities {
            popup: Some(rec.clone()),
            ..Default::default()
        });
        assert_eq!(
            lifespan.on_before_popup(
                None,
                None,
                1,
                Some(&url),
                None,
                WindowOpenDisposition::NEW_FOREGROUND_TAB,
                1,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            1
        );
        let captured = rec.0.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(captured[0].policy_allowed);
        assert!(captured[0].user_gesture);
    }

    #[test]
    fn screen_origin_only_triggers_monitor_change_when_monitor_rects_change() {
        let vp = ViewportState::new(Viewport {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        assert_eq!(vp.set_screen_origin(0, 0), (true, true));
        assert_eq!(vp.set_screen_origin(0, 0), (false, false));
        assert_eq!(vp.set_screen_origin(12, 16), (true, false));
    }

    #[test]
    fn viewport_state_snapshot_is_atomic_for_scale_and_origin() {
        let vp = ViewportState::new(Viewport {
            width: 800,
            height: 600,
            scale: 1.0,
        });
        assert_eq!(vp.snapshot(), (Viewport { width: 800, height: 600, scale: 1.0 }, None));
        let _ = vp.set_screen_origin(100, 200);
        vp.set(Viewport {
            width: 1200,
            height: 800,
            scale: 1.5,
        });
        assert_eq!(
            vp.snapshot(),
            (
                Viewport {
                    width: 1200,
                    height: 800,
                    scale: 1.5,
                },
                Some((100, 200))
            )
        );
        assert_eq!(vp.screen_point(10, 20), Some((115, 230)));
    }

    #[test]
    fn osr_paint_ignores_zero_and_oversized_16385_frames_without_faulting() {
        let (_registry, state, observers) = frame_fixture();
        let handler = RuntimeRenderHandler::new(state);
        let dummy_px = [1u8, 2, 3, 255];

        // CPU on_paint: 0-dimension and > 16384 frames are dropped without fault.
        handler.on_paint(None, PaintElementType::VIEW, None, dummy_px.as_ptr(), 0, 100);
        handler.on_paint(None, PaintElementType::VIEW, None, dummy_px.as_ptr(), 100, 0);
        handler.on_paint(
            None,
            PaintElementType::VIEW,
            None,
            dummy_px.as_ptr(),
            MAX_OSR_TEXTURE_DIMENSION + 1,
            10,
        );
        handler.on_paint(
            None,
            PaintElementType::VIEW,
            None,
            dummy_px.as_ptr(),
            10,
            MAX_OSR_TEXTURE_DIMENSION + 1,
        );

        // GPU on_accelerated_paint: 0-dimension and > 16384 frames are dropped without fault.
        let mut zero_info = cef::AcceleratedPaintInfo::default();
        zero_info.extra.coded_size = cef::Size { width: 0, height: 100 };
        zero_info.extra.visible_rect = cef::Rect { x: 0, y: 0, width: 0, height: 100 };
        handler.on_accelerated_paint(None, PaintElementType::VIEW, None, Some(&zero_info));

        let mut oversized_info = cef::AcceleratedPaintInfo::default();
        oversized_info.extra.coded_size = cef::Size {
            width: MAX_OSR_TEXTURE_DIMENSION + 1,
            height: 16,
        };
        oversized_info.extra.visible_rect = cef::Rect {
            x: 0,
            y: 0,
            width: MAX_OSR_TEXTURE_DIMENSION + 1,
            height: 16,
        };
        handler.on_accelerated_paint(None, PaintElementType::VIEW, None, Some(&oversized_info));

        assert_eq!(observers.frames.load(Ordering::Relaxed), 0);
        assert_eq!(observers.failures.load(Ordering::Relaxed), 0);
    }
}
