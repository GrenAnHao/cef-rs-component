//! Managed original-frame source acquisition. Native visitor release is NOT
//! inferred from business cancellation, visit return, or local wrapper Drop.
use crate::{menu_bridge::Scope, registry::UiRegistry};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId, menu::ContextMenuRequestId, ports::EngineError, source_view::*,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
};
type Status = Arc<Mutex<SourceOperationStatus>>;
#[derive(Clone)]
pub(crate) enum SourceScope {
    Menu(Arc<Scope>),
    HostMenu(Arc<Scope>),
    HostDirect {
        browser: BrowserId,
        context: Arc<rust_cef_core::menu_context::MenuContext>,
        registry: Weak<UiRegistry>,
    },
}
impl SourceScope {
    pub(crate) fn serial(&self) -> Option<u64> {
        match self {
            Self::Menu(s) | Self::HostMenu(s) => Some(s.serial),
            Self::HostDirect { .. } => None,
        }
    }
    pub(crate) fn context(&self) -> &Arc<rust_cef_core::menu_context::MenuContext> {
        match self {
            Self::Menu(s) | Self::HostMenu(s) => &s.context,
            Self::HostDirect { context, .. } => context,
        }
    }
    pub(crate) fn registry(&self) -> &Weak<UiRegistry> {
        match self {
            Self::Menu(s) | Self::HostMenu(s) => &s.registry,
            Self::HostDirect { registry, .. } => registry,
        }
    }
    pub(crate) fn current(&self) -> bool {
        match self {
            Self::Menu(s) => s.current(),
            Self::HostMenu(s) => s
                .registry
                .upgrade()
                .is_some_and(|r| r.host_menu_scope_alive(s)),
            Self::HostDirect {
                browser, registry, ..
            } => registry
                .upgrade()
                .is_some_and(|r| r.host_direct_source_alive(*browser)),
        }
    }
    pub(crate) fn operation_alive(&self) -> bool {
        match self {
            Self::Menu(s) => s.operation_alive(),
            Self::HostMenu(s) => s
                .registry
                .upgrade()
                .is_some_and(|r| r.host_menu_scope_alive(s)),
            Self::HostDirect {
                browser, registry, ..
            } => registry
                .upgrade()
                .is_some_and(|r| r.host_direct_source_alive(*browser)),
        }
    }
}
#[derive(Clone)]
pub(crate) struct Data {
    pub(crate) scope: SourceScope,
    pub(crate) sink: Arc<dyn SourceViewSink>,
    pub(crate) status: Status,
}
struct Row {
    data: Data,
    close_requested: bool,
}
struct State {
    limits: Option<SourceViewLimits>,
    next: u64,
    rows: BTreeMap<SourceOperationId, Row>,
    started: u64,
    finished: u64,
    faulted: bool,
    first_failure: Option<(SourceOperationId, SourceFailure)>,
}
#[derive(Clone, Debug)]
pub struct SourceViewDiagnostics {
    pub started: u64,
    pub finished: u64,
    pub pending: u64,
    pub native_live: u64,
    pub host_live: u64,
    pub faulted: bool,
    pub first_failure: Option<(SourceOperationId, SourceFailure)>,
    pub active: Vec<SourceOperationStatus>,
}
pub(crate) struct SourceViews {
    state: Mutex<State>,
    #[cfg(feature = "test-support")]
    visitor_probe: Mutex<VisitorProbe>,
}

/// A single test-owned native reference, not a synthesized callback/fence.
/// If the probe cannot finish, abandon the reference to process exit rather
/// than letting a main-thread or erroneous Drop retire a CEF UI resource.
#[cfg(feature = "test-support")]
#[derive(Default)]
enum VisitorProbe {
    #[default]
    Off,
    Armed(BrowserId),
    Held {
        id: SourceOperationId,
        ui: ThreadId,
        visitor: std::mem::ManuallyDrop<CefStringVisitor>,
        release_posted: bool,
    },
    Released,
}

fn status(s: &Status) -> SourceOperationStatus {
    s.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
impl SourceViews {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                limits: None,
                next: 1,
                rows: BTreeMap::new(),
                started: 0,
                finished: 0,
                faulted: false,
                first_failure: None,
            }),
            #[cfg(feature = "test-support")]
            visitor_probe: Mutex::new(VisitorProbe::Off),
        })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| {
            let mut s = e.into_inner();
            s.faulted = true;
            s
        })
    }
    pub fn configure(&self, limits: SourceViewLimits) -> Result<(), EngineError> {
        let mut s = self.lock();
        if s.limits.is_some() || s.faulted {
            return Err(EngineError::InvalidRequest);
        }
        s.limits = Some(limits);
        Ok(())
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn arm_visitor_probe(&self, browser: BrowserId) -> Result<(), EngineError> {
        let mut probe = self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*probe, VisitorProbe::Off) {
            return Err(EngineError::InvalidRequest);
        }
        *probe = VisitorProbe::Armed(browser);
        Ok(())
    }

    #[cfg(feature = "test-support")]
    fn drop_or_hold_visitor(&self, id: SourceOperationId, visitor: CefStringVisitor) {
        let mut probe = self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*probe, VisitorProbe::Armed(owner) if owner == id.browser) {
            *probe = VisitorProbe::Held {
                id,
                ui: std::thread::current().id(),
                visitor: std::mem::ManuallyDrop::new(visitor),
                release_posted: false,
            };
        } else {
            drop(probe);
            drop(visitor);
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn held_visitor_probe(&self) -> Option<SourceOperationId> {
        match &*self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner()) {
            VisitorProbe::Held { id, .. } => Some(*id),
            _ => None,
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn reserve_visitor_release_probe(
        &self,
        id: SourceOperationId,
    ) -> Result<(), EngineError> {
        let mut probe = self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *probe {
            VisitorProbe::Held {
                id: held,
                release_posted,
                ..
            } if *held == id && !*release_posted => {
                *release_posted = true;
                Ok(())
            }
            _ => Err(EngineError::InvalidRequest),
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn rollback_visitor_release_probe(&self, id: SourceOperationId) {
        if let VisitorProbe::Held {
            id: held,
            release_posted,
            ..
        } = &mut *self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner())
            && *held == id
        {
            *release_posted = false;
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn release_visitor_probe(&self, id: SourceOperationId) {
        let mut probe = self.visitor_probe.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(&*probe, VisitorProbe::Held { id: held, ui, release_posted: true, .. }
            if *held == id && *ui == std::thread::current().id())
        {
            drop(probe);
            self.fault(id, SourceFailure::Invariant);
            return;
        }
        let slot = std::mem::replace(&mut *probe, VisitorProbe::Released);
        drop(probe);
        if let VisitorProbe::Held { visitor, .. } = slot {
            // The real last visitor-field Release still owns SourceReleased.
            drop(std::mem::ManuallyDrop::into_inner(visitor));
        }
    }

    pub fn configured(&self) -> bool {
        self.lock().limits.is_some()
    }
    pub(crate) fn max_source_bytes(&self) -> Option<usize> {
        self.lock().limits.map(|l| l.max_source_bytes.get())
    }
    pub(crate) fn is_cancelled(&self, id: SourceOperationId) -> bool {
        self.lock()
            .rows
            .get(&id)
            .is_none_or(|r| status(&r.data.status).cancelled)
    }
    pub fn pending(&self) -> u64 {
        let s = self.lock();
        s.rows.len() as u64 + u64::from(s.faulted)
    }
    pub fn pending_for(&self, browser: BrowserId) -> u64 {
        let s = self.lock();
        s.rows.keys().filter(|id| id.browser == browser).count() as u64 + u64::from(s.faulted)
    }
    pub fn snapshot(&self) -> SourceViewDiagnostics {
        let s = self.lock();
        let active: Vec<_> = s.rows.values().map(|r| status(&r.data.status)).collect();
        SourceViewDiagnostics {
            started: s.started,
            finished: s.finished,
            pending: s.rows.len() as u64 + u64::from(s.faulted),
            native_live: active.iter().filter(|x| x.native_live).count() as u64,
            host_live: active.iter().filter(|x| x.host_live).count() as u64,
            faulted: s.faulted,
            first_failure: s.first_failure.clone(),
            active,
        }
    }
    pub fn begin(
        self: &Arc<Self>,
        menu: ContextMenuRequestId,
        scope: &Arc<Scope>,
        sink: &Arc<dyn SourceViewSink>,
    ) -> Result<(SourceOperationId, usize), EngineError> {
        self.begin_host(menu, SourceScope::Menu(scope.clone()), sink)
    }
    pub(crate) fn begin_host(
        self: &Arc<Self>,
        menu: ContextMenuRequestId,
        scope: SourceScope,
        sink: &Arc<dyn SourceViewSink>,
    ) -> Result<(SourceOperationId, usize), EngineError> {
        let mut s = self.lock();
        let limits = s.limits.ok_or(EngineError::NotReady)?;
        if s.faulted || s.next == 0 {
            return Err(EngineError::InvalidRequest);
        }
        if s.rows.len() >= limits.capacity.get() {
            return Err(EngineError::Backpressure);
        }
        let id = SourceOperationId {
            browser: menu.browser,
            sequence: s.next,
        };
        s.next = s.next.checked_add(1).unwrap_or(0);
        let shared = Arc::new(Mutex::new(SourceOperationStatus {
            operation: id,
            menu,
            phase: SourcePhase::Fetching,
            native_live: true,
            host_live: false,
            callback_received: false,
            cancelled: false,
            source_bytes: 0,
            first_failure: None,
            first_close_failure: None,
            outcome: None,
        }));
        s.rows.insert(
            id,
            Row {
                data: Data {
                    scope,
                    sink: sink.clone(),
                    status: shared,
                },
                close_requested: false,
            },
        );
        s.started += 1;
        Ok((id, limits.max_source_bytes.get()))
    }
    pub(crate) fn abort_unstarted(&self, id: SourceOperationId) {
        let old = {
            let mut s = self.lock();
            if let Some(row) = s.rows.remove(&id) {
                s.started = s.started.saturating_sub(1);
                Some(row)
            } else {
                None
            }
        };
        drop(old);
    }
    pub(crate) fn data(&self, id: SourceOperationId) -> Option<Data> {
        self.lock().rows.get(&id).map(|r| r.data.clone())
    }
    fn failure_locked(s: &mut State, id: SourceOperationId, e: SourceFailure) {
        s.first_failure.get_or_insert((id, e.clone()));
        if let Some(r) = s.rows.get(&id) {
            let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
            if let SourceFailure::Close(error) = &e {
                q.first_close_failure.get_or_insert(error.clone());
            }
            q.first_failure.get_or_insert(e);
            q.phase = SourcePhase::Closing;
        }
    }
    pub fn fail(&self, id: SourceOperationId, e: SourceFailure) {
        let old = {
            let mut s = self.lock();
            Self::failure_locked(&mut s, id, e);
            Self::retire_locked(&mut s, id)
        };
        drop(old);
    }
    fn data_failure(&self, id: SourceOperationId, data: &Data, e: SourceFailure) {
        {
            let mut q = data.status.lock().unwrap_or_else(|e| e.into_inner());
            if let SourceFailure::Close(error) = &e {
                q.first_close_failure.get_or_insert(error.clone());
            }
            q.first_failure.get_or_insert(e.clone());
        }
        self.fail(id, e);
    }
    pub fn fault(&self, id: SourceOperationId, e: SourceFailure) {
        let mut s = self.lock();
        s.faulted = true;
        Self::failure_locked(&mut s, id, e);
    }
    fn retire_locked(s: &mut State, id: SourceOperationId) -> Option<Row> {
        let r = s.rows.get(&id)?;
        let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
        if q.native_live || q.host_live || q.phase != SourcePhase::Closing {
            return None;
        }
        q.outcome = Some(if q.cancelled {
            SourceOutcome::Cancelled
        } else if q.first_failure.is_some() {
            SourceOutcome::Failed
        } else {
            SourceOutcome::Closed
        });
        q.phase = SourcePhase::Finished;
        drop(q);
        s.finished += 1;
        s.rows.remove(&id)
    }
    pub fn received(self: &Arc<Self>, id: SourceOperationId) -> bool {
        let (accepted, duplicate) = {
            let mut s = self.lock();
            let Some(r) = s.rows.get(&id) else {
                s.faulted = true;
                return false;
            };
            let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
            if q.callback_received {
                drop(q);
                Self::failure_locked(&mut s, id, SourceFailure::DuplicateCallback);
                (false, true)
            } else {
                q.callback_received = true;
                (!q.cancelled && q.phase == SourcePhase::Fetching, false)
            }
        };
        if duplicate {
            let _ = self.close_failure(id);
        }
        accepted
    }
    pub(crate) fn unissued(&self, id: SourceOperationId) {
        let old = {
            let mut s = self.lock();
            if let Some(r) = s.rows.get(&id) {
                r.data
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .native_live = false;
            }
            Self::retire_locked(&mut s, id)
        };
        drop(old);
    }
    pub fn released(&self, id: SourceOperationId) {
        let old = {
            let mut s = self.lock();
            if let Some(r) = s.rows.get(&id) {
                let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
                q.native_live = false;
                let missing = !q.callback_received && !q.cancelled;
                drop(q);
                if missing {
                    Self::failure_locked(&mut s, id, SourceFailure::NoCallback);
                }
            } else {
                s.faulted = true;
            }
            Self::retire_locked(&mut s, id)
        };
        drop(old);
    }
    pub(crate) fn begin_publish(&self, id: SourceOperationId, bytes: usize) -> Option<Data> {
        let s = self.lock();
        let r = s.rows.get(&id)?;
        let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
        if q.cancelled || q.phase != SourcePhase::Fetching {
            return None;
        }
        q.source_bytes = bytes;
        q.phase = SourcePhase::Queued;
        q.host_live = true;
        Some(r.data.clone())
    }
    pub(crate) fn control(
        self: &Arc<Self>,
        id: SourceOperationId,
        data: &Data,
    ) -> Arc<dyn SourceViewControl> {
        Arc::new(Control {
            id,
            book: Arc::downgrade(self),
            shared: data.status.clone(),
        })
    }
    pub fn cancel(
        self: &Arc<Self>,
        browser: Option<BrowserId>,
        serial: Option<u64>,
        only: Option<SourceOperationId>,
    ) -> Result<(), EngineError> {
        self.retire(browser, serial, only, false, true)
    }
    fn close_failure(self: &Arc<Self>, id: SourceOperationId) -> Result<(), EngineError> {
        self.retire(Some(id.browser), None, Some(id), false, false)
    }
    fn retire(
        self: &Arc<Self>,
        browser: Option<BrowserId>,
        serial: Option<u64>,
        only: Option<SourceOperationId>,
        retry: bool,
        cancelled: bool,
    ) -> Result<(), EngineError> {
        let (close, retired) = {
            let mut s = self.lock();
            let ids: Vec<_> = s
                .rows
                .iter()
                .filter(|(id, r)| {
                    browser.is_none_or(|b| id.browser == b)
                        && serial.is_none_or(|n| r.data.scope.serial() == Some(n))
                        && only.is_none_or(|n| **id == n)
                })
                .map(|(id, _)| *id)
                .collect();
            let mut close = Vec::new();
            let mut retired = Vec::new();
            for id in ids {
                let r = s.rows.get_mut(&id).unwrap();
                let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
                q.cancelled |= cancelled;
                q.phase = SourcePhase::Closing;
                if q.host_live && (!r.close_requested || retry) {
                    r.close_requested = true;
                    close.push((id, r.data.clone()));
                }
                drop(q);
                if let Some(r) = Self::retire_locked(&mut s, id) {
                    retired.push(r);
                }
            }
            (close, retired)
        };
        drop(retired);
        let mut error = None;
        for (id, d) in close {
            let control = self.control(id, &d);
            if let Err(e) = d.sink.close(id, control) {
                self.data_failure(id, &d, SourceFailure::Close(e.clone()));
                error.get_or_insert(e);
            }
            if let Some(registry) = d.scope.registry().upgrade() {
                if let Err(e) = registry.menu_wake() {
                    self.data_failure(id, &d, SourceFailure::Wake(e.clone()));
                    error.get_or_insert(e);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
    fn presented(&self, id: SourceOperationId) -> Result<(), EngineError> {
        let s = self.lock();
        let r = s.rows.get(&id).ok_or(EngineError::Closed)?;
        let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
        if q.cancelled || q.phase != SourcePhase::Queued || !q.host_live {
            return Err(EngineError::InvalidRequest);
        }
        q.phase = SourcePhase::Presented;
        Ok(())
    }
    fn closed(&self, id: SourceOperationId) -> Result<(), EngineError> {
        let old = {
            let mut s = self.lock();
            let r = s.rows.get(&id).ok_or(EngineError::Closed)?;
            let mut q = r.data.status.lock().unwrap_or_else(|e| e.into_inner());
            if !q.host_live {
                return Err(EngineError::InvalidRequest);
            }
            q.host_live = false;
            q.phase = SourcePhase::Closing;
            drop(q);
            Self::retire_locked(&mut s, id)
        };
        drop(old);
        Ok(())
    }
}
struct Control {
    id: SourceOperationId,
    book: Weak<SourceViews>,
    shared: Status,
}
impl SourceViewControl for Control {
    fn status(&self) -> SourceOperationStatus {
        status(&self.shared)
    }
    fn is_current(&self) -> bool {
        let Some(book) = self.book.upgrade() else {
            return false;
        };
        let Some(d) = book.data(self.id) else {
            return false;
        };
        let q = status(&d.status);
        if q.cancelled || q.phase == SourcePhase::Closing {
            return false;
        }
        if q.phase == SourcePhase::Presented {
            d.scope.operation_alive()
        } else {
            d.scope.current()
        }
    }
    fn presented(&self) -> Result<(), EngineError> {
        let b = self.book.upgrade().ok_or(EngineError::Closed)?;
        let d = b.data(self.id).ok_or(EngineError::Closed)?;
        if !d.scope.operation_alive() {
            return Err(EngineError::Closed);
        }
        b.presented(self.id)
    }
    fn closed(&self) -> Result<(), EngineError> {
        self.book
            .upgrade()
            .ok_or(EngineError::Closed)?
            .closed(self.id)
    }
    fn request_close(&self) -> Result<(), EngineError> {
        self.book.upgrade().ok_or(EngineError::Closed)?.retire(
            Some(self.id.browser),
            None,
            Some(self.id),
            true,
            true,
        )
    }
}
// Native interface fields may be cloned by cef-rs. Count actual field owners,
// not local CefStringVisitor wrappers and not visit calls.
struct VisitorLife {
    book: Arc<SourceViews>,
    registry: Weak<UiRegistry>,
    id: SourceOperationId,
    limit: usize,
    ui: ThreadId,
    fields: AtomicUsize,
}
struct VisitorField(Arc<VisitorLife>);
impl Clone for VisitorField {
    fn clone(&self) -> Self {
        self.0.fields.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}
impl Drop for VisitorField {
    fn drop(&mut self) {
        if self.0.fields.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        if std::thread::current().id() != self.0.ui {
            self.0.book.fault(self.0.id, SourceFailure::WrongThread);
            return;
        }
        // A data-only async task on this SAME CEF UI sequence runs only after
        // final release and any reentrant visit/trampoline have returned.
        let Some(registry) = self.0.registry.upgrade() else {
            self.0.book.fault(self.0.id, SourceFailure::Invariant);
            return;
        };
        if let Err(e) = registry.post_source_release(self.0.id) {
            self.0.book.fault(self.0.id, SourceFailure::Post(e));
        }
    }
}
wrap_string_visitor! {
    struct SourceVisitor {field:VisitorField}
    impl CefStringVisitor {
        fn visit(&self,string:Option<&CefString>){
            let life=self.field.0.clone();
            if std::thread::current().id()!=life.ui{life.book.fault(life.id,SourceFailure::WrongThread);return;}
            if !life.book.received(life.id){return;}
            // Borrow only during this callback. Avoid Display/to_string's full
            // intermediate conversion. Getter's CEF allocation precedes budget.
            let result=copy_source(string,life.limit);
            let Some(registry)=life.registry.upgrade()else{life.book.fault(life.id,SourceFailure::Invariant);return};
            if let Err(e)=registry.post_source_result(life.id,result){life.book.fail(life.id,SourceFailure::Post(e));}
        }
    }
}
fn copy_source(string: Option<&CefString>, limit: usize) -> Result<SourceText, SourceFailure> {
    let string = string.ok_or(SourceFailure::MissingText)?;
    let ptr: *const cef::sys::_cef_string_utf16_t = string.into();
    if ptr.is_null() {
        return Err(SourceFailure::MissingText);
    }
    // SAFETY: StringVisitor's borrowed CEF header/buffer remains immutable and
    // valid through the synchronous visit. Shared helper checks header/budget.
    let text =
        unsafe { crate::menu_model::read_native_utf16(&*ptr, limit) }.map_err(|e| match e {
            crate::menu_model::CopyError::LabelBudget => SourceFailure::Budget,
            _ => SourceFailure::InvalidText,
        })?;
    SourceText::new(
        text,
        std::num::NonZeroUsize::new(limit).ok_or(SourceFailure::Budget)?,
    )
}
fn start_current(scope: &Scope, data: &Data) -> bool {
    scope.current() && !status(&data.status).cancelled
}
impl UiRegistry {
    pub(crate) fn start_source(
        &self,
        scope: Arc<Scope>,
        menu: ContextMenuRequestId,
        browser: &Browser,
        frame: &Frame,
    ) -> Result<SourceOperationId, EngineError> {
        if !scope.current() {
            return Err(EngineError::InvalidRequest);
        }
        let sink = scope
            .registration
            .upgrade()
            .and_then(|r| r.source.get().cloned())
            .ok_or(EngineError::NotReady)?;
        let (id, limit) = {
            let state = self.lock_source_admission(scope.browser)?;
            let result = self.sources.begin(menu, &scope, &sink);
            drop(state);
            result?
        };
        let data = self.sources.data(id).ok_or(EngineError::Closed)?;
        if let Err(e) = sink.started(id, menu, self.sources.control(id, &data)) {
            self.sources.fail(id, SourceFailure::Start(e.clone()));
            self.sources.unissued(id);
            return Err(e);
        }
        if !start_current(&scope, &data) || !self.menu_original_frame_valid(&scope, browser, frame)
        {
            let _ = self
                .sources
                .cancel(Some(id.browser), Some(scope.serial), Some(id));
            self.sources.unissued(id);
            return Err(EngineError::Closed);
        }
        let field = VisitorField(Arc::new(VisitorLife {
            book: self.sources.clone(),
            registry: scope.registry.clone(),
            id,
            limit,
            ui: std::thread::current().id(),
            fields: AtomicUsize::new(1),
        }));
        let mut visitor = SourceVisitor::new(field);
        self.menu_audit().record();
        frame.source(Some(&mut visitor));
        #[cfg(feature = "test-support")]
        self.sources.drop_or_hold_visitor(id, visitor);
        #[cfg(not(feature = "test-support"))]
        drop(visitor);
        Ok(id)
    }
    pub(crate) fn issue_host_visitor(
        &self,
        id: SourceOperationId,
        limit: usize,
        registry: Weak<UiRegistry>,
        frame: &Frame,
    ) {
        let field = VisitorField(Arc::new(VisitorLife {
            book: self.sources.clone(),
            registry,
            id,
            limit,
            ui: std::thread::current().id(),
            fields: AtomicUsize::new(1),
        }));
        let mut visitor = SourceVisitor::new(field);
        self.menu_audit().record();
        frame.source(Some(&mut visitor));
        #[cfg(feature = "test-support")]
        self.sources.drop_or_hold_visitor(id, visitor);
        #[cfg(not(feature = "test-support"))]
        drop(visitor);
    }
    pub(crate) fn deliver_source(
        &self,
        id: SourceOperationId,
        result: Result<SourceText, SourceFailure>,
    ) {
        let Some(data) = self.sources.data(id) else {
            return;
        };
        if let Err(e) = result.as_ref() {
            self.sources.fail(id, e.clone());
            let _ = self.menu_wake();
            return;
        }
        let source = result.unwrap();
        let (permit, pair, current) = match &data.scope {
            SourceScope::Menu(scope) => {
                let permit = if scope.admission() {
                    scope.lease.try_enter(&scope.stamp).ok()
                } else {
                    None
                };
                let pair = permit
                    .as_ref()
                    .and_then(|_| self.menu_original_frame(scope));
                let current = !self.is_draining() && scope.current();
                (permit, pair, current)
            }
            SourceScope::HostMenu(scope) => {
                let alive = self.host_menu_scope_alive(scope);
                let permit = if alive {
                    scope.lease.try_enter(&scope.stamp).ok()
                } else {
                    None
                };
                let pair = permit
                    .as_ref()
                    .and_then(|_| self.host_browser_main_frame(id.browser));
                let current = !self.is_draining() && alive;
                (permit, pair, current)
            }
            SourceScope::HostDirect { browser, .. } => {
                let alive = self.host_direct_source_alive(*browser);
                let pair = if alive {
                    self.host_browser_main_frame(*browser)
                } else {
                    None
                };
                let current = !self.is_draining() && alive;
                (None, pair, current)
            }
        };
        if pair.is_none() || !current {
            let _ = self
                .sources
                .cancel(Some(id.browser), data.scope.serial(), Some(id));
            return;
        }
        let Some(data) = self.sources.begin_publish(id, source.len()) else {
            return;
        };
        let request = SourceViewRequest {
            operation: id,
            menu: status(&data.status).menu,
            context: data.scope.context().clone(),
            source,
        };
        let control = self.sources.control(id, &data);
        if let Err(e) = data.sink.publish(request, control) {
            self.sources
                .data_failure(id, &data, SourceFailure::Publish(e));
            let _ = self.sources.close_failure(id);
        }
        if let Err(e) = self.menu_wake() {
            self.sources.data_failure(id, &data, SourceFailure::Wake(e));
            let _ = self.sources.close_failure(id);
        }
        if !data.scope.operation_alive() {
            let _ = self
                .sources
                .cancel(Some(id.browser), data.scope.serial(), Some(id));
        }
        drop(permit);
        drop(pair);
    }
}
#[cfg(test)]
mod tests;
