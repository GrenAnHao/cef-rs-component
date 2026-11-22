//! Test-only fixed Page.enable/Page.crash bridge. Never a generic CDP API.
#![cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]
use crate::executor::NativeAudit;
use cef::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RendererProbeAction {
    Enable,
    Crash,
}
#[derive(Default)]
struct Results {
    enable_id: AtomicI32,
    crash_id: AtomicI32,
    replies: Mutex<BTreeMap<i32, bool>>,
    detached: AtomicBool,
    failed: AtomicBool,
}
impl Results {
    fn record(&self, id: i32, success: bool) {
        let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
        if id <= 0 || replies.len() >= 16 || replies.insert(id, success).is_some() {
            self.failed.store(true, Ordering::Release);
        }
    }
    fn enabled(&self) -> Option<bool> {
        if self.failed.load(Ordering::Acquire) {
            return Some(false);
        }
        let id = self.enable_id.load(Ordering::Acquire);
        if id == -2 {
            return Some(false);
        }
        let result = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .copied();
        result.or_else(|| self.detached.load(Ordering::Acquire).then_some(false))
    }
    fn detach(&self) {
        // Unlike the dialog probe, detachment AFTER preflight is expected on
        // crash. It is still not a RendererTerminated or Closed observation.
        if self.enabled() != Some(true) {
            self.failed.store(true, Ordering::Release);
        }
        self.detached.store(true, Ordering::Release);
    }
    fn crash_submitted(&self) -> bool {
        let id = self.crash_id.load(Ordering::Acquire);
        id > 0
            && !self.failed.load(Ordering::Acquire)
            && self
                .replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                != Some(&false)
    }
}
wrap_dev_tools_message_observer! {
    struct RendererObserver { expected_native:i32, results:Arc<Results>, audit:Arc<NativeAudit>, }
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self,browser:Option<&mut Browser>,message_id:i32,success:i32,_result:Option<&[u8]>){
            self.audit.record();
            if browser.is_none_or(|b|b.identifier()!=self.expected_native){self.results.failed.store(true,Ordering::Release);return;}
            self.results.record(message_id,success!=0);
        }
        fn on_dev_tools_agent_detached(&self,_browser:Option<&mut Browser>){self.results.detach();}
    }
}
pub(crate) struct RendererProbe {
    results: Arc<Results>,
    registration: Option<Registration>,
}
impl RendererProbe {
    pub(crate) fn new(native: &Browser, host: &BrowserHost, audit: &Arc<NativeAudit>) -> Self {
        let results = Arc::new(Results::default());
        audit.record();
        let mut observer =
            RendererObserver::new(native.identifier(), results.clone(), audit.clone());
        audit.record();
        let registration = host.add_dev_tools_message_observer(Some(&mut observer));
        if registration.is_none() {
            results.failed.store(true, Ordering::Release);
        }
        Self {
            results,
            registration,
        }
    }
    pub(crate) fn send(
        &self,
        host: &BrowserHost,
        action: RendererProbeAction,
        audit: &NativeAudit,
    ) {
        let (slot, method, hint) = match action {
            RendererProbeAction::Enable => (&self.results.enable_id, "Page.enable", 20001),
            RendererProbeAction::Crash => {
                if self.results.enabled() != Some(true) {
                    self.results.failed.store(true, Ordering::Release);
                    return;
                }
                (&self.results.crash_id, "Page.crash", 20002)
            }
        };
        if self.registration.is_none()
            || slot
                .compare_exchange(0, -1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            self.results.failed.store(true, Ordering::Release);
            return;
        }
        audit.record();
        let id = host.execute_dev_tools_method(hint, Some(&CefString::from(method)), None);
        slot.store(if id > 0 { id } else { -2 }, Ordering::Release);
        // CEF may have delivered a reply before returning the adjusted id. The
        // owned cache above correlates it afterwards. Never await crash success.
    }
    pub(crate) fn enabled(&self) -> Option<bool> {
        self.results.enabled()
    }
    pub(crate) fn crash_submitted(&self) -> bool {
        self.results.crash_submitted()
    }
    pub(crate) fn has_reference(&self) -> bool {
        self.registration.is_some()
    }
    pub(crate) fn retire(mut self) {
        drop(self.registration.take());
    }
}
impl Drop for RendererProbe {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            std::mem::forget(registration);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preflight_handles_reentrant_adjusted_reply_and_crash_detach_is_not_exit_proof() {
        let r = Results::default();
        r.enable_id.store(-1, Ordering::Release);
        r.record(22000, true);
        assert_eq!(r.enabled(), None);
        r.enable_id.store(22000, Ordering::Release);
        assert_eq!(r.enabled(), Some(true));
        r.crash_id.store(22001, Ordering::Release);
        r.detach();
        assert!(r.crash_submitted());
        r.record(22001, false);
        assert!(!r.crash_submitted());
        let early = Results::default();
        early.detach();
        early.enable_id.store(1, Ordering::Release);
        early.record(1, true);
        assert_eq!(early.enabled(), Some(false));
    }
}
