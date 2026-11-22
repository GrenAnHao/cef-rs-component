//! Private accounting only, NOT an enabled DevTools service. No native values or
//! host callbacks are held under this lock. Issued ShowDevTools has no documented
//! failure callback: void return/HasDevTools(false)/parent close never retire it.
use rust_cef_core::{
    contracts::{BrowserId, ProfileHandle, RuntimeGeneration},
    ports::EngineError,
};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Mutex, MutexGuard},
};

pub(crate) mod context;
pub(crate) mod native;
pub(crate) mod prepare;
pub(crate) mod resources;
pub(crate) mod show;
pub(crate) mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuxiliaryId {
    pub owner: BrowserId,
    pub serial: u64,
    sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuxiliaryCloseTicket {
    pub id: AuxiliaryId,
    pub native_id: i32,
    nonce: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Reserved,
    Issued,
    Created(i32),
    BeforeClose(i32),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextCheck {
    Unchecked,
    Unknown,
    Same,
    Different,
}
struct Row {
    id: AuxiliaryId,
    owner_native_id: i32,
    phase: Phase,
    close_requested: bool,
    references_released: bool,
    callback_returned: bool,
    client_profile: Option<ProfileHandle>,
    preissue_abandoned: bool,
    context_live: bool,
    context_check: ContextCheck,
    native_ready: bool,
    native_released: bool,
    host_released: bool,
    next_close: u64,
    close_posted: Option<u64>,
    close_sent: bool,
    close_attempted: bool,
    close_failed: bool,
    /// Result of the browser host's own HasDevTools() association check, sampled
    /// on the CEF UI thread right after ShowDevTools. None = never sampled.
    associated: Option<bool>,
}
struct State {
    capacity: Option<NonZeroUsize>,
    /// Association sampled after an explicit close_dev_tools, kept outside Row so
    /// it survives row retirement.
    after_close: BTreeMap<BrowserId, bool>,
    next: u64,
    draining: bool,
    faulted: bool,
    rows: BTreeMap<BrowserId, Row>,
    close_post_failures: u64,
    /// Serial source for host-initiated opens; never reused, never zero.
    requests: u64,
}
pub(crate) struct DevToolsLedger {
    runtime: RuntimeGeneration,
    state: Mutex<State>,
}
impl DevToolsLedger {
    pub fn new(runtime: RuntimeGeneration) -> Self {
        Self {
            runtime,
            state: Mutex::new(State {
                capacity: None,
                after_close: BTreeMap::new(),
                next: 1,
                requests: 1,
                draining: false,
                faulted: false,
                rows: BTreeMap::new(),
                close_post_failures: 0,
            }),
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| {
            let mut s = e.into_inner();
            s.faulted = true;
            s
        })
    }
    // Configuration is private until the real host-owned service is implemented.
    /// True once the host opted in to managed DevTools capacity.
    pub fn is_configured(&self) -> bool {
        self.lock().capacity.is_some()
    }
    pub fn configure(&self, capacity: NonZeroUsize) -> Result<(), EngineError> {
        let mut s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        if s.faulted || s.capacity.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        s.capacity = Some(capacity);
        Ok(())
    }
    /// Monotonic, non-reusing request serial for one host-initiated open. It is
    /// independent from the AuxiliaryId sequence and refuses to wrap to zero.
    pub fn begin_request(&self) -> Result<u64, EngineError> {
        let mut s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        if s.faulted || s.capacity.is_none() {
            return Err(EngineError::NotReady);
        }
        let serial = s.requests;
        if serial == 0 {
            return Err(EngineError::InvalidRequest);
        }
        s.requests = serial.checked_add(1).unwrap_or(0);
        Ok(serial)
    }
    /// Read-only retirement-tail snapshot for diagnostics. Never mutates.
    pub fn tails(&self, owner: BrowserId) -> Option<String> {
        let s = self.lock();
        s.rows.get(&owner).map(|row| {
            format!(
                "refs={} callback={} native_ready={} native_released={} host_released={} close_sent={} close_failed={} context={} client={}",
                row.references_released, row.callback_returned, row.native_ready,
                row.native_released, row.host_released, row.close_sent, row.close_failed,
                row.context_live, row.client_profile.is_some()
            )
        })
    }
    /// Read-only stage name for diagnostics. Never mutates a row or retires it.
    /// Association observed right after an explicit close_dev_tools.
    pub fn after_close(&self, owner: BrowserId) -> Option<bool> {
        self.lock().after_close.get(&owner).copied()
    }
    pub fn note_after_close(&self, owner: BrowserId, associated: bool) {
        self.lock().after_close.insert(owner, associated);
    }
    /// Auxiliary id currently owned by this Browser, if a row exists.
    pub fn row_id(&self, owner: BrowserId) -> Option<AuxiliaryId> {
        self.lock().rows.get(&owner).map(|row| row.id)
    }
    /// Association observed after ShowDevTools, from the host's HasDevTools().
    pub fn associated(&self, owner: BrowserId) -> Option<bool> {
        self.lock().rows.get(&owner).and_then(|row| row.associated)
    }
    pub fn note_associated(&self, id: AuxiliaryId, associated: bool) {
        let mut s = self.lock();
        if let Some(row) = s.rows.get_mut(&id.owner) {
            if row.id == id {
                row.associated = Some(associated);
            }
        }
    }
    pub fn phase_name(&self, owner: BrowserId) -> Option<&'static str> {
        let s = self.lock();
        s.rows.get(&owner).map(|row| match row.phase {
            Phase::Reserved => "reserved",
            Phase::Issued => "issued",
            Phase::Created(_) => "created",
            Phase::BeforeClose(_) => "before-close",
        })
    }
    /// Pre-native admission for a host-initiated managed DevTools open. Pure
    /// bookkeeping: nothing here calls CEF, acquires a native wrapper or
    /// attests the original Browser; those stay with the UI-side open path.
    pub fn preflight_open(&self, owner: BrowserId) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        if s.capacity.is_none() {
            return Err(EngineError::NotReady);
        }
        if s.rows.contains_key(&owner) {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
    // Future callers must also hold the original Registry lifecycle/admission gate
    // and validate the original menu scope, main native identity and host owner.
    pub fn reserve(
        &self,
        owner: BrowserId,
        serial: u64,
        owner_native_id: i32,
    ) -> Result<AuxiliaryId, EngineError> {
        if owner.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        if owner.local() == 0 || serial == 0 || owner_native_id <= 0 {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        if s.faulted || s.next == 0 {
            return Err(EngineError::InvalidRequest);
        }
        let capacity = s.capacity.ok_or(EngineError::NotReady)?;
        if s.rows.len() >= capacity.get() || s.rows.contains_key(&owner) {
            return Err(EngineError::Backpressure);
        }
        let id = AuxiliaryId {
            owner,
            serial,
            sequence: s.next,
        };
        s.next = s.next.checked_add(1).unwrap_or(0);
        s.rows.insert(
            owner,
            Row {
                id,
                owner_native_id,
                phase: Phase::Reserved,
                close_requested: false,
                references_released: false,
                callback_returned: false,
                client_profile: None,
                preissue_abandoned: false,
                context_live: false,
                context_check: ContextCheck::Unchecked,
                native_ready: false,
                native_released: false,
                host_released: false,
                next_close: 1,
                close_posted: None,
                close_sent: false,
                close_attempted: false,
                close_failed: false,
                associated: None,
            },
        );
        Ok(id)
    }
    fn row(s: &mut State, id: AuxiliaryId) -> Result<&mut Row, EngineError> {
        if s.faulted {
            return Err(EngineError::InvalidRequest);
        }
        s.rows
            .get_mut(&id.owner)
            .filter(|r| r.id == id)
            .ok_or(EngineError::InvalidRequest)
    }
    pub fn client_preflight(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Issued || row.client_profile.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
    /// Records the one lifecycle client for an already-issued operation. The
    /// profile is original metadata, not proof of the auxiliary native context.
    pub fn attach_client(
        &self,
        id: AuxiliaryId,
        profile: ProfileHandle,
    ) -> Result<(), EngineError> {
        if profile.runtime() != id.owner.runtime() {
            return Err(EngineError::WrongRuntime);
        }
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Issued || row.client_profile.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        row.client_profile = Some(profile);
        Ok(())
    }
    /// Build the dedicated Rust-owned interface BEFORE native admission. A
    /// preparation error must not manufacture an irrevocably-issued operation.
    pub fn attach_prepared_client(
        &self,
        id: AuxiliaryId,
        profile: ProfileHandle,
    ) -> Result<(), EngineError> {
        if profile.runtime() != id.owner.runtime() {
            return Err(EngineError::WrongRuntime);
        }
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Reserved || row.client_profile.is_some() || row.preissue_abandoned {
            return Err(EngineError::InvalidRequest);
        }
        if row.close_requested {
            return Err(EngineError::Closing);
        }
        row.client_profile = Some(profile);
        Ok(())
    }
    pub fn preflight_reserved_origin(
        &self,
        id: AuxiliaryId,
        native_owner: i32,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.owner_native_id != native_owner
            || row.phase != Phase::Reserved
            || row.client_profile.is_some()
            || row.preissue_abandoned
        {
            return Err(EngineError::InvalidRequest);
        }
        if row.close_requested {
            return Err(EngineError::Closing);
        }
        Ok(())
    }
    /// Pure abandonment first, interface destruction later and OUTSIDE locks.
    /// An extra interface clone keeps this Reserved row pending until its last
    /// same-UI field is actually gone. Close intent alone never clears a row.
    pub fn abandon_preissue(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Reserved || row.preissue_abandoned {
            return Err(EngineError::InvalidRequest);
        }
        row.preissue_abandoned = true;
        if row.client_profile.is_none() && !row.context_live {
            s.rows.remove(&id.owner);
        }
        Ok(())
    }
    /// Abandon an issued slot that never reached native creation or a client.
    /// This is the only legal way to clear Issued without a native identity:
    /// it refuses if a client, live context, native id or close command exists,
    /// so it can never hide a real auxiliary browser or a posted close.
    pub fn abandon_issued(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Issued
            || row.client_profile.is_some()
            || row.context_live
            || row.native_ready
            || row.close_sent
            || row.close_posted.is_some()
        {
            return Err(EngineError::InvalidRequest);
        }
        s.rows.remove(&id.owner);
        Ok(())
    }
    pub(crate) fn attach_context(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Reserved
            || row.preissue_abandoned
            || row.context_live
            || row.client_profile.is_some()
        {
            return Err(EngineError::InvalidRequest);
        }
        if row.close_requested {
            return Err(EngineError::Closing);
        }
        row.context_live = true;
        Ok(())
    }
    /// Actual original-UI wrapper release must precede this metadata gate.
    pub(super) fn context_released(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if !row.context_live {
            return Err(EngineError::InvalidRequest);
        }
        row.context_live = false;
        if row.phase == Phase::Reserved {
            if row.preissue_abandoned && row.client_profile.is_none() {
                s.rows.remove(&id.owner);
            }
        } else {
            row.references_released = row.native_released && row.host_released;
            if row.references_released && row.callback_returned {
                s.rows.remove(&id.owner);
            }
        }
        Ok(())
    }
    pub(super) fn record_context_match(
        &self,
        id: AuxiliaryId,
        native_id: i32,
        observation: Option<bool>,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if !matches!(row.phase,Phase::Created(n)|Phase::BeforeClose(n) if n==native_id)
            || row.context_check != ContextCheck::Unchecked
        {
            return Err(EngineError::InvalidRequest);
        }
        row.context_check = match observation {
            Some(true) => ContextCheck::Same,
            Some(false) => ContextCheck::Different,
            None => ContextCheck::Unknown,
        };
        Ok(())
    }
    /// True only for a never-issued client; false preserves the issued callback
    /// protocol. Called by the last actual Rust interface/callback field on UI.
    pub fn prepared_client_released(
        &self,
        id: AuxiliaryId,
        profile: ProfileHandle,
    ) -> Result<bool, EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.client_profile != Some(profile) {
            return Err(EngineError::InvalidRequest);
        }
        if row.phase != Phase::Reserved {
            return Ok(false);
        }
        row.client_profile = None;
        if row.preissue_abandoned && !row.context_live {
            s.rows.remove(&id.owner);
        }
        Ok(true)
    }
    pub fn fault(&self) {
        self.lock().faulted = true;
    }
    pub fn faulted(&self) -> bool {
        self.lock().faulted
    }
    pub fn client_returned(&self, id: AuxiliaryId, native_id: i32) -> Result<(), EngineError> {
        {
            let mut s = self.lock();
            if Self::row(&mut s, id)?.client_profile.is_none() {
                return Err(EngineError::InvalidRequest);
            }
        }
        self.callback_returned(id, native_id)
    }
    /// Only a proven pre-native failure may undo this exact reservation.
    pub fn rollback(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Reserved
            || row.client_profile.is_some()
            || row.preissue_abandoned
            || row.context_live
        {
            return Err(EngineError::InvalidRequest);
        }
        s.rows.remove(&id.owner);
        Ok(())
    }
    /// Mark BEFORE calling ShowDevTools, which can synchronously reenter.
    pub fn issued(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Reserved {
            return Err(EngineError::InvalidRequest);
        }
        if row.close_requested || row.preissue_abandoned {
            return Err(EngineError::Closing);
        }
        row.phase = Phase::Issued;
        Ok(())
    }
    /// Return whether the late-created auxiliary must immediately be closed.
    /// The dedicated client must correlate this ID AND actual native identity.
    pub fn created(&self, id: AuxiliaryId, native_id: i32) -> Result<bool, EngineError> {
        let mut s = self.lock();
        if native_id <= 0
            || s.rows.values().any(|r| {
                r.owner_native_id == native_id
                    || matches!(r.phase,Phase::Created(n)|Phase::BeforeClose(n) if n==native_id)
            })
        {
            return Err(EngineError::InvalidRequest);
        }
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Issued || native_id == row.owner_native_id {
            return Err(EngineError::InvalidRequest);
        }
        row.phase = Phase::Created(native_id);
        Ok(row.close_requested)
    }
    /// Intent only. Return pure IDs; callers post after ALL outer locks end.
    pub fn request_close(&self, owner: Option<BrowserId>, serial: Option<u64>) -> Vec<AuxiliaryId> {
        let mut s = self.lock();
        s.rows
            .values_mut()
            .filter(|r| {
                owner.is_none_or(|o| r.id.owner == o) && serial.is_none_or(|v| r.id.serial == v)
            })
            .map(|row| {
                row.close_requested = true;
                row.id
            })
            .collect()
    }
    /// An auxiliary Chrome close command targets this exact operation/native ID,
    /// never a selector that could affect a successor with the same owner/serial.
    pub(super) fn native_command_close(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if native_id <= 0 || row.phase != Phase::Created(native_id) {
            return Err(EngineError::InvalidRequest);
        }
        row.close_requested = true;
        Ok(())
    }
    pub fn stop_admission(&self) -> Vec<AuxiliaryId> {
        let mut s = self.lock();
        s.draining = true;
        s.rows
            .values_mut()
            .map(|row| {
                row.close_requested = true;
                row.id
            })
            .collect()
    }
    pub fn resource_profile(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<ProfileHandle, EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Created(native_id) || row.native_ready {
            return Err(EngineError::InvalidRequest);
        }
        row.client_profile.ok_or(EngineError::NotReady)
    }
    pub fn native_adopted(&self, id: AuxiliaryId, native_id: i32) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Created(native_id) || row.native_ready {
            return Err(EngineError::InvalidRequest);
        }
        row.native_ready = true;
        Ok(())
    }
    pub fn reserve_close(
        &self,
        id: AuxiliaryId,
    ) -> Result<Option<AuxiliaryCloseTicket>, EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        let Phase::Created(native_id) = row.phase else {
            return Ok(None);
        };
        if !row.close_requested
            || !row.native_ready
            || row.close_sent
            || row.close_attempted
            || row.close_posted.is_some()
        {
            return Ok(None);
        }
        if row.next_close == 0 {
            return Err(EngineError::InvalidRequest);
        }
        let nonce = row.next_close;
        row.next_close = nonce.checked_add(1).unwrap_or(0);
        row.close_posted = Some(nonce);
        row.close_attempted = true;
        Ok(Some(AuxiliaryCloseTicket {
            id,
            native_id,
            nonce,
        }))
    }
    #[cfg(test)]
    pub fn rollback_close(&self, ticket: AuxiliaryCloseTicket) -> bool {
        let mut s = self.lock();
        let Ok(row) = Self::row(&mut s, ticket.id) else {
            return false;
        };
        if row.phase != Phase::Created(ticket.native_id) || row.close_posted != Some(ticket.nonce) {
            return false;
        }
        row.close_posted = None;
        true
    }
    pub fn close_post_failed(&self, ticket: AuxiliaryCloseTicket) {
        let mut s = self.lock();
        if let Ok(row) = Self::row(&mut s, ticket.id) {
            if row.phase == Phase::Created(ticket.native_id)
                && row.close_posted == Some(ticket.nonce)
            {
                row.close_posted = None;
                row.close_failed = true;
            }
        }
        if let Some(next) = s.close_post_failures.checked_add(1) {
            s.close_post_failures = next;
        } else {
            s.faulted = true;
        }
    }
    /// Only a NEW explicit host retry may reopen admission after post rejection.
    /// Lifecycle supplements/drain/duplicate close intents never call this.
    pub fn retry_close(&self, id: AuxiliaryId) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if !matches!(row.phase, Phase::Created(_))
            || !row.close_requested
            || !row.native_ready
            || row.close_sent
            || row.close_posted.is_some()
            || !row.close_failed
        {
            return Err(EngineError::InvalidRequest);
        }
        row.close_failed = false;
        row.close_attempted = false;
        Ok(())
    }
    pub fn close_post_failures(&self) -> u64 {
        self.lock().close_post_failures
    }
    pub fn take_close(&self, ticket: AuxiliaryCloseTicket) -> bool {
        let mut s = self.lock();
        let Ok(row) = Self::row(&mut s, ticket.id) else {
            return false;
        };
        if row.phase != Phase::Created(ticket.native_id)
            || row.close_posted != Some(ticket.nonce)
            || row.close_sent
            || !row.native_ready
        {
            return false;
        }
        row.close_posted = None;
        row.close_sent = true;
        true
    }
    pub fn native_references_released(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.resource_released(id, native_id, true)
    }
    pub(super) fn window_capture_ready(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Created(native_id) || !row.native_ready || row.host_released {
            return Err(EngineError::NotReady);
        }
        Ok(())
    }
    pub(super) fn window_retirement_ready(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<bool, EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.host_released {
            return Err(EngineError::InvalidRequest);
        }
        match row.phase {
            Phase::Created(n) if n == native_id => Ok(false),
            Phase::BeforeClose(n) if n == native_id => {
                Ok(row.native_released && row.callback_returned)
            }
            _ => Err(EngineError::InvalidRequest),
        }
    }
    /// Real window/resource owner only; never inferred from CEF closure.
    /// Native Chrome windows own their rendering; an OSR host would additionally
    /// need its own render-resource gate and is NOT supported by this service.
    pub(in crate::devtools) fn host_resources_released(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.resource_released(id, native_id, false)
    }
    fn resource_released(
        &self,
        id: AuxiliaryId,
        native_id: i32,
        native: bool,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::BeforeClose(native_id) {
            return Err(EngineError::InvalidRequest);
        }
        let flag = if native {
            &mut row.native_released
        } else {
            &mut row.host_released
        };
        if *flag {
            return Err(EngineError::InvalidRequest);
        }
        *flag = true;
        if row.native_released && row.host_released && !row.context_live {
            row.references_released = true;
            if row.callback_returned {
                s.rows.remove(&id.owner);
            }
        }
        Ok(())
    }
    pub fn before_close(&self, id: AuxiliaryId, native_id: i32) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::Created(native_id) {
            return Err(EngineError::InvalidRequest);
        }
        row.phase = Phase::BeforeClose(native_id);
        row.close_requested = true;
        Ok(())
    }
    fn finish_part(
        &self,
        id: AuxiliaryId,
        native_id: i32,
        references: bool,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        let row = Self::row(&mut s, id)?;
        if row.phase != Phase::BeforeClose(native_id) {
            return Err(EngineError::InvalidRequest);
        }
        let flag = if references {
            &mut row.references_released
        } else {
            &mut row.callback_returned
        };
        if *flag {
            return Err(EngineError::InvalidRequest);
        }
        *flag = true;
        if row.references_released && row.callback_returned {
            s.rows.remove(&id.owner);
        }
        Ok(())
    }
    /// Caller must have actually retired all native references/host client work,
    /// outside locks. The model itself cannot establish that native fact.
    /// The caller has actually released every durable native reference for this
    /// auxiliary (our Browser wrapper), outside all locks. Recorded after
    /// OnBeforeClose so the row can finish once the client tail also returns.
    pub(crate) fn references_released(
        &self,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.finish_part(id, native_id, true)
    }
    /// Future dedicated-client tail fence, not an in-OnBeforeClose notification.
    pub fn callback_returned(&self, id: AuxiliaryId, native_id: i32) -> Result<(), EngineError> {
        self.finish_part(id, native_id, false)
    }
    pub fn pending(&self) -> u64 {
        let s = self.lock();
        s.rows.len() as u64 + u64::from(s.faulted)
    }
    pub fn pending_for(&self, owner: BrowserId) -> u64 {
        let s = self.lock();
        u64::from(s.rows.contains_key(&owner)) + u64::from(s.faulted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(n: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), n)
    }
    fn book(n: usize) -> DevToolsLedger {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        b.configure(NonZeroUsize::new(n).unwrap()).unwrap();
        b
    }
    #[test]
    fn issued_without_native_can_be_abandoned_but_not_after_native_attach() {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        b.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        let id = b.reserve(owner(1), 3, 101).unwrap();
        b.issued(id).unwrap();
        assert!(b.abandon_issued(id).is_ok());
        assert!(b.preflight_open(owner(1)).is_ok());
        let again = b.reserve(owner(1), 4, 101).unwrap();
        b.issued(again).unwrap();
        let _ = b.created(again, 201);
        assert!(matches!(
            b.abandon_issued(again),
            Err(EngineError::InvalidRequest)
        ));
    }
    #[test]
    fn host_open_request_serials_are_monotonic_and_closed_by_default() {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        assert!(matches!(b.begin_request(), Err(EngineError::NotReady)));
        b.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        let first = b.begin_request().unwrap();
        let second = b.begin_request().unwrap();
        assert!(first >= 1 && second > first);
        b.stop_admission();
        assert!(matches!(b.begin_request(), Err(EngineError::Closing)));
    }
    #[test]
    fn host_open_preflight_is_closed_by_default_and_single_instance() {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        assert!(matches!(
            b.preflight_open(owner(1)),
            Err(EngineError::NotReady)
        ));
        b.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        assert!(b.preflight_open(owner(1)).is_ok());
        let a = b.reserve(owner(1), 1, 101).unwrap();
        assert!(matches!(
            b.preflight_open(owner(1)),
            Err(EngineError::InvalidRequest)
        ));
        b.issued(a).unwrap();
        b.stop_admission();
        assert!(matches!(
            b.preflight_open(owner(2)),
            Err(EngineError::Closing)
        ));
    }
    #[test]
    fn configuration_is_single_shot_and_never_reopens_after_drain() {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        assert!(b.configure(NonZeroUsize::new(1).unwrap()).is_ok());
        assert!(matches!(
            b.configure(NonZeroUsize::new(1).unwrap()),
            Err(EngineError::InvalidRequest)
        ));
        let c = DevToolsLedger::new(RuntimeGeneration::new(1));
        c.stop_admission();
        assert!(matches!(
            c.configure(NonZeroUsize::new(1).unwrap()),
            Err(EngineError::Closing)
        ));
    }
    #[test]
    fn auxiliary_close_intent_on_either_side_of_native_readiness_is_not_lost() {
        let b = book(2);
        let a = b.reserve(owner(1), 1, 101).unwrap();
        b.issued(a).unwrap();
        assert!(!b.created(a, 201).unwrap());
        b.request_close(Some(owner(1)), Some(1));
        assert!(b.reserve_close(a).unwrap().is_none());
        b.native_adopted(a, 201).unwrap();
        assert!(b.reserve_close(a).unwrap().is_some());
        let peer = b.reserve(owner(2), 2, 102).unwrap();
        b.issued(peer).unwrap();
        assert!(!b.created(peer, 202).unwrap());
        b.native_adopted(peer, 202).unwrap();
        assert!(b.reserve_close(peer).unwrap().is_none());
        b.request_close(Some(owner(2)), Some(2));
        assert!(b.reserve_close(peer).unwrap().is_some());
        assert_eq!(b.pending(), 2);
    }
    #[test]
    fn auxiliary_close_nonce_rollback_cannot_clear_new_explicit_retry() {
        let b = book(2);
        let a = b.reserve(owner(1), 1, 101).unwrap();
        b.issued(a).unwrap();
        b.created(a, 201).unwrap();
        b.native_adopted(a, 201).unwrap();
        b.request_close(Some(owner(1)), None);
        let first = b.reserve_close(a).unwrap().unwrap();
        assert!(b.reserve_close(a).unwrap().is_none());
        b.close_post_failed(first);
        assert_eq!(b.close_post_failures(), 1);
        assert!(b.reserve_close(a).unwrap().is_none());
        b.retry_close(a).unwrap();
        let retry = b.reserve_close(a).unwrap().unwrap();
        assert_ne!(first, retry);
        assert!(!b.rollback_close(first));
        assert!(!b.take_close(first));
        assert!(b.take_close(retry));
        assert!(!b.take_close(retry));
        assert!(b.reserve_close(a).unwrap().is_none());
        assert_eq!(b.pending(), 1);
        let peer = b.reserve(owner(2), 2, 102).unwrap();
        b.issued(peer).unwrap();
        b.created(peer, 202).unwrap();
        b.native_adopted(peer, 202).unwrap();
        b.request_close(Some(owner(2)), None);
        b.lock().rows.get_mut(&owner(2)).unwrap().next_close = u64::MAX;
        let last = b.reserve_close(peer).unwrap().unwrap();
        b.close_post_failed(last);
        b.retry_close(peer).unwrap();
        assert_eq!(b.reserve_close(peer), Err(EngineError::InvalidRequest));
        assert_eq!(b.pending(), 2);
    }
    #[test]
    fn auxiliary_native_host_and_client_retirement_remain_independent() {
        let b = book(1);
        let a = b.reserve(owner(1), 1, 101).unwrap();
        b.issued(a).unwrap();
        b.created(a, 201).unwrap();
        b.before_close(a, 201).unwrap();
        b.native_references_released(a, 201).unwrap();
        b.callback_returned(a, 201).unwrap();
        assert_eq!(b.pending(), 1);
        assert_eq!(
            b.native_references_released(a, 201),
            Err(EngineError::InvalidRequest)
        );
        b.host_resources_released(a, 201).unwrap();
        assert_eq!(b.pending(), 0);
        let next = b.reserve(owner(1), 2, 101).unwrap();
        b.issued(next).unwrap();
        b.created(next, 202).unwrap();
        b.before_close(next, 202).unwrap();
        b.host_resources_released(next, 202).unwrap();
        b.callback_returned(next, 202).unwrap();
        assert_eq!(b.pending(), 1);
        assert_eq!(
            b.native_references_released(a, 201),
            Err(EngineError::InvalidRequest)
        );
        b.native_references_released(next, 202).unwrap();
        assert_eq!(b.pending(), 0);
    }
    #[test]
    fn devtools_bounds_default_off_and_exact_preissue_rollback() {
        let b = DevToolsLedger::new(RuntimeGeneration::new(1));
        assert_eq!(b.reserve(owner(1), 1, 101), Err(EngineError::NotReady));
        b.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        assert_eq!(
            b.reserve(BrowserId::new(RuntimeGeneration::new(2), 1), 1, 101),
            Err(EngineError::WrongRuntime)
        );
        let id = b.reserve(owner(1), 1, 101).unwrap();
        assert_eq!(b.reserve(owner(2), 1, 102), Err(EngineError::Backpressure));
        b.request_close(Some(owner(1)), Some(1));
        assert_eq!(b.issued(id), Err(EngineError::Closing));
        b.rollback(id).unwrap();
        let next = b.reserve(owner(1), 2, 101).unwrap();
        assert_ne!(id, next);
        assert_eq!(b.rollback(id), Err(EngineError::InvalidRequest));
        b.issued(next).unwrap();
        assert_eq!(b.rollback(next), Err(EngineError::InvalidRequest));
        assert_eq!(b.pending(), 1);
    }
    #[test]
    fn devtools_late_created_needs_close_callback_release_and_tail() {
        let b = book(2);
        let a = b.reserve(owner(1), 4, 101).unwrap();
        let peer = b.reserve(owner(2), 5, 102).unwrap();
        b.issued(a).unwrap();
        b.issued(peer).unwrap();
        b.request_close(Some(owner(1)), Some(3));
        assert!(!b.created(peer, 202).unwrap());
        b.request_close(Some(owner(1)), Some(4));
        assert!(b.created(a, 201).unwrap());
        assert_eq!(
            b.references_released(a, 201),
            Err(EngineError::InvalidRequest)
        );
        b.before_close(a, 201).unwrap();
        assert_eq!(b.pending_for(owner(1)), 1);
        b.callback_returned(a, 201).unwrap();
        assert_eq!(b.pending_for(owner(1)), 1);
        b.references_released(a, 201).unwrap();
        assert_eq!(b.pending_for(owner(1)), 0);
        assert_eq!(b.pending_for(owner(2)), 1);
        assert_eq!(b.before_close(a, 201), Err(EngineError::InvalidRequest));
    }
    #[test]
    fn devtools_unknown_identity_and_sequence_exhaustion_never_alias() {
        let b = book(2);
        let a = b.reserve(owner(1), 1, 101).unwrap();
        let peer = b.reserve(owner(2), 2, 102).unwrap();
        b.issued(a).unwrap();
        b.issued(peer).unwrap();
        assert_eq!(b.created(a, 101), Err(EngineError::InvalidRequest));
        assert_eq!(b.created(a, 102), Err(EngineError::InvalidRequest));
        b.created(a, 201).unwrap();
        assert_eq!(b.created(peer, 201), Err(EngineError::InvalidRequest));
        assert_eq!(b.before_close(a, 202), Err(EngineError::InvalidRequest));
        assert_eq!(b.pending(), 2);
        b.before_close(a, 201).unwrap();
        b.references_released(a, 201).unwrap();
        assert_eq!(
            b.references_released(a, 201),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(b.pending(), 2);
        b.callback_returned(a, 201).unwrap();
        b.lock().next = u64::MAX;
        let last = b.reserve(owner(1), 3, 101).unwrap();
        b.rollback(last).unwrap();
        assert_eq!(
            b.reserve(owner(1), 4, 101),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(b.pending_for(owner(2)), 1);
    }
    #[test]
    fn devtools_drain_and_poison_never_forge_retirement() {
        let b = book(1);
        let a = b.reserve(owner(1), 1, 101).unwrap();
        b.issued(a).unwrap();
        b.stop_admission();
        assert_eq!(b.reserve(owner(2), 2, 102), Err(EngineError::Closing));
        assert!(b.created(a, 201).unwrap());
        assert_eq!(b.rollback(a), Err(EngineError::InvalidRequest));
        assert_eq!(b.pending(), 1);
        let _ = std::panic::catch_unwind(|| {
            let _guard = b.state.lock().unwrap();
            panic!("intentional ledger poison");
        });
        assert_eq!(b.pending(), 2);
        assert_eq!(b.before_close(a, 201), Err(EngineError::InvalidRequest));
        assert_eq!(b.pending(), 2);
    }
}
