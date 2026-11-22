//! Strict serialized Find using fixed CEF's callback-only identifiers. No
//! caller ID is invented. Stop/final never stand in for native Client release:
//! the original Browser/client remains in Runtime's existing close barrier.
//! Correlation requires this Runtime to be the Browser's exclusive Find issuer.
//! Pinned Chromium allocates one global native ID per nonempty non-suppressed
//! StartFinding call; it is NOT the SDK sequence. Old actual-final IDs are a
//! stale-result cutoff, not a caller-ID mapping. Unexpected newer IDs quarantine
//! until OnBeforeClose. Same-query/empty suppressed replies cannot free a slot.
use crate::{client::BrowserClientState, menu_bridge::Scope, registry::UiRegistry};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId, find::*, menu::ContextMenuRequestId, ports::EngineError,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak, atomic::Ordering},
};
type Shared = Arc<Mutex<FindStatus>>;
fn status(q: &Shared) -> FindStatus {
    q.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
#[derive(Clone)]
struct Data {
    id: FindOperationId,
    scope: Arc<Scope>,
    sink: Arc<dyn FindResultSink>,
    status: Shared,
}
struct Cursor {
    highwater: Option<i32>,
    quarantined: bool,
    row: Option<Data>,
}
struct State {
    limits: Option<FindLimits>,
    next: u64,
    cursors: BTreeMap<BrowserId, Cursor>,
    inflight: u64,
    faulted: bool,
    started: u64,
    finished: u64,
    late: u64,
    first_failure: Option<(FindOperationId, FindFailure)>,
}
#[derive(Clone, Debug)]
pub struct FindDiagnostics {
    pub started: u64,
    pub finished: u64,
    pub pending: u64,
    pub active: Vec<FindStatus>,
    pub late_results: u64,
    pub faulted: bool,
    pub first_failure: Option<(FindOperationId, FindFailure)>,
}
pub(crate) struct Finds {
    state: Mutex<State>,
}
struct Work(Arc<Finds>);
impl Drop for Work {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        if let Some(n) = s.inflight.checked_sub(1) {
            s.inflight = n;
        } else {
            s.faulted = true;
        }
    }
}
impl Finds {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                limits: None,
                next: 1,
                cursors: BTreeMap::new(),
                inflight: 0,
                faulted: false,
                started: 0,
                finished: 0,
                late: 0,
                first_failure: None,
            }),
        })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| {
            let mut s = e.into_inner();
            s.faulted = true;
            s
        })
    }
    pub fn configure(&self, limits: FindLimits) -> Result<(), EngineError> {
        let mut s = self.lock();
        if s.limits.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        s.limits = Some(limits);
        Ok(())
    }
    pub fn configured(&self) -> bool {
        self.lock().limits.is_some()
    }
    fn work(self: &Arc<Self>) -> Work {
        let mut s = self.lock();
        s.inflight = s.inflight.checked_add(1).unwrap_or_else(|| {
            s.faulted = true;
            u64::MAX
        });
        Work(self.clone())
    }
    pub fn pending(&self) -> u64 {
        let s = self.lock();
        s.cursors
            .values()
            .filter(|c| c.row.is_some() || c.quarantined)
            .count() as u64
            + s.inflight
            + u64::from(s.faulted)
    }
    pub fn pending_for(&self, browser: BrowserId) -> u64 {
        let s = self.lock();
        u64::from(
            s.cursors
                .get(&browser)
                .is_some_and(|c| c.row.is_some() || c.quarantined),
        ) + s.inflight
            + u64::from(s.faulted)
    }
    pub fn snapshot(&self) -> FindDiagnostics {
        let s = self.lock();
        FindDiagnostics {
            started: s.started,
            finished: s.finished,
            pending: s
                .cursors
                .values()
                .filter(|c| c.row.is_some() || c.quarantined)
                .count() as u64
                + s.inflight
                + u64::from(s.faulted),
            active: s
                .cursors
                .values()
                .filter_map(|c| c.row.as_ref().map(|d| status(&d.status)))
                .collect(),
            late_results: s.late,
            faulted: s.faulted,
            first_failure: s.first_failure.clone(),
        }
    }
    fn begin(
        &self,
        scope: &Arc<Scope>,
        menu: ContextMenuRequestId,
        sink: &Arc<dyn FindResultSink>,
    ) -> Result<Data, EngineError> {
        let mut s = self.lock();
        let limits = s.limits.ok_or(EngineError::NotReady)?;
        if s.faulted || menu.browser != scope.browser {
            return Err(EngineError::InvalidRequest);
        }
        if scope.context.selection.len() > limits.max_query_bytes.get() {
            return Err(EngineError::Backpressure);
        }
        if let Some(c) = s.cursors.get(&scope.browser) {
            if c.row.is_some() || c.quarantined || c.highwater == Some(i32::MAX) {
                return Err(EngineError::Backpressure);
            }
        } else if s.cursors.len() >= limits.capacity.get() {
            return Err(EngineError::Backpressure);
        }
        let sequence = s.next;
        if sequence == 0 {
            return Err(EngineError::InvalidRequest);
        }
        s.next = sequence.checked_add(1).unwrap_or(0);
        let id = FindOperationId {
            browser: scope.browser,
            sequence,
        };
        let data = Data {
            id,
            scope: scope.clone(),
            sink: sink.clone(),
            status: Arc::new(Mutex::new(FindStatus {
                operation: id,
                menu,
                phase: FindPhase::Admitted,
                query_bytes: scope.context.selection.len(),
                issued: false,
                cancel_requested: false,
                stop_attempted: false,
                stop_submitted: false,
                last_result: None,
                outcome: None,
                first_failure: None,
            })),
        };
        s.cursors
            .entry(id.browser)
            .or_insert(Cursor {
                highwater: None,
                quarantined: false,
                row: None,
            })
            .row = Some(data.clone());
        s.started += 1;
        Ok(data)
    }
    fn data(&self, id: FindOperationId) -> Option<Data> {
        self.lock()
            .cursors
            .get(&id.browser)?
            .row
            .as_ref()
            .filter(|d| d.id == id)
            .cloned()
    }
    fn active(&self, browser: BrowserId) -> Option<Data> {
        self.lock().cursors.get(&browser)?.row.clone()
    }
    fn failure(&self, data: &Data, f: FindFailure) {
        let mut s = self.lock();
        let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
        q.first_failure.get_or_insert(f.clone());
        s.first_failure.get_or_insert((data.id, f));
    }
    fn end(&self, data: &Data, outcome: FindOutcome) -> Option<Data> {
        let mut s = self.lock();
        let c = s.cursors.get_mut(&data.id.browser)?;
        if c.row.as_ref().is_none_or(|d| d.id != data.id) {
            return None;
        }
        {
            let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
            q.phase = FindPhase::Finished;
            q.outcome = Some(if q.first_failure.is_some() {
                FindOutcome::Failed
            } else {
                outcome
            });
        }
        let old = c.row.take();
        s.finished += 1;
        old
    }
    fn finish(&self, data: &Data, outcome: FindOutcome) {
        if let Some(old) = self.end(data, outcome) {
            drop(old);
            self.notify_end(data);
        }
    }
    fn notify_end(&self, data: &Data) {
        if let Err(e) = data.sink.finished(status(&data.status)) {
            self.failure(data, FindFailure::Publish(e));
        }
    }
    fn issue(&self, data: &Data) -> bool {
        let s = self.lock();
        if s.cursors
            .get(&data.id.browser)
            .and_then(|c| c.row.as_ref())
            .is_none_or(|d| d.id != data.id)
        {
            return false;
        }
        let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
        if q.cancel_requested {
            return false;
        }
        q.issued = true;
        if q.query_bytes == 0 {
            q.cancel_requested = true;
            q.phase = FindPhase::Cancelling;
        } else {
            q.phase = FindPhase::Searching;
        }
        true
    }
    fn reserve_cancel(&self, id: FindOperationId) -> Result<Data, EngineError> {
        let s = self.lock();
        let d = s
            .cursors
            .get(&id.browser)
            .and_then(|c| c.row.as_ref())
            .filter(|d| d.id == id)
            .ok_or(EngineError::Closed)?
            .clone();
        let mut q = d.status.lock().unwrap_or_else(|e| e.into_inner());
        if q.cancel_requested {
            return Err(EngineError::Backpressure);
        }
        q.cancel_requested = true;
        q.phase = FindPhase::Cancelling;
        drop(q);
        Ok(d)
    }
    fn rollback_cancel(&self, data: &Data, e: EngineError) {
        let s = self.lock();
        if s.cursors
            .get(&data.id.browser)
            .and_then(|c| c.row.as_ref())
            .is_some_and(|d| d.id == data.id)
        {
            let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
            q.cancel_requested = false;
            q.phase = if q.issued {
                FindPhase::Searching
            } else {
                FindPhase::Admitted
            };
        }
        drop(s);
        self.failure(data, FindFailure::Post(e));
    }
    // No returned callback/row is dropped while the book or outer registry locks
    // are held. Late known native IDs never bind to a newer local operation.
    fn result(
        &self,
        browser: BrowserId,
        identifier: i32,
        matches: i32,
        ordinal: i32,
        rect: Option<FindRect>,
        final_update: bool,
    ) -> Option<(Data, FindResult, bool)> {
        let mut s = self.lock();
        let Some(c) = s.cursors.get_mut(&browser) else {
            return None;
        };
        let Some(d) = c.row.clone() else {
            if c.highwater.is_none_or(|old| identifier > old) {
                c.quarantined = true;
            }
            s.late += 1;
            return None;
        };
        let mut q = d.status.lock().unwrap_or_else(|e| e.into_inner());
        if !q.issued {
            c.quarantined = true;
            drop(q);
            s.late += 1;
            return None;
        }
        let current = q.last_result.as_ref().map(|r| r.cef_identifier);
        if identifier < 0
            || matches < -1
            || ordinal < -1
            || rect.is_some_and(|r| r.width < 0 || r.height < 0)
            || (final_update && matches < 0)
        {
            q.first_failure.get_or_insert(FindFailure::InvalidResult);
            drop(q);
            s.first_failure
                .get_or_insert((d.id, FindFailure::InvalidResult));
            return None;
        }
        if current.is_some_and(|id| id != identifier) {
            if current.is_some_and(|id| identifier > id) {
                c.quarantined = true;
                q.first_failure.get_or_insert(FindFailure::Uncorrelated);
                s.first_failure
                    .get_or_insert((d.id, FindFailure::Uncorrelated));
            }
            drop(q);
            s.late += 1;
            return None;
        }
        if current.is_none() && c.highwater.is_some_and(|old| identifier <= old) {
            drop(q);
            s.late += 1;
            return None;
        }
        let result = FindResult {
            operation: d.id,
            cef_identifier: identifier,
            matches,
            active_ordinal: ordinal,
            selection_rect: rect,
            final_update,
        };
        q.last_result = Some(result.clone());
        c.highwater = Some(identifier);
        drop(q);
        Some((d, result, final_update))
    }
    pub fn fault(&self) {
        self.lock().faulted = true;
    }
    fn enrolled(&self, browser: BrowserId) -> bool {
        self.lock().cursors.contains_key(&browser)
    }
    pub fn browser_closed(self: &Arc<Self>, browser: BrowserId) {
        let _work = self.work();
        let old = { self.lock().cursors.remove(&browser) };
        if let Some(mut c) = old {
            if let Some(d) = c.row.take() {
                let mut q = d.status.lock().unwrap_or_else(|e| e.into_inner());
                q.cancel_requested = true;
                q.phase = FindPhase::Finished;
                q.outcome = Some(if q.first_failure.is_some() {
                    FindOutcome::Failed
                } else if q.query_bytes == 0 && q.issued {
                    FindOutcome::EmptySelection
                } else {
                    FindOutcome::Cancelled
                });
                drop(q);
                self.lock().finished += 1;
                self.notify_end(&d);
            }
        }
    }
    fn selected(&self, browser: Option<BrowserId>, serial: Option<u64>) -> Vec<Data> {
        self.lock()
            .cursors
            .iter()
            .filter(|(b, _)| browser.is_none_or(|id| **b == id))
            .filter_map(|(_, c)| {
                c.row
                    .as_ref()
                    .filter(|d| serial.is_none_or(|n| d.scope.serial == n))
                    .cloned()
            })
            .collect()
    }
}
struct Receipt {
    id: FindOperationId,
    registry: Weak<UiRegistry>,
    shared: Shared,
}
impl FindReceipt for Receipt {
    fn status(&self) -> FindStatus {
        status(&self.shared)
    }
    fn cancel(&self) -> Result<(), EngineError> {
        self.registry
            .upgrade()
            .ok_or(EngineError::Closed)?
            .post_find_cancel(self.id)
    }
}
impl UiRegistry {
    pub(crate) fn start_find(
        &self,
        scope: Arc<Scope>,
        menu: ContextMenuRequestId,
        native: &Browser,
        frame: &Frame,
    ) -> Result<FindOperationId, EngineError> {
        if !scope.current() {
            return Err(EngineError::Closed);
        }
        let binding = scope.registration.upgrade().ok_or(EngineError::Closed)?;
        let sink = binding.find.get().ok_or(EngineError::NotReady)?;
        if !self.find_pristine(scope.browser) {
            return Err(EngineError::InvalidRequest);
        }
        let gate = self.lock_source_admission(scope.browser)?;
        let data = self.finds.begin(&scope, menu, sink)?;
        drop(gate);
        let _work = self.finds.work();
        let receipt: Arc<dyn FindReceipt> = Arc::new(Receipt {
            id: data.id,
            registry: scope.registry.clone(),
            shared: data.status.clone(),
        });
        let request = FindRequest {
            operation: data.id,
            menu,
            context: scope.context.clone(),
        };
        if let Err(e) = data.sink.started(request, receipt) {
            self.finds.failure(&data, FindFailure::Start(e));
            self.finds.finish(&data, FindOutcome::Failed);
            return Ok(data.id);
        }
        if !scope.current()
            || !self.menu_original_frame_valid(&scope, native, frame)
            || !self.finds.issue(&data)
        {
            self.finds.finish(&data, FindOutcome::Cancelled);
            return Ok(data.id);
        }
        self.menu_audit().record();
        let Some(host) = native.host() else {
            self.finds.failure(&data, FindFailure::NativeUnavailable);
            self.finds.finish(&data, FindOutcome::Failed);
            return Ok(data.id);
        };
        let text = CefString::from(scope.context.selection.as_str());
        self.menu_audit().record();
        host.find(Some(&text), 1, 0, 0);
        // Empty selection is native stop, not a successful search. Admission
        // marked it Cancelling BEFORE native entry; it stays unresolved until
        // actual final notification or original Browser close.
        if let Err(e) = self.menu_wake() {
            self.finds.failure(&data, FindFailure::Wake(e));
        }
        Ok(data.id)
    }
    pub(crate) fn post_find_cancel(&self, id: FindOperationId) -> Result<(), EngineError> {
        let data = self.finds.reserve_cancel(id)?;
        if let Err(e) = self.post_find_cleanup(id) {
            self.finds.rollback_cancel(&data, e.clone());
            return Err(e);
        }
        Ok(())
    }
    pub(crate) fn cancel_finds(&self, browser: Option<BrowserId>, serial: Option<u64>) {
        let _work = self.finds.work();
        let selected = self.finds.selected(browser, serial);
        for data in selected {
            let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
            q.cancel_requested = true;
            q.phase = FindPhase::Cancelling;
            drop(q);
            self.cancel_find_now(data.id);
        }
    }
    pub(crate) fn cancel_find_now(&self, id: FindOperationId) {
        let _work = self.finds.work();
        let Some(data) = self.finds.data(id) else {
            return;
        };
        let (issued, stopped) = {
            let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
            q.cancel_requested = true;
            q.phase = FindPhase::Cancelling;
            let attempted = q.stop_attempted;
            if q.issued && !attempted {
                q.stop_attempted = true;
            }
            (q.issued, attempted)
        };
        if !issued {
            self.finds.finish(&data, FindOutcome::Cancelled);
            return;
        }
        if !stopped {
            if let Some(native) = self.find_browser(id.browser) {
                self.menu_audit().record();
                if let Some(host) = native.host() {
                    self.menu_audit().record();
                    host.stop_finding(0);
                    data.status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .stop_submitted = true;
                }
            }
        }
        // Stop is not final/drain. Keep an unresolved operation/identity until real
        // final result or original Browser OnBeforeClose. No timeout or implicit retry.
    }
}
wrap_find_handler! {
 pub(crate) struct RuntimeFindHandler {state:Arc<BrowserClientState>}
 impl FindHandler {
  fn on_find_result(&self,browser:Option<&mut Browser>,identifier:i32,count:i32,selection_rect:Option<&Rect>,active_match_ordinal:i32,final_update:i32){
   let Some(registry)=self.state.registry.upgrade() else{return};let _work=registry.finds.work();
   if currently_on(ThreadId::UI)!=1{registry.finds.fault();return;}
   if !self.state.active.load(Ordering::Acquire){return;}
   let Some(native)=browser else{registry.finds.fault();return};
   if !registry.find_native_matches(self.state.browser,native){registry.finds.fault();return;}
   if !registry.finds.enrolled(self.state.browser){self.state.find_unmanaged.store(true,Ordering::Release);return;}
   if let Some(data)=registry.finds.active(self.state.browser){if !data.scope.operation_alive(){registry.cancel_finds(Some(self.state.browser),Some(data.scope.serial));}}
   let rect=selection_rect.map(|r|FindRect{x:r.x,y:r.y,width:r.width,height:r.height});
   if let Some((data,result,done))=registry.finds.result(self.state.browser,identifier,count,active_match_ordinal,rect,final_update!=0){
    if !status(&data.status).cancel_requested{if let Err(e)=data.sink.update(result){registry.finds.failure(&data,FindFailure::Publish(e));registry.cancel_finds(Some(data.id.browser),Some(data.scope.serial));}}
    if done{let q=status(&data.status);let outcome=if q.query_bytes==0{FindOutcome::EmptySelection}else if q.cancel_requested{FindOutcome::Cancelled}else{FindOutcome::Result};registry.finds.finish(&data,outcome);}
    if let Err(e)=registry.menu_wake(){registry.finds.failure(&data,FindFailure::Wake(e));}
   }
  }
 }
}
#[cfg(test)]
mod tests;
