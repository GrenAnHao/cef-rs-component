//! Bounded owned source delivery. No GPUI values or native pointers.
use rust_cef_core::{
    contracts::BrowserId,
    menu::ContextMenuRequestId,
    menu_lease::MenuLease,
    ports::{EngineError, WindowBinding},
    source_view::*,
};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};
pub struct Slot {
    pub control: Arc<dyn SourceViewControl>,
    pub document: Option<SourceViewRequest>,
    pub close: bool,
    pub ack_attempted: bool,
    published: bool,
}
pub struct State {
    pub retired: bool,
    pub last: u64,
    pub slots: BTreeMap<SourceOperationId, Slot>,
}
pub struct Inbox {
    pub binding: WindowBinding,
    pub lease: MenuLease,
    capacity: NonZeroUsize,
    max_bytes: NonZeroUsize,
    pub state: Mutex<State>,
}
impl Inbox {
    pub fn new(
        binding: WindowBinding,
        lease: MenuLease,
        capacity: NonZeroUsize,
        max_bytes: NonZeroUsize,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            lease,
            capacity,
            max_bytes,
            state: Mutex::new(State {
                retired: false,
                last: 0,
                slots: BTreeMap::new(),
            }),
        })
    }
    fn browser(&self, b: BrowserId) -> bool {
        b == self.binding.browser
    }
    pub fn retire(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).retired = true;
    }
    pub fn control(&self, id: SourceOperationId) -> Option<Arc<dyn SourceViewControl>> {
        self.state
            .lock()
            .ok()?
            .slots
            .get(&id)
            .map(|s| s.control.clone())
    }
    pub fn ids(&self) -> Vec<SourceOperationId> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .keys()
            .copied()
            .collect()
    }
    pub fn take_document(&self, id: SourceOperationId) -> Option<SourceViewRequest> {
        self.state.lock().ok()?.slots.get_mut(&id)?.document.take()
    }
    pub fn closing(&self, id: SourceOperationId) -> bool {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.retired || s.slots.get(&id).is_some_and(|s| s.close)
    }
    pub fn reserve_ack(&self, id: SourceOperationId, retry: bool) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = s.slots.get_mut(&id) else {
            return false;
        };
        if slot.ack_attempted && !retry {
            return false;
        }
        slot.ack_attempted = true;
        true
    }
    pub fn reset_ack(&self, id: SourceOperationId) {
        if let Some(slot) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .get_mut(&id)
        {
            slot.ack_attempted = false;
        }
    }
    pub fn remove(&self, id: SourceOperationId) -> Option<Slot> {
        self.state.lock().ok()?.slots.remove(&id)
    }
}
impl SourceViewSink for Inbox {
    fn started(
        &self,
        id: SourceOperationId,
        menu: ContextMenuRequestId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        // Opaque calls occur before taking any internal mutex.
        let q = control.status();
        let lease = self
            .lease
            .snapshot()
            .map_err(|_| EngineError::InvalidRequest)?;
        if !self.browser(id.browser)
            || id.sequence == 0
            || menu.browser != id.browser
            || q.operation != id
            || q.menu != menu
            || q.phase != SourcePhase::Fetching
            || lease.retired
            || lease.faulted
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
        if s.retired {
            return Err(EngineError::Closed);
        }
        if s.slots.len() >= self.capacity.get() {
            return Err(EngineError::Backpressure);
        }
        if id.sequence <= s.last || s.slots.contains_key(&id) {
            return Err(EngineError::InvalidRequest);
        }
        s.last = id.sequence;
        s.slots.insert(
            id,
            Slot {
                control,
                document: None,
                close: false,
                ack_attempted: false,
                published: false,
            },
        );
        Ok(())
    }
    fn publish(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let q = control.status();
        // Runtime can create a fresh wrapper around the SAME operation for each
        // callback. Keep the original receipt; do not require wrapper Arc identity.
        let original = self
            .control(request.operation)
            .ok_or(EngineError::Closed)?
            .status();
        let lease = self
            .lease
            .snapshot()
            .map_err(|_| EngineError::InvalidRequest)?;
        if original.operation != request.operation
            || original.menu != request.menu
            || original.phase != SourcePhase::Queued
            || !original.host_live
            || original.source_bytes != request.source.len()
            || request.context.binding != self.binding
            || request.operation.browser != self.binding.browser
            || q.operation != request.operation
            || q.menu != request.menu
            || q.phase != SourcePhase::Queued
            || !q.host_live
            || request.source.len() > self.max_bytes.get()
            || lease.retired
            || lease.faulted
            || lease.revision != request.context.binding_revision
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
        if s.retired {
            return Err(EngineError::Closed);
        }
        let slot = s
            .slots
            .get_mut(&request.operation)
            .ok_or(EngineError::Closed)?;
        if slot.close || slot.published {
            return Err(EngineError::InvalidRequest);
        }
        slot.published = true;
        slot.document = Some(request);
        Ok(())
    }
    fn close(
        &self,
        id: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        if control.status().operation != id || id.browser != self.binding.browser {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
        let slot = s.slots.get_mut(&id).ok_or(EngineError::Closed)?;
        slot.close = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
