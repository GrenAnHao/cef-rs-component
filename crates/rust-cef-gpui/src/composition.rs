//! Explicit multi-View composition for already-created `CefBrowser` entities.
//!
//! The host owns the `Application`, the windows, the CEF Runtime and its pump.
//! This module only keeps a small routing table from `BrowserId` to the real
//! GPUI window/entity pair. It deliberately provides no implicit behavior:
//!
//! * no `Entity<CefBrowser>` is retained (only `WeakEntity`),
//! * no Runtime receiver/mailbox is touched and no per-frame queue exists,
//! * no global composition registry, no timer, no `spawn`, no `cx.quit`, no
//!   process exit and no CEF initialization,
//! * `dispatch`/`retry_pending` never schedule the next UI turn; the host waits
//!   for its own single wake and calls them from its own UI turn. A bounded
//!   retry policy belongs to the host, not to this table.
//!
//! Registration fixes one `(BrowserId, Window)` pair. The view's
//! `crate::binding::WindowToken` is cloned into the route so that dispatch can
//! verify the binding is still live without holding the entity strongly; its
//! fixed window identity is also checked, so a view can never be dispatched
//! through a foreign window. A wrong window is rejected before a slot is
//! reserved.
//!
//! `dispatch`/`retry_pending` must be called with an exclusive `&mut App` that
//! is not already inside an element or window update; the routing table runs
//! `AnyWindowHandle::update` and must not be re-entered from inside a render
//! callback or another dispatch.
//!
//! This type is UI-thread affine: `WindowToken` is `Rc`-backed and `!Send`, so
//! `UiComposition` is `!Send` as well. Dropping it only drops weak handles and
//! token clones; it does not release the view's atlas image and does not close
//! the Browser. Hosts release the image (while its window is alive) and
//! unregister explicitly.

use std::{error::Error, fmt, num::NonZeroUsize};

use gpui::{AnyWindowHandle, App, Entity, WeakEntity, Window};
use rust_cef_core::contracts::BrowserId;

use crate::{CefBrowser, ViewError, binding::WindowToken};

/// Why a registration was rejected.
///
/// Every failure is reported before the route is inserted. In particular a
/// window-identity failure never consumes one of the fixed slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositionError {
    /// The configured number of registered routes has been reached.
    Capacity { capacity: usize },
    /// This `UiComposition` already has a live route for the Browser.
    AlreadyRegistered { browser: BrowserId },
    /// The entity's fixed window binding does not match the window passed to
    /// `register`.
    WrongWindow { expected: u64, actual: u64 },
    /// The view's binding token was already retired; a dead route is not
    /// inserted and must not be dispatched.
    Retired,
    /// `try_reserve` refused the storage for one more route.
    AllocationFailed,
}

impl fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity { capacity } => {
                write!(
                    formatter,
                    "composition capacity {capacity} is already reached"
                )
            }
            Self::AlreadyRegistered { browser } => {
                write!(formatter, "browser {browser:?} is already registered")
            }
            Self::WrongWindow { expected, actual } => write!(
                formatter,
                "view is bound to window {expected}, not window {actual}"
            ),
            Self::Retired => formatter.write_str("view binding token is retired"),
            Self::AllocationFailed => {
                formatter.write_str("composition could not reserve storage for another route")
            }
        }
    }
}

impl Error for CompositionError {}

/// Which layer of one dispatch attempt failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchFailureKind {
    /// The window route could not be updated, or the weak entity did not
    /// upgrade. The closure may or may not have run.
    ///
    /// A window-route error is kept pending because it does not prove the
    /// window is gone; it is removed only if its binding token retired during
    /// the same failed attempt (still one `Access` entry). Successful terminal
    /// consumption instead only increments `removed`. The message is prefixed
    /// with `window route unavailable: ` or `view entity released: ` so a host
    /// can distinguish the layers without inferring anything from side
    /// effects. The entity-released case always removes the route (counted in
    /// [`DispatchReport::removed`]).
    Access { message: String },
    /// `CefBrowser::consume` (or its window operation) returned a view error.
    /// The route is kept unless its binding token retired during the same
    /// attempt (then it is removed and counted in
    /// [`DispatchReport::removed`]); whether it is retried is reflected in
    /// [`DispatchReport::pending`].
    View(ViewError),
}

/// One per-route error from `dispatch` or `retry_pending`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchFailure {
    pub browser: BrowserId,
    pub kind: DispatchFailureKind,
}

/// Prefixes used by [`DispatchFailureKind::Access`] messages. They are the only
/// way the two access layers are distinguished inside the pinned variant shape;
/// both producers live in this module.
const WINDOW_ROUTE_PREFIX: &str = "window route unavailable: ";
const ENTITY_RELEASED_PREFIX: &str = "view entity released: ";

impl DispatchFailureKind {
    /// The window route itself was unusable. The inner closure may still have
    /// run; the route is kept pending.
    pub fn window_route_unavailable(&self) -> bool {
        matches!(self, Self::Access { message } if message.starts_with(WINDOW_ROUTE_PREFIX))
    }

    /// The window existed but the weak `CefBrowser` entity no longer upgraded;
    /// the route has been removed.
    pub fn entity_released(&self) -> bool {
        matches!(self, Self::Access { message } if message.starts_with(ENTITY_RELEASED_PREFIX))
    }
}

impl DispatchFailure {
    fn access(browser: BrowserId, message: String) -> Self {
        Self {
            browser,
            kind: DispatchFailureKind::Access { message },
        }
    }

    fn view(browser: BrowserId, error: ViewError) -> Self {
        Self {
            browser,
            kind: DispatchFailureKind::View(error),
        }
    }
}

/// Result of one `dispatch`/`retry_pending` call.
///
/// `failures` has at most one entry per attempted route, and the table never
/// holds more than its configured capacity, so `failures.len() <= capacity`.
/// Routes pruned before they could be attempted (released entity or retired
/// token) are counted in `removed` but do not produce a `failures` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    /// Routes whose window/entity update was invoked exactly once by this call.
    pub attempted: usize,
    /// Calls where `CefBrowser::consume` returned `Ok(true)`.
    pub updated: usize,
    /// Invalid routes removed: weak entity gone, token retired, or both.
    pub removed: usize,
    /// Routes still marked pending after this call. These are the only routes a
    /// following `retry_pending` will attempt.
    pub pending: usize,
    /// One entry per failed attempt, bounded by the table capacity.
    pub failures: Vec<DispatchFailure>,
}

/// One explicit input-retry turn, independent of `DispatchReport` frame retries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputRetryReport {
    pub attempted: usize,
    pub accepted: usize,
    pub removed: usize,
    pub pending: usize,
    pub failures: Vec<DispatchFailure>,
}

/// One registered route. All fields are handles or metadata; no entity, frame
/// or window payload is retained.
struct Route {
    browser: BrowserId,
    /// Real GPUI route. It is a handle, not a strong entity reference.
    route: AnyWindowHandle,
    view: WeakEntity<CefBrowser>,
    /// Fixed identity + liveness token cloned from the view at registration.
    token: WindowToken,
    /// Durable routing state: one bool per route, never a per-frame queue.
    pending: bool,
}

/// Fixed-capacity, UI-thread-only routing table for several `CefBrowser` views.
///
/// Register all routes up front or as the host creates views; the capacity is
/// a hard upper bound on simultaneously registered Browser/View pairs. The host
/// calls [`Self::dispatch`] after its single wake and may call
/// [`Self::retry_pending`] at a later, explicitly chosen UI turn. Neither call
/// wakes the application or pumps CEF.
pub struct UiComposition {
    capacity: NonZeroUsize,
    routes: Vec<Route>,
}

impl UiComposition {
    /// Builds an empty table that accepts at most `capacity` live routes.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            routes: Vec::new(),
        }
    }

    /// Registers one existing view in the window it was constructed for.
    ///
    /// The token is read from the entity and its fixed window identity must
    /// match `window.window_handle().window_id().as_u64()`. That check happens
    /// before any slot is taken. A retired token is rejected as well. A live
    /// duplicate `BrowserId` is [`CompositionError::AlreadyRegistered`] and is
    /// checked before capacity; stale routes are pruned first. An explicitly
    /// unregistered but still-live view can be registered again; this does not
    /// create another BrowserAttachment or permit migration. Capacity and
    /// `try_reserve` allocation failures are reported explicitly.
    pub fn register(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &Window,
        cx: &App,
    ) -> Result<(), CompositionError> {
        let token = view.read(cx).window_token();
        let binding = token.binding();
        let browser = binding.browser;
        let actual = window.window_handle().window_id().as_u64();
        if binding.window != actual {
            return Err(CompositionError::WrongWindow {
                expected: binding.window,
                actual,
            });
        }
        // Check liveness before the combined token identity check: a retired
        // token must be reported as retired, not as a window mismatch.
        if !token.is_live() {
            return Err(CompositionError::Retired);
        }
        if !token.matches_window(actual) {
            return Err(CompositionError::WrongWindow {
                expected: binding.window,
                actual,
            });
        }

        // Only a confirmed-dead route is discarded. A live duplicate remains a
        // programming error and is never silently replaced.
        self.prune_invalid(false);
        if self.routes.iter().any(|route| route.browser == browser) {
            return Err(CompositionError::AlreadyRegistered { browser });
        }
        if self.routes.len() >= self.capacity.get() {
            return Err(CompositionError::Capacity {
                capacity: self.capacity.get(),
            });
        }
        self.routes
            .try_reserve(1)
            .map_err(|_| CompositionError::AllocationFailed)?;

        self.routes.push(Route {
            browser,
            route: window.window_handle(),
            view: view.downgrade(),
            token,
            pending: true,
        });
        Ok(())
    }

    /// Removes the route for `browser` if present. Returns whether one existed.
    ///
    /// This does not release the view's image and does not close the Browser;
    /// the host remains responsible for the view's own release path.
    pub fn unregister(&mut self, browser: BrowserId) -> bool {
        if let Some(index) = self
            .routes
            .iter()
            .position(|route| route.browser == browser)
        {
            self.routes.remove(index);
            true
        } else {
            false
        }
    }

    /// Number of routes currently stored, including stale routes that have not
    /// been pruned yet. It never exceeds the configured capacity.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether no route (live or stale-but-unpruned) is stored.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Attempts every registered live route exactly once.
    ///
    /// For each route the real window is updated and, inside that update, the
    /// weak entity's `consume` runs. Three result layers are kept distinct:
    ///
    /// 1. window update failed: the closure may already have run, so it is
    ///    reported as [`DispatchFailureKind::Access`] and the route stays
    ///    pending; it is not treated as closed and not removed,
    /// 2. entity upgrade failed: the route is removed and reported as `Access`,
    /// 3. `consume` failed: the route stays (unless its token retired during
    ///    the same attempt, which removes it) and is reported as
    ///    [`DispatchFailureKind::View`]; `pending` follows
    ///    `CefBrowser::needs_resync` (budget/copy failures), except that
    ///    fixed-identity errors (`WrongWindow`, `BrowserMismatch`,
    ///    `InvalidGeometry`) are never scheduled for retry. They are still
    ///    reported and kept.
    ///
    /// Stale routes (weak entity gone or token retired) are pruned first and
    /// again after each attempt. No timer, wake or loop is started here.
    pub fn dispatch(&mut self, cx: &mut App) -> DispatchReport {
        self.run(cx, false)
    }

    /// Runs only routes already marked `pending`, once each.
    ///
    /// The host chooses when this happens (for example on its next explicit UI
    /// turn, for a bounded number of times). The library never self-retries,
    /// never sleeps and never calls a wake service.
    pub fn retry_pending(&mut self, cx: &mut App) -> DispatchReport {
        self.run(cx, true)
    }

    /// Explicitly attempts each pending input slot at most once in its original
    /// weak window route. Never schedules a wake or touches frame retry flags.
    pub fn retry_input(&mut self, cx: &mut App) -> InputRetryReport {
        let mut report = InputRetryReport {
            removed: self.prune_invalid(false),
            ..Default::default()
        };
        let mut index = 0;
        while index < self.routes.len() {
            let (browser, window, weak) = {
                let route = &self.routes[index];
                (route.browser, route.route, route.view.clone())
            };
            let Some(entity) = weak.upgrade() else {
                self.routes.remove(index);
                report.removed += 1;
                continue;
            };
            if !entity.read(cx).diagnostics().input.pending {
                index += 1;
                continue;
            }
            report.attempted += 1;
            let result = window.update(cx, |_root, window, cx| {
                weak.update(cx, |view, view_cx| view.retry_input(window, view_cx))
            });
            match result {
                Err(error) => {
                    // The window closure may have run. Revoke the original
                    // pending slot without replaying an unconfirmed gesture.
                    entity.update(cx, |view, _| view.cancel_pending_input());
                    report.failures.push(DispatchFailure::access(
                        browser,
                        format!("{}{error:#}", WINDOW_ROUTE_PREFIX),
                    ));
                }
                Ok(Err(error)) => report.failures.push(DispatchFailure::access(
                    browser,
                    format!("{}{error:#}", ENTITY_RELEASED_PREFIX),
                )),
                Ok(Ok(Err(error))) => report.failures.push(DispatchFailure::view(browser, error)),
                Ok(Ok(Ok(true))) => report.accepted += 1,
                Ok(Ok(Ok(false))) => {}
            }
            if !self.routes[index].token.is_live() || weak.upgrade().is_none() {
                self.routes.remove(index);
                report.removed += 1;
            } else {
                index += 1;
            }
        }
        report.pending = self
            .routes
            .iter()
            .filter(|route| {
                route
                    .view
                    .upgrade()
                    .is_some_and(|entity| entity.read(cx).diagnostics().input.pending)
            })
            .count();
        report
    }

    fn run(&mut self, cx: &mut App, pending_only: bool) -> DispatchReport {
        // `retry_pending` must not touch non-pending routes at all, not even
        // by pruning them; only the full `dispatch` prunes every stale route.
        let mut report = DispatchReport {
            removed: self.prune_invalid(pending_only),
            ..DispatchReport::default()
        };

        let mut index = 0;
        while index < self.routes.len() {
            if pending_only && !self.routes[index].pending {
                index += 1;
                continue;
            }

            // Clone only cheap handles. The route borrow of `self.routes` ends
            // here, so the application update can run without aliasing it.
            let (browser, route, weak) = {
                let entry = &self.routes[index];
                (entry.browser, entry.route, entry.view.clone())
            };
            report.attempted += 1;
            let outcome = attempt_route(route, &weak, cx);

            let mut remove = false;
            {
                let entry = &mut self.routes[index];
                match outcome {
                    Attempt::Access(message) => {
                        // A window route error does not prove that the closure
                        // did not run, nor that the window is permanently gone.
                        // Keep the route and let the host decide how many
                        // retries to perform.
                        entry.pending = true;
                        report
                            .failures
                            .push(DispatchFailure::access(browser, message));
                    }
                    Attempt::EntityReleased(message) => {
                        report
                            .failures
                            .push(DispatchFailure::access(browser, message));
                        report.removed += 1;
                        remove = true;
                    }
                    Attempt::Consumed {
                        result: Ok(changed),
                        needs_resync,
                    } => {
                        entry.pending = needs_resync;
                        if changed {
                            report.updated += 1;
                        }
                    }
                    Attempt::Consumed {
                        result: Err(error),
                        needs_resync,
                    } => {
                        // `needs_resync` is the view's own retry signal (a
                        // budget/copy failure). A sticky flag must not keep a
                        // fixed-identity failure in a retry loop.
                        entry.pending = needs_resync && !terminal_view_error(&error);
                        report.failures.push(DispatchFailure::view(browser, error));
                    }
                }

                // The token may have retired during the update (for example
                // while the window was being torn down). Confirm after the
                // attempt rather than assuming it stayed live.
                if !remove && !entry.token.is_live() {
                    remove = true;
                    report.removed += 1;
                    // A successful Source.closed transition is normal removal,
                    // not an Access failure. Existing View/Access errors above
                    // remain reported, including revision exhaustion.
                }
            }

            if remove {
                // Remove rather than replace: a caller cannot observe a
                // half-updated route through the public API in between.
                self.routes.remove(index);
                continue;
            }
            index += 1;
        }

        report.pending = self.routes.iter().filter(|route| route.pending).count();
        debug_assert!(report.failures.len() <= self.capacity.get());
        debug_assert!(report.attempted <= self.capacity.get());
        report
    }

    /// Drops routes whose weak entity no longer upgrades or whose token is
    /// retired. `WeakEntity::upgrade` is a transient confirmation only; the
    /// resulting strong entity is dropped immediately and is never stored.
    ///
    /// With `pending_only`, routes that are not pending are left untouched so a
    /// `retry_pending` call has no effect on them.
    fn prune_invalid(&mut self, pending_only: bool) -> usize {
        let before = self.routes.len();
        self.routes.retain(|route| {
            if pending_only && !route.pending {
                return true;
            }
            route.view.upgrade().is_some() && route.token.is_live()
        });
        before - self.routes.len()
    }
}

enum Attempt {
    /// The window route (or the entity lease inside it) was unavailable. No
    /// assumption is made about whether the inner closure ran.
    Access(String),
    EntityReleased(String),
    Consumed {
        result: Result<bool, ViewError>,
        /// Observed on the same `&mut CefBrowser` immediately after `consume`.
        needs_resync: bool,
    },
}

/// Executes the real three-layer GPUI path for one route.
fn attempt_route(route: AnyWindowHandle, view: &WeakEntity<CefBrowser>, cx: &mut App) -> Attempt {
    let window_result = route.update(cx, |_root, window, cx| {
        view.update(cx, |view, view_cx| {
            let result = view.consume(window, view_cx);
            let needs_resync = view.needs_resync();
            (result, needs_resync)
        })
    });

    match window_result {
        // Layer 1: no usable window route. The closure may have run, so no
        // assumption about side effects is made here.
        Err(error) => Attempt::Access(format!("{}{error:#}", WINDOW_ROUTE_PREFIX)),
        // Layer 2: the window existed but the weak entity did not upgrade.
        Ok(Err(error)) => Attempt::EntityReleased(format!("{}{error:#}", ENTITY_RELEASED_PREFIX)),
        // Layer 3: reborrow-free result of `consume`, plus explicit retry state.
        Ok(Ok((result, needs_resync))) => Attempt::Consumed {
            result,
            needs_resync,
        },
    }
}

/// Fixed-identity errors cannot be repaired by consuming again in the same
/// window. They keep their route but must not become a retry loop.
fn terminal_view_error(error: &ViewError) -> bool {
    matches!(
        error,
        ViewError::BrowserMismatch { .. }
            | ViewError::WrongWindow { .. }
            | ViewError::InvalidGeometry
            | ViewError::SurfaceModeMismatch
            | ViewError::NativeWindowMismatch { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{contracts::RuntimeGeneration, frame_budget::FrameError};

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(11), local)
    }

    #[test]
    fn composition_error_is_a_std_error() {
        let error: Box<dyn Error> = Box::new(CompositionError::Retired);
        assert_eq!(error.to_string(), "view binding token is retired");
        let capacity = CompositionError::Capacity { capacity: 2 };
        assert_eq!(
            capacity.to_string(),
            "composition capacity 2 is already reached"
        );
    }

    #[test]
    fn fixed_identity_errors_are_never_scheduled_for_retry() {
        for error in [
            ViewError::WrongWindow {
                expected: 3,
                actual: 4,
            },
            ViewError::BrowserMismatch {
                expected: browser(1),
                actual: browser(2),
            },
            ViewError::InvalidGeometry,
        ] {
            assert!(terminal_view_error(&error));
        }
    }

    #[test]
    fn copy_failures_are_view_failures_and_not_terminal() {
        for error in [
            ViewError::Image(FrameError::BudgetExceeded),
            ViewError::Image(FrameError::AllocationFailed),
        ] {
            assert!(!terminal_view_error(&error));
            let failure = DispatchFailure::view(browser(1), error.clone());
            assert_eq!(failure.browser, browser(1));
            assert_eq!(failure.kind, DispatchFailureKind::View(error));
        }
    }

    #[test]
    fn access_failures_keep_the_three_layers_distinguishable() {
        let unavailable = DispatchFailure::access(
            browser(2),
            format!("{}window not found", WINDOW_ROUTE_PREFIX),
        );
        assert_eq!(unavailable.browser, browser(2));
        assert_eq!(
            unavailable.kind,
            DispatchFailureKind::Access {
                message: "window route unavailable: window not found".to_owned()
            }
        );
        assert!(unavailable.kind.window_route_unavailable());
        assert!(!unavailable.kind.entity_released());

        let released = DispatchFailure::access(
            browser(2),
            format!("{}entity released", ENTITY_RELEASED_PREFIX),
        );
        assert!(released.kind.entity_released());
        assert!(!released.kind.window_route_unavailable());
    }
}
