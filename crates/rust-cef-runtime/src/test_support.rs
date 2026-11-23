//! Test-only wire protocol; configuration is never evidence of native success.
use std::{collections::BTreeMap, path::PathBuf};

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use crate::dialog_probe::DialogProbeAction;
use rust_cef_core::contracts::{RenderPreference, ThreadMode};
use serde::Serialize;

/// Plain data observed on the current machine, not a renderer device handle.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[derive(Clone, Debug)]
pub struct ProbeDxgiAdapter {
    pub name: String,
    pub software: bool,
    pub luid: (i32, u32),
}

/// Fresh, bounded, read-only DXGI inventory. No COM interface escapes, and
/// enumeration does not create, reset, or change either renderer's device.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub fn probe_dxgi_adapters() -> Result<Vec<ProbeDxgiAdapter>, String> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIFactory1,
    };
    // SAFETY: all returned interfaces are owned and used on this calling thread.
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
        .map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    let mut adapters = Vec::new();
    for index in 0..=32 {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) if index < 32 => adapter,
            Ok(_) => return Err("DXGI inventory exceeds 32 adapters".into()),
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => return Ok(adapters),
            Err(e) => return Err(format!("EnumAdapters1: {e}")),
        };
        let desc = unsafe { adapter.GetDesc1() }.map_err(|e| format!("GetDesc1: {e}"))?;
        adapters.push(ProbeDxgiAdapter {
            name: String::from_utf16_lossy(&desc.Description)
                .trim_matches('\0')
                .to_owned(),
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            luid: (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
        });
    }
    Err("DXGI inventory did not reach its terminator".into())
}

/// One actual CEF menu's pre-issue Context/client preparation and disposal.
/// Test support never calls ShowDevTools or creates a native auxiliary here.
#[derive(Clone, Debug, Serialize)]
pub struct AuxiliaryPreparationReport {
    pub browser: (u64, u64),
    pub menu_sequence: u64,
    pub cef_ui_thread: u64,
    pub pending_before: u64,
    pub pending_prepared: u64,
    pub pending_released: u64,
}

#[derive(Clone, Debug, Default)]
pub struct AuxiliaryPreparationTicket {
    result: std::sync::Arc<
        std::sync::Mutex<
            Option<Result<AuxiliaryPreparationReport, rust_cef_core::ports::EngineError>>,
        >,
    >,
}
impl PartialEq for AuxiliaryPreparationTicket {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.result, &other.result)
    }
}
impl AuxiliaryPreparationTicket {
    pub fn poll(
        &self,
    ) -> Option<Result<AuxiliaryPreparationReport, rust_cef_core::ports::EngineError>> {
        self.result.lock().unwrap().clone()
    }
    pub(crate) fn complete(
        self,
        outcome: Result<AuxiliaryPreparationReport, rust_cef_core::ports::EngineError>,
    ) {
        let mut result = self.result.lock().unwrap();
        if result.is_none() {
            *result = Some(outcome);
        }
    }
}

pub const PRESSURE_EVENT_CAPACITY: usize = 256;
pub const PRESSURE_TITLE_PREFIX: &str = "gpui-event-pressure:";
pub const PRESSURE_TITLE_READY: &str = "gpui-event-pressure-ready";
pub const PRESSURE_FIXTURE_URL: &str = "data:text/html,%3Ctitle%3Egpui-event-pressure-ready%3C%2Ftitle%3E%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(26%2C76%2C126)%22%3E%3C%2Fbody%3E";
/// The other exact test page allowed to receive the same fixed title stimulus.
pub const PRESSURE_SOURCE_FIXTURE_URL: &str = "data:text/html,%3Ctitle%3Egpui-event-pressure-ready%3C%2Ftitle%3E%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(26%2C76%2C126)%22%3E%3Cspan%3Emenu%20bridge%20selection%3C%2Fspan%3E%3C%2Fbody%3E";

/// Fixed, bounded frame-validation controls, never a stable product API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameProbeControl {
    DropView(bool),
    /// Hold VIEW publication at the current generation; inject only at the
    /// specified immediate successor and its exact device-pixel dimensions.
    RejectIncompleteAndHold {
        generation: u64,
        width: u32,
        height: u32,
    },
    /// Refuse whichever VIEW frame arrives next, whatever its generation and
    /// size, without entering the terminal drop state: later frames must still
    /// publish so recovery after exhaustion stays observable.
    RejectNextIncomplete,
    /// Refuse the next `frames` VIEW frames one by one, then pass. Enough
    /// refusals spend the product's 8-request budget, and passing afterwards
    /// keeps recovery observable in the same run.
    RejectIncompleteFor {
        frames: u8,
    },
    /// Refuse every VIEW frame until explicitly released. The product's own
    /// exhaustion cannot depend on a fixed refusal count.
    RejectIncompleteUntilReleased,
    /// Release `RejectIncompleteUntilReleased`; later frames publish again.
    ReleaseIncompleteRefusals,
    CaptureFaultAndInvalidate,
    ResumeCaptureAndInvalidate,
    InvalidateView,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct FrameProbeState {
    pub drop_view: bool,
    pub incomplete_pending: bool,
    #[serde(skip)]
    incomplete_target: Option<(u64, u32, u32)>,
    /// Armed by RejectNextIncomplete: one identity-free refusal, cleared after
    /// it fires and never setting drop_view.
    #[serde(skip)]
    reject_next_once: bool,
    /// Remaining identity-free refusals armed by `RejectIncompleteFor`.
    #[serde(skip)]
    reject_remaining: u8,
    /// Armed by `RejectIncompleteUntilReleased`.
    #[serde(skip)]
    reject_until_released: bool,
    pub applied: u64,
    pub dropped: u64,
    pub incomplete_rejected: u64,
    pub native_rejections: Vec<NativeFrameRejection>,
    pub native_rejections_overflow: bool,
    #[serde(skip)]
    pub(crate) capture_fault_armed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_fault: Option<CallbackCaptureFault>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound_drag_denial: Option<OutboundDragDenial>,
}

pub(crate) enum FrameProbeViewGate {
    Pass,
    Dropped,
    RejectedIncomplete,
}

impl FrameProbeState {
    pub(crate) fn clear_incomplete(&mut self) {
        self.incomplete_target = None;
        self.incomplete_pending = false;
        self.reject_next_once = false;
        self.reject_remaining = 0;
        self.reject_until_released = false;
    }

    /// Arm an open-ended run of identity-free refusals, released explicitly.
    pub(crate) fn arm_reject_until_released(&mut self) -> bool {
        if self.incomplete_pending
            || self.drop_view
            || self.capture_fault_armed
            || self.reject_next_once
            || self.reject_remaining != 0
            || self.reject_until_released
        {
            return false;
        }
        self.reject_until_released = true;
        self.incomplete_pending = true;
        true
    }

    /// Stop refusing so later frames publish again.
    pub(crate) fn release_refusals(&mut self) -> bool {
        if !self.reject_until_released {
            return false;
        }
        self.reject_until_released = false;
        self.incomplete_pending = false;
        true
    }

    /// Arm a bounded run of identity-free refusals. `drop_view` stays false so
    /// the frames after the last refusal publish again.
    pub(crate) fn arm_reject_for(&mut self, frames: u8) -> bool {
        if frames == 0
            || self.incomplete_pending
            || self.drop_view
            || self.capture_fault_armed
            || self.reject_next_once
            || self.reject_remaining != 0
        {
            return false;
        }
        self.reject_remaining = frames;
        self.incomplete_pending = true;
        true
    }

    /// Arm one identity-free refusal that does not end the stream: the pending
    /// flag reports it, but drop_view stays false so the next frame passes.
    pub(crate) fn arm_reject_next_once(&mut self) -> bool {
        if self.incomplete_pending
            || self.drop_view
            || self.capture_fault_armed
            || self.reject_next_once
        {
            return false;
        }
        self.reject_next_once = true;
        self.incomplete_pending = true;
        true
    }

    pub(crate) fn arm_incomplete(&mut self, current: u64, target: (u64, u32, u32)) -> bool {
        if current == 0
            || current.checked_add(1) != Some(target.0)
            || rust_cef_core::frame_store::frame_len(target.1, target.2).is_none()
            || self.incomplete_pending
            || self.drop_view
            || self.capture_fault_armed
        {
            return false;
        }
        self.incomplete_target = Some(target);
        self.incomplete_pending = true;
        true
    }

    pub(crate) fn gate_view(&mut self, frame: (u64, u32, u32)) -> FrameProbeViewGate {
        if self.reject_until_released {
            let (_, width, height) = frame;
            let visible = (0, 0, width as i32, height as i32);
            let content = (0, 0, width.saturating_sub(1) as i32, height as i32);
            if !rust_cef_core::frame_layout::is_full_frame(visible, content) {
                self.incomplete_rejected += 1;
                return FrameProbeViewGate::RejectedIncomplete;
            }
        }
        if self.reject_remaining > 0 {
            let (_, width, height) = frame;
            let visible = (0, 0, width as i32, height as i32);
            let content = (0, 0, width.saturating_sub(1) as i32, height as i32);
            if !rust_cef_core::frame_layout::is_full_frame(visible, content) {
                self.reject_remaining -= 1;
                self.incomplete_rejected += 1;
                if self.reject_remaining == 0 {
                    // Budget spent and released: later frames pass normally.
                    self.incomplete_pending = false;
                }
                return FrameProbeViewGate::RejectedIncomplete;
            }
        }
        if self.reject_next_once {
            let (_, width, height) = frame;
            let visible = (0, 0, width as i32, height as i32);
            let content = (0, 0, width.saturating_sub(1) as i32, height as i32);
            if !rust_cef_core::frame_layout::is_full_frame(visible, content) {
                // Refuse this one frame only: no terminal drop, so a later size
                // change can still publish and prove recovery.
                self.reject_next_once = false;
                self.incomplete_pending = false;
                self.incomplete_rejected += 1;
                return FrameProbeViewGate::RejectedIncomplete;
            }
        }
        if self.incomplete_target == Some(frame) {
            let (_, width, height) = frame;
            let visible = (0, 0, width as i32, height as i32);
            let content = (0, 0, width.saturating_sub(1) as i32, height as i32);
            if !rust_cef_core::frame_layout::is_full_frame(visible, content) {
                self.clear_incomplete();
                self.drop_view = true;
                self.incomplete_rejected += 1;
                return FrameProbeViewGate::RejectedIncomplete;
            }
        }
        if self.drop_view || self.incomplete_pending {
            self.dropped += 1;
            return FrameProbeViewGate::Dropped;
        }
        FrameProbeViewGate::Pass
    }
}

#[cfg(test)]
mod frame_probe_gate_tests {
    use super::*;

    #[test]
    fn only_exact_successor_consumes_incomplete_arm() {
        use FrameProbeViewGate::*;
        let mut state = FrameProbeState::default();
        assert!(!state.arm_incomplete(15, (17, 440, 308)));
        assert!(!state.arm_incomplete(15, (16, 0, 308)));
        assert!(state.arm_incomplete(15, (16, 440, 308)));
        assert!(!state.arm_incomplete(15, (16, 441, 308)));
        for other in [(15, 396, 264), (17, 440, 308), (16, 440, 309)] {
            assert!(matches!(state.gate_view(other), Dropped));
            assert!(state.incomplete_pending && !state.drop_view);
            assert_eq!(state.incomplete_rejected, 0);
        }
        assert!(matches!(
            state.gate_view((16, 440, 308)),
            RejectedIncomplete
        ));
        assert!(matches!(state.gate_view((16, 440, 308)), Dropped));
        assert!(state.drop_view && !state.incomplete_pending);
        assert_eq!((state.incomplete_rejected, state.dropped), (1, 4));
        state.drop_view = false;
        assert!(matches!(state.gate_view((16, 440, 308)), Pass));
    }
}

/// Redacted receipt from the original windowless CEF RenderHandler callback.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OutboundDragDenial {
    pub callbacks: u64,
    pub thread: u64,
    pub active: bool,
    pub arguments_present: bool,
}

/// Owned before/after measurements of a single callback-scoped injection.
#[derive(Clone, Debug, Serialize)]
pub struct CallbackCaptureFault {
    pub browser: (u64, u64),
    pub thread: u64,
    pub backend: &'static str,
    pub expected_error_returned: bool,
    pub same_frame: bool,
    pub good_frame: Option<(u64, u64)>, // surface generation, sequence
    pub generation_after: u64,
    pub budget_used: (usize, usize),
    pub native_io_before: (u64, u64, u64, u64), // open, copy, map, unmap
    pub native_io_after: (u64, u64, u64, u64),
    pub live_imports_after: u64,
}

impl CallbackCaptureFault {
    pub fn complete(&self) -> bool {
        let (open, copy, map, unmap) = self.native_io_before;
        let (after_open, after_copy, after_map, after_unmap) = self.native_io_after;
        self.thread != 0
            && self.expected_error_returned
            && self.same_frame
            && self.good_frame.is_some_and(|(generation, sequence)| {
                generation == self.generation_after && sequence > 0
            })
            && self.budget_used.0 > 0
            && self.budget_used.0 == self.budget_used.1
            && self.live_imports_after == 0
            && match self.backend {
                "Software" => {
                    self.native_io_before == (0, 0, 0, 0) && self.native_io_after == (0, 0, 0, 0)
                }
                "D3d11" => {
                    after_open.checked_sub(open) == Some(1)
                        && after_copy.checked_sub(copy) == Some(1)
                        && after_map.checked_sub(map) == Some(1)
                        && after_unmap.checked_sub(unmap) == Some(1)
                        && map == unmap
                        && after_map == after_unmap
                }
                _ => false,
            }
    }
}

#[derive(Serialize)]
pub struct CaptureFailureRecovery {
    pub callback: CallbackCaptureFault,
    pub event_sequence: u64,
    pub recovered_frame: (u64, u64),
    pub retired_capture: Option<crate::capture::CaptureDiagnostics>,
    pub closed_source: bool,
    pub final_cpu_bytes: Option<usize>,
}

impl CaptureFailureRecovery {
    pub fn complete(&self) -> bool {
        self.callback.complete()
            && self.event_sequence > 0
            && self
                .callback
                .good_frame
                .is_some_and(|(generation, sequence)| {
                    self.recovered_frame.0 == generation && self.recovered_frame.1 > sequence
                })
            && self.closed_source
            && self.final_cpu_bytes == Some(0)
            && self.retired_capture.as_ref().is_some_and(|d| {
                let backend = self.callback.backend;
                let kind = if backend == "D3d11" {
                    "IncompleteFrame"
                } else {
                    "InvalidDimensions"
                };
                d.failures == 1
                    && d.last_failure.as_deref() == Some(kind)
                    && d.last_failure_code.is_none()
                    && d.requested_backend == Some(backend)
                    && d.observed_backend == Some(backend)
                    && d.retired
                    && !d.abandoned
                    && !d.native_resources_live
                    && d.owner_thread == self.callback.thread
                    && d.retirement_thread == self.callback.thread
                    && d.live_imports == 0
                    && d.staging_bytes == 0
                    && d.maps == d.unmaps
            })
    }
}

/// Test-only callback-scoped rejection witness, never a native reference or a
/// replacement for CaptureFailed. Bounded to 32 entries per probe Browser.
#[derive(Clone, Debug, Serialize)]
pub struct NativeFrameRejection {
    pub browser: String,
    pub surface: &'static str,
    pub thread: u64,
    pub failure_ordinal: u64,
    pub frame_sha256: Option<String>,
    pub coded: (i32, i32),
    pub visible: (i32, i32, i32, i32),
    pub content: (i32, i32, i32, i32),
    pub source_generation_before: u64,
    pub source_generation_after: u64,
    pub frame_before: Option<(u64, u64, u32, u32)>,
    pub frame_after: Option<(u64, u64, u32, u32)>,
    pub same_frame: bool,
    pub recovery_status_before: &'static str,
    pub recovery_status_after: &'static str,
    pub target_generation: u64,
    pub target: (u32, u32),
    pub acks_before: u64,
    pub acks_after: u64,
    pub native_io_before: (u64, u64, u64, u64),
    pub native_io_after: (u64, u64, u64, u64),
}

/// Validation-only digest of the actual owned frame bytes, without copying them.
pub fn probe_frame_sha256(frame: &rust_cef_core::ports::OwnedFrame) -> String {
    use sha2::Digest;
    match frame.cpu_pixels() {
        Some(pixels) => format!("{:x}", sha2::Sha256::digest(pixels.as_slice())),
        None => format!("gpu:{}:{}", frame.info.width, frame.info.height),
    }
}

/// Publish readiness by a same-directory rename only after the PID file is
/// fully written and closed. Readers never observe an empty/incomplete marker.
pub fn publish_observer_ready(gate: &std::path::Path, pid: u32) -> std::io::Result<()> {
    let temporary = gate.with_extension("ready.tmp");
    std::fs::write(&temporary, pid.to_string())?;
    std::fs::rename(temporary, gate.with_extension("ready"))
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MenuProbeSnapshot {
    pub deferred: bool,
    pub pending_continuations: u64,
    pub dismissal_work: Vec<u64>,
    pub enabled: bool,
    pub retired: bool,
    pub input_rounds: u64,
    pub cef_mouse_events: u64,
    pub fixture_builds: u64,
    pub captures: u64,
    pub copies: u64,
    pub cancel_calls: u64,
    pub cancel_returns: u64,
    pub dismissed: u64,
    pub dismissed_during_cancel: u64,
    pub uncorrelated_dismissed: u64,
    pub answering: Option<u64>,
    pub cef_ui_thread: u64,
    pub witnesses: Vec<MenuWitness>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct MenuWitness {
    pub sequence: u64,
    pub anchor: (i32, i32),
    pub capture_thread: u64,
    pub browser: (u64, u64),
    pub native_browser_id: i32,
    pub frame_id_bytes: usize,
    pub selection_bytes: usize,
    pub word_bytes: usize,
    pub suggestions: usize,
    pub context_total_bytes: usize,
    pub root_rows: usize,
    pub child_rows: usize,
    pub fixture_faithful: bool,
    /// Explicit fixture metadata only, NOT a physical HWND/live GPUI token.
    pub binding_metadata_only: bool,
}
impl MenuProbeSnapshot {
    pub fn complete(&self, count: u64) -> bool {
        (1..=if self.deferred { 3 } else { 2 }).contains(&count)
            && self.error.is_none()
            && self.input_rounds == count
            && self.cef_mouse_events == count * 2
            && self.fixture_builds == count
            && self.captures == count
            && self.copies == count
            && self.cancel_calls == count
            && self.cancel_returns == count
            && self.dismissed == count
            && self.dismissed_during_cancel == count
            && self.uncorrelated_dismissed == 0
            && self.answering.is_none()
            && self.cef_ui_thread != 0
            && self.witnesses.len() == count as usize
            && self.witnesses.iter().enumerate().all(|(i, w)| {
                w.sequence == i as u64 + 1
                    && w.anchor == (20, 20)
                    && w.capture_thread == self.cef_ui_thread
                    && w.browser.0 > 0
                    && w.browser.1 > 0
                    && w.native_browser_id > 0
                    && w.context_total_bytes <= 8192
                    && w.suggestions <= 5
                    && w.frame_id_bytes > 0
                    && w.root_rows == 6
                    && w.child_rows == 3
                    && w.fixture_faithful
                    && w.binding_metadata_only
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ProbeScenario {
    Smoke,
    Frames,
    Multiple,
    Creating,
    Loading,
    PendingRequest,
    InFlight,
    CloseTimeout,
    /// Explicit-only windowless create-post rollback exercise.
    CreatePostRejected,
    /// Explicit-only D3d11 device creation fault followed by a normal create.
    DeviceCreateFailure,
    /// Explicit-only bounded fault in a real VIEW callback and recovery.
    CaptureFailureRecovery,
    /// Explicit-only CPU-clock measurement of real VIEW copy calls.
    CaptureTiming,
    /// Explicit-only replay of an unmodified, retired Browser's owned pixels.
    RetiredFrameReplay,
    /// Explicit-only Software windowless menu held while the command lane is full.
    CommandPressureMenuClose,
    /// Software host event pressure combined with command pressure and menu close.
    EventPressureMenuClose,
    /// Explicit-only joint title/command pressure, real menu and original profile completion (both Required backends).
    PressureProfileFlushDrain,
    /// Explicit-only accepted create followed by close before OnAfterCreated.
    LateCreateClose,
    /// Explicit-only windowless callback fixture, never part of default All.
    Menu,
    /// Explicit-only retained callback / ordinary reply task / shutdown probe.
    MenuDeferred,
    /// Public Runtime binding/response path with a windowless projected target.
    MenuBridge,
    /// Explicit-only actual original-frame source getter with metadata viewer.
    MenuSource,
    /// Explicit-only bounded source extraction rejection; no viewer opened.
    MenuSourceBudget,
    /// Explicit-only source visitor reference retirement across Browser and Runtime close.
    SourceVisitorDrain,
    /// Source visitor and an original profile flush completion on the same shutdown barrier.
    SourceProfileFlushDrain,
    /// Original B Find shared with A's held visitor and held cookie completion.
    SourceFindProfileDrain,
    /// Saturates A's title lane while the three original operations remain held.
    TitleSourceFindProfileDrain,
    /// Exact managed menu and command pressure beside the same original operations.
    ManagedPressureSourceFindDrain,
    /// The same combined cut, plus a real managed DevTools auxiliary held on the
    /// survivor, so 13.2's auxiliary-before-shutdown condition is exercised in
    /// one run rather than a separate scenario.
    ManagedPressureSourceFindDrainAux,
    /// Explicit-only genuine cookie flush with its UI completion marker held.
    ProfileFlushDrain,
    /// Deliberate failure before pumping an admitted original cookie flush.
    ProfileFlushLateCleanup,
    /// Explicit-only managed DevTools opened and left in flight while the
    /// Browser closes; proves the auxiliary drains to zero before shutdown.
    AuxDevTools,
    /// Explicit-only DevTools opened and closed through CefSharp's own menu
    /// commands 28440 / 28441 on a real context menu.
    MenuDevTools,
    /// Explicit-only serialized browser-wide Find; metadata owner/results only.
    MenuFind,
    /// Explicit-only real Find callback isolation across two windowless Browsers.
    MenuFindIsolation,
    /// Explicit-only original native spelling marker/replacement; dictionary disabled.
    MenuSpelling,
    /// Counting two metadata confirmations through two original Browser closes.
    PrintDrainFence,
    /// Unmodified native edit menu through the windowless public ports.
    MenuEditHistory,
    /// Real CEF desktop UX callbacks, zoom, mute, config validation, and pre-key shortcut intercept.
    DesktopUx,
    /// Real CEF JS bridge, dual-process custom schemes, confined directory/junctions, and interception.
    JsBridgeSchemes,
    /// Opt-in host capabilities, default-deny regression, cookies/preferences, PrintToPdf, and headless CDP.
    HostCapabilities,
    /// OSR CefSharp parity: async outbound drag, IAccessibilityHandler AXTree, Profile/Frame CefURLRequest, and multi-touch dispatch/cancellation.
    OsrParity,
    /// Opt-in Alloy native child HWND host (`SetAsChild`), bounds/visibility/move/focus lifecycle, parity APIs, parent-destroyed safety, and OSR coexistence.
    NativeChildHwnd,
}
impl ProbeScenario {
    pub fn has_event_pressure(self) -> bool {
        matches!(
            self,
            Self::EventPressureMenuClose | Self::PressureProfileFlushDrain
        ) || self.has_joint_title_pressure()
    }
    pub fn has_command_pressure(self) -> bool {
        matches!(
            self,
            Self::CommandPressureMenuClose
                | Self::EventPressureMenuClose
                | Self::PressureProfileFlushDrain
        )
    }
    pub fn has_profile_flush(self) -> bool {
        matches!(
            self,
            Self::ProfileFlushDrain
                | Self::PressureProfileFlushDrain
                | Self::ProfileFlushLateCleanup
        ) || self.has_joint_source_flush()
    }
    pub fn has_joint_title_pressure(self) -> bool {
        matches!(
            self,
            Self::TitleSourceFindProfileDrain
                | Self::ManagedPressureSourceFindDrain
                | Self::ManagedPressureSourceFindDrainAux
        )
    }
    pub fn has_joint_find(self) -> bool {
        self == Self::SourceFindProfileDrain || self.has_joint_title_pressure()
    }
    pub fn has_joint_source_flush(self) -> bool {
        self == Self::SourceProfileFlushDrain || self.has_joint_find()
    }
    pub fn has_source_visitor_drain(self) -> bool {
        self == Self::SourceVisitorDrain || self.has_joint_source_flush()
    }
    /// Combined cut that also keeps a real managed DevTools auxiliary open on
    /// the survivor while the original visitor is held.
    pub fn holds_aux_devtools(self) -> bool {
        self == Self::ManagedPressureSourceFindDrainAux
    }
    pub fn needs_frame(self) -> bool {
        !matches!(self, Self::Creating | Self::Loading)
    }
    pub fn browser_count(self) -> u64 {
        match self {
            Self::Multiple => 3,
            Self::RetiredFrameReplay
            | Self::PrintDrainFence
            | Self::CommandPressureMenuClose
            | Self::EventPressureMenuClose
            | Self::SourceVisitorDrain
            | Self::SourceProfileFlushDrain
            | Self::SourceFindProfileDrain
            | Self::TitleSourceFindProfileDrain
            | Self::ManagedPressureSourceFindDrain
            | Self::ManagedPressureSourceFindDrainAux
            | Self::ProfileFlushDrain
            | Self::PressureProfileFlushDrain
            | Self::MenuFindIsolation
            | Self::JsBridgeSchemes
            | Self::HostCapabilities
            | Self::OsrParity => 2,
            _ => 1,
        }
    }
}

#[derive(Debug)]
pub struct ProbeOptions {
    pub mode: ThreadMode,
    pub backend: RenderPreference,
    pub report: PathBuf,
    pub helper: PathBuf,
    pub observer_gate: Option<PathBuf>,
    pub scenario: ProbeScenario,
}
impl ProbeOptions {
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut arguments = arguments.into_iter();
        while let Some(key) = arguments.next() {
            if !matches!(
                key.as_str(),
                "--mode" | "--backend" | "--report" | "--helper" | "--observer-gate" | "--scenario"
            ) {
                return Err(format!("unknown probe argument: {key}"));
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value: {key}"))?;
            if values.insert(key.clone(), value).is_some() {
                return Err(format!("duplicate argument: {key}"));
            }
        }
        let mut required = |key: &str| values.remove(key).ok_or_else(|| format!("required: {key}"));
        let mode = match required("--mode")?.as_str() {
            "Integrated" => ThreadMode::Integrated,
            "Threaded" => ThreadMode::Threaded,
            _ => {
                return Err(
                    "probe mode must be Integrated or Threaded (All belongs to driver)".into(),
                );
            }
        };
        let backend = match required("--backend")?.as_str() {
            "Software" => RenderPreference::SoftwareRequired,
            "D3d11" => RenderPreference::D3d11Required,
            _ => return Err("probe backend must be Software or D3d11".into()),
        };
        let report = PathBuf::from(required("--report")?);
        let helper = PathBuf::from(required("--helper")?);
        if !report.is_absolute() || !helper.is_absolute() {
            return Err("report and helper paths must be absolute".into());
        }
        let scenario = match values.remove("--scenario").as_deref().unwrap_or("Smoke") {
            "Smoke" => ProbeScenario::Smoke,
            "Frames" => ProbeScenario::Frames,
            "Multiple" => ProbeScenario::Multiple,
            "Creating" => ProbeScenario::Creating,
            "Loading" => ProbeScenario::Loading,
            "PendingRequest" => ProbeScenario::PendingRequest,
            "InFlight" => ProbeScenario::InFlight,
            "CloseTimeout" => ProbeScenario::CloseTimeout,
            "CreatePostRejected" => ProbeScenario::CreatePostRejected,
            "DeviceCreateFailure" if backend == RenderPreference::D3d11Required => {
                ProbeScenario::DeviceCreateFailure
            }
            "DeviceCreateFailure" => return Err("DeviceCreateFailure requires D3d11".into()),
            "CaptureFailureRecovery" => ProbeScenario::CaptureFailureRecovery,
            "CaptureTiming" => ProbeScenario::CaptureTiming,
            "RetiredFrameReplay" => ProbeScenario::RetiredFrameReplay,
            "CommandPressureMenuClose" => ProbeScenario::CommandPressureMenuClose,
            "EventPressureMenuClose" => ProbeScenario::EventPressureMenuClose,
            "PressureProfileFlushDrain" => ProbeScenario::PressureProfileFlushDrain,
            "LateCreateClose" => ProbeScenario::LateCreateClose,
            "Menu" => ProbeScenario::Menu,
            "MenuDeferred" => ProbeScenario::MenuDeferred,
            "MenuBridge" => ProbeScenario::MenuBridge,
            "MenuSource" => ProbeScenario::MenuSource,
            "MenuSourceBudget" => ProbeScenario::MenuSourceBudget,
            "SourceVisitorDrain" => ProbeScenario::SourceVisitorDrain,
            "SourceProfileFlushDrain" => ProbeScenario::SourceProfileFlushDrain,
            "SourceFindProfileDrain" => ProbeScenario::SourceFindProfileDrain,
            "TitleSourceFindProfileDrain" => ProbeScenario::TitleSourceFindProfileDrain,
            "ManagedPressureSourceFindDrain" => ProbeScenario::ManagedPressureSourceFindDrain,
            "ManagedPressureSourceFindDrainAux" => ProbeScenario::ManagedPressureSourceFindDrainAux,
            "ProfileFlushDrain" => ProbeScenario::ProfileFlushDrain,
            "ProfileFlushLateCleanup" => ProbeScenario::ProfileFlushLateCleanup,
            "AuxDevTools" => ProbeScenario::AuxDevTools,
            "MenuDevTools" => ProbeScenario::MenuDevTools,
            "MenuFind" => ProbeScenario::MenuFind,
            "MenuFindIsolation" => ProbeScenario::MenuFindIsolation,
            "MenuSpelling" => ProbeScenario::MenuSpelling,
            "PrintDrainFence" => ProbeScenario::PrintDrainFence,
            "MenuEditHistory" => ProbeScenario::MenuEditHistory,
            "DesktopUx" => ProbeScenario::DesktopUx,
            "JsBridgeSchemes" => ProbeScenario::JsBridgeSchemes,
            "HostCapabilities" => ProbeScenario::HostCapabilities,
            "OsrParity" => ProbeScenario::OsrParity,
            "NativeChildHwnd" => ProbeScenario::NativeChildHwnd,
            _ => return Err("unknown single-process scenario".into()),
        };
        if matches!(
            scenario,
            ProbeScenario::Menu
                | ProbeScenario::MenuDeferred
                | ProbeScenario::MenuBridge
                | ProbeScenario::MenuSource
                | ProbeScenario::MenuSourceBudget
                | ProbeScenario::SourceVisitorDrain
                | ProbeScenario::SourceProfileFlushDrain
                | ProbeScenario::SourceFindProfileDrain
                | ProbeScenario::TitleSourceFindProfileDrain
                | ProbeScenario::MenuFind
                | ProbeScenario::MenuSpelling
                | ProbeScenario::CommandPressureMenuClose
                | ProbeScenario::EventPressureMenuClose
        ) && backend != RenderPreference::SoftwareRequired
        {
            return Err("Menu requires explicit Software backend".into());
        }
        let observer_gate = values.remove("--observer-gate").map(PathBuf::from);
        if observer_gate
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err("observer gate must be absolute".into());
        }
        if scenario == ProbeScenario::PrintDrainFence
            && (observer_gate.is_none() || backend != RenderPreference::SoftwareRequired)
        {
            return Err("PrintDrainFence requires Software and an explicit observer gate".into());
        }
        if scenario == ProbeScenario::MenuEditHistory && observer_gate.is_none() {
            return Err("MenuEditHistory requires a process observer gate".into());
        }
        if scenario == ProbeScenario::ProfileFlushLateCleanup
            && (mode != ThreadMode::Integrated
                || backend != RenderPreference::SoftwareRequired
                || observer_gate.is_none())
        {
            return Err(
                "ProfileFlushLateCleanup requires Integrated Software and an observer gate".into(),
            );
        }
        Ok(Self {
            mode,
            backend,
            report,
            helper,
            observer_gate,
            scenario,
        })
    }
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            ThreadMode::Integrated => "Integrated",
            ThreadMode::Threaded => "Threaded",
        }
    }
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            RenderPreference::SoftwareRequired => "Software",
            RenderPreference::D3d11Required => "D3d11",
            RenderPreference::Auto => unreachable!("probe parser never accepts Auto"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MenuBridgeReport {
    pub phase: u8,
    pub status: crate::ContextMenuStatus,
    pub fixture: crate::menu_bridge_fixture::BridgeFixtureSnapshot,
    pub selection_observed: bool,
    pub stale_selection_rejected: bool,
    pub old_id_rejected: bool,
    pub native_navigation_cancelled: bool,
    pub missing_managed_capability_cancelled: bool,
    pub binding_metadata_only: bool,
    pub pending_after_close: u64,
}
impl MenuBridgeReport {
    pub fn complete(&self) -> bool {
        self.selection_observed
            && self.stale_selection_rejected
            && self.old_id_rejected
            && self.native_navigation_cancelled
            && self.missing_managed_capability_cancelled
            && self.binding_metadata_only
            && self.pending_after_close == 0
            && self.status.retired
            && self.status.opened == 3
            && self.status.published == 3
            && self.status.cancelled == 4
            && self.status.actions_submitted == 1
            && self.status.native_dismissed == 4
            && self.status.post_failures == 0
            && self.status.invalidated >= 1
            && self.status.first_failure.as_deref()
                == Some("missing required host capability for menu command 131")
            && self.fixture.first_error.is_none()
            && self.fixture.published == 3
            && self.fixture.terminal == 3
            && self.fixture.outstanding == 0
            && self.fixture.terminal_ids == [1, 2, 3]
            && self.fixture.frame_id_bytes > 0
            && self.fixture.selection_bytes.len() == 3
            && self.fixture.selection_bytes[0] == 0
            && self.fixture.selection_bytes[1] >= 21
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct EditHistoryMenu {
    pub request: u64,
    pub enabled: [bool; 2],
    pub separators: usize,
    pub refused: Vec<i32>,
    pub stale_refused: bool,
    pub action: Option<i32>,
    pub frame_id_bytes: usize,
    pub anchor: [i32; 2],
    pub terminal: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EditHistoryReport {
    pub browser: Option<(u64, u64)>,
    pub menus: Vec<EditHistoryMenu>,
    pub renderer_witnesses: Vec<String>,
    pub status: crate::ContextMenuStatus,
    pub fixture: crate::menu_bridge_fixture::BridgeFixtureSnapshot,
}

impl EditHistoryReport {
    fn complete(&self) -> bool {
        let undo = cef::MenuId::UNDO.get_raw();
        let redo = cef::MenuId::REDO.get_raw();
        self.browser.is_some_and(|(r, b)| r > 0 && b > 0)
            && self.menus.len() == 3
            && self
                .menus
                .iter()
                .zip([
                    ([false, false], &[undo, redo][..], None),
                    ([true, false], &[redo][..], Some(undo)),
                    ([false, true], &[undo][..], Some(redo)),
                ])
                .enumerate()
                .all(|(i, (m, (enabled, refused, action)))| {
                    m.request == i as u64 + 1
                        && m.enabled == enabled
                        && m.refused == refused
                        && m.action == action
                        && m.stale_refused == (i != 0)
                        && m.separators == 2
                        && m.anchor == [20, 20]
                        && m.frame_id_bytes > 0
                        && m.frame_id_bytes == self.fixture.frame_id_bytes
                        && m.terminal
                })
            && self.renderer_witnesses
                == [
                    "edit-history:1:insertText:1",
                    "edit-history:2:historyUndo:0",
                    "edit-history:3:historyRedo:1",
                ]
            && self.status.opened == 3
            && self.status.published == 3
            && self.status.cancelled == 3
            && self.status.native_dismissed == 3
            && self.status.actions_submitted == 2
            && self.status.post_failures == 0
            && self.status.first_failure.is_none()
            && self.status.retired
            && self.fixture.published == 3
            && self.fixture.terminal == 3
            && self.fixture.outstanding == 0
            && self.fixture.terminal_ids == [1, 2, 3]
            && self.fixture.first_error.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct JsBridgeSchemesReport {
    pub app_page_ready: bool,
    pub authorized_bridge_frozen: bool,
    pub unauthorized_bridge_absent: bool,
    pub same_origin_fetch_ok: bool,
    pub interceptor_status: u16,
    pub interceptor_body_ok: bool,
    pub interceptor_header_hits: u64,
    pub interceptor_disallowed_calls: u64,
    pub external_fetch_blocked: bool,
    pub page_to_host_msg_ok: bool,
    pub host_to_page_ack_ok: bool,
    pub execute_js_ok: bool,
    pub eval_json_ok: bool,
    pub eval_exception_ok: bool,
    pub eval_unsupported_rejected: bool,
    pub unlisted_scheme_nav_denied: bool,
    pub eval_nav_cancelled: bool,
    pub eval_close_cancelled: bool,
    pub dir_file_ok: bool,
    pub dir_traversal_status: u16,
    pub dir_junction_status: u16,
    pub pending_evals_after_close: u64,
}

impl JsBridgeSchemesReport {
    pub fn complete(&self) -> bool {
        self.app_page_ready
            && self.authorized_bridge_frozen
            && self.unauthorized_bridge_absent
            && self.same_origin_fetch_ok
            && self.interceptor_status == 201
            && self.interceptor_body_ok
            && self.interceptor_header_hits >= 1
            && self.interceptor_disallowed_calls == 0
            && self.external_fetch_blocked
            && self.page_to_host_msg_ok
            && self.host_to_page_ack_ok
            && self.execute_js_ok
            && self.eval_json_ok
            && self.eval_exception_ok
            && self.eval_unsupported_rejected
            && self.unlisted_scheme_nav_denied
            && self.eval_nav_cancelled
            && self.eval_close_cancelled
            && self.dir_file_ok
            && self.dir_traversal_status == 404
            && self.dir_junction_status == 404
            && self.pending_evals_after_close == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OsrParityReport {
    pub default_deny_outbound_drag_refused: bool,
    pub optin_outbound_drag_started: bool,
    pub optin_outbound_drag_payload_redacted: bool,
    pub optin_outbound_drag_finished_no_deadlock: bool,
    pub optin_outbound_drag_stale_rejected: bool,
    pub a11y_tree_captured: bool,
    pub a11y_node_count: usize,
    pub a11y_debug_redacted: bool,
    pub a11y_cleared_on_disable: bool,
    pub url_request_profile_cookie_ok: bool,
    pub url_request_frame_post_ok: bool,
    pub url_request_auth_default_denied: bool,
    pub url_request_auth_optin_ok: bool,
    pub url_request_debug_redacted: bool,
    pub url_request_untrusted_redirect_denied: bool,
    pub url_request_disallowed_hits: u64,
    pub url_request_explicit_cancel_ok: bool,
    pub url_request_shutdown_drain_ok: bool,
    pub touch_events_dispatched_ok: bool,
    pub touch_active_cancelled_on_reset: bool,
    pub pending_url_requests_after_shutdown: u64,
}

impl OsrParityReport {
    pub fn complete(&self) -> bool {
        self.default_deny_outbound_drag_refused
            && self.optin_outbound_drag_started
            && self.optin_outbound_drag_payload_redacted
            && self.optin_outbound_drag_finished_no_deadlock
            && self.optin_outbound_drag_stale_rejected
            && self.a11y_tree_captured
            && self.a11y_node_count >= 2
            && self.a11y_debug_redacted
            && self.a11y_cleared_on_disable
            && self.url_request_profile_cookie_ok
            && self.url_request_frame_post_ok
            && self.url_request_auth_default_denied
            && self.url_request_auth_optin_ok
            && self.url_request_debug_redacted
            && self.url_request_untrusted_redirect_denied
            && self.url_request_disallowed_hits == 0
            && self.url_request_explicit_cancel_ok
            && self.url_request_shutdown_drain_ok
            && self.touch_events_dispatched_ok
            && self.touch_active_cancelled_on_reset
            && self.pending_url_requests_after_shutdown == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NativeChildHwndReport {
    pub invalid_parent_rejected: bool,
    pub invalid_bounds_rejected: bool,
    pub child_is_window: bool,
    pub child_parent_matches: bool,
    pub noactivate_stripped: bool,
    pub surface_mode_matches: bool,
    pub zero_osr_frames_on_native: bool,
    pub osr_commands_rejected: bool,
    pub bounds_tracked_ok: bool,
    pub hide_zero_size_ok: bool,
    pub restore_bounds_ok: bool,
    pub notify_parent_moved_ok: bool,
    pub focus_commands_ok: bool,
    pub policy_denied_ok: bool,
    pub eval_script_ok: bool,
    pub print_to_pdf_ok: bool,
    pub devtools_associated_open_ok: bool,
    pub devtools_closed_dissociated_ok: bool,
    pub url_request_ok: bool,
    pub graceful_close_hwnd_gone: bool,
    pub parent_destroyed_first_safe: bool,
    pub coexistence_osr_frame_advanced: bool,
}

impl NativeChildHwndReport {
    pub fn complete(&self) -> bool {
        self.invalid_parent_rejected
            && self.invalid_bounds_rejected
            && self.child_is_window
            && self.child_parent_matches
            && self.noactivate_stripped
            && self.surface_mode_matches
            && self.zero_osr_frames_on_native
            && self.osr_commands_rejected
            && self.bounds_tracked_ok
            && self.hide_zero_size_ok
            && self.restore_bounds_ok
            && self.notify_parent_moved_ok
            && self.focus_commands_ok
            && self.policy_denied_ok
            && self.eval_script_ok
            && self.print_to_pdf_ok
            && self.devtools_associated_open_ok
            && self.devtools_closed_dissociated_ok
            && self.url_request_ok
            && self.graceful_close_hwnd_gone
            && self.parent_destroyed_first_safe
            && self.coexistence_osr_frame_advanced
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HostCapabilitiesReport {
    pub default_deny_js_dialog_denied: bool,
    pub default_deny_popup_denied: bool,
    pub default_deny_download_denied: bool,
    pub default_deny_cdp_rejected: bool,
    pub pixel_verified: bool,
    pub js_dialog_resolved_once: bool,
    pub js_dialog_duplicate_rejected: bool,
    pub popup_captured_and_blocked: bool,
    pub download_forbidden_dest_rejected: bool,
    pub download_confirmed_bytes_ok: bool,
    pub auth_callback_handled: bool,
    pub auth_secrets_redacted: bool,
    pub auth_cache_cleared: bool,
    pub cookie_set_visited_deleted_ok: bool,
    pub cookie_filter_invoked: bool,
    pub cookie_zero_visit_ok: bool,
    pub cookie_secrets_redacted: bool,
    pub pref_allow_ok: bool,
    pub pref_blocked_rejected: bool,
    pub pdf_exported_header_ok: bool,
    pub cdp_method_eval_42_ok: bool,
    pub force_close_download_cancelled: bool,
    pub pending_host_reqs_after_close: u64,
    pub cdp_attached_before_shutdown: u64,
    pub cdp_dropped_on_ui_before_shutdown: u64,
    pub cdp_active_after_shutdown: u64,
}

impl HostCapabilitiesReport {
    pub fn complete(&self) -> bool {
        self.default_deny_js_dialog_denied
            && self.default_deny_popup_denied
            && self.default_deny_download_denied
            && self.default_deny_cdp_rejected
            && self.pixel_verified
            && self.js_dialog_resolved_once
            && self.js_dialog_duplicate_rejected
            && self.popup_captured_and_blocked
            && self.download_forbidden_dest_rejected
            && self.download_confirmed_bytes_ok
            && self.auth_callback_handled
            && self.auth_secrets_redacted
            && self.auth_cache_cleared
            && self.cookie_set_visited_deleted_ok
            && self.cookie_filter_invoked
            && self.cookie_zero_visit_ok
            && self.cookie_secrets_redacted
            && self.pref_allow_ok
            && self.pref_blocked_rejected
            && self.pdf_exported_header_ok
            && self.cdp_method_eval_42_ok
            && self.force_close_download_cancelled
            && self.pending_host_reqs_after_close == 0
            && self.cdp_attached_before_shutdown >= 1
            && self.cdp_dropped_on_ui_before_shutdown == self.cdp_attached_before_shutdown
            && self.cdp_active_after_shutdown == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DesktopUxReport {
    pub cursor_pointing_hand: bool,
    pub tooltip_text: Option<String>,
    pub status_observed: bool,
    pub loading_progress_max: u16,
    pub console_warn_observed: bool,
    pub zoom_event_milli: Option<i32>,
    pub zoom_snapshot_milli: Option<i32>,
    pub muted_event: Option<bool>,
    pub muted_snapshot: bool,
    pub shortcut_id: Option<u32>,
    pub passthrough_key_title: Option<String>,
    pub page_saw_shortcut_key: bool,
    pub pulled_ux_event_count: usize,
}

impl DesktopUxReport {
    fn complete(&self) -> bool {
        self.cursor_pointing_hand
            && self.tooltip_text.as_deref() == Some("Desktop UX Tooltip")
            && self.status_observed
            && self.loading_progress_max > 0
            && self.console_warn_observed
            && self.zoom_event_milli == Some(1500)
            && self.zoom_snapshot_milli == Some(1500)
            && self.muted_event == Some(true)
            && self.muted_snapshot
            && self.shortcut_id == Some(1)
            && self.passthrough_key_title.as_deref() == Some("desktop-ux:key-a")
            && !self.page_saw_shortcut_key
            && self.pulled_ux_event_count >= 5
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PrintFenceReport {
    pub metadata_only: bool,
    pub ids: [[u64; 3]; 2],
    pub a_close_sequence: u64,
    pub a_closed_source: bool,
    pub b_owned_live_at_a_close: bool,
    pub b_closed_source: bool,
    pub cancelled_original_a_only: bool,
    pub wrong_owner_and_replay_refused: bool,
    pub both_shutdown_gates_blocked: Vec<bool>,
    pub pending_by_cut: Vec<u64>,
    pub ready_after_explicit_retirement: bool,
    pub cleanup_errors: Vec<String>,
}
impl PrintFenceReport {
    fn complete(&self) -> bool {
        let [a, b] = self.ids;
        self.metadata_only
            && a[0] > 0
            && a[0] == b[0]
            && a[1] > 0
            && b[1] > a[1]
            && a[2] > 0
            && b[2] > a[2]
            && self.a_close_sequence > 0
            && self.a_closed_source
            && self.b_closed_source
            && self.b_owned_live_at_a_close
            && self.cancelled_original_a_only
            && self.wrong_owner_and_replay_refused
            && self.both_shutdown_gates_blocked == [true, true]
            && self.pending_by_cut == [2, 2, 2, 1, 1, 1, 0]
            && self.ready_after_explicit_retirement
            && self.cleanup_errors.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SpellingProbeReport {
    pub native_context: Option<crate::spelling_probe::SpellingContextSnapshot>,
    pub activation_input_submitted: bool,
    pub activation_witness: Option<[bool; 2]>,
    pub metadata_owner_only: bool,
    pub phase: u8,
    pub word_bytes: usize,
    pub selection_bytes: usize,
    pub suggestion_count: usize,
    pub frame_id_bytes: usize,
    pub replacement_bytes: usize,
    pub expected_hash: u32,
    pub observed_hash: u32,
    pub original_input_event_count: u32,
    pub effect_observed: bool,
    pub menu_terminal_separate: bool,
    pub dictionary_disabled_rejected: bool,
    pub menu_retired: bool,
    pub menu_failure: Option<String>,
    pub status: crate::SpellingStatus,
    pub fixture: crate::menu_bridge_fixture::BridgeFixtureSnapshot,
}
impl SpellingProbeReport {
    pub fn complete(&self) -> bool {
        self.metadata_owner_only
            && self.activation_input_submitted
            && self.activation_witness == Some([true, true])
            && self.phase == 3
            && self.word_bytes == 9
            && self.selection_bytes == 9
            && self.suggestion_count > 0
            && self.frame_id_bytes > 0
            && self.replacement_bytes > 0
            && self.expected_hash == self.observed_hash
            && self.original_input_event_count == 2
            && self.effect_observed
            && self.menu_terminal_separate
            && self.dictionary_disabled_rejected
            && self.menu_retired
            && self.menu_failure.as_deref()
                == Some("missing required host capability for menu command 206")
            && self.status.native_issued == 1
            && self.status.replacement_issued == 1
            && self.status.dictionary_issued == 0
            && !self.status.effect_verified
            && self.status.rejected == 0
            && self.status.first_failure.is_none()
            && self.fixture.published == 1
            && self.fixture.terminal == 1
            && self.fixture.outstanding == 0
            && self.fixture.first_error.is_none()
    }
}
/// DevTools opened and closed through the CefSharp menu commands.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MenuDevToolsReport {
    pub configured: bool,
    /// Command IDs the published model actually offered.
    pub offered: Vec<i32>,
    pub associated_after_menu_open: Option<bool>,
    pub pending_after_menu_open: u64,
    pub associated_after_menu_close: Option<bool>,
    pub pending_after_menu_close: u64,
    pub phase: Option<String>,
}
/// Managed DevTools opened while a real Browser is alive, then drained by close.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AuxDevToolsReport {
    pub configured: bool,
    pub pending_after_open: u64,
    pub phase_after_open: Option<String>,
    /// Browser host's own HasDevTools() after ShowDevTools: Some(true) is the
    /// only value proving a real DevTools browser exists (CefSharp's check).
    pub associated_after_open: Option<bool>,
    pub pending_after_close: u64,
    pub phase_after_close: Option<String>,
    pub closed: bool,
    /// Sampled immediately before request_shutdown: must already be zero, so the
    /// auxiliary is retired *before* shutdown begins rather than during it.
    pub pending_before_shutdown: u64,
    /// Explicit close_dev_tools path (CefSharp CloseDevTools): association seen
    /// right after opening, association re-sampled after the explicit close, and
    /// whether the auxiliary slot retired without closing the parent Browser.
    pub explicit_associated_after_open: Option<bool>,
    pub explicit_associated_after_close: Option<bool>,
    pub explicit_retired_after_close: bool,
}

#[derive(Serialize)]
pub struct ProbeReport {
    pub schema: u32,
    pub scenario: ProbeScenario,
    pub valid_frame_browsers: u64,
    pub loading_request_observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub early_close_backend: Option<EarlyCloseBackendWitness>,
    pub timeout_blocked_observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_timeout: Option<CloseTimeoutWitness>,
    pub in_flight_peak: u64,
    pub pending_requests: u64,
    pub cancelled_requests: u64,
    pub request_cancel_thread_id: Option<u64>,
    pub cef_shutdown_calls: u64,
    pub native_calls: u64,
    pub native_calls_after_shutdown: u64,
    pub post_shutdown_rejections: u64,
    pub requested_mode: &'static str,
    pub requested_backend: &'static str,
    pub actual_mode: Option<&'static str>,
    pub actual_backend: Option<&'static str>,
    pub pid: u32,
    pub main_thread_id: u64,
    pub init_thread_id: Option<u64>,
    pub cef_ui_thread_id: Option<u64>,
    pub shutdown_thread_id: Option<u64>,
    pub app_reference_retained: Option<bool>,
    pub manual_pump_count: u64,
    pub tasks_posted: u64,
    pub tasks_completed: u64,
    pub task_post_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_post_rejection: Option<CreatePostRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_create_failure: Option<DeviceCreateFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_failure_recovery: Option<CaptureFailureRecovery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_timing: Option<CaptureTimingWitness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_frame_replay: Option<RetiredFrameReplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_pressure_menu_close: Option<CommandPressureMenuClose>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_pressure_drain: Option<ManagedPressureDrain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_pressure: Option<EventPressure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_create_close: Option<LateCreateClose>,
    pub pending_tasks: u64,
    pub pending_tasks_at_shutdown: u64,
    pub created: u64,
    pub cancelled_creates: u64,
    pub closed: u64,
    pub frames: u64,
    pub first_frame_width: u32,
    pub first_frame_height: u32,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub menu_probe: Option<MenuProbeSnapshot>,
    pub menu_bridge: Option<MenuBridgeReport>,
    pub source_view: Option<crate::source_probe_report::SourceProbeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_visitor_drain: Option<SourceVisitorDrain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_flush_drain: Option<ProfileFlushDrain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_flush_cleanup: Option<LateFlushCleanup>,
    pub find: Option<crate::find_probe_report::FindProbeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub find_isolation: Option<crate::find_probe_report::FindIsolationReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joint_find_drain: Option<crate::find_probe_report::FindJointDrain>,
    pub spelling: Option<SpellingProbeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_fence: Option<PrintFenceReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux_devtools: Option<AuxDevToolsReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_devtools: Option<MenuDevToolsReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_history: Option<EditHistoryReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_ux: Option<DesktopUxReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub js_bridge_schemes: Option<JsBridgeSchemesReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_capabilities: Option<HostCapabilitiesReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osr_parity: Option<OsrParityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_child_hwnd: Option<NativeChildHwndReport>,
    pub menu_pending_witnesses: Vec<u64>,
    pub menu_retired_lease_with_pending: bool,
    pub menu_explicit_cancel_posts: u64,
}
pub use crate::capture::timing::{VIEW_COPY_SAMPLE_CAPACITY, ViewCopySample};
pub const MANUAL_PUMP_SAMPLE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TimingDistribution {
    pub samples: usize,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub max_ns: u64,
}
impl TimingDistribution {
    pub fn of(samples: impl IntoIterator<Item = u64>) -> Option<Self> {
        let mut sorted: Vec<_> = samples.into_iter().collect();
        let n = sorted.len();
        if n == 0 {
            return None;
        }
        sorted.sort_unstable();
        Some(Self {
            samples: n,
            p50_ns: sorted[n.div_ceil(2) - 1],
            p95_ns: sorted[n.checked_mul(95)?.div_ceil(100) - 1],
            max_ns: sorted[n - 1],
        })
    }
}

#[derive(Serialize)]
pub struct CaptureTimingWitness {
    pub browser: (u64, u64),
    pub generation: u64,
    pub baseline_sequence: u64,
    pub baseline_capture: crate::runtime::CaptureDiagnostics,
    pub observer_released: bool,
    pub invalidates_applied: u64,
    pub published_sequences: Vec<u64>,
    pub copy_samples: Vec<ViewCopySample>,
    pub copy_ns: Option<TimingDistribution>,
    pub manual_pump_calls: u64,
    pub manual_pump_samples_ns: Vec<u64>,
    pub manual_pump_ns: Option<TimingDistribution>,
    pub rearm_refused: bool,
    pub source_closed: bool,
    pub final_cpu_bytes: Option<usize>,
    pub peak_cpu_bytes: Option<usize>,
    pub cpu_limit_bytes: Option<usize>,
    pub retired_capture: Option<crate::runtime::CaptureDiagnostics>,
}
impl CaptureTimingWitness {
    pub fn complete(&self, report: &ProbeReport) -> bool {
        let n = VIEW_COPY_SAMPLE_CAPACITY;
        let Some(final_capture) = self.retired_capture.as_ref() else {
            return false;
        };
        let baseline = &self.baseline_capture;
        let backend = report.requested_backend;
        let capture_ok = match backend {
            "D3d11" => {
                baseline.d3d11_captures > 0
                    && baseline.software_captures == 0
                    && final_capture.d3d11_captures
                        >= baseline.d3d11_captures.saturating_add(n as u64)
                    && final_capture.software_captures == 0
                    && final_capture.maps == final_capture.unmaps
                    && final_capture.maps >= baseline.maps.saturating_add(n as u64)
            }
            "Software" => {
                baseline.software_captures > 0
                    && baseline.d3d11_captures == 0
                    && final_capture.software_captures
                        >= baseline.software_captures.saturating_add(n as u64)
                    && final_capture.d3d11_captures == 0
                    && final_capture.maps == 0
                    && final_capture.unmaps == 0
            }
            _ => false,
        };
        self.browser.0 > 0
            && self.browser.1 > 0
            && self.generation > 0
            && self.baseline_sequence > 0
            && self.observer_released
            && self.invalidates_applied == n as u64
            && self.published_sequences.len() == n
            && self.copy_samples.len() == n
            && self.published_sequences[0] > self.baseline_sequence
            && self.published_sequences.windows(2).all(|w| w[0] < w[1])
            && self
                .copy_samples
                .iter()
                .zip(&self.published_sequences)
                .all(|(sample, sequence)| {
                    sample.browser == self.browser
                        && sample.generation == self.generation
                        && sample.sequence == *sequence
                        && (sample.width, sample.height) == (320, 240)
                        && Some(sample.ui_thread) == report.cef_ui_thread_id
                })
            && self.copy_ns.is_some_and(|s| s.max_ns > 0)
            && self.copy_ns == TimingDistribution::of(self.copy_samples.iter().map(|s| s.copy_ns))
            && self.manual_pump_ns
                == TimingDistribution::of(self.manual_pump_samples_ns.iter().copied())
            && match report.requested_mode {
                "Integrated" => {
                    self.manual_pump_calls > 0
                        && self.manual_pump_samples_ns.len() as u64
                            == self
                                .manual_pump_calls
                                .min(MANUAL_PUMP_SAMPLE_CAPACITY as u64)
                        && self.manual_pump_calls <= report.manual_pump_count
                        && self.manual_pump_ns.is_some_and(|s| s.max_ns > 0)
                }
                "Threaded" => {
                    self.manual_pump_calls == 0
                        && self.manual_pump_samples_ns.is_empty()
                        && report.manual_pump_count == 0
                }
                _ => false,
            }
            && self.rearm_refused
            && self.source_closed
            && self.final_cpu_bytes == Some(0)
            && self
                .peak_cpu_bytes
                .zip(self.cpu_limit_bytes)
                .is_some_and(|(peak, limit)| {
                    peak >= 320 * 240 * 4
                        && peak <= limit
                        && limit == rust_cef_core::frame_budget::DEFAULT_CPU_FRAME_BUDGET
                })
            && baseline.observed_backend == Some(backend)
            && final_capture.observed_backend == Some(backend)
            && baseline.failures == 0
            && final_capture.failures == 0
            && final_capture.retired
            && !final_capture.abandoned
            && !final_capture.native_resources_live
            && final_capture.live_imports == 0
            && final_capture.staging_bytes == 0
            && Some(final_capture.owner_thread) == report.cef_ui_thread_id
            && final_capture.retirement_thread == final_capture.owner_thread
            && capture_ok
    }
}

pub use crate::profile_flush::CookieFlushMarkerProbe;

pub type ProbeBrowserEvent = ((u64, u64), u64);

#[derive(Default, Serialize)]
pub struct LateFlushCleanup {
    pub profile: (u64, u64),
    pub sequence: u64,
    pub admitted_before_forced_failure: bool,
    pub callback_thread_at_first_cleanup: Option<u64>,
    pub attempted_before_drain: bool,
    pub release_callback_thread: Option<u64>,
    pub release_task: Option<(u64, u64)>,
    pub final_ticket_completed: bool,
    pub final_pending: u64,
    pub final_completed: u64,
    pub normal_shutdown_after_failure: bool,
}

#[derive(Serialize)]
pub struct ProfileFlushDrain {
    pub profile: (u64, u64),
    pub default_profile: (u64, u64),
    pub distinct_native_contexts: bool,
    pub sequence: u64,
    pub callback_thread: u64,
    pub first: (u64, u64),
    pub survivor: (u64, u64),
    pub source_closed_after_first: bool,
    pub profile_ready_after_first: bool,
    pub pending_after_first: u64,
    pub accepted_before_release: u64,
    pub completed_before_release: u64,
    pub survivor_frame_before: u64,
    pub survivor_frame_after: u64,
    pub closed_events: Vec<ProbeBrowserEvent>,
    pub poll_only_flush: bool,
    pub finish_only_flush: bool,
    pub shutdown_before_release: Option<u64>,
    pub new_flush_refused_while_draining: bool,
    pub wrong_profile_refused: bool,
    pub wrong_sequence_refused: bool,
    pub release_task: Option<(u64, u64)>,
    pub marker_faulted: bool,
    pub original_ticket_completed: bool,
    pub final_accepted: u64,
    pub final_completed: u64,
    pub final_pending: u64,
    pub final_pending_profiles: u64,
    pub final_profile_refs: u64,
    pub root_lease_released: bool,
    pub final_cpu_bytes: Option<usize>,
}
impl ProfileFlushDrain {
    pub fn complete(&self, report: &ProbeReport) -> bool {
        self.profile.0 > 0
            && self.profile.1 > 0
            && self.default_profile == (self.profile.0, 0)
            && self.distinct_native_contexts
            && self.first.0 == self.profile.0
            && self.first.1 > 0
            && self.survivor.0 == self.profile.0
            && self.survivor.1 > self.first.1
            && self.sequence > 0
            && self.callback_thread > 0
            && Some(self.callback_thread) == report.cef_ui_thread_id
            && self.source_closed_after_first
            && self.profile_ready_after_first
            && self.pending_after_first == 1
            && self.accepted_before_release == 1
            && self.completed_before_release == 0
            && self.survivor_frame_before > 0
            && self.survivor_frame_after > self.survivor_frame_before
            && matches!(self.closed_events.as_slice(), [(a, first_seq), (b, last_seq)]
                if *a == self.first && *b == self.survivor && *first_seq > 0 && last_seq > first_seq)
            && self.poll_only_flush
            && self.finish_only_flush
            && self.shutdown_before_release == Some(0)
            && self.new_flush_refused_while_draining
            && self.wrong_profile_refused
            && self.wrong_sequence_refused
            && self
                .release_task
                .is_some_and(|(runtime, seq)| runtime == self.profile.0 && seq > 0)
            && !self.marker_faulted
            && self.original_ticket_completed
            && self.final_accepted == 1
            && self.final_completed == 1
            && self.final_pending == 0
            && self.final_pending_profiles == 0
            && self.final_profile_refs == 0
            && self.root_lease_released
            && self.final_cpu_bytes == Some(0)
            && report.task_post_failures == 0
    }
}

#[derive(Serialize)]
pub struct EarlyCloseBackendWitness {
    pub backend_browser: (u64, u64),
    pub target_browser: Option<(u64, u64)>,
    pub frame_generation: u64,
    pub frame_sequence: u64,
    pub capture: crate::runtime::CaptureDiagnostics,
    pub observer_released: bool,
    pub target_before_drain: Option<&'static str>,
    pub closed_events: Vec<ProbeBrowserEvent>,
    pub cancelled_creation: Option<ProbeBrowserEvent>,
    pub backend_source_closed: bool,
    pub retired_capture: Option<crate::runtime::CaptureDiagnostics>,
    pub target_source_closed: bool,
}

impl EarlyCloseBackendWitness {
    pub fn complete(&self, report: &ProbeReport) -> bool {
        let Some(target) = self.target_browser else {
            return false;
        };
        let Some(ui) = report.cef_ui_thread_id else {
            return false;
        };
        let capture = &self.capture;
        if self.backend_browser.0 == 0
            || self.backend_browser.1 == 0
            || target.0 != self.backend_browser.0
            || target.1 <= self.backend_browser.1
            || self.frame_generation == 0
            || self.frame_sequence == 0
            || capture.requested_backend != Some("D3d11")
            || capture.observed_backend != Some("D3d11")
            || capture.owner_thread != ui
            || !capture.native_resources_live
            || capture.d3d11_captures == 0
            || capture.software_captures != 0
            || capture.maps < capture.d3d11_captures
            || capture.maps != capture.unmaps
            || !self.observer_released
            || !self.backend_source_closed
            || !self.retired_capture.as_ref().is_some_and(|final_capture| {
                final_capture.retired
                    && !final_capture.abandoned
                    && !final_capture.native_resources_live
                    && final_capture.staging_bytes == 0
                    && final_capture.live_imports == 0
                    && final_capture.maps == final_capture.unmaps
                    && final_capture.failures == 0
                    && final_capture.retirement_thread == ui
                    && final_capture.owner_thread == ui
                    && final_capture.d3d11_captures >= capture.d3d11_captures
                    && final_capture.software_captures == 0
            })
            || !self.target_source_closed
            || report.task_post_failures != 0
            || report.valid_frame_browsers == 0
            || (report.first_frame_width, report.first_frame_height) != (320, 240)
            || report.created.checked_add(report.cancelled_creates) != Some(2)
            || self.closed_events.len() as u64 != report.created
            || self.closed_events.iter().any(|(_, seq)| *seq == 0)
            || self
                .closed_events
                .iter()
                .filter(|(id, _)| *id == self.backend_browser)
                .count()
                != 1
        {
            return false;
        }
        let target_close = self
            .closed_events
            .iter()
            .filter(|(id, _)| *id == target)
            .count();
        match report.scenario {
            ProbeScenario::Creating if self.target_before_drain == Some("Creating") => {
                match self.cancelled_creation {
                    Some((id, seq)) => {
                        id == target
                            && seq > 0
                            && report.cancelled_creates == 1
                            && target_close == 0
                    }
                    None => report.cancelled_creates == 0 && target_close == 1,
                }
            }
            ProbeScenario::Loading => {
                self.target_before_drain == Some("Alive")
                    && report.loading_request_observed
                    && report.cancelled_creates == 0
                    && self.cancelled_creation.is_none()
                    && target_close == 1
            }
            _ => false,
        }
    }
}

#[derive(Serialize)]
pub struct CreatePostRejection {
    pub rejected_browser: (u64, u64),
    pub failed_event_sequence: u64,
    pub rejected_frame_source_closed: bool,
    pub recovered_browser: Option<(u64, u64)>,
}

/// Plain IDs only. No callback, sink or frame handle can cross the UI task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedPressureCut {
    pub menu: rust_cef_core::menu::ContextMenuRequestId,
    pub source: rust_cef_core::source_view::SourceOperationId,
    pub find: rust_cef_core::find::FindOperationId,
    pub profile: rust_cef_core::contracts::ProfileHandle,
    pub flush_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PressureExpectation {
    ProbeMenu(Option<(rust_cef_core::contracts::ProfileHandle, u64)>),
    ManagedMenu(ManagedPressureCut),
    WindowMenu(
        rust_cef_core::menu::ContextMenuRequestId,
        u64,
        Option<WindowPeerFlush>,
        Option<rust_cef_core::source_view::SourceOperationId>,
        Option<rust_cef_core::find::FindOperationId>,
        Option<WindowPrintCut>,
    ),
    NoMenu,
}

impl PressureExpectation {
    pub(crate) fn flush(self) -> Option<(rust_cef_core::contracts::ProfileHandle, u64)> {
        match self {
            Self::ProbeMenu(flush) => flush,
            Self::ManagedMenu(cut) => Some((cut.profile, cut.flush_sequence)),
            Self::WindowMenu(_, _, flush, _, _, _) => flush.map(|f| (f.profile, f.sequence)),
            Self::NoMenu => None,
        }
    }

    pub(crate) fn peer_flush(self) -> Option<WindowPeerFlush> {
        match self {
            Self::WindowMenu(_, _, flush, _, _, _) => flush,
            _ => None,
        }
    }
}

/// Two metadata-only confirmations; no GPUI or provider resource crosses CEF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPrintCut {
    pub ids: [rust_cef_core::print::PrintId; 2],
    pub windows: [u64; 2],
}
impl WindowPrintCut {
    pub fn identities(self) -> [[u64; 3]; 2] {
        self.ids.map(|id| {
            [
                id.browser.runtime().value(),
                id.browser.local(),
                id.sequence.get(),
            ]
        })
    }
}

/// The unchanged named profile of the surviving Browser owns the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPeerFlush {
    pub browser: rust_cef_core::contracts::BrowserId,
    pub profile: rust_cef_core::contracts::ProfileHandle,
    pub sequence: u64,
}
impl WindowPeerFlush {
    pub fn marker(self) -> [u64; 3] {
        [
            self.profile.runtime().value(),
            self.profile.local(),
            self.sequence,
        ]
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedPressureReceipt {
    pub menu: [u64; 3],
    pub source: [u64; 3],
    pub find: [u64; 3],
    pub title: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedCloseCancel {
    pub task: (u64, u64),
    pub menu: [u64; 3],
}

#[derive(Serialize)]
pub struct LateManagedMenuReply {
    pub menu: [u64; 3],
    pub owner_closed: ProbeBrowserEvent,
    pub command_id: i32,
    pub select_error: &'static str,
    pub cancel_error: &'static str,
    pub tasks_posted: [u64; 2],
    pub native_calls: [u64; 2],
}

#[derive(Serialize)]
pub struct ManagedPressureDrain {
    pub command: CommandPressure,
    pub menu_delivery: crate::menu_bridge_fixture::BridgeFixtureSnapshot,
    pub menu_status: crate::ContextMenuStatus,
    pub late_reply: LateManagedMenuReply,
}

impl ManagedPressureDrain {
    fn complete(
        &self,
        ui: u64,
        source: &SourceVisitorDrain,
        find: &crate::find_probe_report::FindJointDrain,
        flush: &ProfileFlushDrain,
    ) -> bool {
        let m = &self.menu_delivery;
        let owner = &self.menu_status;
        let late = &self.late_reply;
        self.command.complete()
            && find.closed_events.first() == Some(&late.owner_closed)
            && late.command_id == 117
            && late.select_error == "InvalidRequest" && late.cancel_error == "InvalidRequest"
            && late.tasks_posted[0] > 0 && late.tasks_posted[0] == late.tasks_posted[1]
            && late.native_calls[0] > 0 && late.native_calls[0] == late.native_calls[1]
            && self.command.thread == ui
            && self.command.held_cookie_flush_at_close == Some([flush.profile.0, flush.profile.1, flush.sequence])
            && self.command.close_task.zip(flush.release_task).is_some_and(|(close, release)| {
                close.0 == flush.first.0 && close.0 == release.0 && close.1 < release.1
            })
            && self.command.held_managed_at_close.as_ref().is_some_and(|r| {
                self.command.cancelled_exact_menu(r.menu) && late.menu == r.menu
                    && r.menu[0..2] == source.operation[0..2] && r.source == source.operation
                    && r.find == find.pending_before_first_close && Some(r.title) == find.title_at_joint_cut
                    && matches!(m.terminal_ids.as_slice(), [initial, pending] if *initial > 0 && *initial < *pending && *pending == r.menu[2])
            })
            && m.published == 2 && m.terminal == 2 && m.outstanding == 0 && m.first_error.is_none()
            && owner.retired && owner.opened == 2 && owner.published == 2 && owner.cancelled == 2
            && owner.actions_submitted == 1 && owner.post_failures == 0 && owner.first_failure.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CommandPressure {
    pub thread: u64,
    pub menu_pending_before: u64,
    pub menu_pending_at_close: u64,
    /// Exact profile runtime/local and flush sequence at the CEF UI Close boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_cookie_flush_at_close: Option<[u64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_flush_browser_at_close: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_managed_at_close: Option<ManagedPressureReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_window_menu_at_close: Option<WindowMenuCut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_window_prints_at_close: Option<[[u64; 3]; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_menu_cancelled_by_close: Option<ManagedCloseCancel>,
    pub first_sequence: u64,
    pub accepted: u64,
    pub pending_at_close: usize,
    pub overflow_refused: bool,
    pub close_task: Option<(u64, u64)>,
    pub new_command_refused_after_intent: bool,
    pub dispatched: u64,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct WindowMenuCut {
    pub menu: [u64; 3],
    pub gpui_window: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<[u64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub find: Option<[u64; 3]>,
}

impl CommandPressure {
    pub fn complete_with_window_menu(
        &self,
        expected: WindowMenuCut,
        flush: Option<WindowPeerFlush>,
    ) -> bool {
        self.held_cookie_flush_at_close == flush.map(WindowPeerFlush::marker)
            && self.peer_flush_browser_at_close
                == flush.map(|f| (f.browser.runtime().value(), f.browser.local()))
            && self.held_managed_at_close.is_none()
            && self.held_window_menu_at_close == Some(expected)
            && self.cancelled_exact_menu(expected.menu)
            && self.complete_with_menu_count(1)
    }

    pub fn cancelled_exact_menu(&self, menu: [u64; 3]) -> bool {
        self.managed_menu_cancelled_by_close
            .as_ref()
            .is_some_and(|r| r.menu == menu && Some(r.task) == self.close_task)
    }

    pub fn complete(&self) -> bool {
        self.complete_with_menu_count(1)
    }

    pub fn complete_without_menu(&self) -> bool {
        self.held_cookie_flush_at_close.is_none()
            && self.peer_flush_browser_at_close.is_none()
            && self.held_managed_at_close.is_none()
            && self.held_window_menu_at_close.is_none()
            && self.managed_menu_cancelled_by_close.is_none()
            && self.complete_with_menu_count(0)
    }

    fn complete_with_menu_count(&self, menu_count: u64) -> bool {
        self.thread != 0
            && self.menu_pending_before == menu_count
            && self.menu_pending_at_close == menu_count
            && self.first_sequence > 0
            && self.accepted == 256
            && self.pending_at_close == 256
            && self.overflow_refused
            && self
                .close_task
                .is_some_and(|(runtime, sequence)| runtime > 0 && sequence > 0)
            && self.new_command_refused_after_intent
            && self.dispatched == self.accepted
            && self.error.is_none()
    }
}

#[derive(Serialize)]
pub struct SourceVisitorDrain {
    pub operation: [u64; 3],
    pub survivor: (u64, u64),
    pub source_closed: bool,
    pub callback_received_before_release: bool,
    pub native_live_after_first_close: u64,
    pub host_live_after_first_close: u64,
    pub survivor_frame_before: u64,
    pub survivor_frame_after: u64,
    /// Which witness proved the survivor is still live while the other Browser
    /// closed. None means the ordinary new-View-frame witness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survivor_witness: Option<String>,
    /// True only when this cut deliberately held a real managed DevTools
    /// auxiliary on the survivor (combined 13.2 run).
    pub aux_held_during_cut: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux_associated_during_cut: Option<bool>,
    /// Sampled after the cut; must be zero before any shutdown begins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux_pending_before_shutdown: Option<u64>,
    /// Explicit close_dev_tools retired the auxiliary inside this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux_retired_before_shutdown: Option<bool>,
    pub closed_events: Vec<((u64, u64), u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerated_capture: Option<AcceleratedPressureCapture>,
    pub shutdown_calls_before_release: Option<u64>,
    pub blocked_only_by_source: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush_marker_at_joint_barrier: Option<[u64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_retired_before_flush_release: Option<bool>,
    pub explicit_release_posted: bool,
    pub held_reference_gone: bool,
}
impl SourceVisitorDrain {
    pub fn complete(
        &self,
        source: &crate::source_probe_report::SourceProbeReport,
        flush: Option<&ProfileFlushDrain>,
    ) -> bool {
        let [runtime, local, sequence] = self.operation;
        let barrier = match flush {
            Some(f) => {
                !self.blocked_only_by_source
                    && self.flush_marker_at_joint_barrier
                        == Some([f.profile.0, f.profile.1, f.sequence])
                    && self.source_retired_before_flush_release == Some(true)
                    && f.first == (runtime, local)
                    && f.survivor == self.survivor
                    && f.survivor_frame_before == self.survivor_frame_before
                    && f.survivor_frame_after == self.survivor_frame_after
                    && f.closed_events == self.closed_events
            }
            None => self.blocked_only_by_source && self.flush_marker_at_joint_barrier.is_none(),
        };
        barrier
            && runtime == self.survivor.0
            && local > 0
            && self.survivor.1 > local
            && sequence > 0
            && self.source_closed
            && self.callback_received_before_release
            && self.native_live_after_first_close == 1
            && self.host_live_after_first_close == 0
            && self.survivor_frame_before > 0
            && self.survivor_frame_after > self.survivor_frame_before
            && matches!(self.closed_events.as_slice(), [(a, seq1), (b, seq2)]
                if *a == (runtime,local) && *b == self.survivor && *seq1 > 0 && seq2 > seq1)
            && self.shutdown_calls_before_release == Some(0)
            && self.explicit_release_posted
            && self.held_reference_gone
            && source.phase == 3
            && source.metadata_owner_only
            && !source.budget_case
            && source.started == 1
            && source.finished == 1
            && source.publications == 1
            && source.close_requests == 1
            && source.close_acks == 1
            && source.barrier_after_menu_terminal
            && source.operation_ids == [self.operation]
            && source.source_bytes.len() == 1
            && source.source_bytes[0] > 21
            && source.source_bytes[0] <= 8192
            && source.final_pending == 0
            && source.final_native_live == 0
            && source.final_host_live == 0
            && !source.faulted
            && source.first_failure.is_none()
            && source.sink_error.is_none()
            && source.outcomes == ["Some(Cancelled)"]
    }
}

#[derive(Serialize)]
pub struct EventPressure {
    pub browser: (u64, u64),
    pub queued: usize,
    pub rejected: u64,
    pub last_rejected_sequence: u64,
    pub accepted_sequences_ordered: bool,
    pub retained_titles_ordered: bool,
    pub resynced_title: u64,
    pub ordinary_depth_at_first_close: usize,
    pub first_close_sequence: u64,
}
impl EventPressure {
    pub fn complete(&self, command: &CommandPressureMenuClose) -> bool {
        self.complete_for(command.closed_browser, &command.closed_events)
    }

    fn complete_for(&self, browser: (u64, u64), closed: &[ProbeBrowserEvent]) -> bool {
        self.browser == browser
            && self.queued == PRESSURE_EVENT_CAPACITY
            && self.rejected == 1
            && self.last_rejected_sequence > 0
            && self.accepted_sequences_ordered
            && self.retained_titles_ordered
            && self.resynced_title == PRESSURE_EVENT_CAPACITY as u64 + 1
            && self.ordinary_depth_at_first_close == PRESSURE_EVENT_CAPACITY
            && self.first_close_sequence > self.last_rejected_sequence
            && closed.first() == Some(&(self.browser, self.first_close_sequence))
    }
}

#[derive(Serialize)]
pub struct CommandPressureMenuClose {
    pub pressure: CommandPressure,
    pub closed_browser: (u64, u64),
    pub survivor_browser: (u64, u64),
    pub frame_before: u64,
    pub frame_after: u64,
    pub first_source_closed: bool,
    pub closed_events: Vec<((u64, u64), u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerated_capture: Option<AcceleratedPressureCapture>,
}

#[derive(Serialize)]
pub struct AcceleratedPressureCapture {
    pub first_retired: crate::runtime::CaptureDiagnostics,
    pub survivor_maps_before_invalidate: u64,
    pub survivor_live_after_invalidate: crate::runtime::CaptureDiagnostics,
    pub survivor_retired: Option<crate::runtime::CaptureDiagnostics>,
}

impl AcceleratedPressureCapture {
    pub fn complete(&self, ui: u64) -> bool {
        let captured = |d: &crate::runtime::CaptureDiagnostics| {
            ui > 0
                && d.owner_thread == ui
                && d.requested_backend == Some("D3d11")
                && d.observed_backend == Some("D3d11")
                && d.d3d11_captures > 0
                && d.software_captures == 0
                && d.opens >= d.d3d11_captures
                && d.copies >= d.d3d11_captures
                && d.maps >= d.d3d11_captures
                && d.maps == d.unmaps
                && d.live_imports == 0
                && d.failures == 0
                && !d.abandoned
        };
        let retired = |d: &crate::runtime::CaptureDiagnostics| {
            captured(d)
                && d.retired
                && d.retirement_thread == ui
                && !d.native_resources_live
                && d.staging_bytes == 0
        };
        let live = &self.survivor_live_after_invalidate;
        retired(&self.first_retired)
            && captured(live)
            && !live.retired
            && live.native_resources_live
            && live.staging_bytes >= 320 * 240 * 4
            && self.survivor_maps_before_invalidate > 0
            && live.maps > self.survivor_maps_before_invalidate
            && self
                .survivor_retired
                .as_ref()
                .is_some_and(|d| retired(d) && d.maps >= live.maps)
    }
}

impl CommandPressureMenuClose {
    pub fn complete(&self) -> bool {
        self.pressure.complete()
            && self
                .pressure
                .close_task
                .is_some_and(|(runtime, _)| runtime == self.closed_browser.0)
            && matches!(self.closed_events.as_slice(), [(first, seq1), (second, seq2)]
                if *first == self.closed_browser && *second == self.survivor_browser && *seq1 > 0 && seq2 > seq1)
            && self.closed_browser.0 == self.survivor_browser.0
            && self.closed_browser.1 > 0
            && self.survivor_browser.1 > self.closed_browser.1
            && self.frame_before > 0
            && self.frame_after > self.frame_before
            && self.first_source_closed
    }
}

#[derive(Serialize)]
pub struct RetiredFrameReplay {
    pub old_browser: (u64, u64),
    pub live_browser: (u64, u64),
    pub generation: u64,
    pub retired_sequence: u64,
    pub live_sequence_before: u64,
    pub live_sequence_after_repaint: u64,
    pub closed_error: Option<String>,
    pub live_error: Option<String>,
    pub old_source_closed_empty: bool,
    pub live_frame_unchanged: bool,
    pub old_cpu_bytes_before_drop: usize,
    pub old_cpu_bytes_after_drop: usize,
}

impl RetiredFrameReplay {
    pub fn complete(&self) -> bool {
        self.old_browser.0 != 0
            && self.old_browser.0 == self.live_browser.0
            && self.old_browser.1 > 0
            && self.old_browser.1 < self.live_browser.1
            && self.generation > 0
            && self.retired_sequence > 0
            && self.live_sequence_before > 0
            && self.live_sequence_after_repaint > self.live_sequence_before
            && self.closed_error.as_deref() == Some("Closed")
            && self.live_error.as_deref() == Some("WrongIdentity")
            && self.old_source_closed_empty
            && self.live_frame_unchanged
            && self.old_cpu_bytes_before_drop == 320 * 240 * 4
            && self.old_cpu_bytes_after_drop == 0
    }
}

#[derive(Serialize)]
pub struct DeviceCreateFailure {
    pub rejected_browser: (u64, u64),
    pub failed_event_sequence: u64,
    pub rejected_frame_source_closed: bool,
    pub capture: crate::capture::CaptureDiagnostics,
    pub recovered_browser: Option<(u64, u64)>,
}

impl DeviceCreateFailure {
    pub fn rejected_without_fallback(&self) -> bool {
        let capture = &self.capture;
        self.failed_event_sequence > 0
            && self.rejected_frame_source_closed
            && capture.requested_backend == Some("D3d11")
            && capture.observed_backend.is_none()
            && capture.failures == 1
            && capture.last_failure.as_deref() == Some("DeviceCreation")
            && capture.last_failure_code.is_none()
            && capture.software_captures == 0
            && capture.d3d11_captures == 0
            && capture.retired
            && !capture.abandoned
            && !capture.native_resources_live
            && capture.owner_thread != 0
            && capture.retirement_thread == capture.owner_thread
    }
}

#[derive(Serialize)]
pub struct LateCreateClose {
    pub late_browser: (u64, u64),
    pub callback_after_accepted_close_intent: bool,
    pub frame_source_closed: bool,
    pub frames_before_recovery: u64,
    pub recovered_browser: Option<(u64, u64)>,
}

#[derive(Serialize)]
pub struct CloseTimeoutWitness {
    pub browser: (u64, u64),
    pub poll_open_browsers: Option<Vec<(u64, u64)>>,
    pub finish_open_browsers: Option<Vec<(u64, u64)>>,
    pub shutdown_calls_before_release: u64,
    pub closed_events_before_release: u64,
    pub browser_closing_before_release: bool,
}

impl CloseTimeoutWitness {
    pub fn complete(&self) -> bool {
        self.poll_open_browsers.as_deref() == Some(&[self.browser])
            && self.finish_open_browsers.as_deref() == Some(&[self.browser])
            && self.shutdown_calls_before_release == 0
            && self.closed_events_before_release == 0
            && self.browser_closing_before_release
    }
}

impl ProbeReport {
    pub fn started(options: &ProbeOptions, main_thread_id: u64) -> Self {
        Self {
            schema: 1,
            scenario: options.scenario,
            valid_frame_browsers: 0,
            loading_request_observed: false,
            early_close_backend: None,
            timeout_blocked_observed: false,
            close_timeout: None,
            in_flight_peak: 0,
            pending_requests: 0,
            cancelled_requests: 0,
            request_cancel_thread_id: None,
            cef_shutdown_calls: 0,
            native_calls: 0,
            native_calls_after_shutdown: 0,
            post_shutdown_rejections: 0,
            requested_mode: options.mode_name(),
            requested_backend: options.backend_name(),
            actual_mode: None,
            actual_backend: None,
            pid: std::process::id(),
            main_thread_id,
            init_thread_id: None,
            cef_ui_thread_id: None,
            shutdown_thread_id: None,
            app_reference_retained: None,
            manual_pump_count: 0,
            tasks_posted: 0,
            tasks_completed: 0,
            task_post_failures: 0,
            create_post_rejection: None,
            device_create_failure: None,
            capture_failure_recovery: None,
            capture_timing: None,
            retired_frame_replay: None,
            command_pressure_menu_close: None,
            managed_pressure_drain: None,
            event_pressure: None,
            late_create_close: None,
            pending_tasks: 0,
            pending_tasks_at_shutdown: 0,
            created: 0,
            cancelled_creates: 0,
            closed: 0,
            frames: 0,
            first_frame_width: 0,
            first_frame_height: 0,
            exit_code: None,
            error: None,
            menu_probe: None,
            menu_bridge: None,
            source_view: None,
            source_visitor_drain: None,
            profile_flush_drain: None,
            late_flush_cleanup: None,
            find: None,
            find_isolation: None,
            joint_find_drain: None,
            spelling: None,
            print_fence: None,
            aux_devtools: None,
            menu_devtools: None,
            edit_history: None,
            desktop_ux: None,
            js_bridge_schemes: None,
            host_capabilities: None,
            osr_parity: None,
            native_child_hwnd: None,
            menu_pending_witnesses: Vec::new(),
            menu_retired_lease_with_pending: false,
            menu_explicit_cancel_posts: 0,
        }
    }
    pub fn native_success(&self) -> bool {
        self.error.is_none()
            && self.exit_code == Some(0)
            && self.actual_mode == Some(self.requested_mode)
            && (self.actual_backend == Some(self.requested_backend)
                || (!self.scenario.needs_frame()
                    && self.requested_backend == "Software"
                    && self.actual_backend.is_none()))
            && self.main_thread_id != 0
            && self.init_thread_id == Some(self.main_thread_id)
            && self.shutdown_thread_id == Some(self.main_thread_id)
            && self.app_reference_retained == Some(false)
            && self.cef_ui_thread_id.is_some_and(|id| {
                id != 0
                    && match self.requested_mode {
                        "Integrated" => id == self.main_thread_id,
                        "Threaded" => id != self.main_thread_id && self.manual_pump_count == 0,
                        _ => false,
                    }
            })
            && self.tasks_posted > 0
            && self.pending_tasks == 0
            && self.tasks_completed.checked_add(self.task_post_failures) == Some(self.tasks_posted)
            && self.native_calls > 0
            && self.native_calls_after_shutdown == 0
            && self.post_shutdown_rejections > 0
            && self.cef_shutdown_calls == 1
            && self.pending_requests == 0
            && self.closed == self.created
            && (self.scenario != ProbeScenario::CreatePostRejected
                || self.task_post_failures == 1
                    && self.create_post_rejection.as_ref().is_some_and(|witness| {
                        witness.failed_event_sequence > 0
                            && witness.rejected_frame_source_closed
                            && witness.recovered_browser.is_some_and(|(runtime, local)| {
                                runtime == witness.rejected_browser.0
                                    && local > witness.rejected_browser.1
                            })
                    }))
            && (self.scenario != ProbeScenario::DeviceCreateFailure
                || self.task_post_failures == 0
                    && self.device_create_failure.as_ref().is_some_and(|witness| {
                        witness.rejected_without_fallback()
                            && Some(witness.capture.owner_thread) == self.cef_ui_thread_id
                            && witness.recovered_browser.is_some_and(|(runtime, local)| {
                                runtime == witness.rejected_browser.0
                                    && local > witness.rejected_browser.1
                            })
                    }))
            && (self.scenario != ProbeScenario::CaptureTiming
                || self.task_post_failures == 0
                    && self
                        .capture_timing
                        .as_ref()
                        .is_some_and(|w| w.complete(self)))
            && (self.scenario != ProbeScenario::CaptureFailureRecovery
                || self.task_post_failures == 0
                    && self
                        .capture_failure_recovery
                        .as_ref()
                        .is_some_and(|witness| {
                            witness.complete()
                                && Some(witness.callback.thread) == self.cef_ui_thread_id
                                && witness.callback.backend == self.requested_backend
                        }))
            && (self.scenario != ProbeScenario::RetiredFrameReplay
                || self.task_post_failures == 0
                    && self
                        .retired_frame_replay
                        .as_ref()
                        .is_some_and(RetiredFrameReplay::complete))
            && (!self.scenario.has_command_pressure()
                || self.task_post_failures == 0
                    && self.command_pressure_menu_close.as_ref().is_some_and(|w| {
                        w.complete()
                            && Some(w.pressure.thread) == self.cef_ui_thread_id
                            && self.menu_probe.as_ref().is_some_and(|m| {
                                m.retired
                                    && m.complete(1)
                                    && m.deferred
                                    && m.pending_continuations == 0
                                    && m.dismissal_work == [1]
                                    && Some(m.cef_ui_thread) == self.cef_ui_thread_id
                                    && m.witnesses[0].browser == w.closed_browser
                            })
                            && (!self.scenario.has_event_pressure()
                                || self
                                    .event_pressure
                                    .as_ref()
                                    .is_some_and(|events| events.complete(w)))
                    }))
            && (self.scenario != ProbeScenario::Menu
                || self.menu_probe.as_ref().is_some_and(|m| {
                    m.retired && m.complete(2) && Some(m.cef_ui_thread) == self.cef_ui_thread_id
                }))
            && (self.scenario != ProbeScenario::MenuDeferred
                || self.menu_probe.as_ref().is_some_and(|m| {
                    m.deferred
                        && m.retired
                        && !m.enabled
                        && m.complete(3)
                        && m.pending_continuations == 0
                        && m.dismissal_work == [1, 1, 1]
                        && Some(m.cef_ui_thread) == self.cef_ui_thread_id
                }) && self.menu_pending_witnesses == [1, 1, 1]
                    && self.menu_retired_lease_with_pending
                    && self.menu_explicit_cancel_posts == 2)
            && (self.scenario != ProbeScenario::MenuBridge
                || self
                    .menu_bridge
                    .as_ref()
                    .is_some_and(MenuBridgeReport::complete))
            && (!matches!(
                self.scenario,
                ProbeScenario::MenuSource | ProbeScenario::MenuSourceBudget
            ) || self
                .source_view
                .as_ref()
                .is_some_and(crate::source_probe_report::SourceProbeReport::complete))
            && (!self.scenario.has_source_visitor_drain()
                || self.task_post_failures == 0
                    && self
                        .source_visitor_drain
                        .as_ref()
                        .zip(self.source_view.as_ref())
                        .is_some_and(|(w, s)| {
                            if self.scenario.has_joint_source_flush() {
                                self.profile_flush_drain
                                    .as_ref()
                                    .is_some_and(|f| w.complete(s, Some(f)))
                            } else {
                                w.complete(s, None)
                            }
                        }))
            && (self.scenario != ProbeScenario::MenuEditHistory
                || self
                    .edit_history
                    .as_ref()
                    .is_some_and(EditHistoryReport::complete))
            && (self.scenario != ProbeScenario::DesktopUx
                || self
                    .desktop_ux
                    .as_ref()
                    .is_some_and(DesktopUxReport::complete))
            && (self.scenario != ProbeScenario::JsBridgeSchemes
                || self
                    .js_bridge_schemes
                    .as_ref()
                    .is_some_and(JsBridgeSchemesReport::complete))
            && (self.scenario != ProbeScenario::HostCapabilities
                || self
                    .host_capabilities
                    .as_ref()
                    .is_some_and(HostCapabilitiesReport::complete))
            && (self.scenario != ProbeScenario::OsrParity
                || self
                    .osr_parity
                    .as_ref()
                    .is_some_and(OsrParityReport::complete))
            && (self.scenario != ProbeScenario::PrintDrainFence
                || self
                    .print_fence
                    .as_ref()
                    .is_some_and(PrintFenceReport::complete))
            && (self.scenario != ProbeScenario::MenuSpelling
                || self
                    .spelling
                    .as_ref()
                    .is_some_and(SpellingProbeReport::complete))
            && (self.scenario != ProbeScenario::MenuFind
                || self.find.as_ref().is_some_and(|r| {
                    r.complete()
                        && r.callback_threads
                            .iter()
                            .all(|t| Some(*t) == self.cef_ui_thread_id)
                }))
            && (self.scenario != ProbeScenario::MenuFindIsolation
                || self.task_post_failures == 0
                    && self.find_isolation.as_ref().is_some_and(|w| {
                        self.cef_ui_thread_id
                            .is_some_and(|thread| w.complete(thread))
                    }))
            && (!self.scenario.has_joint_find()
                || self.joint_find_drain.as_ref().is_some_and(|find| {
                    self.source_visitor_drain
                        .as_ref()
                        .zip(self.profile_flush_drain.as_ref())
                        .is_some_and(|(source, flush)| {
                            self.cef_ui_thread_id
                                .is_some_and(|ui| find.complete(ui, source, flush))
                                && (!self.scenario.has_joint_title_pressure()
                                    || self.event_pressure.as_ref().is_some_and(|events| {
                                        events.complete_for(flush.first, &find.closed_events)
                                            && find.title_at_joint_cut
                                                == Some(events.resynced_title)
                                    }))
                                && (self.scenario != ProbeScenario::ManagedPressureSourceFindDrain
                                    || self.managed_pressure_drain.as_ref().is_some_and(
                                        |pressure| {
                                            self.cef_ui_thread_id.is_some_and(|ui| {
                                                pressure.complete(ui, source, find, flush)
                                                    && (self.requested_backend != "D3d11"
                                                        || source
                                                            .accelerated_capture
                                                            .as_ref()
                                                            .is_some_and(|capture| {
                                                                capture.complete(ui)
                                                            }))
                                            })
                                        },
                                    ))
                        })
                }))
            && (!self.scenario.has_profile_flush()
                || self.profile_flush_drain.as_ref().is_some_and(|w| {
                    w.complete(self)
                        && (self.scenario != ProbeScenario::PressureProfileFlushDrain
                            || self
                                .command_pressure_menu_close
                                .as_ref()
                                .is_some_and(|pressure| {
                                    (self.requested_backend != "D3d11"
                                        || self.cef_ui_thread_id.is_some_and(|ui| {
                                            pressure
                                                .accelerated_capture
                                                .as_ref()
                                                .is_some_and(|capture| capture.complete(ui))
                                        }))
                                        && pressure.pressure.held_cookie_flush_at_close
                                            == Some([w.profile.0, w.profile.1, w.sequence])
                                        && pressure.closed_browser == w.first
                                        && pressure.survivor_browser == w.survivor
                                        && pressure.frame_before == w.survivor_frame_before
                                        && pressure.frame_after == w.survivor_frame_after
                                        && pressure.closed_events == w.closed_events
                                        && pressure
                                            .pressure
                                            .close_task
                                            .zip(w.release_task)
                                            .is_some_and(|(close, release)| {
                                                close.0 == release.0 && close.1 < release.1
                                            })
                                }))
                }))
            && match self.scenario {
                ProbeScenario::Creating | ProbeScenario::Loading
                    if self.requested_backend == "D3d11" =>
                {
                    self.early_close_backend
                        .as_ref()
                        .is_some_and(|w| w.complete(self))
                }
                ProbeScenario::Creating => {
                    self.created.checked_add(self.cancelled_creates) == Some(1)
                }
                ProbeScenario::Loading => self.created == 1 && self.loading_request_observed,
                ProbeScenario::LateCreateClose => {
                    self.created == 2
                        && self.valid_frame_browsers == 1
                        && self.frames > 0
                        && self.first_frame_width > 0
                        && self.first_frame_height > 0
                        && self.task_post_failures == 0
                        && self.late_create_close.as_ref().is_some_and(|witness| {
                            witness.callback_after_accepted_close_intent
                                && witness.frame_source_closed
                                && witness.frames_before_recovery == 0
                                && witness.recovered_browser.is_some_and(|(runtime, local)| {
                                    runtime == witness.late_browser.0
                                        && local > witness.late_browser.1
                                })
                        })
                }
                ProbeScenario::NativeChildHwnd => {
                    self.created == 3
                        && self.valid_frame_browsers == 1
                        && self.frames > 0
                        && self.first_frame_width > 0
                        && self.first_frame_height > 0
                        && self.task_post_failures == 0
                        && self
                            .native_child_hwnd
                            .as_ref()
                            .is_some_and(NativeChildHwndReport::complete)
                }
                _ => {
                    self.created == self.scenario.browser_count()
                        && self.valid_frame_browsers == self.scenario.browser_count()
                        && self.frames > 0
                        && self.first_frame_width > 0
                        && self.first_frame_height > 0
                        && (self.scenario != ProbeScenario::PendingRequest
                            || self.cancelled_requests == 1
                                && self.request_cancel_thread_id.is_some()
                                && self.request_cancel_thread_id == self.cef_ui_thread_id)
                        && (self.scenario != ProbeScenario::CloseTimeout
                            || self.timeout_blocked_observed
                                && self
                                    .close_timeout
                                    .as_ref()
                                    .is_some_and(CloseTimeoutWitness::complete))
                        && (self.scenario != ProbeScenario::InFlight
                            || self.in_flight_peak > 0 && self.pending_tasks_at_shutdown > 0)
                }
            }
    }
}
