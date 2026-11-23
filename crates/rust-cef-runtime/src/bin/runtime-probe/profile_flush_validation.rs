//! Real named-profile flush with only its observed CEF completion marker held.
//! There is no retained native wrapper, artificial completion, or claimed disk stall.
use rust_cef_runtime::{
    contracts::{
        BrowserId, ProfileConfig, ProfileHandle, ProfileStorage, SessionState, ThreadMode,
    },
    ports::{BrowserHandle, CloseMode, EngineError, FrameSource, SurfaceKind},
    runtime::{
        CookieFlushStatus, CookieFlushTicket, ProfileCreateError, ProfileState, Runtime,
        ShutdownError, ShutdownStatus, ShutdownTicket,
    },
    test_support::{
        CommandPressureMenuClose, CookieFlushMarkerProbe, FrameProbeControl, LateFlushCleanup,
        ProbeBrowserEvent, ProfileFlushDrain, SourceVisitorDrain,
    },
};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::owned_release::OwnedRelease;

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

fn browser_id(id: BrowserId) -> (u64, u64) {
    (id.runtime().value(), id.local())
}
fn profile_id(id: ProfileHandle) -> (u64, u64) {
    (id.runtime().value(), id.local())
}

pub(super) struct FlushCase {
    named: ProfileHandle,
    default: ProfileHandle,
    original: Option<CookieFlushTicket>,
    original_sequence: Option<u64>,
}
impl FlushCase {
    pub(super) fn open(
        rt: &mut Runtime,
        root: &Path,
        mode: ThreadMode,
        tick: Tick,
    ) -> Result<Self, String> {
        let default = rt.default_profile_handle();
        if !matches!(rt.profile_state(default), Ok(ProfileState::Ready))
            || !rt
                .profile_config(default)
                .is_ok_and(|c| c.storage == ProfileStorage::Memory)
        {
            return Err("default Memory context not ready for flush fixture".into());
        }
        let registration = rt
            .create_profile(ProfileConfig {
                name: "FlushDrain".into(),
                storage: ProfileStorage::Persistent {
                    directory: root.join("FlushDrain"),
                },
            })
            .map_err(|e| format!("named flush profile: {e:?}"))?;
        let named = registration.handle;
        if !registration.is_new || named == default || named.runtime() != default.runtime() {
            return Err("fixture did not create a new named context".into());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match rt.profile_state(named) {
                Ok(ProfileState::Ready) => break,
                Ok(ProfileState::Pending) if Instant::now() < deadline => tick(rt, mode)?,
                other => return Err(format!("named flush profile not ready: {other:?}")),
            }
        }
        let compare = rt
            .compare_profiles(default, named)
            .map_err(|e| format!("native context comparison: {e:?}"))?;
        loop {
            match compare.poll() {
                Some(Ok(result)) if !result.is_same && !result.is_sharing => break,
                None if Instant::now() < deadline => tick(rt, mode)?,
                other => {
                    return Err(format!(
                        "native profile separation not confirmed: {other:?}"
                    ));
                }
            }
        }
        Ok(Self {
            named,
            default,
            original: None,
            original_sequence: None,
        })
    }

    pub(super) fn named(&self) -> ProfileHandle {
        self.named
    }

    fn pending(&self) -> bool {
        self.original
            .as_ref()
            .is_some_and(|t| t.poll() == CookieFlushStatus::Pending)
    }

    pub(super) fn begin_original(
        &mut self,
        rt: &Runtime,
    ) -> Result<CookieFlushMarkerProbe, String> {
        rt.probe_hold_cookie_flush_marker(self.named)
            .map_err(|e| format!("arm named flush marker: {e:?}"))?;
        self.original = Some(
            rt.flush_profile_cookies(self.named)
                .map_err(|e| format!("actual named cookie-store flush: {e:?}"))?,
        );
        let marker = rt
            .probe_cookie_flush_marker()
            .filter(|m| m.profile == self.named && m.sequence > 0)
            .ok_or("accepted flush has no original marker identity")?;
        self.original_sequence = Some(marker.sequence);
        Ok(marker)
    }

    pub(super) fn original_marker(&self, rt: &Runtime) -> Option<CookieFlushMarkerProbe> {
        rt.probe_cookie_flush_marker()
            .filter(|m| m.profile == self.named && Some(m.sequence) == self.original_sequence)
    }

    pub(super) fn hold_original(
        &mut self,
        rt: &mut Runtime,
        handles: &[BrowserHandle],
        mode: ThreadMode,
        tick: Tick,
        deadline: Instant,
        witness: &mut Option<ProfileFlushDrain>,
    ) -> Result<(), String> {
        let [first, survivor] = handles else {
            return Err("flush fixture needs exactly two Browsers".into());
        };
        let a = first.snapshot().browser;
        let b = survivor.snapshot().browser;
        self.begin_original(rt)?;
        let marker = loop {
            let d = rt.diagnostics();
            if let Some(m) = self.original_marker(rt) {
                if m.faulted || m.profile != self.named || m.sequence == 0 || m.release_reserved {
                    return Err("named cookie flush callback witness failed".into());
                }
                if m.callback_thread > 0
                    && Some(m.callback_thread) == d.cef_ui_thread_id
                    && d.cookie_flushes_accepted == 1
                    && d.cookie_flushes_completed == 0
                    && d.pending_cookie_flushes == 1
                {
                    break m;
                }
            }
            if !self.pending() || Instant::now() >= deadline {
                return Err(
                    "original cookie flush did not reach the held UI completion boundary".into(),
                );
            }
            tick(rt, mode)?;
        };
        if !self.pending() {
            return Err("flush result was published before releasing the marker".into());
        }
        *witness = Some(ProfileFlushDrain {
            profile: profile_id(self.named),
            default_profile: profile_id(self.default),
            distinct_native_contexts: true,
            sequence: marker.sequence,
            callback_thread: marker.callback_thread,
            first: browser_id(a),
            survivor: browser_id(b),
            source_closed_after_first: false,
            profile_ready_after_first: false,
            pending_after_first: 0,
            accepted_before_release: 0,
            completed_before_release: 0,
            survivor_frame_before: 0,
            survivor_frame_after: 0,
            closed_events: vec![],
            poll_only_flush: false,
            finish_only_flush: false,
            shutdown_before_release: None,
            new_flush_refused_while_draining: false,
            wrong_profile_refused: false,
            wrong_sequence_refused: false,
            release_task: None,
            marker_faulted: false,
            original_ticket_completed: false,
            final_accepted: 0,
            final_completed: 0,
            final_pending: 0,
            final_pending_profiles: 0,
            final_profile_refs: 0,
            root_lease_released: false,
            final_cpu_bytes: None,
        });
        Ok(())
    }

    fn check_first_retired(
        &self,
        rt: &Runtime,
        source: &dyn FrameSource,
        w: &mut ProfileFlushDrain,
    ) -> Result<(), String> {
        let snapshot = source.snapshot();
        w.source_closed_after_first =
            snapshot.closed && snapshot.view.frame.is_none() && snapshot.popup.frame.is_none();
        w.profile_ready_after_first =
            matches!(rt.profile_state(self.named), Ok(ProfileState::Ready));
        w.pending_after_first = rt.diagnostics().pending_cookie_flushes;
        if !w.source_closed_after_first
            || !w.profile_ready_after_first
            || w.pending_after_first != 1
            || !self.pending()
        {
            return Err("first close did not keep the original profile request pending".into());
        }
        Ok(())
    }

    pub(super) fn after_pressured_close(
        &self,
        rt: &Runtime,
        first: &BrowserHandle,
        pressure: &CommandPressureMenuClose,
        w: &mut ProfileFlushDrain,
    ) -> Result<(), String> {
        let a = first.snapshot();
        if browser_id(a.browser) != w.first
            || a.state != SessionState::Closed
            || pressure.closed_browser != w.first
            || pressure.survivor_browser != w.survivor
            || pressure.pressure.held_cookie_flush_at_close
                != Some([w.profile.0, w.profile.1, w.sequence])
            || pressure.frame_before == 0
            || pressure.frame_after <= pressure.frame_before
        {
            return Err(
                "pressured close did not retain original flush identity and survivor repaint"
                    .into(),
            );
        }
        let source = rt
            .frame_source(a.browser)
            .map_err(|e| format!("pressured original source: {e:?}"))?;
        self.check_first_retired(rt, source.as_ref(), w)?;
        w.survivor_frame_before = pressure.frame_before;
        w.survivor_frame_after = pressure.frame_after;
        Ok(())
    }

    pub(super) fn after_source_close(
        &self,
        rt: &Runtime,
        first: &BrowserHandle,
        source: &SourceVisitorDrain,
        w: &mut ProfileFlushDrain,
    ) -> Result<(), String> {
        let a = first.snapshot();
        if a.state != SessionState::Closed
            || browser_id(a.browser) != w.first
            || (source.operation[0], source.operation[1]) != w.first
            || source.survivor != w.survivor
            || !source.source_closed
            || source.survivor_frame_before == 0
            || source.survivor_frame_after <= source.survivor_frame_before
        {
            return Err("source close did not keep the original Browser identities".into());
        }
        let frames = rt
            .frame_source(a.browser)
            .map_err(|e| format!("original Source operation frame source: {e:?}"))?;
        self.check_first_retired(rt, frames.as_ref(), w)?;
        w.survivor_frame_before = source.survivor_frame_before;
        w.survivor_frame_after = source.survivor_frame_after;
        Ok(())
    }

    pub(super) fn close_first(
        &mut self,
        rt: &mut Runtime,
        handles: &[BrowserHandle],
        closed: &Mutex<Vec<ProbeBrowserEvent>>,
        mode: ThreadMode,
        tick: Tick,
        witness: &mut Option<ProfileFlushDrain>,
    ) -> Result<(), String> {
        let [first, survivor] = handles else {
            return Err("flush fixture needs exactly two Browsers".into());
        };
        let a_source = rt
            .frame_source(first.snapshot().browser)
            .map_err(|e| format!("original flush frame source: {e:?}"))?;
        let b = survivor.snapshot().browser;
        let b_source = rt
            .frame_source(b)
            .map_err(|e| format!("survivor frame source: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        self.hold_original(rt, handles, mode, tick, deadline, witness)?;
        let w = witness.as_mut().ok_or("observed flush witness missing")?;
        first.request_close(CloseMode::Force);
        loop {
            if first.snapshot().state == SessionState::Closed
                && matches!(closed.lock().unwrap().as_slice(), [(id,sequence)] if *id==w.first && *sequence>0)
            {
                break;
            }
            if !self.pending() || !survivor.snapshot().is_ready() || Instant::now() >= deadline {
                return Err(
                    "first Browser did not close independently of the pending flush".into(),
                );
            }
            tick(rt, mode)?;
        }
        self.check_first_retired(rt, a_source.as_ref(), w)?;
        let before = b_source
            .latest(SurfaceKind::View)
            .ok_or("survivor baseline frame missing")?;
        let generation = before.info.surface_generation;
        w.survivor_frame_before = before.info.sequence;
        drop(before);
        rt.probe_frame_control(b, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("survivor repaint: {e:?}"))?;
        loop {
            if !self.pending() || !survivor.snapshot().is_ready() || Instant::now() >= deadline {
                return Err(
                    "survivor did not repaint while the original profile marker was held".into(),
                );
            }
            if let Some(f) = b_source.latest(SurfaceKind::View)
                && f.info.browser == b
                && f.info.surface_generation == generation
                && (f.info.width, f.info.height) == (320, 240)
                && f.info.sequence > w.survivor_frame_before
                && f.cpu_pixels()
                    .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                    == Some(&[126, 76, 26, 255])
                && rt
                    .probe_frame_state(b)
                    .is_some_and(|state| state.applied == 1)
            {
                w.survivor_frame_after = f.info.sequence;
                return Ok(());
            }
            tick(rt, mode)?;
        }
    }

    pub(super) fn prove_barrier(
        &self,
        rt: &mut Runtime,
        shutdown: &ShutdownTicket,
        mode: ThreadMode,
        tick: Tick,
        w: &mut ProfileFlushDrain,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !self.pending() {
                return Err("original flush became terminal before release".into());
            }
            let status = rt.poll_shutdown(shutdown);
            if matches!(status,ShutdownStatus::Draining {ref open_browsers,pending_tasks:0,pending_requests:0,
                pending_profiles:0,pending_cookie_flushes:1,post_failures:0} if open_browsers.is_empty())
            {
                w.poll_only_flush = true;
                break;
            }
            if matches!(status, ShutdownStatus::Ready | ShutdownStatus::Stopped)
                || Instant::now() >= deadline
            {
                return Err(format!(
                    "shutdown did not retain only the original flush: {status:?}"
                ));
            }
            tick(rt, mode)?;
        }
        w.finish_only_flush = matches!(rt.finish_shutdown(),Err(ShutdownError::Blocked {ref open_browsers,
            pending_tasks:0,pending_requests:0,pending_profiles:0,pending_cookie_flushes:1,post_failures:0}) if open_browsers.is_empty());
        let d = rt.diagnostics();
        w.shutdown_before_release = Some(d.cef_shutdown_calls);
        w.accepted_before_release = d.cookie_flushes_accepted;
        w.completed_before_release = d.cookie_flushes_completed;
        if !w.finish_only_flush
            || d.cef_shutdown_calls != 0
            || d.cookie_flushes_accepted != 1
            || d.cookie_flushes_completed != 0
        {
            return Err(
                "finish_shutdown did not refuse with the exact original flush pending".into(),
            );
        }
        w.new_flush_refused_while_draining = matches!(
            rt.flush_profile_cookies(self.named),
            Err(ProfileCreateError::NotReady)
        );
        w.wrong_profile_refused = matches!(
            rt.probe_release_cookie_flush_marker(self.default, w.sequence),
            Err(EngineError::InvalidRequest)
        );
        let wrong_sequence = w
            .sequence
            .checked_add(1)
            .ok_or("non-wrapping sequence witness unavailable")?;
        w.wrong_sequence_refused = matches!(
            rt.probe_release_cookie_flush_marker(self.named, wrong_sequence),
            Err(EngineError::InvalidRequest)
        );
        if !w.new_flush_refused_while_draining
            || !w.wrong_profile_refused
            || !w.wrong_sequence_refused
            || !self.pending()
        {
            return Err("draining or exact marker rejection did not hold".into());
        }
        Ok(())
    }

    pub(super) fn release_once(
        &self,
        rt: &Runtime,
        state: &mut OwnedRelease,
        witness: Option<&mut ProfileFlushDrain>,
        late: Option<&mut LateFlushCleanup>,
    ) -> Result<(), String> {
        if state.attempted() {
            return Ok(());
        }
        let diagnostics = rt.diagnostics();
        let ready = self.original_marker(rt).filter(|marker| {
            marker.callback_thread != 0
                && Some(marker.callback_thread) == diagnostics.cef_ui_thread_id
                && !marker.release_reserved
                && !marker.faulted
                && self.pending()
                && diagnostics.cookie_flushes_accepted == 1
                && diagnostics.cookie_flushes_completed == 0
        });
        let Some(marker) = state.claim(ready) else {
            return Ok(());
        };
        let task = rt
            .probe_release_cookie_flush_marker(marker.profile, marker.sequence)
            .map_err(|e| format!("explicit original flush completion post: {e:?}"))?;
        let task = Some((task.runtime.value(), task.sequence));
        if let Some(w) = witness {
            w.release_task = task;
        }
        if let Some(w) = late {
            w.release_callback_thread = Some(marker.callback_thread);
            w.release_task = task;
        }
        Ok(())
    }

    pub(super) fn finish_late(&self, rt: &Runtime, finished: bool, w: &mut LateFlushCleanup) {
        let d = rt.diagnostics();
        w.final_ticket_completed = self
            .original
            .as_ref()
            .is_some_and(|t| t.poll() == CookieFlushStatus::Complete);
        w.final_pending = d.pending_cookie_flushes;
        w.final_completed = d.cookie_flushes_completed;
        w.normal_shutdown_after_failure =
            finished && !d.root_lease_held && d.cef_shutdown_calls == 1;
    }

    pub(super) fn finish(
        &self,
        rt: &Runtime,
        handles: &[BrowserHandle],
        closed: &Mutex<Vec<ProbeBrowserEvent>>,
        finished: bool,
        w: &mut ProfileFlushDrain,
    ) {
        let d = rt.diagnostics();
        w.closed_events = closed.lock().unwrap().clone();
        w.marker_faulted = !rt.probe_cookie_flush_marker().is_some_and(|m| {
            m.profile == self.named
                && m.sequence == w.sequence
                && m.callback_thread == w.callback_thread
                && m.release_reserved
                && !m.faulted
        });
        w.original_ticket_completed = self
            .original
            .as_ref()
            .is_some_and(|t| t.poll() == CookieFlushStatus::Complete);
        w.final_accepted = d.cookie_flushes_accepted;
        w.final_completed = d.cookie_flushes_completed;
        w.final_pending = d.pending_cookie_flushes;
        w.final_pending_profiles = d.pending_profiles;
        w.final_profile_refs = d.profile_references_retained;
        w.root_lease_released = finished && !d.root_lease_held;
        w.final_cpu_bytes = handles.iter().try_fold(0usize, |bytes, h| {
            bytes.checked_add(rt.frame_budget_stats(h.snapshot().browser).ok()?.used)
        });
    }
}
