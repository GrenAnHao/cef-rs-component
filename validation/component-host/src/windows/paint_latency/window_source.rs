//! Metadata-only source owner for the original A menu. It retains the owned
//! payload in Queued, never calls presented(), and never creates a viewer.
use super::*;
use rust_cef_gpui::{
    ports::WindowBinding,
    runtime::{ShutdownError, ShutdownTicket},
    source::{
        SourceOperationId, SourceOutcome, SourcePhase, SourceViewControl, SourceViewLimits,
        SourceViewRequest, SourceViewSink,
    },
    test_support::ContextMenuRequestId,
};

#[derive(Default)]
struct Slot {
    menu: Option<ContextMenuRequestId>,
    original: Option<(SourceOperationId, Arc<dyn SourceViewControl>)>,
    document: Option<SourceViewRequest>,
    close: Option<Arc<dyn SourceViewControl>>,
    published: bool,
    close_delivered: bool,
}
struct Sink {
    binding: WindowBinding,
    slot: Mutex<Slot>,
}

impl SourceViewSink for Sink {
    fn started(
        &self,
        id: SourceOperationId,
        menu: ContextMenuRequestId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let actual = control.status().operation;
        let mut s = self.slot.lock().map_err(|_| EngineError::InvalidRequest)?;
        if id != actual
            || id.browser != self.binding.browser
            || id.sequence == 0
            || s.menu != Some(menu)
            || s.original.is_some()
        {
            return Err(EngineError::InvalidRequest);
        }
        s.original = Some((id, control));
        Ok(())
    }
    fn publish(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let actual = control.status().operation;
        let mut s = self.slot.lock().map_err(|_| EngineError::InvalidRequest)?;
        if request.context.binding != self.binding
            || s.menu != Some(request.menu)
            || s.published
            || actual != request.operation
            || !s
                .original
                .as_ref()
                .is_some_and(|(id, _)| *id == request.operation)
        {
            return Err(EngineError::InvalidRequest);
        }
        s.document = Some(request);
        s.published = true;
        Ok(())
    }
    fn close(
        &self,
        id: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let actual = control.status().operation;
        let mut s = self.slot.lock().map_err(|_| EngineError::InvalidRequest)?;
        if s.close_delivered
            || actual != id
            || !s
                .original
                .as_ref()
                .is_some_and(|(original, _)| *original == id)
        {
            return Err(EngineError::InvalidRequest);
        }
        s.close = Some(control);
        s.close_delivered = true;
        Ok(())
    }
}

pub(crate) struct Held {
    sink: Arc<Sink>,
    source_bytes: usize,
    source_hash: u64,
    host_ack: bool,
    after_a: bool,
    after_b: bool,
    joint_barrier: bool,
    release_attempted: bool,
    release_posted: bool,
}

pub(super) fn key(id: SourceOperationId) -> [u64; 3] {
    [
        id.browser.runtime().value(),
        id.browser.local(),
        id.sequence,
    ]
}

impl Held {
    pub(super) fn arm(rt: &Runtime, binding: WindowBinding) -> Result<Self, String> {
        rt.configure_source_views(SourceViewLimits {
            capacity: NonZeroUsize::new(1).unwrap(),
            max_source_bytes: NonZeroUsize::new(8192).unwrap(),
        })
        .map_err(|e| format!("window Source bounds: {e:?}"))?;
        rt.probe_hold_source_visitor(binding.browser)
            .map_err(|e| format!("arm original window visitor: {e:?}"))?;
        Ok(Self {
            sink: Arc::new(Sink {
                binding,
                slot: Mutex::new(Slot::default()),
            }),
            source_bytes: 0,
            source_hash: 0,
            host_ack: false,
            after_a: false,
            after_b: false,
            joint_barrier: false,
            release_attempted: false,
            release_posted: false,
        })
    }
    pub(super) fn sink(&self) -> Arc<dyn SourceViewSink> {
        self.sink.clone()
    }
    pub(super) fn arm_menu(&self, id: ContextMenuRequestId) -> Result<(), String> {
        let mut s = self.sink.slot.lock().unwrap();
        if id.browser != self.sink.binding.browser || id.sequence == 0 || s.menu.is_some() {
            return Err("wrong original Source menu".into());
        }
        s.menu = Some(id);
        Ok(())
    }
    fn control(&self) -> Option<(SourceOperationId, Arc<dyn SourceViewControl>)> {
        self.sink.slot.lock().unwrap().original.clone()
    }

    pub(super) fn observe_queued(
        &mut self,
        rt: &Runtime,
    ) -> Result<Option<SourceOperationId>, String> {
        // Copy only owned Arc handles while locked, then observe native ledger.
        let document = self.sink.slot.lock().unwrap().document.clone();
        let Some(doc) = document else { return Ok(None) };
        let (id, control) = self.control().ok_or("missing original Source control")?;
        let q = control.status();
        if doc.operation != id
            || q.operation != id
            || q.menu != doc.menu
            || q.phase != SourcePhase::Queued
            || !q.native_live
            || !q.host_live
            || q.cancelled
            || !q.callback_received
            || q.first_failure.is_some()
            || rt.probe_held_source_visitor() != Some(id)
            || !doc
                .source
                .as_str()
                .contains("<span>menu bridge selection</span>")
        {
            return Err("original Source did not stay queued with its native visitor".into());
        }
        self.source_bytes = doc.source.len();
        self.source_hash = doc
            .source
            .as_str()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
        Ok(Some(id))
    }

    pub(super) fn ack_close(&mut self) -> Result<(), String> {
        let (control, document) = {
            let mut s = self.sink.slot.lock().unwrap();
            let Some(c) = s.close.take() else {
                return Ok(());
            };
            (c, s.document.take())
        };
        // No viewer was opened. Drop the owned payload before acknowledging.
        drop(document);
        control
            .closed()
            .map_err(|e| format!("retire original host Source payload: {e:?}"))?;
        self.host_ack = true;
        Ok(())
    }

    fn cancelled_held(&self, rt: &Runtime) -> Option<SourceOperationId> {
        let (id, c) = self.control()?;
        let q = c.status();
        let d = rt.source_view_diagnostics();
        (q.operation == id
            && self.host_ack
            && q.cancelled
            && q.callback_received
            && q.native_live
            && !q.host_live
            && q.first_failure.is_none()
            && q.first_close_failure.is_none()
            && rt.probe_held_source_visitor() == Some(id)
            && !d.faulted
            && d.pending == 1
            && d.native_live == 1
            && d.host_live == 0)
            .then_some(id)
    }

    pub(super) fn observed_closed(&mut self, rt: &Runtime, b: bool) -> Result<(), String> {
        if self.cancelled_held(rt).is_none() || b && !self.after_a {
            return Err("original visitor missing across ordered Closed".into());
        }
        if b {
            self.after_b = true
        } else {
            self.after_a = true
        }
        Ok(())
    }

    /// True permits the existing flush proof/release. Failing scene cleanup
    /// also waits for the original visitor, within the same outer deadline.
    pub(crate) fn drain_turn(
        &mut self,
        rt: &mut Runtime,
        ticket: &ShutdownTicket,
        flush: Option<&window_flush::Held>,
        prove: bool,
    ) -> Result<bool, String> {
        self.ack_close()?;
        if self.release_attempted {
            return Ok(self.fully_retired(rt));
        }
        let id = if prove {
            if !self.after_a || !self.after_b {
                return Err("source proof missing ordered Browser terminals".into());
            }
            let Some(id) = self.cancelled_held(rt) else {
                return Err("visitor missing at joint barrier".into());
            };
            if !flush.is_some_and(|f| f.exact_held_for_source_barrier(rt)) {
                return Err("original flush absent at Source barrier".into());
            }
            let status = rt.poll_shutdown(ticket);
            if !matches!(status,ShutdownStatus::Draining {ref open_browsers,pending_tasks:0,pending_requests:1,pending_profiles:0,pending_cookie_flushes:1,post_failures:0} if open_browsers.is_empty())
            {
                if matches!(status, ShutdownStatus::Draining { .. }) {
                    return Ok(false);
                }
                return Err(format!("missing joint Source/flush drain: {status:?}"));
            }
            if !matches!(rt.finish_shutdown(),Err(ShutdownError::Blocked {ref open_browsers,pending_tasks:0,pending_requests:1,pending_profiles:0,pending_cookie_flushes:1,post_failures:0}) if open_browsers.is_empty())
                || rt.diagnostics().cef_shutdown_calls != 0
            {
                return Err("joint Source/flush finish did not refuse".into());
            }
            self.joint_barrier = true;
            id
        } else {
            let Some(id) = rt.probe_held_source_visitor() else {
                return Ok(rt.source_view_diagnostics().pending == 0);
            };
            if self.control().is_none_or(|(original, _)| original != id) {
                return Err("refusing non-original failure cleanup visitor".into());
            }
            id
        };
        self.release_attempted = true;
        rt.probe_release_source_visitor(id)
            .map_err(|e| format!("release original window Source: {e:?}"))?;
        self.release_posted = true;
        Ok(false)
    }

    fn fully_retired(&self, rt: &Runtime) -> bool {
        let Some((id, c)) = self.control() else {
            return false;
        };
        let q = c.status();
        let d = rt.source_view_diagnostics();
        self.release_posted
            && q.operation == id
            && q.outcome == Some(SourceOutcome::Cancelled)
            && !q.native_live
            && !q.host_live
            && q.first_failure.is_none()
            && q.first_close_failure.is_none()
            && rt.probe_held_source_visitor().is_none()
            && !d.faulted
            && d.pending == 0
            && d.native_live == 0
            && d.host_live == 0
            && d.active.is_empty()
    }

    pub(crate) fn report(&self, rt: &Runtime) -> Result<String, String> {
        let (id, control) = self.control().ok_or("missing Source retirement control")?;
        let menu = control.status().menu;
        if !(self.after_a
            && self.after_b
            && self.host_ack
            && self.joint_barrier
            && self.fully_retired(rt))
            || self.source_bytes == 0
            || self.source_hash == 0
        {
            return Err("joint Source did not retire before flush".into());
        }
        Ok(object(vec![
            ("operation", format!("{:?}", key(id))),
            ("source_menu_sequence", menu.sequence.to_string()),
            ("source_bytes", self.source_bytes.to_string()),
            ("source_hash", self.source_hash.to_string()),
            ("queued_without_viewer", "true".into()),
            ("host_ack", "true".into()),
            ("native_held_after_a_closed", "true".into()),
            ("native_held_after_b_closed", "true".into()),
            ("joint_source_flush_shutdown_blocked", "true".into()),
            ("source_released_before_flush", "true".into()),
            ("source_outcome", quote("Cancelled")),
            ("final_source_pending", "0".into()),
        ]))
    }
}
