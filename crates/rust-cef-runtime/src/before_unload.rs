//! Owned beforeunload requests for CEF's `JsdialogCallback`.
//!
//! CEF `javascript_dialog_manager.cc` (`3ca6a87`, 20-56) answers a dropped pending
//! callback `false` and `cont` may re-enter dialog handlers, so no callback is
//! called or dropped under this mutex. Audit first, answer outside every lock;
//! CEF consumes it in `cont`, so a later wrapper drop cannot answer again.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use cef::*;
use rust_cef_core::{contracts::BrowserId, ports::RequestId};

pub(crate) const BEFORE_UNLOAD_DEADLINE: Duration = Duration::from_secs(10);

/// Local closing-attempt identity captured at callback ingress. A retained
/// prompt keeps its allocation distinct from any successor; no native object.
#[derive(Clone, Default)]
pub(crate) struct ClosePromptCut(Arc<()>);

impl ClosePromptCut {
    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One unanswered request per full [`BrowserId`] at most; sequences never reuse.
pub(crate) struct BeforeUnloadRequests(Mutex<Book>);

struct Book {
    /// Zero means exhausted. The first request gets sequence 1.
    next_checked_sequence: u64,
    /// Complete identity; a local id alone conflates runtime generations.
    pending: BTreeMap<(BrowserId, u64), PendingBeforeUnload>,
}

impl Book {
    fn insert(
        &mut self,
        browser: BrowserId,
        reply: Reply,
        close_cut: Option<ClosePromptCut>,
        now: Instant,
    ) -> Result<RequestId, Reply> {
        let occupied = self
            .pending
            .range((browser, 0)..=(browser, u64::MAX))
            .next()
            .is_some();
        if occupied || self.next_checked_sequence == 0 {
            return Err(reply);
        }
        let sequence = self.next_checked_sequence;
        // u64::MAX is usable, then sequences are exhausted.
        self.next_checked_sequence = sequence.checked_add(1).unwrap_or(0);
        let id = RequestId { browser, sequence };
        self.pending.insert(
            (browser, sequence),
            PendingBeforeUnload {
                id,
                close_cut,
                deadline: now.checked_add(BEFORE_UNLOAD_DEADLINE).unwrap_or(now),
                reply_posted: false,
                reply: Some(reply),
            },
        );
        Ok(id)
    }
}

/// Dropping it unanswered drops a live callback (CEF answers `false`); never
/// do that under a lock. Call `answer` or `abandon`.
pub(crate) struct PendingBeforeUnload {
    pub(crate) id: RequestId,
    pub(crate) close_cut: Option<ClosePromptCut>,
    pub(crate) deadline: Instant,
    pub(crate) reply_posted: bool,
    reply: Option<Reply>,
}

enum Reply {
    Native(JsdialogCallback),
    #[cfg(test)]
    Test(Box<dyn FnOnce(bool) + Send + 'static>),
}

impl PendingBeforeUnload {
    /// Answers once on the CEF UI sequence outside all locks; audit first. CEF
    /// consumes the request in `cont`, so the later drop here is inert.
    pub(crate) fn answer(mut self, allow: bool) {
        match self.reply.take() {
            Some(Reply::Native(callback)) => callback.cont(allow as i32, None),
            #[cfg(test)]
            Some(Reply::Test(callback)) => callback(allow),
            None => {}
        }
    }

    /// Unexpected owner teardown cannot safely Release a CEF continuation.
    pub(crate) fn abandon(mut self) {
        if let Some(Reply::Native(callback)) = self.reply.take() {
            std::mem::forget(callback);
        }
    }
}

impl BeforeUnloadRequests {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(Book {
            next_checked_sequence: 1,
            pending: BTreeMap::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// CEF UI only. A duplicate browser or exhausted ids returns the original
    /// callback after releasing the lock; terminate it outside.
    pub(crate) fn insert(
        &self,
        browser: BrowserId,
        callback: JsdialogCallback,
        close_cut: Option<ClosePromptCut>,
        now: Instant,
    ) -> Result<RequestId, JsdialogCallback> {
        let inserted = {
            let mut book = self.lock();
            book.insert(browser, Reply::Native(callback), close_cut, now)
        };
        inserted.map_err(|reply| match reply {
            Reply::Native(callback) => callback,
            #[cfg(test)]
            Reply::Test(_) => unreachable!("native table holds only native callbacks"),
        })
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: RequestId) -> bool {
        self.lock().pending.contains_key(&(id.browser, id.sequence))
    }

    /// Claims the single answer slot; `false` if reserved, taken, or foreign.
    pub(crate) fn reserve_reply(&self, id: RequestId) -> bool {
        self.set_reserve(id, true)
    }

    pub(crate) fn unreserve_reply(&self, id: RequestId) -> bool {
        self.set_reserve(id, false)
    }

    fn set_reserve(&self, id: RequestId, value: bool) -> bool {
        let mut book = self.lock();
        let Some(pending) = book
            .pending
            .get_mut(&(id.browser, id.sequence))
            .filter(|pending| pending.reply_posted != value)
        else {
            return false;
        };
        pending.reply_posted = value;
        true
    }

    pub(crate) fn deadline(&self, id: RequestId) -> Option<Instant> {
        self.lock()
            .pending
            .get(&(id.browser, id.sequence))
            .map(|pending| pending.deadline)
    }

    pub(crate) fn take(&self, id: RequestId) -> Option<PendingBeforeUnload> {
        self.lock().pending.remove(&(id.browser, id.sequence))
    }

    pub(crate) fn take_browser(&self, browser: BrowserId) -> Vec<PendingBeforeUnload> {
        let mut book = self.lock();
        let keys: Vec<_> = book
            .pending
            .range((browser, 0)..=(browser, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        keys.into_iter()
            .filter_map(|key| book.pending.remove(&key))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn take_all(&self) -> Vec<PendingBeforeUnload> {
        std::mem::take(&mut self.lock().pending)
            .into_values()
            .collect()
    }

    pub(crate) fn pending(&self) -> u64 {
        self.lock().pending.len() as u64
    }

    pub(crate) fn abandon(&self) {
        let removed = std::mem::take(&mut self.lock().pending);
        for pending in removed.into_values() {
            pending.abandon();
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_test(
        &self,
        browser: BrowserId,
        close_cut: Option<ClosePromptCut>,
        now: Instant,
        callback: Box<dyn FnOnce(bool) + Send + 'static>,
    ) -> Result<RequestId, Box<dyn FnOnce(bool) + Send + 'static>> {
        let inserted = {
            let mut book = self.lock();
            book.insert(browser, Reply::Test(callback), close_cut, now)
        };
        inserted.map_err(|reply| match reply {
            #[cfg(test)]
            Reply::Test(callback) => callback,
            Reply::Native(_) => unreachable!("test table holds only test callbacks"),
        })
    }
}

impl Drop for BeforeUnloadRequests {
    fn drop(&mut self) {
        // Unexpected owner teardown is not a native UI cancellation path.
        // The normal reset/answer/close paths have already taken every entry.
        self.abandon();
    }
}

/// Data-only tests; they do not exercise native CEF callback lifetime.
#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;

    fn browser(runtime: u64, local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(runtime), local)
    }

    fn noop() -> Box<dyn FnOnce(bool) + Send + 'static> {
        Box::new(|_| {})
    }
    fn noop_id(table: &BeforeUnloadRequests, browser: BrowserId, now: Instant) -> RequestId {
        table.insert_test(browser, None, now, noop()).ok().unwrap()
    }

    #[test]
    fn identities_sequences_and_reservation() {
        let table = BeforeUnloadRequests::new();
        let (mine, foreign) = (browser(1, 10), browser(2, 10));
        let now = Instant::now();
        let first = noop_id(&table, mine, now);
        // One unanswered per browser; a rejection consumes no sequence.
        assert!(
            table
                .insert_test(mine, Some(ClosePromptCut::default()), now, noop())
                .is_err()
        );
        let other = noop_id(&table, foreign, now);
        assert_ne!(first.sequence, other.sequence);
        // Reserve once, roll back on a failed post.
        assert!(!table.unreserve_reply(first));
        assert!(table.reserve_reply(first) && !table.reserve_reply(first));
        assert!(table.unreserve_reply(first) && table.reserve_reply(first));
        // Identity is the full browser id, not the local id.
        let stale = RequestId {
            browser: foreign,
            sequence: first.sequence,
        };
        assert!(!table.contains(stale) && !table.reserve_reply(stale));
        assert!(table.take(stale).is_none() && table.deadline(first).is_some());
        // A taken sequence is never reissued; one browser drains alone.
        assert!(table.take(first).is_some());
        let again = noop_id(&table, mine, now);
        assert!(again.sequence > other.sequence);
        assert_eq!(table.take_browser(mine)[0].id, again);
        let all = table.take_all();
        assert_eq!((all.len(), all[0].id), (1, other));
        assert_eq!(table.pending(), 0);
    }

    #[test]
    fn answer_and_abandon_never_run_under_the_lock() {
        let table = Arc::new(BeforeUnloadRequests::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (reenter, count) = (table.clone(), calls.clone());
        let callback = Box::new(move |allow: bool| {
            // `answer` holds no lock, so re-entering `pending` cannot deadlock.
            assert_eq!(reenter.pending(), 0);
            assert!(allow);
            count.fetch_add(1, Ordering::SeqCst);
        });
        let id = table
            .insert_test(
                browser(1, 12),
                Some(ClosePromptCut::default()),
                Instant::now(),
                callback,
            )
            .ok()
            .unwrap();
        table.take(id).expect("request is pending").answer(true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(table.take(id).is_none());

        let abandoned = noop_id(&table, browser(1, 13), Instant::now());
        table.abandon();
        assert_eq!((table.pending(), table.contains(abandoned)), (0, false));
    }
}
