//! Non-default validation bridge; the opt-in scene proves pixels separately.
//! User-approved host-owned visibility, NOT automatic component policy.
//! Requests are one-shot, application is separately observed, Drop does nothing.
use super::pointer_probe::PointerProbe;
use super::*;
use rust_cef_gpui::{
    contracts::SessionState,
    ports::{BrowserCommand, BrowserHandle, CommandId},
    runtime::FrameRecoveryDiagnostics,
};
use std::ffi::c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn ProcessIdToSessionId(process: u32, session: *mut u32) -> i32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn ShowWindowAsync(hwnd: isize, command: i32) -> i32;
}
#[link(name = "wtsapi32")]
unsafe extern "system" {
    fn WTSQuerySessionInformationW(
        server: *mut c_void,
        session: u32,
        class: i32,
        data: *mut *mut c_void,
        bytes: *mut u32,
    ) -> i32;
    fn WTSFreeMemory(data: *mut c_void);
}
#[repr(C)]
struct OsVersion {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OsVersion) -> i32;
}

// SDK WTSINFOEXW: DWORD Level, then an 8-byte aligned union containing
// WTSINFOEX_LEVEL1_W (it contains LARGE_INTEGER). Read ONLY the 20-byte
// discriminator/session/state/flags prefix, never user/domain/input fields.
const WTS_INFO_EX_BYTES: usize = 232;
fn unlocked_prefix(data: &[u8], returned_bytes: u32, session: u32, os_major: u32) -> bool {
    let word = |at| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    os_major >= 10
        && returned_bytes >= WTS_INFO_EX_BYTES as u32
        && data.len() >= 20
        && word(0) == 1
        && word(8) == session
        && word(12) == 0
        && word(16) == 1
}
struct WtsBuffer(*mut c_void);
impl Drop for WtsBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WTSFreeMemory(self.0) };
        }
    }
}
pub(super) fn require_unlocked_session() -> Result<(), String> {
    let mut version = OsVersion {
        size: std::mem::size_of::<OsVersion>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // Windows 7 reverses LOCK/UNLOCK flags. CEF146's Windows10+ boundary is
    // required here too, not guessed from a compatibility-manifest version.
    if unsafe { RtlGetVersion(&mut version) } != 0 || version.major < 10 {
        return Err("visibility requires known Windows10+ session semantics".into());
    }
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return Err("visibility current-session query failed".into());
    }
    let mut data = std::ptr::null_mut();
    let mut bytes = 0;
    let accepted = unsafe {
        WTSQuerySessionInformationW(std::ptr::null_mut(), session, 25, &mut data, &mut bytes)
    };
    let data = WtsBuffer(data);
    if accepted == 0 || data.0.is_null() || bytes < WTS_INFO_EX_BYTES as u32 {
        return Err("visibility session readiness unknown".into());
    }
    // Only the prefix is materialized. Length validation uses the complete
    // x64 SDK struct size; no username/domain strings enter a Rust value/log.
    let prefix = unsafe { std::slice::from_raw_parts(data.0.cast::<u8>(), 20) };
    if !unlocked_prefix(prefix, bytes, session, version.major) {
        return Err("visibility session locked/disconnected/unknown; no window operation".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Ready,
    MinimizeRequested,
    HidePosted,
    Hidden,
    RestoreRequested,
    ShowPosted,
    SourceRecovered,
    Retired,
    Failed,
}
#[derive(Debug)]
struct Ledger {
    phase: Phase,
    minimize_requests: u8,
    restore_requests: u8,
}
impl Ledger {
    fn new() -> Self {
        Self {
            phase: Phase::Ready,
            minimize_requests: 0,
            restore_requests: 0,
        }
    }
    fn minimize(&mut self) -> Result<(), String> {
        if self.phase != Phase::Ready || self.minimize_requests != 0 {
            return Err("duplicate/out-of-phase minimize".into());
        }
        self.minimize_requests = 1;
        self.phase = Phase::MinimizeRequested;
        Ok(())
    }
    fn hidden_ready(&mut self, ready: bool) -> Result<bool, String> {
        match self.phase {
            Phase::HidePosted => Ok(ready),
            Phase::Hidden if ready => Ok(true),
            Phase::Hidden => {
                self.phase = Phase::Failed;
                Err("hidden interval interrupted; old quiet clock is permanently ineligible".into())
            }
            _ => Err("hidden readiness out of phase".into()),
        }
    }
    fn hidden_result(&mut self, result: Result<bool, String>) -> Result<bool, String> {
        if self.phase == Phase::Hidden && result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }
    fn restore(&mut self) -> Result<(), String> {
        if self.phase != Phase::Hidden || self.minimize_requests != 1 || self.restore_requests != 0
        {
            return Err("duplicate/out-of-phase restore".into());
        }
        self.restore_requests = 1;
        self.phase = Phase::RestoreRequested;
        Ok(())
    }
    fn retire(&mut self) -> Result<(), String> {
        if self.phase != Phase::SourceRecovered
            || self.minimize_requests != 1
            || self.restore_requests != 1
        {
            return Err("visibility retirement before source recovery".into());
        }
        self.phase = Phase::Retired;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    hwnd: u64,
    pid: u32,
    thread: u32,
}
struct Native {
    visible: bool,
    iconic: bool,
}
fn native(probe: &PointerProbe, owner: Owner) -> Result<Native, String> {
    let (original_hwnd, original_pid, thread) = probe.owned_identity()?; // Original live property + WNDPROC, not just a recycled number.
    let mut pid = 0;
    let hwnd = owner.hwnd as isize;
    if thread != owner.thread
        || unsafe { GetCurrentThreadId() } != thread
        || unsafe { IsWindow(hwnd) } == 0
        || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } != thread
        || pid != owner.pid
        || pid != unsafe { GetCurrentProcessId() }
        || original_hwnd != owner.hwnd
        || original_pid != owner.pid
    {
        return Err("visibility original owned HWND/thread lost".into());
    }
    Ok(Native {
        visible: unsafe { IsWindowVisible(hwnd) } != 0,
        iconic: unsafe { IsIconic(hwnd) } != 0,
    })
}

/// Inert until explicitly constructed by the opt-in single-original-window scene.
/// Initial root<->HWND correspondence must also be independently proven by that
/// scene's external stage0 owner/full-client gate, not trusted title alone.
/// No default scenario uses this bridge; no GUI acceptance is reported here.
pub(super) struct OwnerVisibility {
    root: WindowHandle<HostRoot>,
    handle: BrowserHandle,
    browser: BrowserId,
    owner: Owner,
    ledger: Ledger,
    viewport: Viewport,
    before_hide_generation: u64,
    hidden_generation: u64,
    retained: Option<Arc<OwnedFrame>>,
    hidden_recovery: Option<FrameRecoveryDiagnostics>,
    hidden_at: Option<Instant>,
    hidden_quiet_ms: Option<u128>,
    hidden_observed_ms: Option<u128>,
    last_hidden_observation: Option<String>,
    hide_id: Option<CommandId>,
    show_id: Option<CommandId>,
}
impl OwnerVisibility {
    pub(super) fn bind(
        root: WindowHandle<HostRoot>,
        handle: BrowserHandle,
        probe: &PointerProbe,
        viewport: Viewport,
    ) -> Result<Self, String> {
        if APP_BORROW.with(Cell::get) {
            return Err("visibility bind inside GPUI borrow".into());
        }
        if viewport.width <= 1
            || viewport.height <= 1
            || !viewport.scale.is_finite()
            || viewport.scale <= 0.
        {
            return Err("visibility invalid viewport".into());
        }
        require_unlocked_session()?;
        let info = probe.info()?;
        let owner = Owner {
            hwnd: info.hwnd,
            pid: info.pid,
            thread: probe.owner_thread()?,
        };
        let n = native(probe, owner)?;
        let snapshot = handle.snapshot();
        if !n.visible || n.iconic || snapshot.state != SessionState::Alive {
            return Err("visibility requires live visible initial window/Browser".into());
        }
        Ok(Self {
            root,
            handle,
            browser: snapshot.browser,
            owner,
            ledger: Ledger::new(),
            viewport,
            before_hide_generation: 0,
            hidden_generation: 0,
            retained: None,
            hidden_recovery: None,
            hidden_at: None,
            hidden_quiet_ms: None,
            hidden_observed_ms: None,
            last_hidden_observation: None,
            hide_id: None,
            show_id: None,
        })
    }
    fn qualify(&self, probe: &PointerProbe) -> Result<Native, String> {
        if APP_BORROW.with(Cell::get) {
            return Err("visibility native call inside GPUI borrow".into());
        }
        require_unlocked_session()?;
        let snapshot = self.handle.snapshot();
        if snapshot.browser != self.browser
            || snapshot.state != SessionState::Alive
            || matches!(self.ledger.phase, Phase::Retired | Phase::Failed)
        {
            return Err("visibility Browser not Alive or gate retired".into());
        }
        native(probe, self.owner)
    }
    pub(super) fn minimize(
        &mut self,
        cx: &mut AsyncApp,
        probe: &PointerProbe,
    ) -> Result<(), String> {
        let n = self.qualify(probe)?;
        if !n.visible || n.iconic {
            return Err("minimize initial native state changed".into());
        }
        self.ledger.minimize()?; // Never reset after an Access/native failure; no retry operation.
        if let Err(e) = update(cx, self.root, |_, w, _| w.minimize_window()) {
            self.ledger.phase = Phase::Failed;
            return Err(e);
        }
        Ok(())
    }
    pub(super) fn hide_if_minimized(
        &mut self,
        rt: &Runtime,
        probe: &PointerProbe,
    ) -> Result<bool, String> {
        if self.ledger.phase != Phase::MinimizeRequested {
            return Err("hide out of phase".into());
        }
        if !self.qualify(probe)?.iconic {
            return Ok(false);
        } // Observe only; do not minimize again.
        let id = self.browser;
        let s = rt
            .frame_source(id)
            .map_err(|e| format!("visibility source: {e:?}"))?
            .snapshot();
        let f = s
            .view
            .frame
            .as_ref()
            .ok_or("visibility has no retained source frame")?;
        if s.closed
            || !s.view.visible
            || s.viewport != Some(self.viewport)
            || s.view.generation != f.info.surface_generation
            || f.info.browser != id
            || f.info.surface != SurfaceKind::View
        {
            return Err("visibility pre-hide source identity/viewport changed".into());
        }
        self.retained = Some(f.clone());
        self.before_hide_generation = s.view.generation;
        self.ledger.phase = Phase::Failed; // Admission failure is not an implicit retry slot.
        let command = self
            .handle
            .submit(BrowserCommand::SetVisible(false))
            .map_err(|e| format!("visibility hide admission: {e:?}"))?;
        if command.browser != id || command.sequence == 0 {
            return Err("hide command identity invalid".into());
        }
        self.hide_id = Some(command);
        self.ledger.phase = Phase::HidePosted;
        Ok(true)
    }
    pub(super) fn observe_hidden(
        &mut self,
        rt: &Runtime,
        probe: &PointerProbe,
    ) -> Result<bool, String> {
        let result = self.observe_hidden_inner(rt, probe);
        self.ledger.hidden_result(result)
    }
    fn observe_hidden_inner(&mut self, rt: &Runtime, probe: &PointerProbe) -> Result<bool, String> {
        if !matches!(self.ledger.phase, Phase::HidePosted | Phase::Hidden) {
            return Err("hidden observation out of phase".into());
        }
        if !self.qualify(probe)?.iconic {
            return Err("owned window restored before explicit host restore".into());
        }
        let id = self.browser;
        let s = rt
            .frame_source(id)
            .map_err(|e| format!("hidden source: {e:?}"))?
            .snapshot();
        let r = rt
            .frame_recovery_diagnostics(id)
            .map_err(|e| format!("hidden recovery: {e:?}"))?;
        if s.closed || r.post_failures != 0 {
            return Err("hidden terminal/post failure".into());
        }
        if !self
            .ledger
            .hidden_ready(!s.view.visible && r.status == "Hidden" && !r.queued)?
        {
            return Ok(false);
        }
        let retained = self.retained.as_ref().ok_or("missing pre-hide owner")?;
        if s.viewport != Some(self.viewport)
            || s.view.generation <= self.before_hide_generation
            || !s
                .view
                .frame
                .as_ref()
                .is_some_and(|f| Arc::ptr_eq(f, retained))
            || s.popup.visible
            || s.popup.frame.is_some()
        {
            return Err("hidden mailbox failed retention/generation/popup contract".into());
        }
        if let Some(base) = &self.hidden_recovery {
            if r.requests_total != base.requests_total
                || r.native_invalidate_entries != base.native_invalidate_entries
                || r.acks != base.acks
                || r.exhaustions != base.exhaustions
                || s.view.generation != self.hidden_generation
            {
                return Err("hidden interval advanced refresh/ack/generation".into());
            }
        } else {
            self.hidden_generation = s.view.generation;
            self.hidden_recovery = Some(r.clone());
            self.hidden_at = Some(Instant::now());
            self.ledger.phase = Phase::Hidden;
        }
        // Only successful observations advance this span. Merely serializing
        // later, waiting for an observer, or painting cannot add quiet time.
        self.hidden_observed_ms = self.hidden_at.map(|t| t.elapsed().as_millis());
        self.last_hidden_observation = Some(object(vec![
            ("source_generation", s.view.generation.to_string()),
            ("source_visible", bool_json(s.view.visible)),
            ("popup_visible", bool_json(s.popup.visible)),
            ("popup_frame_present", bool_json(s.popup.frame.is_some())),
            ("retained_same_arc", "true".into()),
            ("recovery", super::exhaustion_evidence::recovery_json(&r)),
        ]));
        Ok(true)
    }
    pub(super) fn quiet_ready(&self) -> bool {
        self.ledger.phase == Phase::Hidden && self.hidden_observed_ms.is_some_and(|ms| ms >= 250)
    }
    pub(super) fn restore_after_quiet(
        &mut self,
        rt: &Runtime,
        probe: &PointerProbe,
    ) -> Result<(), String> {
        if !self.observe_hidden(rt, probe)? || !self.quiet_ready() {
            return Err("restore requires actual Hidden and 250ms quiet".into());
        }
        // Fresh unlocked-session and original-property qualification after the
        // last runtime snapshot. No App/Window/RefCell borrow spans this call.
        match self.qualify(probe) {
            Ok(n) if n.iconic => {}
            Ok(_) => {
                self.ledger.phase = Phase::Failed;
                return Err("restore requires original minimized HWND".into());
            }
            Err(original) => {
                self.ledger.phase = Phase::Failed;
                return Err(original);
            }
        }
        self.ledger.restore()?;
        // Freeze the measured hidden interval; later source/paint/observer time
        // must not be relabelled as additional hidden quiet time.
        self.hidden_quiet_ms = self.hidden_observed_ms;
        // User explicitly approved ordinary SW_RESTORE's possible activation.
        // This is NOT Window::activate_window/ALT, no SetForegroundWindow or
        // policy change. Nonzero means accepted, not restored.
        if unsafe { ShowWindowAsync(self.owner.hwnd as isize, 9) } == 0 {
            self.ledger.phase = Phase::Failed;
            return Err("owned ordinary restore request refused; no retry".into());
        }
        Ok(())
    }
    pub(super) fn show_if_restored(&mut self, probe: &PointerProbe) -> Result<bool, String> {
        if self.ledger.phase != Phase::RestoreRequested {
            return Err("show out of phase".into());
        }
        let n = self.qualify(probe)?;
        if n.iconic || !n.visible {
            return Ok(false);
        } // Request != actual native state.
        self.ledger.phase = Phase::Failed;
        let command = self
            .handle
            .submit(BrowserCommand::SetVisible(true))
            .map_err(|e| format!("visibility show admission: {e:?}"))?;
        if command.browser != self.handle.snapshot().browser
            || !self
                .hide_id
                .is_some_and(|h| h.browser == command.browser && command.sequence > h.sequence)
        {
            return Err("visibility command identity/order mismatch".into());
        }
        self.show_id = Some(command);
        self.ledger.phase = Phase::ShowPosted;
        Ok(true)
    }
    /// Source-only recovery witness. Caller must still prove GPUI paint, all
    /// desktop pixels and release/Closing. This never reports task acceptance.
    pub(super) fn observe_source_recovered(
        &mut self,
        rt: &Runtime,
        probe: &PointerProbe,
    ) -> Result<Option<Arc<OwnedFrame>>, String> {
        if self.ledger.phase != Phase::ShowPosted {
            return Err("source recovery out of phase".into());
        }
        let n = self.qualify(probe)?;
        if n.iconic || !n.visible {
            return Err("restored native state lost".into());
        }
        let id = self.browser;
        let s = rt
            .frame_source(id)
            .map_err(|e| format!("shown source: {e:?}"))?
            .snapshot();
        let r = rt
            .frame_recovery_diagnostics(id)
            .map_err(|e| format!("shown recovery: {e:?}"))?;
        if s.closed || r.post_failures != 0 {
            return Err("shown terminal/post failure".into());
        }
        let Some(f) = s.view.frame.as_ref() else {
            return Ok(None);
        };
        let old = self
            .retained
            .as_ref()
            .ok_or("missing hidden retained source")?;
        let hidden = self
            .hidden_recovery
            .as_ref()
            .ok_or("missing hidden recovery")?;
        let dims = (
            (self.viewport.width as f32 * self.viewport.scale).ceil() as u32,
            (self.viewport.height as f32 * self.viewport.scale).ceil() as u32,
        );
        if !s.view.visible
            || s.viewport != Some(self.viewport)
            || s.view.generation <= self.hidden_generation
            || f.info.browser != id
            || f.info.surface != SurfaceKind::View
            || f.info.surface_generation != s.view.generation
            || f.info.sequence <= old.info.sequence
            || (f.info.width, f.info.height) != dims
            || r.status != "Recovered"
            || r.generation != s.view.generation
            || (r.expected_width, r.expected_height) != dims
            || r.acks <= hidden.acks
            || r.exhaustions != hidden.exhaustions
            || r.queued
        {
            return Ok(None);
        }
        self.ledger.phase = Phase::SourceRecovered;
        Ok(Some(f.clone()))
    }
    /// Stored evidence only: no native query, input or window operation. The
    /// scene and independent observer must still prove applied state and pixels.
    pub(super) fn evidence_json(&self) -> String {
        let bid = |id: BrowserId| format!("[{},{}]", id.runtime().value(), id.local());
        let command = |id: Option<CommandId>| {
            id.map_or_else(
                || "null".into(),
                |id| {
                    object(vec![
                        ("browser", bid(id.browser)),
                        ("sequence", id.sequence.to_string()),
                    ])
                },
            )
        };
        object(vec![
            ("phase", quote(&format!("{:?}", self.ledger.phase))),
            ("browser", bid(self.browser)),
            (
                "owner",
                object(vec![
                    ("hwnd", self.owner.hwnd.to_string()),
                    ("pid", self.owner.pid.to_string()),
                    ("thread", self.owner.thread.to_string()),
                ]),
            ),
            (
                "viewport",
                object(vec![
                    ("width", self.viewport.width.to_string()),
                    ("height", self.viewport.height.to_string()),
                    ("scale", self.viewport.scale.to_string()),
                ]),
            ),
            (
                "minimize_requests",
                self.ledger.minimize_requests.to_string(),
            ),
            ("restore_requests", self.ledger.restore_requests.to_string()),
            (
                "before_hide_generation",
                self.before_hide_generation.to_string(),
            ),
            ("hidden_generation", self.hidden_generation.to_string()),
            ("hide_command", command(self.hide_id)),
            ("show_command", command(self.show_id)),
            (
                "retained",
                self.retained.as_ref().map_or_else(
                    || "null".into(),
                    |f| {
                        object(vec![
                            ("browser", bid(f.info.browser)),
                            ("generation", f.info.surface_generation.to_string()),
                            ("sequence", f.info.sequence.to_string()),
                            ("width", f.info.width.to_string()),
                            ("height", f.info.height.to_string()),
                        ])
                    },
                ),
            ),
            (
                "hidden_recovery",
                self.hidden_recovery
                    .as_ref()
                    .map_or_else(|| "null".into(), super::exhaustion_evidence::recovery_json),
            ),
            (
                "quiet_ms",
                self.hidden_quiet_ms
                    .or(self.hidden_observed_ms)
                    .map_or_else(|| "null".into(), |v| v.to_string()),
            ),
            (
                "last_hidden_observation",
                self.last_hidden_observation
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            ("quiet_ms_frozen", bool_json(self.hidden_quiet_ms.is_some())),
            ("popup_clear_transition_verified", "false".into()),
        ])
    }
    pub(super) fn retire(&mut self) -> Result<(), String> {
        self.ledger.retire()?;
        self.retained = None; // Retire the pixel lease, never reset counters/IDs.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_prefix_is_x64_sized_versioned_same_active_unlocked_session_only() {
        // SDK layout only, never construct/log the native user/domain fields.
        #[repr(C)]
        struct Level1 {
            session: u32,
            state: i32,
            flags: i32,
            station: [u16; 33],
            user: [u16; 21],
            domain: [u16; 18],
            times: [i64; 5],
            counters: [u32; 6],
        }
        #[repr(C)]
        struct InfoEx {
            level: u32,
            data: Level1,
        }
        assert_eq!(std::mem::size_of::<InfoEx>(), WTS_INFO_EX_BYTES);
        assert_eq!(std::mem::offset_of!(InfoEx, data), 8);
        assert_eq!(std::mem::offset_of!(Level1, flags), 8);
        assert_eq!(std::mem::size_of::<OsVersion>(), 276);
        let mut bytes = [0u8; 20];
        for (at, n) in [(0, 1u32), (8, 3), (12, 0), (16, 1)] {
            bytes[at..at + 4].copy_from_slice(&n.to_le_bytes());
        }
        assert!(unlocked_prefix(&bytes, 232, 3, 10));
        assert!(!unlocked_prefix(&bytes, 231, 3, 10));
        assert!(!unlocked_prefix(&bytes[..19], 232, 3, 10));
        assert!(!unlocked_prefix(&bytes, 232, 4, 10));
        assert!(!unlocked_prefix(&bytes, 232, 3, 6));
        for (at, n) in [(0, 2u32), (12, 4), (16, 0), (16, u32::MAX), (16, 2)] {
            let mut bad = bytes;
            bad[at..at + 4].copy_from_slice(&n.to_le_bytes());
            assert!(!unlocked_prefix(&bad, 232, 3, 10));
        }
    }
    #[test]
    fn explicit_visibility_approval_is_required_and_cannot_borrow_foreground_consent() {
        assert!(visibility_operations_flag(None, HostScenario::Visibility).is_err());
        assert!(visibility_operations_flag(Some("false"), HostScenario::Visibility).is_err());
        assert_eq!(
            visibility_operations_flag(Some("true"), HostScenario::Visibility),
            Ok(true)
        );
        assert!(visibility_operations_flag(Some("yes"), HostScenario::Visibility).is_err());
        assert!(initial_foreground_flag(Some("true"), HostScenario::Visibility).is_err());
        for scene in [
            HostScenario::Embed,
            HostScenario::Dispatch,
            HostScenario::Popup,
            HostScenario::Input,
            HostScenario::ImeObserve,
            HostScenario::Resize,
            HostScenario::Unfocused,
            HostScenario::Exhaustion,
        ] {
            assert_eq!(visibility_operations_flag(None, scene), Ok(false));
            assert!(visibility_operations_flag(Some("true"), scene).is_err());
        }
    }
    #[test]
    fn interrupted_hidden_interval_cannot_reuse_old_quiet_clock_or_restore() {
        let mut ledger = Ledger::new();
        ledger.minimize().unwrap();
        ledger.phase = Phase::HidePosted;
        assert!(!ledger.hidden_ready(false).unwrap());
        assert!(ledger.hidden_ready(true).unwrap());
        ledger.phase = Phase::Hidden;
        assert!(ledger.hidden_ready(true).unwrap());
        assert!(ledger.hidden_ready(false).is_err());
        assert_eq!(ledger.phase, Phase::Failed);
        assert!(ledger.hidden_ready(true).is_err());
        assert!(ledger.restore().is_err());
        assert_eq!((ledger.minimize_requests, ledger.restore_requests), (1, 0));
        // Every error from the real wrapper, not just a ready=false branch,
        // permanently rejects the previous interval and keeps the primary.
        let mut ledger = Ledger::new();
        ledger.minimize().unwrap();
        ledger.phase = Phase::Hidden;
        let primary = "native/session/source observation failed".to_string();
        assert_eq!(ledger.hidden_result(Err(primary.clone())), Err(primary));
        assert_eq!(ledger.phase, Phase::Failed);
        assert!(ledger.hidden_ready(true).is_err());
        assert!(ledger.restore().is_err());
    }
    #[test]
    fn one_shot_native_requests_and_retirement_keep_counters_without_implicit_retry() {
        let mut l = Ledger::new();
        assert!(l.restore().is_err());
        assert!(l.retire().is_err());
        l.minimize().unwrap();
        assert!(l.minimize().is_err());
        assert!(l.restore().is_err());
        l.phase = Phase::HidePosted;
        assert!(l.restore().is_err());
        l.phase = Phase::Hidden;
        l.restore().unwrap();
        assert!(l.restore().is_err());
        l.phase = Phase::ShowPosted;
        assert!(l.retire().is_err());
        l.phase = Phase::SourceRecovered;
        l.retire().unwrap();
        assert_eq!(l.phase, Phase::Retired);
        assert_eq!((l.minimize_requests, l.restore_requests), (1, 1));
        assert!(l.minimize().is_err());
        assert!(l.restore().is_err());
        assert!(l.retire().is_err());
        l.phase = Phase::Failed;
        assert!(l.minimize().is_err());
        assert!(l.restore().is_err());
        assert_eq!((l.minimize_requests, l.restore_requests), (1, 1));
    }
}
