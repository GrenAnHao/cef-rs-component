//! Lightweight runtime-side frame HTML source and display text visitor (`Task 1.4`).
//!
//! Uses CEF's `Frame::source` / `Frame::text` directly on the CEF UI thread while
//! reusing the UI-thread `VisitorField` release invariant from `source_view.rs`,
//! keeping `source_view.rs` and its context-menu ledger completely untouched.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::ThreadId,
};

use cef::*;
use rust_cef_core::{contracts::BrowserId, js_bridge::JsFrameScope};

use crate::{
    attachment::ReplyOnce,
    js_bridge_native::{frame_origin_allowed, resolve_verified_frame},
    registry::UiRegistry,
};

pub const DEFAULT_FRAME_TEXT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FRAME_TEXT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameTextKind {
    Source,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameTextError {
    Closed,
    NotReady,
    PolicyDenied,
    FrameNotFound,
    BudgetExceeded,
    InvalidText,
    WrongThread,
    Cancelled,
    MissingText,
}

struct ActiveFrameText {
    browser: BrowserId,
    reply: ReplyOnce<Result<String, FrameTextError>>,
}

/// Tracks in-flight `Frame::source` / `Frame::text` visitor requests so browser
/// close or drain cancels pending futures deterministically.
pub(crate) struct FrameTextLedger {
    next_id: AtomicU64,
    active: Mutex<BTreeMap<u64, ActiveFrameText>>,
    off_ui_drops: AtomicU64,
}

impl FrameTextLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            active: Mutex::new(BTreeMap::new()),
            off_ui_drops: AtomicU64::new(0),
        })
    }

    pub(crate) fn register(
        &self,
        browser: BrowserId,
        reply: ReplyOnce<Result<String, FrameTextError>>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, ActiveFrameText { browser, reply });
        id
    }

    pub(crate) fn complete(&self, id: u64, result: Result<String, FrameTextError>) {
        let entry = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if let Some(active) = entry {
            active.reply.complete(result);
        }
    }

    pub(crate) fn cancel_browser(&self, browser: BrowserId) {
        let mut to_cancel = Vec::new();
        {
            let mut guard = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids: Vec<u64> = guard
                .iter()
                .filter_map(|(&id, entry)| (entry.browser == browser).then_some(id))
                .collect();
            for id in ids {
                if let Some(entry) = guard.remove(&id) {
                    to_cancel.push(entry);
                }
            }
        }
        for entry in to_cancel {
            entry.reply.complete(Err(FrameTextError::Closed));
        }
    }

    pub(crate) fn cancel_all(&self) {
        let drained: Vec<ActiveFrameText> = std::mem::take(
            &mut *self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_values()
        .collect();
        for entry in drained {
            entry.reply.complete(Err(FrameTextError::Closed));
        }
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub(crate) fn off_ui_drops(&self) -> u64 {
        self.off_ui_drops.load(Ordering::Acquire)
    }
}

struct VisitorLife {
    ledger: Arc<FrameTextLedger>,
    id: u64,
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
            self.0.ledger.off_ui_drops.fetch_add(1, Ordering::AcqRel);
            self.0
                .ledger
                .complete(self.0.id, Err(FrameTextError::WrongThread));
            return;
        }
        self.0
            .ledger
            .complete(self.0.id, Err(FrameTextError::Cancelled));
    }
}

wrap_string_visitor! {
    struct FrameTextVisitor {
        field: VisitorField,
    }

    impl CefStringVisitor {
        fn visit(&self, string: Option<&CefString>) {
            let life = self.field.0.clone();
            if std::thread::current().id() != life.ui {
                life.ledger
                    .complete(life.id, Err(FrameTextError::WrongThread));
                return;
            }
            let result = copy_frame_text(string, life.limit);
            life.ledger.complete(life.id, result);
        }
    }
}

fn copy_frame_text(string: Option<&CefString>, limit: usize) -> Result<String, FrameTextError> {
    let string = string.ok_or(FrameTextError::MissingText)?;
    let ptr: *const cef::sys::_cef_string_utf16_t = string.into();
    if ptr.is_null() {
        return Err(FrameTextError::MissingText);
    }
    // SAFETY: `CefStringVisitor::visit` guarantees the borrowed UTF-16 buffer
    // remains valid and immutable for the synchronous duration of the visit.
    unsafe { crate::menu_model::read_native_utf16(&*ptr, limit) }.map_err(|err| match err {
        crate::menu_model::CopyError::LabelBudget => FrameTextError::BudgetExceeded,
        _ => FrameTextError::InvalidText,
    })
}

pub(crate) fn run_frame_text_task(
    registry: &UiRegistry,
    on_ui: bool,
    browser: BrowserId,
    kind: FrameTextKind,
    scope: JsFrameScope,
    max_bytes: usize,
    reply: ReplyOnce<Result<String, FrameTextError>>,
) {
    if !on_ui {
        reply.complete(Err(FrameTextError::WrongThread));
        return;
    }
    if registry.is_draining() {
        reply.complete(Err(FrameTextError::Closed));
        return;
    }
    let Some(native) = registry.ready_browser_native(browser) else {
        reply.complete(Err(FrameTextError::NotReady));
        return;
    };
    let client_state = registry.browser_client_state(browser);
    let Some(frame) = resolve_verified_frame(&native, registry.audit_ref(), &scope) else {
        reply.complete(Err(FrameTextError::FrameNotFound));
        return;
    };
    if let Some(client) = client_state.as_ref()
        && frame_origin_allowed(client, &frame).is_none()
    {
        reply.complete(Err(FrameTextError::PolicyDenied));
        return;
    }
    let limit = max_bytes.clamp(1, MAX_FRAME_TEXT_BYTES);
    let id = registry.frame_texts.register(browser, reply);
    let field = VisitorField(Arc::new(VisitorLife {
        ledger: registry.frame_texts.clone(),
        id,
        limit,
        ui: std::thread::current().id(),
        fields: AtomicUsize::new(1),
    }));
    let mut visitor = FrameTextVisitor::new(field);
    registry.audit_ref().record();
    match kind {
        FrameTextKind::Source => frame.source(Some(&mut visitor)),
        FrameTextKind::Text => frame.text(Some(&mut visitor)),
    }
    drop(visitor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use std::sync::mpsc;

    #[test]
    fn visitor_enforces_ui_thread_drop_and_close_cancellation() {
        let ledger = FrameTextLedger::new();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);

        let (tx1, rx1) = mpsc::channel();
        let reply1 = ReplyOnce::new(
            move |res| {
                let _ = tx1.send(res);
            },
            || Err(FrameTextError::Closed),
        );
        let id1 = ledger.register(browser, reply1);
        let field1 = VisitorField(Arc::new(VisitorLife {
            ledger: ledger.clone(),
            id: id1,
            limit: 1024,
            ui: std::thread::current().id(),
            fields: AtomicUsize::new(1),
        }));
        let cloned = field1.clone();
        drop(field1);
        assert_eq!(ledger.pending(), 1);

        // Dropping final VisitorField off the UI thread must record fault and reject.
        std::thread::spawn(move || drop(cloned)).join().unwrap();
        assert_eq!(ledger.off_ui_drops(), 1);
        assert_eq!(ledger.pending(), 0);
        assert_eq!(rx1.recv().unwrap(), Err(FrameTextError::WrongThread));

        // Closing browser resolves any still-pending visitor immediately as Closed.
        let (tx2, rx2) = mpsc::channel();
        let reply2 = ReplyOnce::new(
            move |res| {
                let _ = tx2.send(res);
            },
            || Err(FrameTextError::Closed),
        );
        let id2 = ledger.register(browser, reply2);
        let field2 = VisitorField(Arc::new(VisitorLife {
            ledger: ledger.clone(),
            id: id2,
            limit: 1024,
            ui: std::thread::current().id(),
            fields: AtomicUsize::new(1),
        }));
        ledger.cancel_browser(browser);
        assert_eq!(rx2.recv().unwrap(), Err(FrameTextError::Closed));
        drop(field2);
        assert_eq!(ledger.pending(), 0);
    }

    #[test]
    fn copy_frame_text_enforces_budget_and_utf16_decoding() {
        let small = CefString::from("<html>ok</html>");
        assert_eq!(
            copy_frame_text(Some(&small), 64),
            Ok("<html>ok</html>".to_owned())
        );
        assert_eq!(
            copy_frame_text(Some(&small), 4),
            Err(FrameTextError::BudgetExceeded)
        );
        assert_eq!(copy_frame_text(None, 64), Err(FrameTextError::MissingText));
    }
}
