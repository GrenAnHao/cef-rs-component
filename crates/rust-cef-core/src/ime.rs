//! Bounded, owned IME composition geometry. No native or GPUI reference.
//!
//! CEF's `OnImeCompositionRangeChanged` reports a borrowed selected range plus
//! one rectangle per character in view coordinates. This module retains only
//! owned, bounded copies and one monotonic revision per Browser. The runtime
//! gates terminal callbacks and known reset/close cutpoints; this reducer does
//! not identify a callback's native transaction. Its revision proves local
//! observation order and known invalidations only. Fixed CEF carries no IME
//! transaction id, so an identically
//! shaped callback that was already in flight under the same observed input
//! epoch cannot be distinguished from a newer one. The last rectangle is
//! deliberately not interpreted as the caret: `range` is the observed range and
//! is never synthesized.
//!
//! The retained geometry contains no input text. The observed input epoch is
//! recorded only when an adapter callback really arrives; it is a local
//! observation, not an identity carried by the native callback (fixed CEF has
//! no IME transaction id).
use crate::{
    contracts::BrowserId,
    input::{MAX_INPUT_RANGE_UTF16, Utf16Range},
};

/// Largest character-rectangle count retained per Browser.
pub const MAX_IME_CHARACTER_BOUNDS: usize = 4096;

/// One character rectangle in CEF view coordinates (DIP).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImeRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Why a callback could not become usable geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeRejection {
    /// Missing, inverted, or beyond [`MAX_INPUT_RANGE_UTF16`].
    InvalidRange,
    /// Negative extent, an `i32` overflow at a rectangle edge, or an adapter
    /// that disagrees with its own reported count.
    InvalidGeometry,
    /// The adapter reported no geometry array at all. Distinct from an
    /// explicitly empty array, which is [`ImeStatus::Cleared`].
    MissingGeometry,
    /// More rectangles than [`MAX_IME_CHARACTER_BOUNDS`]; refused before any
    /// collection.
    OverflowAllocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeStatus {
    /// No usable observation: never reported, or invalidated by reset/close.
    Unavailable,
    Ready,
    /// CEF explicitly reported an empty character list.
    Cleared,
    Rejected(ImeRejection),
    /// Revision exhaustion; permanent and never revived.
    Retired,
}

/// Owned diagnostic view of the latest observation for one Browser. Reading it
/// never mutates the retained state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImeSnapshot {
    pub browser: BrowserId,
    pub revision: u64,
    pub observed_input_epoch: u64,
    pub range: Option<Utf16Range>,
    pub character_bounds: Vec<ImeRect>,
    pub status: ImeStatus,
}

/// Adapter-reported geometry. `count` is the full reported length so an
/// over-cap callback can be refused before its rectangles are collected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeReportedBounds {
    /// The native callback supplied no array, not an empty one.
    Missing,
    /// `count` rectangles were reported; `retained` holds the bounded copy.
    Reported {
        count: usize,
        retained: Vec<ImeRect>,
    },
    /// The bounded copy could not be reserved; nothing was collected.
    Unallocated,
}

impl ImeReportedBounds {
    /// Bounded adapter constructor. An over-cap report is refused without
    /// copying anything, and a failed reservation is reported instead of
    /// aborting the process. A supplier that ends early leaves a partial copy
    /// that `ImeState::observe` rejects as inconsistent geometry.
    pub fn from_reported(count: usize, mut next: impl FnMut() -> Option<ImeRect>) -> Self {
        if count > MAX_IME_CHARACTER_BOUNDS {
            return Self::Reported {
                count,
                retained: Vec::new(),
            };
        }
        let mut retained = Vec::new();
        if retained.try_reserve_exact(count).is_err() {
            return Self::Unallocated;
        }
        for _ in 0..count {
            let Some(rect) = next() else {
                return Self::Reported { count, retained };
            };
            retained.push(rect);
        }
        Self::Reported { count, retained }
    }
}

/// Single latest slot plus its revision. Owned data only; cloneable for tests.
#[derive(Clone, Debug)]
pub struct ImeState {
    browser: BrowserId,
    revision: u64,
    observed_input_epoch: u64,
    range: Option<Utf16Range>,
    character_bounds: Vec<ImeRect>,
    status: ImeStatus,
}

impl ImeState {
    pub fn new(browser: BrowserId) -> Self {
        Self {
            browser,
            revision: 0,
            observed_input_epoch: 0,
            range: None,
            character_bounds: Vec::new(),
            status: ImeStatus::Unavailable,
        }
    }

    /// Retained observation revision. `0` means nothing was ever observed or
    /// invalidated, so a reader can refuse to use it as fresh geometry.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// A retired slot (revision exhaustion) never accepts another observation.
    pub fn is_retired(&self) -> bool {
        self.status == ImeStatus::Retired
    }

    pub fn snapshot(&self) -> ImeSnapshot {
        ImeSnapshot {
            browser: self.browser,
            revision: self.revision,
            observed_input_epoch: self.observed_input_epoch,
            range: self.range,
            character_bounds: self.character_bounds.clone(),
            status: self.status,
        }
    }

    /// Applies one adapter callback. Returns whether the retained state changed
    /// (callers use this to wake the host outside their own lock).
    ///
    /// Every refusal clears the previous geometry: a stale rectangle must not
    /// survive a rejection under the same observed epoch.
    pub fn observe(
        &mut self,
        observed_input_epoch: u64,
        reported_range: Option<Utf16Range>,
        reported_bounds: ImeReportedBounds,
    ) -> bool {
        if self.status == ImeStatus::Retired {
            return false;
        }
        self.observed_input_epoch = observed_input_epoch;
        // An out-of-limit or inverted range is never retained; a valid one is
        // kept exactly as reported even when the geometry is refused.
        let range = reported_range.filter(|range| valid_range(*range));
        let (status, bounds) = match reported_bounds {
            ImeReportedBounds::Missing => (
                ImeStatus::Rejected(ImeRejection::MissingGeometry),
                Vec::new(),
            ),
            ImeReportedBounds::Unallocated => (
                ImeStatus::Rejected(ImeRejection::OverflowAllocation),
                Vec::new(),
            ),
            ImeReportedBounds::Reported { count, retained } => {
                if count > MAX_IME_CHARACTER_BOUNDS {
                    // Nothing was collected past the cap; keep only the status.
                    (
                        ImeStatus::Rejected(ImeRejection::OverflowAllocation),
                        Vec::new(),
                    )
                } else if retained.len() != count {
                    // Never trust a partial prefix: the rectangle index and
                    // the UTF-16 range would disagree.
                    (
                        ImeStatus::Rejected(ImeRejection::InvalidGeometry),
                        Vec::new(),
                    )
                } else if count == 0 {
                    (ImeStatus::Cleared, Vec::new())
                } else if retained.iter().any(|rect| !valid_rect(*rect)) {
                    (
                        ImeStatus::Rejected(ImeRejection::InvalidGeometry),
                        Vec::new(),
                    )
                } else if range.is_none() {
                    // Without an observed range the rectangles cannot be
                    // matched to a composition selection; do not guess one.
                    (ImeStatus::Rejected(ImeRejection::InvalidRange), Vec::new())
                } else {
                    (ImeStatus::Ready, retained)
                }
            }
        };
        self.status = status;
        self.range = range;
        self.character_bounds = bounds;
        self.bump_revision();
        true
    }

    /// Clears geometry after reset/close/draining or a lost text input target.
    /// The observed epoch is retained: it is an observation, not a new one.
    pub fn invalidate(&mut self) -> bool {
        if self.status == ImeStatus::Retired {
            return false;
        }
        if self.status == ImeStatus::Unavailable
            && self.range.is_none()
            && self.character_bounds.is_empty()
        {
            return false;
        }
        self.status = ImeStatus::Unavailable;
        self.range = None;
        self.character_bounds = Vec::new();
        self.bump_revision();
        true
    }

    fn bump_revision(&mut self) {
        match self.revision.checked_add(1) {
            Some(next) => self.revision = next,
            None => {
                // Never wrap into a value a reader could mistake for a fresh
                // observation; retirement is permanent.
                self.revision = u64::MAX;
                self.status = ImeStatus::Retired;
                self.range = None;
                self.character_bounds = Vec::new();
            }
        }
    }
}

fn valid_range(range: Utf16Range) -> bool {
    range.start <= range.end && u64::from(range.end) <= MAX_INPUT_RANGE_UTF16 as u64
}

fn valid_rect(rect: ImeRect) -> bool {
    rect.width >= 0
        && rect.height >= 0
        && rect.x.checked_add(rect.width).is_some()
        && rect.y.checked_add(rect.height).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RuntimeGeneration;

    fn browser() -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), 7)
    }

    fn range(start: u32, end: u32) -> Option<Utf16Range> {
        Some(Utf16Range { start, end })
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> ImeRect {
        ImeRect {
            x,
            y,
            width,
            height,
        }
    }

    fn reported(bounds: &[ImeRect]) -> ImeReportedBounds {
        ImeReportedBounds::Reported {
            count: bounds.len(),
            retained: bounds.to_vec(),
        }
    }

    #[test]
    fn bounded_bounds_are_retained_and_over_cap_is_refused_without_collecting() {
        let mut state = ImeState::new(browser());
        let bounds = [rect(1, 2, 3, 4), rect(5, 6, 7, 8)];
        assert!(state.observe(3, range(0, 4), reported(&bounds)));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.status, ImeStatus::Ready);
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.observed_input_epoch, 3);
        assert_eq!(snapshot.range, range(0, 4));
        assert_eq!(snapshot.character_bounds, bounds.to_vec());
        assert_eq!(snapshot.character_bounds.last(), Some(&bounds[1]));

        // The cap is checked against the reported count before any copy.
        let mut copied = 0usize;
        let over = ImeReportedBounds::from_reported(MAX_IME_CHARACTER_BOUNDS + 1, || {
            copied += 1;
            Some(rect(0, 0, 1, 1))
        });
        assert_eq!(
            over,
            ImeReportedBounds::Reported {
                count: MAX_IME_CHARACTER_BOUNDS + 1,
                retained: Vec::new(),
            }
        );
        assert_eq!(copied, 0, "over-cap reports must not be collected");
        assert!(state.observe(3, range(0, 4), over));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.status,
            ImeStatus::Rejected(ImeRejection::OverflowAllocation)
        );
        assert!(snapshot.character_bounds.is_empty());
        assert_eq!(snapshot.revision, 2);

        // A copy that cannot be reserved is refused, not aborted.
        assert!(state.observe(3, range(0, 4), ImeReportedBounds::Unallocated));
        assert_eq!(
            state.snapshot().status,
            ImeStatus::Rejected(ImeRejection::OverflowAllocation)
        );

        let full = vec![rect(1, 1, 1, 1); MAX_IME_CHARACTER_BOUNDS];
        assert!(state.observe(4, range(0, MAX_INPUT_RANGE_UTF16 as u32), reported(&full),));
        assert_eq!(
            state.snapshot().character_bounds.len(),
            MAX_IME_CHARACTER_BOUNDS
        );
    }

    #[test]
    fn empty_and_missing_geometry_are_distinguishable() {
        let mut state = ImeState::new(browser());
        assert!(state.observe(1, range(2, 2), reported(&[])));
        let cleared = state.snapshot();
        assert_eq!(cleared.status, ImeStatus::Cleared);
        assert_eq!(cleared.range, range(2, 2));
        assert!(cleared.character_bounds.is_empty());

        assert!(state.observe(1, range(2, 2), ImeReportedBounds::Missing));
        let missing = state.snapshot();
        assert_eq!(
            missing.status,
            ImeStatus::Rejected(ImeRejection::MissingGeometry)
        );
        assert_ne!(missing.status, cleared.status);
        assert!(missing.character_bounds.is_empty());
    }

    #[test]
    fn invalid_range_or_geometry_rejects_and_clears_previous_bounds() {
        let mut state = ImeState::new(browser());
        let bounds = [rect(10, 10, 4, 20)];
        assert!(state.observe(2, range(0, 1), reported(&bounds)));
        assert_eq!(state.snapshot().status, ImeStatus::Ready);

        state.observe(2, range(5, 4), reported(&bounds));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.status,
            ImeStatus::Rejected(ImeRejection::InvalidRange)
        );
        assert!(snapshot.character_bounds.is_empty());
        assert_eq!(snapshot.range, None);

        state.observe(
            2,
            range(0, (MAX_INPUT_RANGE_UTF16 + 1) as u32),
            reported(&bounds),
        );
        assert_eq!(
            state.snapshot().status,
            ImeStatus::Rejected(ImeRejection::InvalidRange)
        );

        state.observe(2, range(0, 1), reported(&[rect(0, 0, -1, 4)]));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.status,
            ImeStatus::Rejected(ImeRejection::InvalidGeometry)
        );
        assert!(snapshot.character_bounds.is_empty());
        assert_eq!(
            snapshot.range,
            range(0, 1),
            "a valid observed range is preserved, not guessed"
        );

        state.observe(2, range(0, 1), reported(&[rect(i32::MAX, 0, 1, 1)]));
        assert_eq!(
            state.snapshot().status,
            ImeStatus::Rejected(ImeRejection::InvalidGeometry)
        );

        state.observe(2, None, reported(&bounds));
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.status,
            ImeStatus::Rejected(ImeRejection::InvalidRange)
        );
        assert!(snapshot.character_bounds.is_empty());
        assert_eq!(snapshot.range, None);
    }

    #[test]
    fn revision_is_nonzero_monotonic_and_overflow_retires_permanently() {
        let mut state = ImeState::new(browser());
        assert_eq!(state.snapshot().revision, 0);
        let bounds = [rect(0, 0, 1, 1)];
        let observe = |state: &mut ImeState| state.observe(1, range(0, 1), reported(&bounds));
        assert!(observe(&mut state));
        assert!(observe(&mut state));
        assert_eq!(state.snapshot().revision, 2);

        state.revision = u64::MAX;
        assert!(observe(&mut state));
        let retired = state.snapshot();
        assert_eq!(retired.status, ImeStatus::Retired);
        assert_eq!(retired.revision, u64::MAX);
        assert!(retired.character_bounds.is_empty());

        // Permanent: neither a later callback nor an invalidation revives it.
        assert!(!observe(&mut state));
        assert!(!state.invalidate());
        assert_eq!(state.snapshot().status, ImeStatus::Retired);
    }

    #[test]
    fn reading_or_invalidating_never_synthesizes_an_epoch_or_revision() {
        let mut state = ImeState::new(browser());
        let before = state.snapshot();
        assert_eq!(before, state.snapshot());
        assert_eq!(before.revision, 0);
        assert_eq!(before.observed_input_epoch, 0);
        assert!(!state.invalidate(), "nothing to clear");

        let bounds = [rect(1, 1, 2, 2)];
        state.observe(9, range(0, 2), reported(&bounds));
        let ready = state.snapshot();
        assert!(state.invalidate());
        let cleared = state.snapshot();
        assert_eq!(cleared.status, ImeStatus::Unavailable);
        assert_eq!(
            cleared.observed_input_epoch, 9,
            "the epoch is the last callback observation, not a cleared value"
        );
        assert_eq!(cleared.revision, ready.revision + 1);
        assert!(!state.invalidate(), "already unavailable");
    }
}
