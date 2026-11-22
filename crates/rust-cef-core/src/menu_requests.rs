//! Host-owned context-menu request bookkeeping.
//!
//! This is pure data: no mutex, no CEF callback, no UI. One [`MenuRequests`]
//! book belongs to one runtime generation and keeps at most one current opening
//! per full [`BrowserId`] (runtime plus local identity), bounded by a host
//! supplied capacity.
//!
//! Every path that removes or refuses a request returns the owner's payload or
//! the pending record, so a native adapter can end its lock borrow before it
//! invokes or drops anything. This type cannot enforce a caller's lock
//! discipline; it only avoids retaining or silently dropping continuations.
//! There are no automatic retries, timeouts, or drop-cancel policies here.
//!
//! Sequences live in [`ContextMenuRequestId`], a distinct type from the
//! beforeunload `RequestId`, so equal numbers in the two continuation domains
//! are never mistaken for one another.
//!
//! A pending record retains the id, the payload, the reservation and bounded
//! command metadata only. Labels are not duplicated: the normalized model handed
//! to the UI is the single owner of label text.
//!
//! [`MenuLimits`] bounds the logical model: entry count and per-label/total
//! label UTF-8 byte lengths. Those are validation limits, not allocation caps;
//! they do not bound `Vec`/`String` capacity, the allocator or process RSS, the
//! adapter's copy of the original native model, or the opaque payload `T`.
//! Nothing here claims an exact heap bound, and no compaction is attempted.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use crate::{
    contracts::BrowserId,
    menu::{
        ContextMenu, ContextMenuRequestId, ContextMenuResponse, MenuEntry, MenuLimits,
        MenuModelError,
    },
    menu_context::{MenuContext, MenuContextError, MenuContextLimits, MenuPresentation},
    menu_model::{MenuModel, MenuTreeError},
    ports::{EngineError, WindowBinding},
};

/// Why a new opening was refused. The rejected payload travels with the error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuRequestError {
    /// `ContextMenu::validate` refused the raw model before normalization.
    Model(MenuModelError),
    Hierarchy(MenuTreeError),
    Context(MenuContextError),
    /// The book is full and the browser has no current opening to replace.
    Capacity,
    /// The checked u64 menu id sequence has no value left. Ids never wrap.
    SequenceExhausted,
}

/// A pending opening owned by the book.
///
/// Displaced, taken and drained records carry the original payload back to the
/// caller. `commands` retains only `(command_id, enabled)` for each command of
/// the validated model; its length is bounded by the logical
/// `MenuLimits::max_entries` entry count, not by a `Vec` capacity guarantee.
/// Labels stay in the normalized model returned by [`MenuRequests::open`].
#[derive(Debug)]
pub struct PendingMenu<T> {
    id: ContextMenuRequestId,
    commands: Vec<(i32, bool)>,
    context: Option<Arc<MenuContext>>,
    payload: T,
}

impl<T> PendingMenu<T> {
    pub fn id(&self) -> ContextMenuRequestId {
        self.id
    }

    /// `(command_id, enabled)` for each command in the normalized model.
    pub fn commands(&self) -> &[(i32, bool)] {
        &self.commands
    }

    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Authoritative captured data, not native window/frame liveness proof.
    pub fn context(&self) -> Option<&Arc<MenuContext>> {
        self.context.as_ref()
    }

    /// Returns the payload to its owner without invoking anything.
    pub fn into_payload(self) -> T {
        self.payload
    }

    fn allows_selection(&self, command_id: i32) -> bool {
        // Validation rejects duplicate IDs; contextual metadata already folds
        // in the parent-enabled gate. Checked state never grants selection.
        self.commands
            .iter()
            .any(|(id, enabled)| *id == command_id && *enabled)
    }
}

/// A successfully inserted opening.
#[derive(Debug)]
pub struct OpenedMenu<T> {
    pub id: ContextMenuRequestId,
    /// Normalized owned model for the UI; the only retained label owner.
    pub menu: ContextMenu,
    /// Previous opening for the same browser, if any, with its payload.
    pub displaced: Option<PendingMenu<T>>,
}

/// A contextual opening using the new model. Legacy `open` remains flat.
#[derive(Debug)]
pub struct OpenedMenuModel<T> {
    pub presentation: MenuPresentation,
    pub displaced: Option<PendingMenu<T>>,
}

/// A refused opening. The original payload is always returned.
#[derive(Debug)]
pub struct RejectedMenu<T> {
    pub error: MenuRequestError,
    pub payload: T,
}

/// A reply whose reservation token matched the current request.
#[derive(Debug)]
pub struct CompletedMenu<T> {
    pub id: ContextMenuRequestId,
    pub response: ContextMenuResponse,
    pub context: Option<Arc<MenuContext>>,
    pub payload: T,
}

/// Opaque Copy proof that one reply attempt owns the request.
///
/// Fields are private so callers cannot fabricate a token. The attempt nonce is
/// checked and never reused within one book, which makes a token from an older
/// reservation unable to roll back or consume a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuReplyReservation {
    id: ContextMenuRequestId,
    attempt: u64,
    response: ContextMenuResponse,
}

impl MenuReplyReservation {
    pub fn id(self) -> ContextMenuRequestId {
        self.id
    }

    pub fn attempt(self) -> u64 {
        self.attempt
    }

    pub fn response(self) -> ContextMenuResponse {
        self.response
    }
}

struct PendingSlot<T> {
    menu: PendingMenu<T>,
    /// Attempt nonce of the current reservation, if any.
    reserved_attempt: Option<u64>,
}

/// Bounded, host-owned menu request book. Pure `std` data.
///
/// Construct one book per runtime generation and keep it on the host's chosen
/// thread; every operation takes `&mut self`, so this type is neither a lock nor
/// a synchronization primitive. Capacity bounds how many browsers can have a
/// current opening at once, not bytes.
///
/// Dropping the book drops any payload still pending; callers that own native
/// continuations should [`MenuRequests::drain`] first and end their borrow
/// before those payloads are dropped.
pub struct MenuRequests<T> {
    capacity: NonZeroUsize,
    limits: MenuLimits,
    /// Zero means the menu id sequence is exhausted.
    next_sequence: u64,
    /// Zero means the reservation attempt counter is exhausted.
    next_attempt: u64,
    /// Full browser identity keeps runtime generations apart.
    pending: BTreeMap<BrowserId, PendingSlot<T>>,
}

impl<T> MenuRequests<T> {
    pub fn new(capacity: NonZeroUsize, limits: MenuLimits) -> Self {
        Self {
            capacity,
            limits,
            next_sequence: 1,
            next_attempt: 1,
            pending: BTreeMap::new(),
        }
    }

    /// Validates and normalizes an opening.
    ///
    /// The raw model is validated before normalization and before any existing
    /// opening for `browser` is replaced. Refusal returns the payload; a full
    /// book still permits replacing the same browser. A refused opening never
    /// disturbs the current one.
    ///
    /// Validation applies the logical [`MenuLimits`] to the raw model; it does
    /// not cap allocation and is not a heap measurement.
    pub fn open(
        &mut self,
        browser: BrowserId,
        mut menu: ContextMenu,
        payload: T,
    ) -> Result<OpenedMenu<T>, RejectedMenu<T>> {
        if let Err(error) = menu.validate(self.limits) {
            return Err(RejectedMenu {
                error: MenuRequestError::Model(error),
                payload,
            });
        }

        if let Some(error) = self.insertion_error(browser) {
            return Err(RejectedMenu { error, payload });
        }
        menu.normalize();
        let commands: Vec<(i32, bool)> = menu
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Command {
                    command_id,
                    enabled,
                    ..
                } => Some((*command_id, *enabled)),
                MenuEntry::Separator => None,
            })
            .collect();
        let (id, displaced) = self.insert(browser, commands, None, payload);
        Ok(OpenedMenu {
            id,
            menu,
            displaced,
        })
    }

    /// Validates context and all raw model rows before replacing anything.
    /// One immutable context is shared with UI; authoritative completion keeps
    /// the original even if a consumer mutates its own Arc::make_mut copy.
    pub fn open_model(
        &mut self,
        browser: BrowserId,
        mut model: MenuModel,
        context: MenuContext,
        context_limits: MenuContextLimits,
        payload: T,
    ) -> Result<OpenedMenuModel<T>, RejectedMenu<T>> {
        if let Err(error) = context.validate(browser, context_limits) {
            return Err(RejectedMenu {
                error: MenuRequestError::Context(error),
                payload,
            });
        }
        if (context.x, context.y) != (model.x, model.y) {
            return Err(RejectedMenu {
                error: MenuRequestError::Context(MenuContextError::AnchorMismatch),
                payload,
            });
        }
        if let Err(error) = model.validate(self.limits) {
            return Err(RejectedMenu {
                error: MenuRequestError::Hierarchy(error),
                payload,
            });
        }
        if let Some(error) = self.insertion_error(browser) {
            return Err(RejectedMenu { error, payload });
        }
        model.normalize();
        let commands = model.command_metadata();
        let context = Arc::new(context);
        let (request, displaced) = self.insert(browser, commands, Some(context.clone()), payload);
        Ok(OpenedMenuModel {
            presentation: MenuPresentation {
                request,
                model,
                context,
            },
            displaced,
        })
    }

    fn insertion_error(&self, browser: BrowserId) -> Option<MenuRequestError> {
        if self.next_sequence == 0 {
            Some(MenuRequestError::SequenceExhausted)
        } else if !self.pending.contains_key(&browser) && self.pending.len() >= self.capacity.get()
        {
            Some(MenuRequestError::Capacity)
        } else {
            None
        }
    }

    fn insert(
        &mut self,
        browser: BrowserId,
        commands: Vec<(i32, bool)>,
        context: Option<Arc<MenuContext>>,
        payload: T,
    ) -> (ContextMenuRequestId, Option<PendingMenu<T>>) {
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).unwrap_or(0);
        let id = ContextMenuRequestId { browser, sequence };
        let displaced = self
            .pending
            .insert(
                browser,
                PendingSlot {
                    menu: PendingMenu {
                        id,
                        commands,
                        context,
                        payload,
                    },
                    reserved_attempt: None,
                },
            )
            .map(|slot| slot.menu);
        (id, displaced)
    }

    /// Reserves the one reply slot for the current opening.
    ///
    /// Foreign, stale, zero-sequence, missing and already-reserved requests are
    /// refused. A `Select` must name an enabled command retained in the pending
    /// metadata; `Cancel` is always valid. Refusal does not consume an attempt
    /// nonce. Exhaustion of the checked attempt counter is
    /// [`EngineError::InvalidRequest`].
    pub fn reserve_reply(
        &mut self,
        id: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<MenuReplyReservation, EngineError> {
        self.reserve_reply_inner(id, None, response)
    }

    /// Contextual replies must name the captured window and revision; the
    /// legacy unbound lane cannot answer these requests, even with a valid ID.
    pub fn reserve_context_reply(
        &mut self,
        id: ContextMenuRequestId,
        binding: WindowBinding,
        revision: u64,
        response: ContextMenuResponse,
    ) -> Result<MenuReplyReservation, EngineError> {
        self.reserve_reply_inner(id, Some((binding, revision)), response)
    }

    fn binding_matches(
        context: &Option<Arc<MenuContext>>,
        binding: Option<(WindowBinding, u64)>,
    ) -> bool {
        match (context, binding) {
            (None, None) => true,
            (Some(context), Some((binding, revision))) => {
                context.matches_binding(binding, revision)
            }
            _ => false,
        }
    }

    fn reserve_reply_inner(
        &mut self,
        id: ContextMenuRequestId,
        binding: Option<(WindowBinding, u64)>,
        response: ContextMenuResponse,
    ) -> Result<MenuReplyReservation, EngineError> {
        if id.sequence == 0 {
            return Err(EngineError::InvalidRequest);
        }
        let Some(slot) = self.pending.get_mut(&id.browser) else {
            return Err(EngineError::InvalidRequest);
        };
        if slot.menu.id != id
            || slot.reserved_attempt.is_some()
            || !Self::binding_matches(&slot.menu.context, binding)
        {
            return Err(EngineError::InvalidRequest);
        }
        if let ContextMenuResponse::Select { command_id } = response
            && !slot.menu.allows_selection(command_id)
        {
            return Err(EngineError::InvalidRequest);
        }
        if self.next_attempt == 0 {
            return Err(EngineError::InvalidRequest);
        }

        let attempt = self.next_attempt;
        // u64::MAX is issued once, then attempts are exhausted without wrap.
        self.next_attempt = attempt.checked_add(1).unwrap_or(0);
        slot.reserved_attempt = Some(attempt);

        Ok(MenuReplyReservation {
            id,
            attempt,
            response,
        })
    }

    /// Releases the reservation only when `token` names the current request and
    /// its current attempt. A stale token cannot clear a later retry.
    pub fn rollback(&mut self, token: MenuReplyReservation) -> bool {
        let Some(slot) = self.pending.get_mut(&token.id.browser) else {
            return false;
        };
        if slot.menu.id != token.id || slot.reserved_attempt != Some(token.attempt) {
            return false;
        }
        slot.reserved_attempt = None;
        true
    }

    /// Consumes the exact currently reserved request, returning its id, response
    /// and original payload. A stale token returns `None` and leaves the book
    /// untouched.
    pub fn take_reserved(&mut self, token: MenuReplyReservation) -> Option<CompletedMenu<T>> {
        self.take_reserved_inner(token, None)
    }

    /// Rechecks captured binding metadata at consumption. Actual frame/window
    /// liveness must still be qualified by the native/UI adapter, not this book.
    pub fn take_context_reserved(
        &mut self,
        token: MenuReplyReservation,
        binding: WindowBinding,
        revision: u64,
    ) -> Option<CompletedMenu<T>> {
        self.take_reserved_inner(token, Some((binding, revision)))
    }

    fn take_reserved_inner(
        &mut self,
        token: MenuReplyReservation,
        binding: Option<(WindowBinding, u64)>,
    ) -> Option<CompletedMenu<T>> {
        let current = self.pending.get(&token.id.browser)?;
        if current.menu.id != token.id
            || current.reserved_attempt != Some(token.attempt)
            || !Self::binding_matches(&current.menu.context, binding)
        {
            return None;
        }
        let slot = self.pending.remove(&token.id.browser)?;
        let PendingMenu {
            id,
            context,
            payload,
            ..
        } = slot.menu;
        Some(CompletedMenu {
            id,
            response: token.response,
            context,
            payload,
        })
    }

    /// Removes the current opening for the exact id and returns its payload.
    /// A reservation, if any, becomes stale.
    pub fn take(&mut self, id: ContextMenuRequestId) -> Option<PendingMenu<T>> {
        let matches = self
            .pending
            .get(&id.browser)
            .is_some_and(|slot| slot.menu.id == id);
        if !matches {
            return None;
        }
        self.pending.remove(&id.browser).map(|slot| slot.menu)
    }

    /// Removes the one opening for the full browser identity.
    pub fn take_browser(&mut self, browser: BrowserId) -> Option<PendingMenu<T>> {
        self.pending.remove(&browser).map(|slot| slot.menu)
    }

    /// Removes every pending opening, returning all payloads to the owner.
    pub fn drain(&mut self) -> Vec<PendingMenu<T>> {
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|slot| slot.menu)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::contracts::RuntimeGeneration;

    fn nz(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test limit is nonzero")
    }

    fn limits() -> MenuLimits {
        MenuLimits {
            max_entries: nz(4),
            max_label_bytes: nz(32),
            max_total_label_bytes: nz(128),
        }
    }

    fn menu() -> ContextMenu {
        ContextMenu {
            entries: vec![MenuEntry::Command {
                command_id: 1,
                label: "Item".into(),
                enabled: true,
                checked: false,
            }],
            x: 0,
            y: 0,
        }
    }

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn sequence_exhaustion_is_checked_and_ids_never_wrap() {
        let mut book = MenuRequests::new(nz(2), limits());
        book.next_sequence = u64::MAX;
        let first = book
            .open(browser(1), menu(), "first")
            .expect("last sequence is usable");
        assert_eq!(first.id.sequence, u64::MAX);

        let rejected = book
            .open(browser(2), menu(), "second")
            .expect_err("sequence must be exhausted");
        assert_eq!(rejected.error, MenuRequestError::SequenceExhausted);
        assert_eq!(rejected.payload, "second");

        // Replacing the same browser also needs a fresh sequence; the current
        // opening and its payload survive the refusal.
        let rejected = book
            .open(browser(1), menu(), "third")
            .expect_err("sequence must be exhausted");
        assert_eq!(rejected.error, MenuRequestError::SequenceExhausted);
        assert_eq!(rejected.payload, "third");
        assert_eq!(
            book.take(first.id).expect("opening preserved").payload(),
            &"first"
        );
    }

    #[test]
    fn attempt_exhaustion_is_checked_and_nonces_never_wrap() {
        let mut book = MenuRequests::new(nz(2), limits());
        book.next_attempt = u64::MAX;
        let opened = book
            .open(browser(3), menu(), "fourth")
            .expect("request opens");
        let token = book
            .reserve_reply(opened.id, ContextMenuResponse::Cancel)
            .expect("last attempt is usable");
        assert_eq!(token.attempt(), u64::MAX);
        assert_eq!(
            book.reserve_reply(opened.id, ContextMenuResponse::Cancel),
            Err(EngineError::InvalidRequest)
        );
        assert!(book.rollback(token));
        // Rolling back does not revive an exhausted attempt counter.
        assert_eq!(
            book.reserve_reply(opened.id, ContextMenuResponse::Cancel),
            Err(EngineError::InvalidRequest)
        );
    }
}
