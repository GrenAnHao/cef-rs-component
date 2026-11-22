//! Asynchronous browser `PrintToPdf` export with destination confinement and
//! shutdown-barrier tracking. Strictly separate from physical printing (`131`).
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    host_capabilities::{
        HostCapabilityError, PdfExportId, PdfExportOptions, PdfExportOutcome,
        validate_pdf_destination,
    },
    ports::UiDispatcher,
};

use crate::executor::NativeAudit;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdfExportStatus {
    Pending,
    Completed(PdfExportOutcome),
    Failed(HostCapabilityError),
}

pub struct PdfExportTicket {
    id: PdfExportId,
    state: Arc<Mutex<PdfExportStatus>>,
}

impl PdfExportTicket {
    pub fn id(&self) -> PdfExportId {
        self.id
    }

    pub fn poll(&self) -> PdfExportStatus {
        lock(&self.state).clone()
    }
}

impl std::fmt::Debug for PdfExportTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfExportTicket")
            .field("id", &self.id)
            .field("status", &self.poll())
            .finish()
    }
}

struct PdfPendingGuard {
    pending: Arc<AtomicU64>,
    finished: bool,
}

impl PdfPendingGuard {
    fn new(pending: Arc<AtomicU64>) -> Self {
        Self {
            pending,
            finished: false,
        }
    }

    fn release(&mut self) {
        if !self.finished {
            self.finished = true;
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for PdfPendingGuard {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct PdfExportRegistry {
    next_seq: AtomicU64,
    pending: Arc<AtomicU64>,
    closed: AtomicBool,
    entries: Mutex<BTreeMap<PdfExportId, Arc<Mutex<PdfExportStatus>>>>,
}

impl PdfExportRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            next_seq: AtomicU64::new(1),
            pending: Arc::new(AtomicU64::new(0)),
            closed: AtomicBool::new(false),
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn pending_count(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn reserve(
        &self,
        browser: BrowserId,
        destination: &PathBuf,
        options: &PdfExportOptions,
        forbidden_roots: &[PathBuf],
    ) -> Result<(PathBuf, PdfExportTicket), HostCapabilityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostCapabilityError::BrowserClosed);
        }
        options.validate()?;
        let clean = validate_pdf_destination(destination, forbidden_roots)?;
        let sequence = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err(HostCapabilityError::QueueFull);
        }
        let id = PdfExportId { browser, sequence };
        let cell = Arc::new(Mutex::new(PdfExportStatus::Pending));
        lock(&self.entries).insert(id, cell.clone());
        self.pending.fetch_add(1, Ordering::AcqRel);
        Ok((clean, PdfExportTicket { id, state: cell }))
    }

    pub(crate) fn rollback_reserve(&self, id: PdfExportId, err: HostCapabilityError) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
        if let Some(cell) = lock(&self.entries).remove(&id) {
            *lock(&cell) = PdfExportStatus::Failed(err);
        }
    }

    pub(crate) fn close_all(&self) {
        self.closed.store(true, Ordering::Release);
        for (_, cell) in std::mem::take(&mut *lock(&self.entries)) {
            let mut status = lock(&cell);
            if matches!(*status, PdfExportStatus::Pending) {
                *status = PdfExportStatus::Failed(HostCapabilityError::Cancelled);
            }
        }
    }

    pub(crate) fn run_print_to_pdf(
        &self,
        on_ui: bool,
        id: PdfExportId,
        destination: PathBuf,
        options: PdfExportOptions,
        native: Option<Browser>,
        audit: &NativeAudit,
        ui: &Arc<dyn UiDispatcher>,
    ) {
        let mut guard = PdfPendingGuard::new(self.pending.clone());
        let state = lock(&self.entries)
            .remove(&id)
            .unwrap_or_else(|| Arc::new(Mutex::new(PdfExportStatus::Pending)));
        let fail = |guard: &mut PdfPendingGuard, error: HostCapabilityError| {
            *lock(&state) = PdfExportStatus::Failed(error);
            guard.release();
            let _ = ui.wake();
        };
        if !on_ui {
            fail(&mut guard, HostCapabilityError::Cancelled);
            return;
        }
        let Some(host) = native.filter(|b| b.is_valid() != 0).and_then(|b| b.host()) else {
            fail(&mut guard, HostCapabilityError::BrowserClosed);
            return;
        };
        let settings = PdfPrintSettings {
            size: std::mem::size_of::<cef::sys::_cef_pdf_print_settings_t>(),
            landscape: options.landscape as i32,
            print_background: options.print_background as i32,
            scale: options.scale,
            paper_width: options.paper_width,
            paper_height: options.paper_height,
            prefer_css_page_size: options.prefer_css_page_size as i32,
            margin_type: PdfPrintMarginType::DEFAULT,
            page_ranges: CefString::from(options.page_ranges.as_str()),
            generate_document_outline: options.generate_document_outline as i32,
            ..Default::default()
        };
        let collector = Arc::new(Mutex::new(PdfCollector {
            id,
            out: state,
            guard: Some(guard),
            ui: ui.clone(),
        }));
        let mut cb = RuntimePdfCallback::new(collector);
        let path_str = crate::runtime::dos_path(&destination);
        let cef_path = CefString::from(path_str.as_str());
        audit.record();
        host.print_to_pdf(Some(&cef_path), Some(&settings), Some(&mut cb));
        drop(cb);
        drop(host);
    }
}

struct PdfCollector {
    id: PdfExportId,
    out: Arc<Mutex<PdfExportStatus>>,
    guard: Option<PdfPendingGuard>,
    ui: Arc<dyn UiDispatcher>,
}

impl PdfCollector {
    fn finish(&mut self, status: PdfExportStatus) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        *lock(&self.out) = status;
        guard.release();
        let _ = self.ui.wake();
    }
}

impl Drop for PdfCollector {
    fn drop(&mut self) {
        self.finish(PdfExportStatus::Failed(HostCapabilityError::Cancelled));
    }
}

wrap_pdf_print_callback! {
    struct RuntimePdfCallback {
        collector: Arc<Mutex<PdfCollector>>,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, _path: Option<&CefString>, ok: ::std::os::raw::c_int) {
            let mut collector = lock(&self.collector);
            let id = collector.id;
            collector.finish(PdfExportStatus::Completed(PdfExportOutcome {
                id,
                ok: ok != 0,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use rust_cef_core::ports::EngineError;

    struct DummyUi;
    impl UiDispatcher for DummyUi {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn pdf_destination_validation_and_callback_drop_guard() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("gpui-cef-pdf-test-{nonce}"));
        let cache = base.join("cache");
        let export_dir = base.join("exports");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&export_dir).unwrap();

        let reg = PdfExportRegistry::new();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 5);
        assert_eq!(
            reg.reserve(
                browser,
                &cache.join("bad.pdf"),
                &PdfExportOptions::default(),
                &[cache.clone()],
            )
            .unwrap_err(),
            HostCapabilityError::ForbiddenDestination
        );
        assert_eq!(
            reg.reserve(
                browser,
                &export_dir.join("not-pdf.txt"),
                &PdfExportOptions::default(),
                &[cache.clone()],
            )
            .unwrap_err(),
            HostCapabilityError::InvalidDestination
        );

        let (_clean, ticket) = reg
            .reserve(
                browser,
                &export_dir.join("ok.pdf"),
                &PdfExportOptions::default(),
                &[cache],
            )
            .unwrap();
        assert_eq!(reg.pending_count(), 1);
        reg.run_print_to_pdf(
            false,
            ticket.id(),
            export_dir.join("ok.pdf"),
            PdfExportOptions::default(),
            None,
            &NativeAudit::new(),
            &(Arc::new(DummyUi) as Arc<dyn UiDispatcher>),
        );
        assert_eq!(reg.pending_count(), 0);
        assert_eq!(
            ticket.poll(),
            PdfExportStatus::Failed(HostCapabilityError::Cancelled)
        );

        // Callback drop without invocation also decrements pending and fails cleanly.
        let pending = Arc::new(AtomicU64::new(1));
        let out = Arc::new(Mutex::new(PdfExportStatus::Pending));
        let collector = Arc::new(Mutex::new(PdfCollector {
            id: PdfExportId {
                browser,
                sequence: 99,
            },
            out: out.clone(),
            guard: Some(PdfPendingGuard::new(pending.clone())),
            ui: Arc::new(DummyUi),
        }));
        let cb = RuntimePdfCallback::new(collector);
        drop(cb);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(
            *lock(&out),
            PdfExportStatus::Failed(HostCapabilityError::Cancelled)
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
