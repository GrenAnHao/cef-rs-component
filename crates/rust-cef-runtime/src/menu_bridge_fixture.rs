//! Explicit-only windowless native fixture target. This is NOT a UI owner or
//! production presenter; it uses the public production Runtime binding path.
use rust_cef_core::{
    menu::ContextMenuRequestId,
    menu_context::MenuPresentation,
    menu_lease::{MenuLease, MenuLeaseStamp},
    ports::{ContextMenuDeliverySink, ContextMenuResponseSink, MenuDeliveryError},
};
use std::sync::{Arc, Mutex};
pub struct BridgeDelivery {
    pub presentation: MenuPresentation,
    pub response: Arc<dyn ContextMenuResponseSink>,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BridgeFixtureSnapshot {
    pub published: u64,
    pub terminal: u64,
    pub outstanding: u64,
    pub last_sequence: u64,
    pub frame_id_bytes: usize,
    pub selection_bytes: Vec<usize>,
    pub terminal_ids: Vec<u64>,
    pub first_error: Option<String>,
}
pub struct MenuBridgeFixture {
    lease: MenuLease,
    state: Mutex<(Option<BridgeDelivery>, BridgeFixtureSnapshot)>,
}
impl MenuBridgeFixture {
    pub fn new(lease: MenuLease) -> Arc<Self> {
        Arc::new(Self {
            lease,
            state: Mutex::new((None, BridgeFixtureSnapshot::default())),
        })
    }
    pub fn take(&self) -> Option<BridgeDelivery> {
        self.state.lock().unwrap().0.take()
    }
    pub fn snapshot(&self) -> BridgeFixtureSnapshot {
        self.state.lock().unwrap().1.clone()
    }
}
impl ContextMenuDeliverySink for MenuBridgeFixture {
    fn capture(&self) -> Result<MenuLeaseStamp, MenuDeliveryError> {
        self.lease.capture().map_err(MenuDeliveryError::Lease)
    }
    fn publish(
        &self,
        presentation: MenuPresentation,
        response: Arc<dyn ContextMenuResponseSink>,
        stamp: MenuLeaseStamp,
    ) -> Result<(), MenuDeliveryError> {
        if !self
            .lease
            .is_current(&stamp)
            .map_err(MenuDeliveryError::Lease)?
        {
            return Err(MenuDeliveryError::Stale);
        }
        let mut s = self.state.lock().unwrap();
        if s.0.is_some() || s.1.outstanding != 0 || s.1.published >= 3 {
            s.1.first_error
                .get_or_insert("fixture publication overlap/limit".into());
            return Err(MenuDeliveryError::Backpressure);
        }
        s.1.published += 1;
        s.1.outstanding = 1;
        s.1.last_sequence = presentation.request.sequence;
        s.1.frame_id_bytes = presentation.context.frame_id.len();
        s.1.selection_bytes
            .push(presentation.context.selection.len());
        s.0 = Some(BridgeDelivery {
            presentation,
            response,
        });
        Ok(())
    }
    fn dismiss(&self, id: ContextMenuRequestId) -> Result<(), MenuDeliveryError> {
        let old = {
            let mut s = self.state.lock().unwrap();
            if id.sequence != s.1.last_sequence
                || s.1.outstanding != 1
                || s.1.terminal_ids.len() >= 3
            {
                s.1.first_error
                    .get_or_insert("fixture terminal correlation/limit".into());
                return Err(MenuDeliveryError::WrongScope);
            }
            s.1.outstanding = 0;
            s.1.terminal += 1;
            s.1.terminal_ids.push(id.sequence);
            s.0.take()
        };
        drop(old);
        Ok(())
    }
}
pub fn fixture_url() -> String {
    let html = "<body style='margin:0;font:24px Arial;background:rgb(26,76,126)'><span>menu bridge selection</span><script>document.title='menu-bridge-ready';document.addEventListener('selectionchange',()=>{document.title=window.getSelection().toString().trim()==='menu bridge selection'?'menu-bridge-selected':'menu-bridge-empty';});</script></body>";
    let mut url = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}
