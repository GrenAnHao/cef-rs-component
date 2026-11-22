//! UI-thread window binding and paint generations.
//!
//! A [`WindowToken`] is the component-local proof that a Browser is still bound
//! to one host window. It owns no GPUI, CEF or native object: the only identity
//! it carries is [`WindowBinding`], which already pairs the full `BrowserId`
//! (runtime generation plus local id) with the host window id.
//!
//! Clones share one `Rc<Cell<..>>`, so retirement and revision bumps are visible
//! through every clone. The `Rc` also keeps the token `!Send + !Sync`, which is
//! intended: the token belongs to the GPUI foreground thread.
//!
//! [`PaintStamp`] is an owned `Copy` value, not another handle. A stamp captures
//! the complete binding plus the revision current at the time it was taken. Once
//! the token advances -- because the view was replaced or released -- stamps from
//! before the advance can no longer import or paint their old pixels, even if
//! the host reuses the same window id or the same Browser local id under a new
//! runtime generation.

use std::{cell::Cell, error::Error, fmt, rc::Rc};

use rust_cef_core::ports::WindowBinding;

/// The shared state behind a [`WindowToken`]. `Copy` so `Cell::get` can read a
/// coherent snapshot without leaving the cell borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BindingState {
    binding: WindowBinding,
    revision: u64,
    retired: bool,
}

/// A permanently fatal binding failure.
///
/// There is deliberately only one reason to fail: the token can no longer issue
/// a new revision. Reaching it is terminal; callers must retire this view,
/// not reconstruct a token to bypass the one-shot attachment contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingError {
    /// The revision space is exhausted, or the token was already retired.
    Exhausted,
}

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted => formatter.write_str("window binding revision exhausted"),
        }
    }
}

impl Error for BindingError {}

/// An owned paint generation for one live window binding.
///
/// Contains no `Rc` and no native identity: copying it is cheap and safe, while
/// validity is always decided against the token at import/paint time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaintStamp {
    binding: WindowBinding,
    revision: u64,
}

/// Component-local window binding token shared by every clone on the UI thread.
///
/// The first live revision is 1. [`WindowToken::retire`] and revision overflow
/// are permanent: a retired token never becomes live again and never yields a
/// stamp again.
#[derive(Clone)]
pub struct WindowToken {
    state: Rc<Cell<BindingState>>,
}

impl WindowToken {
    /// Binds a token to one Browser/window pair at revision 1.
    pub fn new(binding: WindowBinding) -> Self {
        Self {
            state: Rc::new(Cell::new(BindingState {
                binding,
                revision: 1,
                retired: false,
            })),
        }
    }

    /// The immutable binding this token was created with; retirement does not
    /// clear it, so diagnostics can still report the stale identity.
    pub fn binding(&self) -> WindowBinding {
        self.state.get().binding
    }

    /// Whether this token can still issue stamps.
    pub fn is_live(&self) -> bool {
        !self.state.get().retired
    }

    /// Live and bound to this exact host window id.
    pub fn matches_window(&self, window: u64) -> bool {
        let state = self.state.get();
        !state.retired && state.binding.window == window
    }

    /// The current paint generation, or `None` once retired.
    #[must_use]
    pub fn stamp(&self) -> Option<PaintStamp> {
        let state = self.state.get();
        if state.retired {
            None
        } else {
            Some(PaintStamp {
                binding: state.binding,
                revision: state.revision,
            })
        }
    }

    /// Whether `stamp` may still import or paint into `actual_window`.
    ///
    /// This checks the whole binding: runtime generation, Browser local id,
    /// window id, current revision and liveness. A matching window `u64` alone
    /// is never sufficient.
    pub fn permits(&self, stamp: PaintStamp, actual_window: u64) -> bool {
        let state = self.state.get();
        !state.retired
            && stamp.binding == state.binding
            && stamp.binding.window == actual_window
            && stamp.revision == state.revision
    }

    /// A cancelled render pass from the same still-live binding. It remains
    /// forbidden to paint, but is not a wrong-window/retirement failure.
    pub fn is_obsolete(&self, stamp: PaintStamp, actual_window: u64) -> bool {
        let state = self.state.get();
        !state.retired
            && stamp.binding == state.binding
            && state.binding.window == actual_window
            && stamp.revision < state.revision
    }

    /// Issues the next revision, invalidating every stamp taken before it.
    ///
    /// Uses checked arithmetic. On overflow the token is permanently retired and
    /// this returns [`BindingError::Exhausted`]; the revision never saturates or
    /// wraps. Advancing a retired token is also `Exhausted`, because issuing a
    /// stamp after retirement would revive an invalid binding.
    ///
    /// Takes `&self` because the revision lives in shared interior state: every
    /// clone observes the bump. Ordering of advances is the caller's job.
    pub fn advance(&self) -> Result<(), BindingError> {
        let state = self.state.get();
        if state.retired {
            return Err(BindingError::Exhausted);
        }
        let Some(revision) = state.revision.checked_add(1) else {
            self.state.set(BindingState {
                retired: true,
                ..state
            });
            return Err(BindingError::Exhausted);
        };
        self.state.set(BindingState { revision, ..state });
        Ok(())
    }

    /// Permanently retires the shared binding.
    ///
    /// Returns `true` only for the live-to-retired transition; later calls,
    /// including from clones, return `false`. No operation can revive the token.
    pub fn retire(&self) -> bool {
        let state = self.state.get();
        if state.retired {
            return false;
        }
        self.state.set(BindingState {
            retired: true,
            ..state
        });
        true
    }
}

impl fmt::Debug for WindowToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.get();
        formatter
            .debug_struct("WindowToken")
            .field("binding", &state.binding)
            .field("revision", &state.revision)
            .field("retired", &state.retired)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};

    fn browser(runtime: u64, local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(runtime), local)
    }

    fn binding() -> WindowBinding {
        WindowBinding {
            browser: browser(7, 3),
            window: 41,
        }
    }

    /// Builds shared state directly so overflow is reachable without billions
    /// of advance calls. This is the real `WindowToken`, not a parallel fake.
    fn token_at(binding: WindowBinding, revision: u64) -> WindowToken {
        WindowToken {
            state: Rc::new(Cell::new(BindingState {
                binding,
                revision,
                retired: false,
            })),
        }
    }

    #[test]
    fn only_live_matching_older_revision_is_expected_obsolete() {
        let token = WindowToken::new(binding());
        let old = token.stamp().unwrap();
        assert!(!token.is_obsolete(old, 41));
        token.advance().unwrap();
        assert!(!token.permits(old, 41));
        assert!(token.is_obsolete(old, 41));
        assert!(!token.is_obsolete(old, 42));
        let future = token_at(binding(), 3).stamp().unwrap();
        assert!(!token.is_obsolete(future, 41));
        let foreign = token_at(
            WindowBinding {
                browser: browser(8, 3),
                window: 41,
            },
            4,
        );
        assert!(!foreign.is_obsolete(old, 41));
        token.retire();
        assert!(!token.is_obsolete(old, 41));
    }

    #[test]
    fn new_token_starts_live_at_revision_one() {
        let token = WindowToken::new(binding());
        assert!(token.is_live());
        assert_eq!(token.binding(), binding());
        assert!(token.matches_window(41));
        assert!(!token.matches_window(42));
        let stamp = token.stamp().expect("a live token has a stamp");
        assert_eq!(stamp.binding, binding());
        assert_eq!(stamp.revision, 1);
        assert!(token.permits(stamp, 41));
    }

    #[test]
    fn clone_shares_retirement_and_stamp_invalidation() {
        let token = WindowToken::new(binding());
        let clone = token.clone();
        let stamp = clone.stamp().expect("live clone issues the same stamp");
        assert_eq!(stamp, token.stamp().expect("shared revision"));

        assert!(clone.retire(), "first retirement wins");
        assert!(!token.is_live());
        assert!(!clone.is_live());
        assert_eq!(token.stamp(), None);
        assert_eq!(clone.stamp(), None);
        assert!(!clone.permits(stamp, 41), "retired token rejects old stamp");
        assert!(
            !token.matches_window(41),
            "a retired window no longer matches"
        );
        assert_eq!(token.binding(), binding(), "identity stays for diagnostics");
    }

    #[test]
    fn retire_is_idempotent_and_never_revives() {
        let token = WindowToken::new(binding());
        let clone = token.clone();
        assert!(token.retire());
        assert!(!token.retire(), "second retirement reports no transition");
        assert!(!clone.retire(), "a clone observes the same retirement");
        assert!(!token.is_live());
        assert_eq!(clone.advance(), Err(BindingError::Exhausted));
        assert!(!clone.is_live(), "failed advance does not revive");
        assert_eq!(clone.stamp(), None);
        assert!(!clone.matches_window(41));
    }

    #[test]
    fn advance_invalidates_old_stamp_and_new_stamp_permits() {
        let token = WindowToken::new(binding());
        let old = token.stamp().expect("live token");

        token.advance().expect("first advance fits in u64");
        assert!(!token.permits(old, 41), "old revision cannot import/paint");

        let new = token.stamp().expect("still live");
        assert_eq!(new.revision, 2);
        assert_eq!(new.binding, binding());
        assert!(token.permits(new, 41));
        assert!(!token.permits(old, 41));
    }

    #[test]
    fn wrong_window_rejects_stamp_even_with_same_browser() {
        let token = WindowToken::new(binding());
        let other_window = WindowToken::new(WindowBinding {
            browser: binding().browser,
            window: 42,
        });
        let stamp = other_window.stamp().expect("live other window");

        assert!(!token.permits(stamp, 41));
        assert!(!token.permits(stamp, 42));
        assert!(!token.matches_window(42));
    }

    #[test]
    fn wrong_browser_or_runtime_generation_rejects_stamp() {
        let token = WindowToken::new(binding());

        let other_local = WindowToken::new(WindowBinding {
            browser: browser(7, 4),
            window: 41,
        });
        assert!(!token.permits(other_local.stamp().expect("live"), 41));

        let other_runtime = WindowToken::new(WindowBinding {
            browser: browser(8, 3),
            window: 41,
        });
        assert!(
            !token.permits(other_runtime.stamp().expect("live"), 41),
            "same local id under a new runtime generation is not the same Browser"
        );

        let own = token.stamp().expect("live");
        assert!(!token.permits(own, 42), "actual window must match too");
    }

    #[test]
    fn overflow_permanently_retires_without_saturating_or_wrapping() {
        let token = token_at(binding(), u64::MAX);
        let stale = token.stamp().expect("live at max");

        assert_eq!(token.advance(), Err(BindingError::Exhausted));
        assert!(!token.is_live(), "overflow retires the token");
        assert_eq!(token.stamp(), None);
        assert!(!token.retire(), "overflow already retired it");
        assert_eq!(token.advance(), Err(BindingError::Exhausted));
        assert!(!token.is_live(), "never revives");
        assert!(!token.permits(stale, 41));

        let clone = token.clone();
        assert_eq!(clone.advance(), Err(BindingError::Exhausted));
        assert_eq!(clone.stamp(), None);
    }
}
