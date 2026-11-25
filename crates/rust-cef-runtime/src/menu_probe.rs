//! Explicit test-support only windowless Native callback witness. Never a
//! default presenter/service or Win32/GPUI input acceptance path.
use crate::{
    client::BrowserClientState,
    executor::current_thread_id,
    menu_context::{CaptureScope, copy_cef_request},
};
use cef::*;
use rust_cef_core::{
    menu::MenuLimits,
    menu_context::MenuContextLimits,
    menu_model::{MenuChild, MenuItem},
    ports::WindowBinding,
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use crate::test_support::{MenuProbeSnapshot, MenuWitness};
struct DeferredScope {
    lease: rust_cef_core::menu_lease::MenuLease,
    reply: Option<(
        rust_cef_core::menu::ContextMenuRequestId,
        WindowBinding,
        u64,
    )>,
}
pub(crate) struct MenuProbe(Mutex<MenuProbeSnapshot>, Mutex<Option<DeferredScope>>);
impl MenuProbe {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(MenuProbeSnapshot::default()), Mutex::new(None))
    }
    pub(crate) fn enable_deferred(
        &self,
        lease: rust_cef_core::menu_lease::MenuLease,
    ) -> Result<(), rust_cef_core::ports::EngineError> {
        lease
            .capture()
            .map_err(|_| rust_cef_core::ports::EngineError::InvalidRequest)?;
        let mut s = self.0.lock().unwrap();
        if s.input_rounds != 0 || s.retired || s.deferred {
            return Err(rust_cef_core::ports::EngineError::InvalidRequest);
        }
        *self.1.lock().unwrap() = Some(DeferredScope { lease, reply: None });
        s.deferred = true;
        Ok(())
    }
    fn deferred(&self) -> bool {
        self.0.lock().unwrap().deferred
    }
    fn capture_stamp(&self) -> Result<rust_cef_core::menu_lease::MenuLeaseStamp, String> {
        let lease = self
            .1
            .lock()
            .unwrap()
            .as_ref()
            .map(|x| x.lease.clone())
            .ok_or("missing deferred lease")?;
        lease
            .capture()
            .map_err(|e| format!("deferred lease capture: {e:?}"))
    }
    fn retained(&self, p: &rust_cef_core::menu_context::MenuPresentation) {
        self.1
            .lock()
            .unwrap()
            .as_mut()
            .expect("configured deferred scope")
            .reply = Some((p.request, p.context.binding, p.context.binding_revision));
    }
    pub(crate) fn reply_scope(
        &self,
        browser: rust_cef_core::contracts::BrowserId,
    ) -> Result<
        (
            rust_cef_core::menu::ContextMenuRequestId,
            WindowBinding,
            u64,
        ),
        rust_cef_core::ports::EngineError,
    > {
        self.1
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|x| x.reply)
            .filter(|x| x.0.browser == browser)
            .ok_or(rust_cef_core::ports::EngineError::InvalidRequest)
    }
    fn dismissed_work(&self, work: u64, active: bool) {
        let mut s = self.0.lock().unwrap();
        if !active || work == 0 || s.dismissal_work.len() >= 3 {
            s.error
                .get_or_insert("deferred cancellation lost work/scope accounting".into());
            return;
        }
        s.dismissal_work.push(work);
    }
    pub(crate) fn snapshot(&self) -> MenuProbeSnapshot {
        self.0.lock().unwrap().clone()
    }
    pub(crate) fn enabled(&self) -> bool {
        self.0.lock().unwrap().enabled
    }
    fn was_used(&self) -> bool {
        self.0.lock().unwrap().input_rounds != 0
    }
    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.0.lock().unwrap().error.get_or_insert(error.into());
    }
    pub(crate) fn start_input(&self) -> bool {
        let mut s = self.0.lock().unwrap();
        if s.retired
            || s.error.is_some()
            || s.input_rounds >= if s.deferred { 3 } else { 2 }
            || s.input_rounds != s.cancel_returns
            || s.answering.is_some()
        {
            s.error
                .get_or_insert("menu probe input outside bounded idle fixture".into());
            return false;
        }
        s.enabled = true;
        s.input_rounds += 1;
        s.cef_ui_thread = current_thread_id();
        true
    }
    pub(crate) fn sent_mouse(&self) {
        self.0.lock().unwrap().cef_mouse_events += 1;
    }
    pub(crate) fn retire(&self) {
        let mut s = self.0.lock().unwrap();
        if s.answering.is_some() {
            s.error.get_or_insert("retired inside cancel".into());
        }
        s.retired = true;
        s.enabled = false;
    }
    fn built(&self) {
        self.0.lock().unwrap().fixture_builds += 1;
    }
    fn begin(&self) -> Option<u64> {
        let mut s = self.0.lock().unwrap();
        if s.captures >= if s.deferred { 3 } else { 2 } || s.answering.is_some() {
            s.error
                .get_or_insert("unexpected menu callback count/reentry".into());
            return None;
        }
        s.captures += 1;
        Some(s.captures)
    }
    fn copied(&self, w: MenuWitness) {
        let mut s = self.0.lock().unwrap();
        if !w.fixture_faithful || w.anchor != (20, 20) || w.capture_thread != s.cef_ui_thread {
            s.error
                .get_or_insert("native copy fidelity/anchor/thread mismatch".into());
        }
        if s.witnesses.len() >= if s.deferred { 3 } else { 2 } {
            s.error.get_or_insert("menu witness overflow".into());
            return;
        }
        s.copies += 1;
        s.witnesses.push(w);
    }
    pub(crate) fn cancelling(&self, id: u64) {
        let mut s = self.0.lock().unwrap();
        s.cancel_calls += 1;
        s.answering = Some(id);
    }
    pub(crate) fn returned(&self, id: u64) {
        let mut s = self.0.lock().unwrap();
        if s.answering != Some(id) {
            s.error.get_or_insert("cancel scope changed".into());
        }
        s.answering = None;
        s.cancel_returns += 1;
    }
    fn dismissed(&self) {
        let mut s = self.0.lock().unwrap();
        if s.retired {
            s.error
                .get_or_insert("native dismissal after retirement".into());
        }
        s.dismissed += 1;
        if s.answering.is_some() {
            s.dismissed_during_cancel += 1;
        } else {
            s.uncorrelated_dismissed += 1;
            s.error
                .get_or_insert("uncorrelated native dismissal".into());
        }
    }
}

fn faithful(m: &rust_cef_core::menu_model::MenuModel) -> bool {
    if m.entries.len() != 6 {
        return false;
    }
    let MenuItem::Command(check) = &m.entries[0] else {
        return false;
    };
    let MenuItem::Command(ordinary) = &m.entries[2] else {
        return false;
    };
    let MenuItem::Command(hidden) = &m.entries[3] else {
        return false;
    };
    let MenuItem::Command(radio) = &m.entries[4] else {
        return false;
    };
    let MenuItem::Submenu(g) = &m.entries[5] else {
        return false;
    };
    if g.entries.len() != 3 {
        return false;
    }
    let MenuChild::Command(child) = &g.entries[0] else {
        return false;
    };
    let MenuChild::Command(disabled) = &g.entries[2] else {
        return false;
    };
    check.label == "Probe checked"
        && ordinary.label == "Probe ordinary"
        && hidden.label == "Probe hidden"
        && radio.label == "Probe root radio"
        && g.label == "Probe group"
        && child.label == "Probe child radio"
        && disabled.label == "Probe disabled"
        && ordinary.radio_group.is_none()
        && hidden.enabled
        && hidden.checked.is_none()
        && hidden.radio_group.is_none()
        && radio.enabled
        && radio.visible
        && child.enabled
        && child.visible
        && disabled.visible
        && disabled.checked.is_none()
        && disabled.radio_group.is_none()
        && check.command_id == 26501
        && check.checked == Some(true)
        && check.visible
        && check.enabled
        && check.radio_group.is_none()
        && matches!(m.entries[1], MenuItem::Separator { visible: true })
        && ordinary.command_id == 26502
        && ordinary.checked.is_none()
        && ordinary.visible
        && ordinary.enabled
        && hidden.command_id == 26503
        && !hidden.visible
        && radio.command_id == 26504
        && radio.radio_group == Some(7)
        && radio.checked == Some(false)
        && g.visible
        && g.enabled
        && child.command_id == 26506
        && child.radio_group == Some(7)
        && child.checked == Some(true)
        && matches!(g.entries[1], MenuChild::Separator { visible: true })
        && disabled.command_id == 26507
        && !disabled.enabled
        && !m.allows_selection(26503)
        && !m.allows_selection(26507)
        && m.allows_selection(26502)
}
fn fixture(model: &cef::MenuModel, record: impl Fn()) -> bool {
    macro_rules! call {
        ($e:expr) => {{
            record();
            if $e == 0 {
                return false;
            }
        }};
    }
    call!(model.clear());
    let a = CefString::from("Probe checked");
    call!(model.add_check_item(26501, Some(&a)));
    call!(model.set_checked(26501, 1));
    call!(model.add_separator());
    let a = CefString::from("Probe ordinary");
    call!(model.add_item(26502, Some(&a)));
    let a = CefString::from("Probe hidden");
    call!(model.add_item(26503, Some(&a)));
    call!(model.set_visible(26503, 0));
    let a = CefString::from("Probe root radio");
    call!(model.add_radio_item(26504, Some(&a), 7));
    let a = CefString::from("Probe group");
    record();
    let Some(child) = model.add_sub_menu(26505, Some(&a)) else {
        return false;
    };
    let a = CefString::from("Probe child radio");
    call!(child.add_radio_item(26506, Some(&a), 7));
    call!(child.set_checked(26506, 1));
    call!(child.add_separator());
    let a = CefString::from("Probe disabled");
    call!(child.add_item(26507, Some(&a)));
    call!(child.set_enabled(26507, 0));
    true
}

wrap_context_menu_handler! {
    pub(crate) struct MenuProbeHandler { state: Arc<BrowserClientState> }
    impl ContextMenuHandler {
        fn on_before_context_menu(&self,browser:Option<&mut Browser>,_frame:Option<&mut Frame>,_params:Option<&mut ContextMenuParams>,model:Option<&mut cef::MenuModel>){
            if !self.state.menu_probe.enabled(){return;}
            if currently_on(ThreadId::UI)==0{self.state.menu_probe.fail("fixture wrong CEF thread");return;}
            let Some(model)=model else{self.state.menu_probe.fail("missing native menu model");return};
            let Some(registry)=self.state.registry.upgrade() else{self.state.menu_probe.fail("missing registry");return};
            if !browser.is_some_and(|native|registry.menu_probe_matches(self.state.browser,native)){self.state.menu_probe.fail("fixture native mapping refused");return;}
            if !fixture(model,||registry.menu_probe_native_call()){self.state.menu_probe.fail("native menu fixture construction failed");return;}
            self.state.menu_probe.built();
        }
        fn run_context_menu(&self,browser:Option<&mut Browser>,frame:Option<&mut Frame>,params:Option<&mut ContextMenuParams>,model:Option<&mut cef::MenuModel>,callback:Option<&mut RunContextMenuCallback>)->i32 {
            if !self.state.menu_probe.enabled(){
                self.state.native_context_menu.mark_opened();
                return 0;
            }
            let probe=&self.state.menu_probe;
            let Some(callback)=callback else{probe.fail("missing native continuation");return 1};
            let id=probe.begin();
            let result=(||{
                let (Some(browser),Some(frame),Some(params),Some(model))=(browser,frame,params,model) else{return Err("missing native context argument".to_owned())};
                if currently_on(ThreadId::UI)==0{return Err("capture wrong CEF thread".into());}
                let registry=self.state.registry.upgrade().ok_or("missing registry")?;
                if !registry.menu_probe_matches(self.state.browser,browser){return Err("native/full BrowserId mapping refused".into());}
                let nz=|n|NonZeroUsize::new(n).unwrap();
                let stamp=if probe.deferred(){Some(probe.capture_stamp()?)}else{None};
                let scope=CaptureScope {browser:self.state.browser,binding:stamp.as_ref().map_or(WindowBinding {browser:self.state.browser,window:self.state.browser.local()},|s|s.binding()),revision:stamp.as_ref().map_or(1,|s|s.revision())};
                let (owned,context)=copy_cef_request(scope,browser,frame,params,model,
                    MenuLimits {max_entries:nz(32),max_label_bytes:nz(256),max_total_label_bytes:nz(2048)},
                    MenuContextLimits {max_frame_id_bytes:nz(512),max_text_bytes:nz(4096),max_suggestions:nz(5),max_total_bytes:nz(8192)})
                    .map_err(|e|format!("native owned capture: {e:?}"))?;
                let w=MenuWitness {sequence:id.ok_or("capture id refused")?,anchor:(context.x,context.y),capture_thread:current_thread_id(),browser:(self.state.browser.runtime().value(),self.state.browser.local()),native_browser_id:browser.identifier(),frame_id_bytes:context.frame_id.len(),
                    selection_bytes:context.selection.len(),word_bytes:context.misspelled_word.len(),suggestions:context.dictionary_suggestions.len(),context_total_bytes:context.frame_id.len()+context.selection.len()+context.misspelled_word.len()+context.dictionary_suggestions.iter().map(String::len).sum::<usize>(),
                    root_rows:owned.entries.len(),child_rows:owned.entries.iter().map(|x|if let MenuItem::Submenu(g)=x{g.entries.len()}else{0}).sum(),fixture_faithful:faithful(&owned),binding_metadata_only:true};
                if probe.deferred(){
                    if !registry.menus.can_capture(){return Err("deferred continuation table unavailable".into());}
                    let retained=crate::menu_native::RetainedMenu::capture(&registry.menus,callback,probe.clone());
                    match registry.menus.open(owned,context,MenuContextLimits {max_frame_id_bytes:nz(512),max_text_bytes:nz(4096),max_suggestions:nz(5),max_total_bytes:nz(8192)},retained){
                        Ok((presentation,displaced))=>{
                            if let Some(work)=displaced { probe.fail("fixture unexpectedly replaced an outstanding opening");registry.menu_probe_cancel_work(work); }
                            let mut w=w;w.sequence=presentation.request.sequence;
                            probe.retained(&presentation);probe.copied(w);
                        }
                        Err((e,work))=>{probe.fail(format!("deferred retain refused: {e:?}"));registry.menu_probe_cancel_work(work);}
                    }
                    return Ok::<bool,String>(true);
                }
                probe.copied(w);Ok(false)
            })();
            match result { Ok(true)=>return 1, Err(e)=>probe.fail(e), Ok(false)=>{} }
            // No registry/probe/request/GPUI lock or borrow spans native Cancel.
            // Dismissed is observation only and never takes any current request.
            let id=id.unwrap_or(0);probe.cancelling(id);
            if let Some(registry)=self.state.registry.upgrade(){registry.menu_probe_native_call();}
            callback.cancel();
            probe.returned(id);1
        }
        fn on_context_menu_dismissed(&self,_browser:Option<&mut Browser>,_frame:Option<&mut Frame>){
            if !self.state.menu_probe.enabled(){
                self.state.native_context_menu.mark_dismissed();
                return;
            }
            if self.state.menu_probe.was_used(){
                if self.state.menu_probe.deferred(){
                    if let Some(registry)=self.state.registry.upgrade(){
                        self.state.menu_probe.dismissed_work(registry.pending_requests(),registry.menus.is_active(self.state.browser));
                    }
                }
                self.state.menu_probe.dismissed();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_probe_reentry_dismissal_is_observation_not_request_cleanup() {
        let p = MenuProbe::new();
        assert!(p.start_input());
        let id = p.begin().unwrap();
        p.cancelling(id);
        p.dismissed();
        p.returned(id);
        let s = p.snapshot();
        assert_eq!(s.dismissed_during_cancel, 1);
        assert!(s.answering.is_none());
        assert_eq!(s.cancel_returns, 1);
        p.dismissed();
        assert_eq!(p.snapshot().uncorrelated_dismissed, 1);
        assert_eq!(p.snapshot().cancel_returns, 1);
    }
    #[test]
    fn probe_never_retries_or_resets_failure_and_caps_two_openings() {
        let p = MenuProbe::new();
        assert!(p.start_input());
        assert!(!p.start_input());
        assert!(p.snapshot().error.is_some());
        assert!(!p.start_input());
        let q = MenuProbe::new();
        for _ in 0..2 {
            assert!(q.start_input());
            let id = q.begin().unwrap();
            q.cancelling(id);
            q.dismissed();
            q.returned(id);
        }
        assert!(!q.start_input());
        q.retire();
        assert!(q.snapshot().retired);
        assert!(!q.enabled());
    }
}
