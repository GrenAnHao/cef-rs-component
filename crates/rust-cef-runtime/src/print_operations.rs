//! Data-only Runtime retirement gate for the Core print ledger. No printer,
//! CEF Print entry, GPUI owner or provider is installed by this module.
use rust_cef_core::{contracts::BrowserId, print::PrintBook};
use std::{
    num::NonZeroU64,
    sync::{Mutex, OnceLock},
};

/// Never enabled by default. All production retirement paths can observe the
/// same ledger; only test-support has an admission path until a real provider
/// can establish original ownership and positively attest its job results.
#[derive(Default)]
pub(crate) struct PrintOperations(OnceLock<Mutex<PrintBook<()>>>);

impl PrintOperations {
    pub(crate) fn pending(&self) -> u64 {
        self.0.get().map_or(0, |book| {
            // A poisoned ledger must not disappear from the shutdown gate.
            book.lock().map_or(1, |book| book.pending_count() as u64)
        })
    }

    pub(crate) fn pending_for(&self, browser: BrowserId) -> bool {
        self.0
            .get()
            .is_some_and(|book| book.lock().map_or(true, |book| book.pending_for(browser)))
    }

    pub(crate) fn invalidate(&self, browser: BrowserId, registration: Option<NonZeroU64>) {
        if let Some(book) = self.0.get()
            && let Ok(mut book) = book.lock()
        {
            book.invalidate_browser(browser, registration);
        }
    }

    pub(crate) fn browser_closed(&self, browser: BrowserId) {
        if let Some(book) = self.0.get()
            && let Ok(mut book) = book.lock()
        {
            let _ = book.close_browser(browser);
        }
    }

    pub(crate) fn drain(&self) {
        if let Some(book) = self.0.get()
            && let Ok(mut book) = book.lock()
        {
            book.begin_drain();
        }
    }
}

#[cfg(feature = "test-support")]
mod probe {
    use super::*;
    use rust_cef_core::{
        contracts::RuntimeGeneration,
        menu::ContextMenuRequestId,
        ports::{EngineError, WindowBinding},
        print::{NoJobOutcome, PrintError, PrintId, PrintOrigin, PrintOutcome, PrintStatus},
    };
    use std::num::NonZeroUsize;

    impl PrintOperations {
        pub(crate) fn probe_arm<const N: usize>(
            &self,
            runtime: RuntimeGeneration,
            owners: [WindowBinding; N],
        ) -> Result<[PrintId; N], EngineError> {
            let capacity = NonZeroUsize::new(N).ok_or(EngineError::InvalidRequest)?;
            let mut book = PrintBook::new(runtime, capacity);
            let mut open = |binding: WindowBinding| {
                let browser = binding.browser;
                book.enroll(browser).map_err(error)?;
                let origin = PrintOrigin {
                    binding,
                    registration: NonZeroU64::new(1).unwrap(),
                    document_epoch: NonZeroU64::new(1).unwrap(),
                };
                book.begin(
                    ContextMenuRequestId {
                        browser,
                        sequence: 1,
                    },
                    origin,
                    (),
                )
                .map_err(|e| error(e.error))
            };
            // No Native CEF callback/print provider: these are only metadata
            // confirmations anchored on the real already-live BrowserIds.
            let ids: [PrintId; N] = owners
                .into_iter()
                .map(&mut open)
                .collect::<Result<Vec<_>, _>>()?
                .try_into()
                .map_err(|_| EngineError::InvalidRequest)?;
            self.0
                .set(Mutex::new(book))
                .map_err(|_| EngineError::InvalidRequest)?;
            Ok(ids)
        }

        pub(crate) fn probe_status(&self, id: PrintId) -> Option<PrintStatus> {
            self.0.get()?.lock().ok()?.status(id)
        }

        pub(crate) fn probe_cancelled(&self, id: PrintId) -> Result<(), EngineError> {
            let mut b = self
                .0
                .get()
                .ok_or(EngineError::NotReady)?
                .lock()
                .map_err(|_| EngineError::InvalidRequest)?;
            if !b.status(id).is_some_and(|w| w.cancel_requested) {
                return Err(EngineError::InvalidRequest);
            }
            // Probe invariant: no confirmation or claim ever reached any
            // native system; unlike provider timeouts, this is known no-job.
            b.no_job_before_claim(id, NoJobOutcome::Cancelled)
                .map_err(error)
        }

        pub(crate) fn probe_retired(&self, id: PrintId) -> Result<PrintStatus, EngineError> {
            let receipt = {
                let mut b = self
                    .0
                    .get()
                    .ok_or(EngineError::NotReady)?
                    .lock()
                    .map_err(|_| EngineError::InvalidRequest)?;
                if !b
                    .status(id)
                    .is_some_and(|s| s.outcome == Some(PrintOutcome::Cancelled))
                {
                    return Err(EngineError::InvalidRequest);
                }
                b.resources_retired(id).map_err(error)?;
                b.take_retired(id).ok_or(EngineError::InvalidRequest)?
            };
            Ok(receipt.status)
        }
    }

    fn error(error: PrintError) -> EngineError {
        match error {
            PrintError::Closing => EngineError::Closing,
            PrintError::WrongRuntime => EngineError::WrongRuntime,
            PrintError::Backpressure => EngineError::Backpressure,
            _ => EngineError::InvalidRequest,
        }
    }
}
