//! Holds only an owned marker for the real cookie-store callback. This test
//! does not suspend disk work, retain a CEF wrapper, or inject a completion.
use super::*;
use rust_cef_gpui::{
    contracts::ProfileHandle,
    runtime::{
        CookieFlushStatus, CookieFlushTicket, ProfileCreateError, ProfileState, ShutdownError,
        ShutdownTicket,
    },
    test_support::{CookieFlushMarkerProbe, WindowPeerFlush},
};

pub(crate) struct Held {
    peer: BrowserId,
    profile: ProfileHandle,
    ticket: CookieFlushTicket,
    original_sequence: Option<u64>,
    callback_thread: u64,
    after_a: bool,
    after_b: bool,
    only_flush_blocked: bool,
    release_attempted: bool,
    release_task: Option<(u64, u64)>,
    extra_completed_before_cleanup: bool,
}

impl Held {
    pub(super) fn arm(
        rt: &Runtime,
        peer: BrowserId,
        profile: ProfileHandle,
    ) -> Result<Self, String> {
        if profile.local() == 0
            || profile.runtime() != peer.runtime()
            || !rt
                .profile_config(profile)
                .is_ok_and(|c| matches!(c.storage, ProfileStorage::Persistent { .. }))
        {
            return Err("flush must use original named persistent profile of B".into());
        }
        rt.probe_hold_cookie_flush_marker(profile)
            .map_err(|e| format!("arm original B flush marker: {e:?}"))?;
        let ticket = rt
            .flush_profile_cookies(profile)
            .map_err(|e| format!("B cookie store flush: {e:?}"))?;
        let original_sequence = rt
            .probe_cookie_flush_marker()
            .filter(|m| m.profile == profile && m.sequence != 0)
            .map(|m| m.sequence);
        // Retain the original ticket for the outer cleanup, even if the
        // subsequent readiness or close proof fails.
        Ok(Self {
            peer,
            profile,
            ticket,
            original_sequence,
            callback_thread: 0,
            after_a: false,
            after_b: false,
            only_flush_blocked: false,
            release_attempted: false,
            release_task: None,
            extra_completed_before_cleanup: false,
        })
    }

    fn original(&self, rt: &Runtime) -> Option<CookieFlushMarkerProbe> {
        rt.probe_cookie_flush_marker()
            .filter(|m| m.profile == self.profile && Some(m.sequence) == self.original_sequence)
    }

    fn release_candidate(&self, rt: &Runtime) -> Result<Option<CookieFlushMarkerProbe>, String> {
        let marker = self
            .original(rt)
            .ok_or("original B marker identity unavailable")?;
        if marker.faulted
            || marker.release_reserved
            || self.ticket.poll() != CookieFlushStatus::Pending
        {
            return Err("original B marker faulted, reserved, or completed early".into());
        }
        if marker.callback_thread == 0 {
            return Ok(None);
        }
        if Some(marker.callback_thread) != rt.diagnostics().cef_ui_thread_id {
            return Err("original B callback was not on the observed CEF UI".into());
        }
        Ok(Some(marker))
    }

    fn held(&self, rt: &Runtime) -> Result<Option<CookieFlushMarkerProbe>, String> {
        let candidate = self.release_candidate(rt)?;
        let d = rt.diagnostics();
        Ok(candidate.filter(|_| {
            d.cookie_flushes_accepted == 1
                && d.cookie_flushes_completed == 0
                && d.pending_cookie_flushes == 1
        }))
    }

    pub(super) fn qualify_ready(
        &mut self,
        rt: &Runtime,
    ) -> Result<Option<WindowPeerFlush>, String> {
        Ok(self.held(rt)?.map(|m| {
            self.callback_thread = m.callback_thread;
            WindowPeerFlush {
                browser: self.peer,
                profile: m.profile,
                sequence: m.sequence,
            }
        }))
    }

    pub(super) fn exact_held_for_source_barrier(&self, rt: &Runtime) -> bool {
        self.after_b
            && self
                .held(rt)
                .is_ok_and(|m| m.is_some_and(|m| m.callback_thread == self.callback_thread))
    }

    pub(super) fn observe_after_close(
        &mut self,
        rt: &Runtime,
        survivor_closed: bool,
    ) -> Result<(), String> {
        let Some(m) = self.held(rt)? else {
            return Err("original B flush did not remain held across Closed".into());
        };
        if m.callback_thread != self.callback_thread
            || (survivor_closed && !self.after_a)
            || !matches!(rt.profile_state(self.profile), Ok(ProfileState::Ready))
        {
            return Err("original B flush lost profile, sequence, or close order".into());
        }
        if survivor_closed {
            self.after_b = true;
        } else {
            self.after_a = true;
        }
        Ok(())
    }

    pub(crate) fn attempted(&self) -> bool {
        self.release_attempted
    }

    /// Called only from the existing owner drain loop, with no GPUI borrow.
    /// On failure an original late-arriving marker can still be released once;
    /// cleanup is not counted as a successful flush-only barrier observation.
    pub(crate) fn drain_turn(
        &mut self,
        rt: &mut Runtime,
        ticket: &ShutdownTicket,
        prove: bool,
    ) -> Result<(), String> {
        if self.release_attempted {
            return Ok(());
        }
        let ready = if prove {
            self.held(rt)?
        } else {
            self.release_candidate(rt)?
        };
        let Some(marker) = ready else { return Ok(()) };
        if self.callback_thread == 0 && !prove {
            self.callback_thread = marker.callback_thread;
        }
        if self.callback_thread != marker.callback_thread {
            return Err("original flush callback identity changed".into());
        }
        if prove {
            if !(self.after_a && self.after_b) {
                return Err("missing B flush at original Closed boundaries".into());
            }
            let status = rt.poll_shutdown(ticket);
            if !matches!(status, ShutdownStatus::Draining { ref open_browsers, pending_tasks:0,pending_requests:0,pending_profiles:0,pending_cookie_flushes:1,post_failures:0 } if open_browsers.is_empty())
            {
                if matches!(status, ShutdownStatus::Draining { .. }) {
                    return Ok(());
                }
                return Err(format!(
                    "shutdown did not retain the original B flush: {status:?}"
                ));
            }
            if !matches!(rt.finish_shutdown(),Err(ShutdownError::Blocked { ref open_browsers, pending_tasks:0,pending_requests:0,pending_profiles:0,pending_cookie_flushes:1,post_failures:0 }) if open_browsers.is_empty())
                || rt.diagnostics().cef_shutdown_calls != 0
            {
                return Err("finish did not refuse shutdown with exactly B's pending flush".into());
            }
            let default = rt.default_profile_handle();
            let wrong = marker
                .sequence
                .checked_add(1)
                .ok_or("flush sequence exhausted")?;
            if !matches!(
                rt.flush_profile_cookies(self.profile),
                Err(ProfileCreateError::NotReady)
            ) || rt.probe_release_cookie_flush_marker(default, marker.sequence)
                != Err(EngineError::InvalidRequest)
                || rt.probe_release_cookie_flush_marker(self.profile, wrong)
                    != Err(EngineError::InvalidRequest)
                || self.held(rt)?.is_none()
            {
                return Err("shutdown or wrong flush identity was accepted".into());
            }
            self.only_flush_blocked = true;
        }
        self.release_attempted = true;
        let task = rt
            .probe_release_cookie_flush_marker(self.profile, marker.sequence)
            .map_err(|e| format!("release the original B marker: {e:?}"))?;
        self.release_task = Some((task.runtime.value(), task.sequence));
        Ok(())
    }

    pub(super) fn observe_extra_completed(&mut self, rt: &Runtime) -> Result<(), String> {
        let d = rt.diagnostics();
        let candidate = self
            .release_candidate(rt)?
            .ok_or("original B marker not available for failure cleanup")?;
        if d.cookie_flushes_accepted != 2
            || d.cookie_flushes_completed != 1
            || d.pending_cookie_flushes != 1
            || candidate.callback_thread != self.callback_thread
        {
            return Err(
                "extra flush did not complete while original B marker stayed pending".into(),
            );
        }
        self.extra_completed_before_cleanup = true;
        Ok(())
    }

    pub(crate) fn cleanup_report(&self, rt: &Runtime) -> Result<String, String> {
        let d = rt.diagnostics();
        let marker = self.original(rt).ok_or("cleanup lost original B marker")?;
        let task = self
            .release_task
            .ok_or("cleanup did not post original completion")?;
        if !self.extra_completed_before_cleanup
            || self.only_flush_blocked
            || !self.release_attempted
            || !marker.release_reserved
            || marker.faulted
            || marker.callback_thread != self.callback_thread
            || self.ticket.poll() != CookieFlushStatus::Complete
            || d.cookie_flushes_accepted != 2
            || d.cookie_flushes_completed != 2
            || d.pending_cookie_flushes != 0
            || d.cef_shutdown_calls != 1
            || d.root_lease_held
        {
            return Err("extra-flush failure cleanup did not retire the original marker".into());
        }
        Ok(object(vec![
            ("peer", identity(self.peer)),
            (
                "marker",
                format!(
                    "[{},{},{}]",
                    self.profile.runtime().value(),
                    self.profile.local(),
                    marker.sequence
                ),
            ),
            ("callback_thread", marker.callback_thread.to_string()),
            ("accepted_completed_pending_before", "[2,1,1]".into()),
            ("accepted_completed_pending_after", "[2,2,0]".into()),
            ("failure_cleanup_only", "true".into()),
            ("original_ticket_complete", "true".into()),
            ("release_task", format!("[{},{}]", task.0, task.1)),
        ]))
    }

    pub(crate) fn report(&self, rt: &Runtime) -> Result<String, String> {
        let d = rt.diagnostics();
        let marker = self
            .original(rt)
            .ok_or("original completed B marker unavailable")?;
        let task = self
            .release_task
            .ok_or("missing original B flush completion task")?;
        if !self.after_a
            || !self.after_b
            || !self.only_flush_blocked
            || !self.release_attempted
            || task.0 != self.profile.runtime().value()
            || marker.callback_thread != self.callback_thread
            || !marker.release_reserved
            || marker.faulted
            || d.pending_cookie_flushes != 0
            || d.cookie_flushes_accepted != 1
            || d.cookie_flushes_completed != 1
            || d.cef_shutdown_calls != 1
            || d.root_lease_held
            || self.ticket.poll() != CookieFlushStatus::Complete
        {
            return Err(
                "original B cookie flush did not complete before exactly one shutdown".into(),
            );
        }
        Ok(object(vec![
            ("peer", identity(self.peer)),
            (
                "marker",
                format!(
                    "[{},{},{}]",
                    self.profile.runtime().value(),
                    self.profile.local(),
                    marker.sequence
                ),
            ),
            ("callback_thread", marker.callback_thread.to_string()),
            ("held_after_a_closed", bool_json(self.after_a)),
            ("held_after_b_closed", bool_json(self.after_b)),
            (
                "poll_and_finish_blocked_only_by_flush",
                bool_json(self.only_flush_blocked),
            ),
            ("shutdown_calls_at_barrier", "0".into()),
            ("accepted_before_release", "1".into()),
            ("completed_before_release", "0".into()),
            ("wrong_identity_and_new_flush_rejected", "true".into()),
            ("release_task", format!("[{},{}]", task.0, task.1)),
            ("original_ticket_complete", "true".into()),
            ("accepted_final", d.cookie_flushes_accepted.to_string()),
            ("completed_final", d.cookie_flushes_completed.to_string()),
            ("pending_final", d.pending_cookie_flushes.to_string()),
        ]))
    }
}
