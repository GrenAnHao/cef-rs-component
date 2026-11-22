use rust_cef_core::{
    find::*,
    menu::ContextMenuRequestId,
    menu_lease::MenuLease,
    ports::{EngineError, WindowBinding},
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub retired: bool,
    pub active: bool,
    pub started: u64,
    pub finished: u64,
    pub delivered: u64,
    pub last: Option<FindStatus>,
    pub faulted: bool,
}
struct Row {
    id: FindOperationId,
    menu: ContextMenuRequestId,
    bytes: usize,
    receipt: Arc<dyn FindReceipt>,
    finished: bool,
    consumed: bool,
    dirty: bool,
    revision: u64,
}
struct State {
    retired: bool,
    highwater: u64,
    row: Option<Row>,
    started: u64,
    finished: u64,
    delivered: u64,
    faulted: bool,
}
pub struct Inbox {
    binding: WindowBinding,
    pub lease: MenuLease,
    max_bytes: NonZeroUsize,
    state: Mutex<State>,
}
impl Inbox {
    pub fn new(binding: WindowBinding, lease: MenuLease, max_bytes: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            binding,
            lease,
            max_bytes,
            state: Mutex::new(State {
                retired: false,
                highwater: 0,
                row: None,
                started: 0,
                finished: 0,
                delivered: 0,
                faulted: false,
            }),
        })
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| {
            let mut s = e.into_inner();
            s.faulted = true;
            s.retired = true;
            s
        })
    }
    #[cfg(test)]
    pub fn state_for_test(&self) -> bool {
        self.state.try_lock().is_ok()
    }
    pub fn retire(&self) {
        self.lock().retired = true;
    }
    pub fn receipt(&self) -> Option<Arc<dyn FindReceipt>> {
        self.lock().row.as_ref().map(|r| r.receipt.clone())
    }
    pub fn snapshot(&self) -> Snapshot {
        let (mut snap, receipt) = {
            let s = self.lock();
            (
                Snapshot {
                    retired: s.retired,
                    active: s.row.as_ref().is_some_and(|r| !r.finished),
                    started: s.started,
                    finished: s.finished,
                    delivered: s.delivered,
                    last: None,
                    faulted: s.faulted,
                },
                s.row.as_ref().map(|r| r.receipt.clone()),
            )
        };
        // Opaque status and last Drop happen with no mutex/RefCell/Entity borrow.
        snap.last = receipt.map(|r| r.status());
        snap
    }
    fn original(
        &self,
        id: FindOperationId,
    ) -> Result<(Arc<dyn FindReceipt>, ContextMenuRequestId, usize), EngineError> {
        let s = self.lock();
        let r = s
            .row
            .as_ref()
            .filter(|r| r.id == id)
            .ok_or(EngineError::Closed)?;
        Ok((r.receipt.clone(), r.menu, r.bytes))
    }
    pub fn take_update(&self) -> Option<FindStatus> {
        let (id, receipt, revision) = {
            let s = self.lock();
            if s.retired || s.faulted {
                return None;
            }
            let r = s.row.as_ref().filter(|r| r.dirty)?;
            (r.id, r.receipt.clone(), r.revision)
        };
        let q = receipt.status();
        let mut s = self.lock();
        if s.retired || s.faulted {
            return None;
        }
        let r = s.row.as_mut().filter(|r| r.id == id && r.dirty)?;
        // Native status is authoritative. A final result may arrive between copy and
        // this lock; leave the newer terminal dirty rather than consume an old view.
        if r.revision != revision || r.finished && q.phase != FindPhase::Finished {
            return None;
        }
        r.dirty = false;
        if r.finished {
            r.consumed = true;
        }
        s.delivered += 1;
        Some(q)
    }
}
impl FindResultSink for Inbox {
    fn started(
        &self,
        request: FindRequest,
        receipt: Arc<dyn FindReceipt>,
    ) -> Result<(), EngineError> {
        let q = receipt.status();
        let c = &request.context;
        if c.binding != self.binding
            || request.operation.browser != self.binding.browser
            || request.menu.browser != self.binding.browser
            || request.operation.sequence == 0
            || request.menu.sequence == 0
            || c.selection.len() > self.max_bytes.get()
            || q.operation != request.operation
            || q.menu != request.menu
            || q.query_bytes != c.selection.len()
            || q.phase != FindPhase::Admitted
            || q.issued
            || q.outcome.is_some()
        {
            return Err(EngineError::InvalidRequest);
        }
        let lease = self.lease.snapshot().map_err(|_| EngineError::Closed)?;
        if lease.retired
            || lease.faulted
            || lease.binding != self.binding
            || lease.revision != c.binding_revision
        {
            return Err(EngineError::Closed);
        }
        let old = {
            let mut s = self.lock();
            if s.retired || s.faulted {
                return Err(EngineError::Closed);
            }
            if request.operation.sequence <= s.highwater {
                return Err(EngineError::Closed);
            }
            if s.row.as_ref().is_some_and(|r| !r.finished || !r.consumed) {
                return Err(EngineError::Backpressure);
            }
            s.highwater = request.operation.sequence;
            s.started += 1;
            s.row.replace(Row {
                id: request.operation,
                menu: request.menu,
                bytes: c.selection.len(),
                receipt,
                finished: false,
                consumed: false,
                dirty: true,
                revision: 1,
            })
        };
        drop(old);
        Ok(())
    }
    fn update(&self, result: FindResult) -> Result<(), EngineError> {
        let (receipt, menu, bytes) = self.original(result.operation)?;
        let q = receipt.status();
        if q.operation != result.operation
            || q.menu != menu
            || q.query_bytes != bytes
            || q.last_result.as_ref() != Some(&result)
            || !q.issued
            || result.cef_identifier < 0
            || result.matches < -1
            || result.active_ordinal < -1
            || (result.final_update && result.matches < 0)
            || result
                .selection_rect
                .is_some_and(|r| r.width < 0 || r.height < 0)
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.lock();
        let r = s
            .row
            .as_mut()
            .filter(|r| r.id == result.operation && !r.finished)
            .ok_or(EngineError::Closed)?;
        r.revision = r
            .revision
            .checked_add(1)
            .ok_or(EngineError::InvalidRequest)?;
        r.dirty = true;
        // Retirement prevents UI delivery, not native completion bookkeeping. A
        // normal retire/result race must not manufacture Publish(Closed) failures.
        Ok(())
    }
    fn finished(&self, q: FindStatus) -> Result<(), EngineError> {
        let (receipt, menu, bytes) = self.original(q.operation)?;
        let original = receipt.status();
        if original.operation != q.operation
            || original.menu != menu
            || original.query_bytes != bytes
            || original.phase != FindPhase::Finished
            || original.outcome.is_none()
            || q.menu != menu
            || q.query_bytes != bytes
            || q.phase != FindPhase::Finished
            || q.outcome != original.outcome
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.lock();
        let r = s
            .row
            .as_mut()
            .filter(|r| r.id == q.operation)
            .ok_or(EngineError::Closed)?;
        if r.finished {
            return Err(EngineError::InvalidRequest);
        }
        r.revision = r
            .revision
            .checked_add(1)
            .ok_or(EngineError::InvalidRequest)?;
        r.finished = true;
        r.dirty = true;
        s.finished += 1;
        Ok(())
    }
}
