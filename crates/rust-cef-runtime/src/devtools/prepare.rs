//! Pre-native menu reservation and dedicated interface preparation only.
//! This does NOT call Show, enable a menu capability, or attest native context,
//! window role/ownership or policies. Those gates still belong to the caller.
use super::AuxiliaryId;
use crate::{menu_bridge::Scope, menu_native::MenuWork, registry::UiRegistry};
use rust_cef_core::{
    menu::ContextMenuRequestId, menu_lease::MenuExecutionPermit, ports::EngineError,
};
use std::sync::{Arc, Weak};

/// !Send/!Sync via the original execution permit. No GPUI object or durable
/// parent Browser/Frame/Host. The one original RequestContext is separately
/// counted until actual UI release; client fields cannot clear across it.
pub(crate) struct PreparedAuxiliary {
    id: AuxiliaryId,
    _menu: ContextMenuRequestId,
    scope: Arc<Scope>,
    registry: Weak<UiRegistry>,
    client: Option<cef::Client>,
    context: Option<Arc<super::context::ExpectedContext>>,
    book: Arc<super::DevToolsLedger>,
    _permit: MenuExecutionPermit,
}
impl PreparedAuxiliary {
    pub fn from_menu(registry: &Arc<UiRegistry>, work: &MenuWork) -> Result<Self, EngineError> {
        // Work comes from the original native request table, not a numeric ID
        // or newly captured stamp. No callback/native work occurs in this path.
        let scope = work.scope().ok_or(EngineError::InvalidRequest)?;
        if !scope.admission() {
            return Err(EngineError::InvalidRequest);
        }
        let permit = scope
            .lease
            .try_enter(&scope.stamp)
            .map_err(|_| EngineError::InvalidRequest)?;
        let menu = work.request();
        let id = registry.reserve_auxiliary_menu(&scope, menu)?;
        Ok(Self {
            id,
            _menu: menu,
            scope,
            registry: Arc::downgrade(registry),
            client: None,
            context: None,
            book: registry.devtools.clone(),
            _permit: permit,
        })
    }
    pub fn id(&self) -> AuxiliaryId {
        self.id
    }
    /// Still not native admission. Later cancellation/reentrancy requires a fresh
    /// native frame check and lifecycle commit BEFORE calling Show, not after.
    pub fn prepare_client(&mut self) -> Result<(), EngineError> {
        if self.client.is_some() || self.context.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        if !self.scope.current() {
            return Err(EngineError::InvalidRequest);
        }
        let registry = self.registry.upgrade().ok_or(EngineError::Closed)?;
        self.context = Some(super::context::ExpectedContext::capture(
            &registry,
            self.id,
            &self.scope,
        )?);
        self.client = Some(super::native::prepare(
            &registry,
            self.id,
            &self.scope,
            Arc::downgrade(self.context.as_ref().expect("captured original context")),
        )?);
        Ok(())
    }
}
impl Drop for PreparedAuxiliary {
    fn drop(&mut self) {
        // Original context must release before the client could retire a Reserved
        // row. A transient Context Arc keeps the independent book gate alive.
        drop(self.context.take());
        if self.book.abandon_preissue(self.id).is_err() {
            self.book.fault();
        }
        // Final interface Release can execute Rust destructors/reenter. ALL
        // Registry/ledger/data guards have ended before this Drop.
        drop(self.client.take());
    }
}
