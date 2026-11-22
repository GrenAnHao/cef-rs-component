//! Per-Browser CPU pixel budget for the two surface slots, every in-flight
//! frame and capture/presentation scratch.
//!
//! The budget counts **CPU pixel capacity** (`len` bytes reserved before the
//! allocation), not allocator headers, process RSS, GPU staging or the GPUI
//! atlas. A single frame still has its own size cap (`frame_store::frame_len`),
//! but that check cannot replace the total: two latest slots plus in-flight
//! captures and scratch all draw from the same ledger.
//!
//! Ownership is deliberately narrow. A [`FrameBuffer`] can be frozen into an
//! immutable [`BudgetedPixels`] whose private `Box<[u8]>` is never exposed and
//! cannot be cloned. The only mutable exit is [`FrameBuffer::try_into_owner`],
//! which hands the exclusive `Vec<u8>` to a real owner such as GPUI's
//! `RenderImage`. The ledger keeps a strong `Arc<T>` of that owner so it can
//! prove, via `Arc::try_unwrap`, that the owner is truly gone before refunding;
//! a weak strong-count of zero would race with `try_unwrap` moving `T` out and
//! with `T`'s fields still being destructed.

use std::{
    alloc::{Layout, alloc_zeroed},
    fmt,
    ops::Deref,
    ptr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

/// 256 MiB of total CPU pixel capacity per Browser, shared by the VIEW and
/// POPUP latest slots, all in-flight frames and capture/presentation scratch.
/// It is not 128 MiB per surface: one large frame may consume the whole total,
/// and the next allocation is then an observable `BudgetExceeded` rejection.
pub const DEFAULT_CPU_FRAME_BUDGET: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// A surface was configured or captured with zero/overflowing geometry.
    InvalidDimensions,
    /// A frame belongs to another Browser, surface identity or budget ledger.
    WrongIdentity,
    /// A capture token or frame was produced under an older surface generation.
    StaleGeneration,
    /// A sequence was zero, duplicated or not newer than the published frame.
    OutOfOrder,
    /// The surface is not visible (a popup cannot be shown while the view is hidden).
    Hidden,
    /// The Browser mailbox has been permanently closed.
    Closed,
    /// The allocation would exceed this Browser's total CPU pixel budget.
    BudgetExceeded,
    /// The platform allocator refused the exact reservation.
    AllocationFailed,
    /// Per-surface frame sequences are exhausted; they never wrap.
    SequenceExhausted,
    /// Per-surface generations are exhausted; they never wrap.
    GenerationExhausted,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidDimensions => "invalid frame dimensions",
            Self::WrongIdentity => "frame identity does not match this mailbox",
            Self::StaleGeneration => "frame belongs to a stale surface generation",
            Self::OutOfOrder => "frame sequence is zero, duplicated or out of order",
            Self::Hidden => "surface is not visible",
            Self::Closed => "frame mailbox is closed",
            Self::BudgetExceeded => "CPU frame budget exceeded",
            Self::AllocationFailed => "pixel allocation failed",
            Self::SequenceExhausted => "frame sequence exhausted",
            Self::GenerationExhausted => "surface generation exhausted",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FrameError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetStats {
    pub limit: usize,
    /// Reserved CPU pixel bytes, including retained owners that are not yet
    /// provably dropped. It may lag a completed destructor until `stats`/
    /// `allocate` reaps it; it must never be refunded early.
    pub used: usize,
    pub peak: usize,
    pub rejections: u64,
}

struct LedgerState {
    used: usize,
    peak: usize,
    rejections: u64,
    retained: Vec<Box<dyn RetainedOwner>>,
}

struct Ledger {
    limit: usize,
    state: Mutex<LedgerState>,
}

/// A clone shares the very same ledger; it is not a second budget.
#[derive(Clone)]
pub struct FrameBudget {
    ledger: Arc<Ledger>,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new(DEFAULT_CPU_FRAME_BUDGET)
    }
}

impl FrameBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            ledger: Arc::new(Ledger {
                limit,
                state: Mutex::new(LedgerState {
                    used: 0,
                    peak: 0,
                    rejections: 0,
                    retained: Vec::new(),
                }),
            }),
        }
    }

    pub fn stats(&self) -> BudgetStats {
        self.ledger.reap();
        let state = self.ledger.lock();
        BudgetStats {
            limit: self.ledger.limit,
            used: state.used,
            peak: state.peak,
            rejections: state.rejections,
        }
    }

    /// Reserves the exact byte capacity before asking the allocator for it.
    pub fn allocate(&self, len: usize) -> Result<FrameBuffer, FrameError> {
        self.ledger.reap();
        let lease = self.ledger.reserve(len)?;
        match zeroed_pixels(len) {
            Ok(pixels) => Ok(FrameBuffer { pixels, lease }),
            // Dropping the lease after this match refunds the reservation.
            Err(error) => Err(error),
        }
    }
}

impl Ledger {
    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reserve(self: &Arc<Self>, len: usize) -> Result<Lease, FrameError> {
        let mut state = self.lock();
        let Some(next) = state.used.checked_add(len) else {
            state.rejections = state.rejections.saturating_add(1);
            return Err(FrameError::BudgetExceeded);
        };
        if next > self.limit {
            state.rejections = state.rejections.saturating_add(1);
            return Err(FrameError::BudgetExceeded);
        }
        state.used = next;
        state.peak = state.peak.max(next);
        Ok(Lease {
            ledger: self.clone(),
            bytes: len,
            charged: true,
        })
    }

    /// Adds a strong owner record without changing `used`: the caller already
    /// holds a charged lease for exactly `bytes`.
    fn retain<T: Send + Sync + 'static>(self: &Arc<Self>, owner: Arc<T>, bytes: usize) {
        let mut state = self.lock();
        state.retained.push(Box::new(RetainedArc {
            owner: Some(owner),
            bytes,
        }));
    }

    /// Takes candidates under the lock, attempts `Arc::try_unwrap` outside it,
    /// and only subtracts bytes after the unique `T` has actually been dropped.
    ///
    /// While a candidate is being inspected, `used` still counts it. That may
    /// over-count temporarily; it never under-counts. No opaque host destructor
    /// runs while the budget lock is held.
    fn reap(&self) {
        let candidates = {
            let mut state = self.lock();
            if state.retained.is_empty() {
                return;
            }
            std::mem::take(&mut state.retained)
        };
        let mut keep: Vec<Box<dyn RetainedOwner>> = Vec::with_capacity(candidates.len());
        let mut refund = 0usize;
        for mut candidate in candidates {
            if let Some(retired) = candidate.try_retire() {
                let bytes = retired.bytes();
                // `drop` runs the real owner's destructor here, outside every
                // budget and mailbox lock.
                drop(retired);
                refund = refund.saturating_add(bytes);
            } else {
                keep.push(candidate);
            }
        }
        if refund != 0 {
            let mut state = self.lock();
            state.used = state.used.saturating_sub(refund);
            state.retained.extend(keep);
        } else if !keep.is_empty() {
            let mut state = self.lock();
            state.retained.extend(keep);
        }
    }
}

struct Lease {
    ledger: Arc<Ledger>,
    bytes: usize,
    charged: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.charged {
            return;
        }
        let mut state = self.ledger.lock();
        state.used = state.used.saturating_sub(self.bytes);
    }
}

/// Private type-erased probe. Only this module implements it; hosts cannot
/// supply arbitrary callbacks that might refund without destroying the owner.
trait RetainedOwner: Send + Sync {
    /// Returns a value box whose drop destroys the unique owner, or `None`
    /// while other strong references still exist.
    fn try_retire(&mut self) -> Option<Box<dyn RetiredOwner>>;
}

trait RetiredOwner: Send + Sync {
    fn bytes(&self) -> usize;
}

struct RetainedArc<T> {
    owner: Option<Arc<T>>,
    bytes: usize,
}

impl<T: Send + Sync + 'static> RetainedOwner for RetainedArc<T> {
    fn try_retire(&mut self) -> Option<Box<dyn RetiredOwner>> {
        let owner = self.owner.take()?;
        match Arc::try_unwrap(owner) {
            Ok(owner) => Some(Box::new(RetiredValue {
                _owner: owner,
                bytes: self.bytes,
            })),
            Err(owner) => {
                // Restore the record; the charge stays in `used`.
                self.owner = Some(owner);
                None
            }
        }
    }
}

/// Dropping this box destroys the unique owner value and all of its fields.
struct RetiredValue<T> {
    // Kept solely for synchronous destruction before refunding the allocation.
    _owner: T,
    bytes: usize,
}

impl<T: Send + Sync + 'static> RetiredOwner for RetiredValue<T> {
    fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Mutable, budget-charged pixel capacity. Not `Clone`: one charge, one owner.
pub struct FrameBuffer {
    pixels: Box<[u8]>,
    lease: Lease,
}

impl fmt::Debug for FrameBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameBuffer")
            .field("len", &self.pixels.len())
            .finish()
    }
}

impl FrameBuffer {
    pub fn as_slice(&self) -> &[u8] {
        &self.pixels
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    pub fn len(&self) -> usize {
        self.pixels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    /// Freezes the bytes into an immutable, still-charged pixel allocation.
    pub fn freeze(self) -> Arc<BudgetedPixels> {
        Arc::new(BudgetedPixels {
            // Field order matters: pixels drop before the lease refunds bytes.
            pixels: self.pixels,
            lease: self.lease,
        })
    }

    /// Hands the exclusive `Vec<u8>` to a trusted external owner adapter.
    ///
    /// Contract for `build`, which the core cannot reflectively prove:
    ///
    /// * On success the returned `T` must actually own the moved `Vec`.
    /// * On failure (`Err(FrameError)`) the closure must have truly released
    ///   the `Vec` before returning: no stashing, `mem::forget`, copy or
    ///   retention outside `T`.
    /// * `T` must not retain this `FrameBudget`: that would form an `Arc`
    ///   cycle and prevent retirement.
    ///
    /// The error type is deliberately `FrameError` (not a generic `E`) so a
    /// fallible adapter cannot hand the charged `Vec` back through `Err`. The
    /// budget strictly covers pixels this crate/Runtime creates through audited
    /// presentation adapters; it is not a claim to police arbitrary host
    /// allocations outside this API.
    ///
    /// `build` runs outside every budget lock. The lease stays charged while
    /// the owner is constructed and is transferred to a ledger-held strong
    /// `Arc<T>` only after construction succeeds. On failure the lease refunds
    /// immediately (the moved `Vec` was dropped by the closure); a panic also
    /// unwinds without double charging.
    pub fn try_into_owner<T>(
        self,
        build: impl FnOnce(Vec<u8>) -> Result<Arc<T>, FrameError>,
    ) -> Result<Arc<T>, FrameError>
    where
        T: Send + Sync + 'static,
    {
        let FrameBuffer { pixels, mut lease } = self;
        let bytes = pixels.len();
        let owner = build(pixels.into_vec())?;
        lease.ledger.retain(owner.clone(), bytes);
        // The ledger now owns a strong reference carrying the same charge, so
        // this lease must not refund a second time when it drops.
        lease.charged = false;
        Ok(owner)
    }

    /// Convenience wrapper for infallible owners.
    pub fn into_owner<T>(self, build: impl FnOnce(Vec<u8>) -> Arc<T>) -> Arc<T>
    where
        T: Send + Sync + 'static,
    {
        match self.try_into_owner(|pixels| Ok(build(pixels))) {
            Ok(owner) => owner,
            Err(error) => unreachable!("infallible owner build failed: {error}"),
        }
    }
}

/// Immutable, still-charged CPU pixels. The inner allocation is private.
pub struct BudgetedPixels {
    // Declaration order is load-bearing: the bytes are freed before the lease
    // takes the ledger lock to return the quota.
    pixels: Box<[u8]>,
    lease: Lease,
}

impl BudgetedPixels {
    pub fn as_slice(&self) -> &[u8] {
        &self.pixels
    }

    pub fn len(&self) -> usize {
        self.pixels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    /// Copies into a fresh `FrameBuffer` charged to the same ledger. This is
    /// the supported way for a component to make an exclusive CPU copy for a
    /// `RenderImage`; it cannot claim the library's budget while bypassing it.
    pub fn copy_buffer(&self) -> Result<FrameBuffer, FrameError> {
        let budget = FrameBudget {
            ledger: self.lease.ledger.clone(),
        };
        let mut copy = budget.allocate(self.pixels.len())?;
        copy.pixels.copy_from_slice(&self.pixels);
        Ok(copy)
    }

    pub(crate) fn shares_ledger(&self, budget: &FrameBudget) -> bool {
        Arc::ptr_eq(&self.lease.ledger, &budget.ledger)
    }
}

impl Deref for BudgetedPixels {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.pixels
    }
}

impl fmt::Debug for BudgetedPixels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print pixel bytes from a callback/error path.
        formatter
            .debug_struct("BudgetedPixels")
            .field("len", &self.pixels.len())
            .finish()
    }
}

fn zeroed_pixels(len: usize) -> Result<Box<[u8]>, FrameError> {
    if len == 0 {
        return Ok(Vec::new().into_boxed_slice());
    }
    let layout = Layout::array::<u8>(len).map_err(|_| FrameError::AllocationFailed)?;
    // SAFETY: `len != 0`, so allocating this layout is defined. The pointer is
    // checked for null before it is used. The returned allocation has exactly
    // `len` bytes and alignment 1, matching `[u8]`.
    let pointer = unsafe { alloc_zeroed(layout) };
    if pointer.is_null() {
        return Err(FrameError::AllocationFailed);
    }
    // SAFETY: `pointer` came from `alloc_zeroed(layout)` with the same `len`,
    // and ownership of that allocation is transferred to exactly one `Box`.
    let slice = ptr::slice_from_raw_parts_mut(pointer, len);
    Ok(unsafe { Box::from_raw(slice) })
}
