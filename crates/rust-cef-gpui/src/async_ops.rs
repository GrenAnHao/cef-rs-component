//! Ergonomic Future-based async helpers and screenshot APIs (`Tasks 3.1 & 3.2`).
//!
//! Backs [`CefBrowser`](crate::CefBrowser) and [`NativeCefBrowser`](crate::NativeCefBrowser)
//! with `evaluate_script_async`, `get_source_async`, `get_text_async`,
//! `wait_for_initial_load_async`, `capture_screenshot_bgra`, and
//! `capture_screenshot_png` using zero new external crate dependencies.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder,
    codecs::png::{PngDecoder, PngEncoder},
};
use rust_cef_core::{
    contracts::MAX_NATIVE_BOUND,
    host_capabilities::{CdpEvent, CdpMethodId, CdpMethodResult, CdpObserver, HostCapabilityError},
    js_bridge::{EvalError, EvalOutcome, EvalRequestId},
    ports::{BrowserEvent, EngineError, FramePayload, OwnedFrame},
};
use rust_cef_runtime::{FrameTextError, RuntimeBrowserBridge};

use crate::js_repository::{JsonVal, parse_json};

pub const DEFAULT_ASYNC_OP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PENDING_ASYNC_OPS: usize = 64;
pub const MAX_PENDING_SCREENSHOTS: usize = 16;
pub const CEF_ERR_ABORTED: i32 = -3;

/// Outcome metadata resolved by `wait_for_initial_load_async`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialLoadResult {
    pub http_status_code: i32,
}

/// Compact premultiplied/pixel BGRA8 screenshot image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenshotBgra {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Unified typed failure enum for async browser and screenshot operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncOpError {
    Closed,
    NotReady,
    TimedOut,
    Cancelled,
    QueueFull,
    PolicyDenied,
    FrameNotFound,
    BudgetExceeded,
    CapabilityDisabled,
    BridgeUnavailable,
    JsException { message: String, line: i32 },
    LoadFailed { error_code: i32 },
    InvalidRequest(String),
    EncodeFailed(String),
    DecodeFailed(String),
    Runtime(EngineError),
    HostCapability(HostCapabilityError),
}

impl fmt::Display for AsyncOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "browser closed"),
            Self::NotReady => write!(f, "browser or frame not ready"),
            Self::TimedOut => write!(f, "operation timed out"),
            Self::Cancelled => write!(f, "operation cancelled"),
            Self::QueueFull => write!(f, "async operation queue is full"),
            Self::PolicyDenied => write!(f, "policy denied operation"),
            Self::FrameNotFound => write!(f, "target frame not found"),
            Self::BudgetExceeded => write!(f, "response payload exceeded byte budget"),
            Self::CapabilityDisabled => write!(f, "capability is disabled on this browser"),
            Self::BridgeUnavailable => write!(f, "JS/runtime bridge unavailable"),
            Self::JsException { message, line } => {
                write!(f, "JS exception at line {line}: {message}")
            }
            Self::LoadFailed { error_code } => write!(f, "page load failed (code={error_code})"),
            Self::InvalidRequest(msg) => write!(f, "invalid request: {msg}"),
            Self::EncodeFailed(msg) => write!(f, "screenshot encode failed: {msg}"),
            Self::DecodeFailed(msg) => write!(f, "screenshot decode failed: {msg}"),
            Self::Runtime(err) => write!(f, "runtime error: {err:?}"),
            Self::HostCapability(err) => write!(f, "host capability error: {err:?}"),
        }
    }
}

impl std::error::Error for AsyncOpError {}

impl From<EvalError> for AsyncOpError {
    fn from(err: EvalError) -> Self {
        match err {
            EvalError::Disabled => Self::CapabilityDisabled,
            EvalError::PolicyDenied => Self::PolicyDenied,
            EvalError::InvalidScript => Self::InvalidRequest("invalid script".into()),
            EvalError::PayloadTooLarge | EvalError::DepthExceeded => Self::BudgetExceeded,
            EvalError::InvalidJson => Self::DecodeFailed("invalid eval result JSON".into()),
            EvalError::UnsupportedResultType => {
                Self::InvalidRequest("unsupported JS result type".into())
            }
            EvalError::QueueFull => Self::QueueFull,
            EvalError::JsException { message, line } => Self::JsException { message, line },
            EvalError::Navigated => Self::Cancelled,
            EvalError::RendererTerminated | EvalError::Closed => Self::Closed,
            EvalError::Timeout => Self::TimedOut,
            EvalError::FrameNotFound => Self::FrameNotFound,
        }
    }
}

impl From<FrameTextError> for AsyncOpError {
    fn from(err: FrameTextError) -> Self {
        match err {
            FrameTextError::Closed | FrameTextError::WrongThread => Self::Closed,
            FrameTextError::NotReady | FrameTextError::MissingText => Self::NotReady,
            FrameTextError::PolicyDenied => Self::PolicyDenied,
            FrameTextError::FrameNotFound => Self::FrameNotFound,
            FrameTextError::BudgetExceeded => Self::BudgetExceeded,
            FrameTextError::InvalidText => Self::DecodeFailed("invalid UTF-16 frame text".into()),
            FrameTextError::Cancelled => Self::Cancelled,
        }
    }
}

impl From<HostCapabilityError> for AsyncOpError {
    fn from(err: HostCapabilityError) -> Self {
        match err {
            HostCapabilityError::BrowserClosed => Self::Closed,
            HostCapabilityError::NotReady => Self::NotReady,
            HostCapabilityError::CapabilityDisabled => Self::CapabilityDisabled,
            HostCapabilityError::QueueFull => Self::QueueFull,
            HostCapabilityError::CdpPayloadTooLarge => Self::BudgetExceeded,
            other => Self::HostCapability(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Zero-dep Future + shared Condvar timer wheel for deadline wakeups
// ---------------------------------------------------------------------------

trait ExpirableSlot: Send + Sync {
    fn expire_if_pending(&self) -> bool;
}

struct TimerEntry {
    deadline: Instant,
    seq: u64,
    slot: std::sync::Weak<dyn ExpirableSlot>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

struct TimerWheel {
    heap: Mutex<BinaryHeap<Reverse<TimerEntry>>>,
    cv: Condvar,
    seq: AtomicU64,
}

impl TimerWheel {
    fn global() -> &'static Self {
        static WHEEL: OnceLock<TimerWheel> = OnceLock::new();
        WHEEL.get_or_init(|| {
            let wheel = TimerWheel {
                heap: Mutex::new(BinaryHeap::new()),
                cv: Condvar::new(),
                seq: AtomicU64::new(1),
            };
            let _ = std::thread::Builder::new()
                .name("rust-cef-gpui-async-timer".into())
                .spawn(|| Self::run_loop());
            wheel
        })
    }

    fn run_loop() {
        let wheel = Self::global();
        let mut guard = wheel
            .heap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let now = Instant::now();
            while let Some(Reverse(top)) = guard.peek() {
                if top.deadline > now {
                    break;
                }
                let entry = guard.pop().expect("peeked").0;
                if let Some(slot) = entry.slot.upgrade() {
                    drop(guard);
                    let _ = slot.expire_if_pending();
                    guard = wheel
                        .heap
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            if let Some(Reverse(top)) = guard.peek() {
                let wait_dur = top.deadline.saturating_duration_since(Instant::now());
                let (next_guard, _) = wheel
                    .cv
                    .wait_timeout(guard, wait_dur)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard = next_guard;
            } else {
                guard = wheel
                    .cv
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    fn schedule(&self, deadline: Instant, slot: std::sync::Weak<dyn ExpirableSlot>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self
            .heap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(Reverse(TimerEntry {
            deadline,
            seq,
            slot,
        }));
        self.cv.notify_one();
    }
}

struct SlotInner<T> {
    result: Option<Result<T, AsyncOpError>>,
    waker: Option<Waker>,
    on_cleanup: Option<Box<dyn FnOnce() + Send>>,
}

/// Shared single-shot completion slot connecting runtime/event callbacks with [`AsyncOpFuture`].
pub struct AsyncOpSlot<T: Send + 'static> {
    deadline: Option<Instant>,
    inner: Mutex<SlotInner<T>>,
}

impl<T: Send + 'static> AsyncOpSlot<T> {
    pub fn new(deadline: Option<Instant>) -> Arc<Self> {
        Arc::new(Self {
            deadline,
            inner: Mutex::new(SlotInner {
                result: None,
                waker: None,
                on_cleanup: None,
            }),
        })
    }

    pub fn set_cleanup(&self, cleanup: impl FnOnce() + Send + 'static) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.result.is_some() {
            drop(guard);
            cleanup();
        } else {
            guard.on_cleanup = Some(Box::new(cleanup));
        }
    }

    pub fn arm_timer(self: &Arc<Self>) {
        if let Some(deadline) = self.deadline {
            let weak: std::sync::Weak<dyn ExpirableSlot> =
                Arc::downgrade(self) as std::sync::Weak<dyn ExpirableSlot>;
            TimerWheel::global().schedule(deadline, weak);
        }
    }

    pub fn complete(&self, value: Result<T, AsyncOpError>) -> bool {
        let (waker, cleanup) = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.result.is_some() {
                return false;
            }
            guard.result = Some(value);
            (guard.waker.take(), guard.on_cleanup.take())
        };
        if let Some(cb) = cleanup {
            cb();
        }
        if let Some(w) = waker {
            w.wake();
        }
        true
    }

    pub fn is_completed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .result
            .is_some()
    }
}

impl<T: Send + 'static> ExpirableSlot for AsyncOpSlot<T> {
    fn expire_if_pending(&self) -> bool {
        self.complete(Err(AsyncOpError::TimedOut))
    }
}

/// Future returned by all `*_async` and `capture_screenshot_*` view methods.
pub struct AsyncOpFuture<T: Send + 'static> {
    slot: Arc<AsyncOpSlot<T>>,
}

impl<T: Send + 'static> AsyncOpFuture<T> {
    pub fn ready(value: Result<T, AsyncOpError>) -> Self {
        let slot = AsyncOpSlot::new(None);
        let _ = slot.complete(value);
        Self { slot }
    }

    pub fn from_slot(slot: Arc<AsyncOpSlot<T>>) -> Self {
        Self { slot }
    }

    /// Maps the success value lazily inside a combinator future.
    pub fn map_ok<U: Send + 'static>(
        self,
        f: impl FnOnce(T) -> Result<U, AsyncOpError> + Send + Unpin + 'static,
    ) -> MappedAsyncOpFuture<T, U, impl FnOnce(T) -> Result<U, AsyncOpError> + Send + Unpin + 'static>
    {
        MappedAsyncOpFuture {
            inner: self,
            mapper: Some(f),
        }
    }
}

impl<T: Send + 'static> Drop for AsyncOpFuture<T> {
    fn drop(&mut self) {
        let cleanup = {
            let mut guard = self
                .slot
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.result.is_none() {
                guard.result = Some(Err(AsyncOpError::Cancelled));
                guard.on_cleanup.take()
            } else {
                None
            }
        };
        if let Some(cb) = cleanup {
            cb();
        }
    }
}

impl<T: Send + 'static> Future for AsyncOpFuture<T> {
    type Output = Result<T, AsyncOpError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let expired = {
            let mut guard = self
                .slot
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(res) = guard.result.take() {
                guard.result = Some(Err(AsyncOpError::Cancelled));
                return Poll::Ready(res);
            }
            if let Some(deadline) = self.slot.deadline
                && Instant::now() >= deadline
            {
                true
            } else {
                guard.waker = Some(cx.waker().clone());
                false
            }
        };
        if expired {
            let _ = self.slot.complete(Err(AsyncOpError::TimedOut));
            let mut guard = self
                .slot
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(res) = guard.result.take() {
                guard.result = Some(Err(AsyncOpError::Cancelled));
                return Poll::Ready(res);
            }
        }
        Poll::Pending
    }
}

/// Helper combinator transforming the `Ok` value of an [`AsyncOpFuture`] without external futures crates.
pub struct MappedAsyncOpFuture<T: Send + 'static, U: Send + 'static, F>
where
    F: FnOnce(T) -> Result<U, AsyncOpError> + Unpin,
{
    inner: AsyncOpFuture<T>,
    mapper: Option<F>,
}

impl<T: Send + 'static, U: Send + 'static, F> Unpin for MappedAsyncOpFuture<T, U, F> where
    F: FnOnce(T) -> Result<U, AsyncOpError> + Unpin
{
}

impl<T: Send + 'static, U: Send + 'static, F> Future for MappedAsyncOpFuture<T, U, F>
where
    F: FnOnce(T) -> Result<U, AsyncOpError> + Unpin,
{
    type Output = Result<U, AsyncOpError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.inner).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(val)) => {
                let Some(mapper) = self.mapper.take() else {
                    return Poll::Ready(Err(AsyncOpError::Cancelled));
                };
                Poll::Ready(mapper(val))
            }
            Poll::Ready(Err(err)) => {
                self.mapper.take();
                Poll::Ready(Err(err))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bounded per-view AsyncOpLedger
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum InitialLoadState {
    Pending,
    Completed(InitialLoadResult),
    Failed { error_code: i32 },
}

struct LedgerInner {
    retired: bool,
    initial_load: InitialLoadState,
    next_load_id: u64,
    next_text_id: u64,
    next_shot_id: u64,
    pending_evals: BTreeMap<u64, Arc<AsyncOpSlot<Option<String>>>>,
    pending_loads: BTreeMap<u64, Arc<AsyncOpSlot<InitialLoadResult>>>,
    pending_texts: BTreeMap<u64, Arc<AsyncOpSlot<String>>>,
    pending_screenshots: BTreeMap<u64, Arc<AsyncOpSlot<Vec<u8>>>>,
}

/// Bounded per-view tracker coordinating async JS evals, frame text/source,
/// initial load completion, and screenshots across retire/close/timeouts.
pub struct AsyncOpLedger {
    inner: Mutex<LedgerInner>,
}

impl AsyncOpLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(LedgerInner {
                retired: false,
                initial_load: InitialLoadState::Pending,
                next_load_id: 1,
                next_text_id: 1,
                next_shot_id: 1,
                pending_evals: BTreeMap::new(),
                pending_loads: BTreeMap::new(),
                pending_texts: BTreeMap::new(),
                pending_screenshots: BTreeMap::new(),
            }),
        })
    }

    fn prune_u64<T: Send + 'static>(map: &mut BTreeMap<u64, Arc<AsyncOpSlot<T>>>) {
        map.retain(|_, slot| !slot.is_completed());
    }

    pub fn begin_eval(
        self: &Arc<Self>,
        id: EvalRequestId,
        timeout: Option<Duration>,
    ) -> Result<AsyncOpFuture<Option<String>>, AsyncOpError> {
        let seq = id.sequence;
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.retired {
            return Err(AsyncOpError::Closed);
        }
        Self::prune_u64(&mut guard.pending_evals);
        if guard.pending_evals.len() >= MAX_PENDING_ASYNC_OPS {
            return Err(AsyncOpError::QueueFull);
        }
        let slot = AsyncOpSlot::new(deadline);
        let weak_ledger = Arc::downgrade(self);
        slot.set_cleanup(move || {
            if let Some(ledger) = weak_ledger.upgrade() {
                ledger
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_evals
                    .remove(&seq);
            }
        });
        guard.pending_evals.insert(seq, Arc::clone(&slot));
        drop(guard);
        slot.arm_timer();
        Ok(AsyncOpFuture::from_slot(slot))
    }

    pub fn fail_eval(&self, id: EvalRequestId, err: AsyncOpError) {
        let slot = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_evals
            .remove(&id.sequence);
        if let Some(slot) = slot {
            let _ = slot.complete(Err(err));
        }
    }

    pub fn on_eval_completed(&self, id: EvalRequestId, outcome: &EvalOutcome) -> bool {
        let slot = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_evals
            .remove(&id.sequence);
        let Some(slot) = slot else {
            return false;
        };
        let mapped = match outcome {
            EvalOutcome::Value(json) => {
                if json == "null" {
                    Ok(None)
                } else {
                    Ok(Some(json.clone()))
                }
            }
            EvalOutcome::Err(err) => Err(AsyncOpError::from(err.clone())),
        };
        let _ = slot.complete(mapped);
        true
    }

    pub fn wait_for_initial_load(
        self: &Arc<Self>,
        already_loaded_hint: Option<InitialLoadResult>,
        timeout: Option<Duration>,
    ) -> AsyncOpFuture<InitialLoadResult> {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.retired {
            return AsyncOpFuture::ready(Err(AsyncOpError::Closed));
        }
        match &guard.initial_load {
            InitialLoadState::Completed(res) => return AsyncOpFuture::ready(Ok(res.clone())),
            InitialLoadState::Failed { error_code } => {
                return AsyncOpFuture::ready(Err(AsyncOpError::LoadFailed {
                    error_code: *error_code,
                }));
            }
            InitialLoadState::Pending => {
                if let Some(hint) = already_loaded_hint {
                    guard.initial_load = InitialLoadState::Completed(hint.clone());
                    return AsyncOpFuture::ready(Ok(hint));
                }
            }
        }
        Self::prune_u64(&mut guard.pending_loads);
        if guard.pending_loads.len() >= MAX_PENDING_ASYNC_OPS {
            return AsyncOpFuture::ready(Err(AsyncOpError::QueueFull));
        }
        let id = guard.next_load_id;
        guard.next_load_id = id.saturating_add(1);
        let slot = AsyncOpSlot::new(deadline);
        let weak_ledger = Arc::downgrade(self);
        slot.set_cleanup(move || {
            if let Some(ledger) = weak_ledger.upgrade() {
                ledger
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_loads
                    .remove(&id);
            }
        });
        guard.pending_loads.insert(id, Arc::clone(&slot));
        drop(guard);
        slot.arm_timer();
        AsyncOpFuture::from_slot(slot)
    }

    /// Processes runtime `BrowserEvent` notifications relevant to async evals,
    /// initial page load completion, and teardown.
    pub fn on_browser_event(&self, event: &BrowserEvent) {
        match event {
            BrowserEvent::LoadFinished {
                is_main_frame: true,
                http_status_code,
            } => {
                let res = InitialLoadResult {
                    http_status_code: *http_status_code,
                };
                let waiters = {
                    let mut guard = self
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.retired {
                        return;
                    }
                    guard.initial_load = InitialLoadState::Completed(res.clone());
                    std::mem::take(&mut guard.pending_loads)
                };
                for (_, slot) in waiters {
                    let _ = slot.complete(Ok(res.clone()));
                }
            }
            BrowserEvent::LoadCancelled { .. } => {
                // `ERR_ABORTED` (-3) during redirects/aborts must not fail `wait_for_initial_load_async`.
            }
            BrowserEvent::LoadFailed {
                is_main_frame: true,
                code,
            } => {
                if *code == CEF_ERR_ABORTED {
                    return;
                }
                let waiters = {
                    let mut guard = self
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.retired {
                        return;
                    }
                    guard.initial_load = InitialLoadState::Failed { error_code: *code };
                    std::mem::take(&mut guard.pending_loads)
                };
                for (_, slot) in waiters {
                    let _ = slot.complete(Err(AsyncOpError::LoadFailed { error_code: *code }));
                }
            }
            BrowserEvent::Closed
            | BrowserEvent::CreateFailed(_)
            | BrowserEvent::RendererTerminated { .. } => {
                self.retire_and_cancel_all();
            }
            _ => {}
        }
    }

    pub fn begin_frame_text(
        self: &Arc<Self>,
        timeout: Option<Duration>,
    ) -> Result<(u64, Arc<AsyncOpSlot<String>>, AsyncOpFuture<String>), AsyncOpError> {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.retired {
            return Err(AsyncOpError::Closed);
        }
        Self::prune_u64(&mut guard.pending_texts);
        if guard.pending_texts.len() >= MAX_PENDING_ASYNC_OPS {
            return Err(AsyncOpError::QueueFull);
        }
        let id = guard.next_text_id;
        guard.next_text_id = id.saturating_add(1);
        let slot = AsyncOpSlot::new(deadline);
        let weak_ledger = Arc::downgrade(self);
        slot.set_cleanup(move || {
            if let Some(ledger) = weak_ledger.upgrade() {
                ledger
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_texts
                    .remove(&id);
            }
        });
        guard.pending_texts.insert(id, Arc::clone(&slot));
        drop(guard);
        slot.arm_timer();
        Ok((id, Arc::clone(&slot), AsyncOpFuture::from_slot(slot)))
    }

    pub fn finish_frame_text(&self, id: u64, res: Result<String, FrameTextError>) -> bool {
        let slot = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_texts
            .remove(&id);
        let Some(slot) = slot else {
            return false;
        };
        let _ = slot.complete(res.map_err(AsyncOpError::from));
        true
    }

    pub fn begin_screenshot(
        self: &Arc<Self>,
        timeout: Option<Duration>,
    ) -> Result<(u64, Arc<AsyncOpSlot<Vec<u8>>>, AsyncOpFuture<Vec<u8>>), AsyncOpError> {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.retired {
            return Err(AsyncOpError::Closed);
        }
        Self::prune_u64(&mut guard.pending_screenshots);
        if guard.pending_screenshots.len() >= MAX_PENDING_SCREENSHOTS {
            return Err(AsyncOpError::QueueFull);
        }
        let id = guard.next_shot_id;
        guard.next_shot_id = id.saturating_add(1);
        let slot = AsyncOpSlot::new(deadline);
        let weak_ledger = Arc::downgrade(self);
        slot.set_cleanup(move || {
            if let Some(ledger) = weak_ledger.upgrade() {
                ledger
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_screenshots
                    .remove(&id);
            }
        });
        guard.pending_screenshots.insert(id, Arc::clone(&slot));
        drop(guard);
        slot.arm_timer();
        Ok((id, Arc::clone(&slot), AsyncOpFuture::from_slot(slot)))
    }

    pub fn begin_screenshot_bgra(
        self: &Arc<Self>,
        timeout: Option<Duration>,
    ) -> Result<
        (
            u64,
            Arc<AsyncOpSlot<ScreenshotBgra>>,
            AsyncOpFuture<ScreenshotBgra>,
        ),
        AsyncOpError,
    > {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.retired {
            return Err(AsyncOpError::Closed);
        }
        Self::prune_u64(&mut guard.pending_screenshots);
        if guard.pending_screenshots.len() >= MAX_PENDING_SCREENSHOTS {
            return Err(AsyncOpError::QueueFull);
        }
        let id = guard.next_shot_id;
        guard.next_shot_id = id.saturating_add(1);
        let slot = AsyncOpSlot::new(deadline);
        let bgra_slot = AsyncOpSlot::new(deadline);
        let weak_ledger = Arc::downgrade(self);
        let bgra_weak = Arc::downgrade(&bgra_slot);
        slot.set_cleanup(move || {
            if let Some(bs) = bgra_weak.upgrade() {
                let _ = bs.complete(Err(AsyncOpError::Cancelled));
            }
            if let Some(ledger) = weak_ledger.upgrade() {
                ledger
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_screenshots
                    .remove(&id);
            }
        });
        guard.pending_screenshots.insert(id, slot);
        drop(guard);
        bgra_slot.arm_timer();
        Ok((
            id,
            Arc::clone(&bgra_slot),
            AsyncOpFuture::from_slot(bgra_slot),
        ))
    }

    pub fn complete_tracked_screenshot<T: Send + 'static>(
        &self,
        id: u64,
        slot: &AsyncOpSlot<T>,
        res: Result<T, AsyncOpError>,
    ) -> bool {
        let still_tracked = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_screenshots
            .remove(&id)
            .is_some();
        if !still_tracked {
            return false;
        }
        let _ = slot.complete(res);
        true
    }

    pub fn finish_screenshot(&self, id: u64, res: Result<Vec<u8>, AsyncOpError>) -> bool {
        let slot = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_screenshots
            .remove(&id);
        let Some(slot) = slot else {
            return false;
        };
        let _ = slot.complete(res);
        true
    }

    /// Deterministically cancels every pending eval, load, text, and screenshot waiter.
    pub fn retire_and_cancel_all(&self) {
        let (evals, loads, texts, shots) = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.retired = true;
            (
                std::mem::take(&mut guard.pending_evals),
                std::mem::take(&mut guard.pending_loads),
                std::mem::take(&mut guard.pending_texts),
                std::mem::take(&mut guard.pending_screenshots),
            )
        };
        for (_, slot) in evals {
            let _ = slot.complete(Err(AsyncOpError::Closed));
        }
        for (_, slot) in loads {
            let _ = slot.complete(Err(AsyncOpError::Closed));
        }
        for (_, slot) in texts {
            let _ = slot.complete(Err(AsyncOpError::Closed));
        }
        for (_, slot) in shots {
            let _ = slot.complete(Err(AsyncOpError::Closed));
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_timeouts_for_test(&self) {
        let slots: Vec<Arc<dyn ExpirableSlot>> = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut out: Vec<Arc<dyn ExpirableSlot>> = Vec::new();
            for s in std::mem::take(&mut guard.pending_evals).into_values() {
                out.push(s);
            }
            for s in std::mem::take(&mut guard.pending_loads).into_values() {
                out.push(s);
            }
            for s in std::mem::take(&mut guard.pending_texts).into_values() {
                out.push(s);
            }
            for s in std::mem::take(&mut guard.pending_screenshots).into_values() {
                out.push(s);
            }
            out
        };
        for slot in slots {
            let _ = slot.expire_if_pending();
        }
    }
}

// ---------------------------------------------------------------------------
// Screenshot codecs & helpers (Task 3.2)
// ---------------------------------------------------------------------------

/// Encodes compact premultiplied/pixel BGRA8 bytes into PNG bytes using `image`'s locked PNG encoder.
pub fn encode_bgra_to_png(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, AsyncOpError> {
    if width == 0 || height == 0 || width > MAX_NATIVE_BOUND || height > MAX_NATIVE_BOUND {
        return Err(AsyncOpError::InvalidRequest(
            "invalid screenshot dimensions".into(),
        ));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| AsyncOpError::InvalidRequest("screenshot byte size overflow".into()))?;
    if bgra.len() != expected {
        return Err(AsyncOpError::InvalidRequest(format!(
            "expected {expected} BGRA bytes, got {}",
            bgra.len()
        )));
    }
    let mut rgba = Vec::with_capacity(expected);
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    let mut out = Vec::new();
    let encoder = PngEncoder::new(&mut out);
    encoder
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|err| AsyncOpError::EncodeFailed(err.to_string()))?;
    Ok(out)
}

/// Decodes PNG bytes into compact BGRA8 pixels using `image`'s locked PNG decoder.
pub fn decode_png_to_bgra(png: &[u8]) -> Result<ScreenshotBgra, AsyncOpError> {
    let decoder = PngDecoder::new(std::io::Cursor::new(png))
        .map_err(|e| AsyncOpError::DecodeFailed(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_NATIVE_BOUND || height > MAX_NATIVE_BOUND {
        return Err(AsyncOpError::DecodeFailed(
            "PNG dimensions exceed bounds".into(),
        ));
    }
    let rgba = DynamicImage::from_decoder(decoder)
        .map_err(|e| AsyncOpError::DecodeFailed(e.to_string()))?
        .into_rgba8();
    let mut bgra = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for px in rgba.as_raw().chunks_exact(4) {
        bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    Ok(ScreenshotBgra {
        width,
        height,
        bgra,
    })
}

/// Zero-dependency RFC-4648 base64 decoder.
pub fn decode_base64(input: &str) -> Result<Vec<u8>, AsyncOpError> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.len() % 4 != 0 {
        return Err(AsyncOpError::DecodeFailed("invalid base64 length".into()));
    }
    if clean.is_empty() {
        return Ok(Vec::new());
    }
    let val = |b: u8| -> Result<u8, AsyncOpError> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a' + 26),
            b'0'..=b'9' => Ok(b - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(AsyncOpError::DecodeFailed(
                "invalid base64 character".into(),
            )),
        }
    };
    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    for (idx, chunk) in clean.chunks_exact(4).enumerate() {
        let is_last = (idx + 1) * 4 == clean.len();
        if chunk[0] == b'=' || chunk[1] == b'=' {
            return Err(AsyncOpError::DecodeFailed(
                "unexpected base64 padding".into(),
            ));
        }
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' {
                return Err(AsyncOpError::DecodeFailed(
                    "malformed base64 padding".into(),
                ));
            }
            out.push((a << 2) | (b >> 4));
        } else if chunk[3] == b'=' {
            if !is_last {
                return Err(AsyncOpError::DecodeFailed(
                    "malformed base64 padding".into(),
                ));
            }
            let c = val(chunk[2])?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        } else {
            let c = val(chunk[2])?;
            let d = val(chunk[3])?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

/// Parses a CDP `Page.captureScreenshot` response JSON payload (`{"data":"..."}`) into raw PNG bytes.
pub fn extract_cdp_screenshot_png(result_json: &str) -> Result<Vec<u8>, AsyncOpError> {
    let parsed = parse_json(result_json).map_err(AsyncOpError::DecodeFailed)?;
    let b64 = parsed
        .obj_get("data")
        .and_then(JsonVal::as_str)
        .ok_or_else(|| {
            AsyncOpError::DecodeFailed("CDP screenshot response missing 'data'".into())
        })?;
    decode_base64(b64)
}

/// Captures compact BGRA pixels from an OSR `OwnedFrame`, returning [`AsyncOpFuture<ScreenshotBgra>`].
pub fn capture_osr_bgra(
    ledger: &Arc<AsyncOpLedger>,
    bridge: Option<&RuntimeBrowserBridge>,
    frame: Option<Arc<OwnedFrame>>,
    timeout: Option<Duration>,
) -> AsyncOpFuture<ScreenshotBgra> {
    let Some(frame) = frame else {
        return AsyncOpFuture::ready(Err(AsyncOpError::NotReady));
    };
    let width = frame.info.width;
    let height = frame.info.height;
    match &frame.payload {
        FramePayload::CpuBgra { buffer, .. } => AsyncOpFuture::ready(Ok(ScreenshotBgra {
            width,
            height,
            bgra: buffer.as_slice().to_vec(),
        })),
        FramePayload::GpuSharedTexture(shared) => {
            let Some(bridge) = bridge else {
                return AsyncOpFuture::ready(Err(AsyncOpError::BridgeUnavailable));
            };
            let timeout_dur = timeout.or(Some(DEFAULT_ASYNC_OP_TIMEOUT));
            let (id, slot, fut) = match ledger.begin_screenshot_bgra(timeout_dur) {
                Ok(v) => v,
                Err(err) => return AsyncOpFuture::ready(Err(err)),
            };
            let ledger_cb = Arc::clone(ledger);
            bridge.readback_shared_texture(shared.clone(), move |res| {
                let mapped = res
                    .map(|bgra| ScreenshotBgra {
                        width,
                        height,
                        bgra,
                    })
                    .map_err(AsyncOpError::Runtime);
                let _ = ledger_cb.complete_tracked_screenshot(id, &slot, mapped);
            });
            fut
        }
    }
}

/// Captures PNG-encoded screenshot bytes from an OSR `OwnedFrame`, returning [`AsyncOpFuture<Vec<u8>>`].
pub fn capture_osr_png(
    ledger: &Arc<AsyncOpLedger>,
    bridge: Option<&RuntimeBrowserBridge>,
    frame: Option<Arc<OwnedFrame>>,
    timeout: Option<Duration>,
) -> AsyncOpFuture<Vec<u8>> {
    let Some(frame) = frame else {
        return AsyncOpFuture::ready(Err(AsyncOpError::NotReady));
    };
    let width = frame.info.width;
    let height = frame.info.height;
    match &frame.payload {
        FramePayload::CpuBgra { buffer, .. } => {
            AsyncOpFuture::ready(encode_bgra_to_png(width, height, buffer.as_slice()))
        }
        FramePayload::GpuSharedTexture(shared) => {
            let Some(bridge) = bridge else {
                return AsyncOpFuture::ready(Err(AsyncOpError::BridgeUnavailable));
            };
            let timeout_dur = timeout.or(Some(DEFAULT_ASYNC_OP_TIMEOUT));
            let (id, slot, fut) = match ledger.begin_screenshot(timeout_dur) {
                Ok(v) => v,
                Err(err) => return AsyncOpFuture::ready(Err(err)),
            };
            let ledger_cb = Arc::clone(ledger);
            bridge.readback_shared_texture(shared.clone(), move |res| {
                let mapped = res
                    .map_err(AsyncOpError::Runtime)
                    .and_then(|bgra| encode_bgra_to_png(width, height, &bgra));
                if !ledger_cb.finish_screenshot(id, mapped.clone()) {
                    let _ = slot.complete(mapped);
                }
            });
            fut
        }
    }
}

/// Requests `Frame::source` or `Frame::text` cleanly through [`RuntimeBrowserBridge`].
pub fn request_frame_text_async(
    ledger: &Arc<AsyncOpLedger>,
    bridge: Option<&RuntimeBrowserBridge>,
    kind: rust_cef_runtime::FrameTextKind,
    scope: rust_cef_core::js_bridge::JsFrameScope,
    timeout: Option<Duration>,
) -> AsyncOpFuture<String> {
    let Some(bridge) = bridge else {
        return AsyncOpFuture::ready(Err(AsyncOpError::BridgeUnavailable));
    };
    let timeout_dur = timeout.or(Some(DEFAULT_ASYNC_OP_TIMEOUT));
    let (id, slot, fut) = match ledger.begin_frame_text(timeout_dur) {
        Ok(v) => v,
        Err(err) => return AsyncOpFuture::ready(Err(err)),
    };
    let ledger_cb = Arc::clone(ledger);
    bridge.request_frame_text(
        kind,
        scope,
        rust_cef_runtime::DEFAULT_FRAME_TEXT_MAX_BYTES,
        move |res| {
            let mapped = res.map_err(AsyncOpError::from);
            if !ledger_cb.finish_frame_text(id, res_into_text(mapped.clone())) {
                let _ = slot.complete(mapped);
            }
        },
    );
    fut
}

fn res_into_text(res: Result<String, AsyncOpError>) -> Result<String, FrameTextError> {
    res.map_err(|err| match err {
        AsyncOpError::Closed => FrameTextError::Closed,
        AsyncOpError::NotReady => FrameTextError::NotReady,
        AsyncOpError::PolicyDenied => FrameTextError::PolicyDenied,
        AsyncOpError::FrameNotFound => FrameTextError::FrameNotFound,
        AsyncOpError::BudgetExceeded => FrameTextError::BudgetExceeded,
        AsyncOpError::DecodeFailed(_) => FrameTextError::InvalidText,
        _ => FrameTextError::Cancelled,
    })
}

enum ScreenshotDelivery {
    Png {
        shot_id: u64,
    },
    Bgra {
        shot_id: u64,
        slot: Arc<AsyncOpSlot<ScreenshotBgra>>,
    },
}

struct TransientCdpObserver {
    ledger: Arc<AsyncOpLedger>,
    delivery: ScreenshotDelivery,
    bridge: RuntimeBrowserBridge,
    token: Mutex<Option<u64>>,
    expected_method_id: Mutex<Option<CdpMethodId>>,
}

impl TransientCdpObserver {
    fn detach_once(&self) {
        if let Some(tok) = self
            .token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            self.bridge.detach_cdp_screenshot(tok);
        }
    }
}

impl CdpObserver for TransientCdpObserver {
    fn on_method_result(&self, result: CdpMethodResult) {
        let expected = *self
            .expected_method_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(exp) = expected
            && result.id != exp
        {
            return;
        }
        let png_res = if result.success {
            extract_cdp_screenshot_png(&result.result_json)
        } else {
            Err(AsyncOpError::HostCapability(
                HostCapabilityError::OperationFailed,
            ))
        };
        self.detach_once();
        match &self.delivery {
            ScreenshotDelivery::Png { shot_id } => {
                let _ = self.ledger.finish_screenshot(*shot_id, png_res);
            }
            ScreenshotDelivery::Bgra { shot_id, slot } => {
                let bgra_res = png_res.and_then(|png| decode_png_to_bgra(&png));
                let _ = self
                    .ledger
                    .complete_tracked_screenshot(*shot_id, slot, bgra_res);
            }
        }
    }

    fn on_event(&self, _event: CdpEvent) {}
}

/// Starts a transient opt-in CDP `Page.captureScreenshot` operation on a `NativeCefBrowser`.
pub fn capture_native_cdp_png(
    ledger: &Arc<AsyncOpLedger>,
    bridge: Option<&RuntimeBrowserBridge>,
    timeout: Option<Duration>,
) -> AsyncOpFuture<Vec<u8>> {
    let Some(bridge) = bridge else {
        return AsyncOpFuture::ready(Err(AsyncOpError::BridgeUnavailable));
    };
    let timeout_dur = timeout.or(Some(DEFAULT_ASYNC_OP_TIMEOUT));
    let (shot_id, slot, fut) = match ledger.begin_screenshot(timeout_dur) {
        Ok(v) => v,
        Err(err) => return AsyncOpFuture::ready(Err(err)),
    };
    let obs = Arc::new(TransientCdpObserver {
        ledger: Arc::clone(ledger),
        delivery: ScreenshotDelivery::Png { shot_id },
        bridge: bridge.clone(),
        token: Mutex::new(None),
        expected_method_id: Mutex::new(None),
    });
    let obs_trait: Arc<dyn CdpObserver> = obs.clone();
    match bridge.start_cdp_screenshot(Some(r#"{"format":"png"}"#.to_owned()), obs_trait) {
        Ok((tok, method_id)) => {
            *obs.token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tok);
            *obs.expected_method_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(method_id);
            let obs_cleanup = Arc::clone(&obs);
            let ledger_cleanup = Arc::downgrade(ledger);
            slot.set_cleanup(move || {
                obs_cleanup.detach_once();
                if let Some(l) = ledger_cleanup.upgrade() {
                    l.inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pending_screenshots
                        .remove(&shot_id);
                }
            });
            fut
        }
        Err(err) => {
            let mapped = AsyncOpError::from(err);
            let _ = ledger.finish_screenshot(shot_id, Err(mapped.clone()));
            AsyncOpFuture::ready(Err(mapped))
        }
    }
}

/// Starts a transient opt-in CDP `Page.captureScreenshot` operation on a `NativeCefBrowser`,
/// decoding the returned PNG bytes into [`ScreenshotBgra`].
pub fn capture_native_cdp_bgra(
    ledger: &Arc<AsyncOpLedger>,
    bridge: Option<&RuntimeBrowserBridge>,
    timeout: Option<Duration>,
) -> AsyncOpFuture<ScreenshotBgra> {
    let Some(bridge) = bridge else {
        return AsyncOpFuture::ready(Err(AsyncOpError::BridgeUnavailable));
    };
    let timeout_dur = timeout.or(Some(DEFAULT_ASYNC_OP_TIMEOUT));
    let (shot_id, slot, fut) = match ledger.begin_screenshot_bgra(timeout_dur) {
        Ok(v) => v,
        Err(err) => return AsyncOpFuture::ready(Err(err)),
    };
    let obs = Arc::new(TransientCdpObserver {
        ledger: Arc::clone(ledger),
        delivery: ScreenshotDelivery::Bgra {
            shot_id,
            slot: Arc::clone(&slot),
        },
        bridge: bridge.clone(),
        token: Mutex::new(None),
        expected_method_id: Mutex::new(None),
    });
    let obs_trait: Arc<dyn CdpObserver> = obs.clone();
    match bridge.start_cdp_screenshot(Some(r#"{"format":"png"}"#.to_owned()), obs_trait) {
        Ok((tok, method_id)) => {
            *obs.token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tok);
            *obs.expected_method_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(method_id);
            let obs_cleanup = Arc::clone(&obs);
            let ledger_cleanup = Arc::downgrade(ledger);
            slot.set_cleanup(move || {
                obs_cleanup.detach_once();
                if let Some(l) = ledger_cleanup.upgrade() {
                    l.inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pending_screenshots
                        .remove(&shot_id);
                }
            });
            fut
        }
        Err(err) => {
            let mapped = AsyncOpError::from(err);
            let _ = ledger.complete_tracked_screenshot(shot_id, &slot, Err(mapped.clone()));
            AsyncOpFuture::ready(Err(mapped))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn test_eval_id(sequence: u64) -> EvalRequestId {
        EvalRequestId {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            sequence,
        }
    }

    fn noop_waker() -> Waker {
        fn no_op(_: *const ()) {}
        fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    fn block_on_ready<F: Future>(mut fut: F) -> F::Output {
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("expected future to be immediately ready"),
        }
    }

    fn poll_once<F: Future>(fut: &mut Pin<&mut F>) -> Poll<F::Output> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    #[test]
    fn async_ops_handle_eval_timeout_close_err_aborted_and_late_arrival() {
        let ledger = AsyncOpLedger::new();
        let eval_id = test_eval_id(10);
        let mut eval_fut = ledger
            .begin_eval(eval_id, Some(Duration::from_millis(20)))
            .expect("admit eval");
        let mut pinned = unsafe { Pin::new_unchecked(&mut eval_fut) };
        assert!(matches!(poll_once(&mut pinned), Poll::Pending));

        // Simulate deterministic timeout expiration, then late eval completion suppressed cleanly.
        ledger.expire_timeouts_for_test();
        assert_eq!(
            poll_once(&mut pinned),
            Poll::Ready(Err(AsyncOpError::TimedOut))
        );
        assert!(!ledger.on_eval_completed(eval_id, &EvalOutcome::Value("42".into())));

        // Wait-for-initial-load ignores ERR_ABORTED (-3) and LoadCancelled, resolves on main LoadFinished.
        let mut load_fut = ledger.wait_for_initial_load(None, Some(Duration::from_secs(5)));
        let mut pinned_load = unsafe { Pin::new_unchecked(&mut load_fut) };
        assert!(matches!(poll_once(&mut pinned_load), Poll::Pending));
        ledger.on_browser_event(&BrowserEvent::LoadCancelled {
            is_main_frame: true,
            code: CEF_ERR_ABORTED,
        });
        ledger.on_browser_event(&BrowserEvent::LoadFailed {
            is_main_frame: true,
            code: CEF_ERR_ABORTED,
        });
        assert!(matches!(poll_once(&mut pinned_load), Poll::Pending));
        ledger.on_browser_event(&BrowserEvent::LoadFinished {
            is_main_frame: true,
            http_status_code: 200,
        });
        assert_eq!(
            poll_once(&mut pinned_load),
            Poll::Ready(Ok(InitialLoadResult {
                http_status_code: 200
            }))
        );

        // Close cancels all pending ops deterministically.
        let eval_id2 = test_eval_id(11);
        let eval_fut2 = ledger.begin_eval(eval_id2, None).expect("admit eval 2");
        let (_, _, text_fut) = ledger.begin_frame_text(None).expect("admit text");
        ledger.retire_and_cancel_all();
        assert_eq!(block_on_ready(eval_fut2), Err(AsyncOpError::Closed));
        assert_eq!(block_on_ready(text_fut), Err(AsyncOpError::Closed));
        assert!(matches!(
            ledger.begin_eval(test_eval_id(12), None),
            Err(AsyncOpError::Closed)
        ));
    }

    #[test]
    fn async_ops_enforce_bounded_queue_limits() {
        let ledger = AsyncOpLedger::new();
        let mut held = Vec::new();
        for idx in 0..MAX_PENDING_ASYNC_OPS {
            held.push(
                ledger
                    .begin_eval(test_eval_id((idx + 1) as u64), None)
                    .expect("within queue bound"),
            );
        }
        assert!(matches!(
            ledger.begin_eval(test_eval_id(9999), None),
            Err(AsyncOpError::QueueFull)
        ));
        drop(held);
    }

    #[test]
    fn screenshot_png_base64_and_cdp_extraction_roundtrip() {
        let width = 2u32;
        let height = 2u32;
        // BGRA pixels
        let bgra = vec![
            0x10, 0x20, 0x30, 0xff, // px(0,0)
            0x40, 0x50, 0x60, 0x80, // px(1,0)
            0x70, 0x80, 0x90, 0xff, // px(0,1)
            0xa0, 0xb0, 0xc0, 0x00, // px(1,1)
        ];
        let png = encode_bgra_to_png(width, height, &bgra).expect("encode PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded = decode_png_to_bgra(&png).expect("decode PNG");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.bgra, bgra);

        // RFC-4648 base64 decode & CDP JSON wrapper
        assert_eq!(decode_base64("SGVsbG8=").expect("b64"), b"Hello");
        assert_eq!(decode_base64("TWFu").expect("b64"), b"Man");
        assert!(decode_base64("abc").is_err());

        let cdp_json =
            JsonVal::Obj(vec![("data".into(), JsonVal::Str("SGVsbG8=".into()))]).to_json_string();
        assert_eq!(
            extract_cdp_screenshot_png(&cdp_json).expect("cdp"),
            b"Hello"
        );
    }

    #[test]
    fn screenshot_osr_cpu_frame_and_error_guards_work() {
        let ledger = AsyncOpLedger::new();
        assert_eq!(
            block_on_ready(capture_osr_bgra(&ledger, None, None, None)),
            Err(AsyncOpError::NotReady)
        );
        assert_eq!(
            block_on_ready(capture_native_cdp_png(&ledger, None, None)),
            Err(AsyncOpError::BridgeUnavailable)
        );

        let bgra = vec![10, 20, 30, 255, 40, 50, 60, 255];
        let budget = rust_cef_core::frame_budget::FrameBudget::new(64);
        let mut pixels = budget.allocate(bgra.len()).expect("budget alloc");
        pixels.as_mut_slice().copy_from_slice(&bgra);
        let info = rust_cef_core::ports::FrameInfo {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            surface: rust_cef_core::ports::SurfaceKind::View,
            surface_generation: 1,
            sequence: 1,
            width: 2,
            height: 1,
            format: rust_cef_core::ports::PixelFormat::PremultipliedBgra8,
        };
        let frame = Arc::new(OwnedFrame::from_cpu_pixels(info, pixels.freeze()));
        let shot = block_on_ready(capture_osr_bgra(
            &ledger,
            None,
            Some(Arc::clone(&frame)),
            None,
        ))
        .expect("osr bgra");
        assert_eq!(shot.width, 2);
        assert_eq!(shot.height, 1);
        assert_eq!(shot.bgra, bgra);

        let png =
            block_on_ready(capture_osr_png(&ledger, None, Some(frame), None)).expect("osr png");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(decode_png_to_bgra(&png).expect("decode").bgra, bgra);
    }
}
