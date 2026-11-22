//! Test-only native dialog control. No remote debugging port or generic CDP API.
//! Observer replies are owned scalars; Registration is retired on CEF UI.
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
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogProbeAction {
    Enable,
    Reset,
    Disable,
}
impl DialogProbeAction {
    fn index(self) -> usize {
        match self {
            Self::Enable => 0,
            Self::Reset => 1,
            Self::Disable => 2,
        }
    }
    fn method(self) -> &'static str {
        match self {
            Self::Enable => "Page.enable",
            Self::Reset => "Page.handleJavaScriptDialog",
            Self::Disable => "Page.disable",
        }
    }
}
fn lock<T>(v: &Mutex<T>) -> MutexGuard<'_, T> {
    v.lock().unwrap_or_else(|e| e.into_inner())
}
#[derive(Default)]
struct Results {
    assigned: [AtomicI32; 3],
    replies: Mutex<BTreeMap<i32, bool>>,
    failed: AtomicBool,
    detached: AtomicBool,
}
impl Results {
    fn record(&self, id: i32, success: bool) {
        let mut replies = lock(&self.replies);
        if id <= 0 || replies.len() >= 32 || replies.insert(id, success).is_some() {
            self.failed.store(true, Ordering::Release);
        }
    }
    fn detach(&self) {
        // Losing the session before the whole enabled/handle/disable sequence
        // completes is sticky failure, even if a late reply later arrives.
        if self.status(DialogProbeAction::Disable) != Some(true) {
            self.failed.store(true, Ordering::Release);
        }
        self.detached.store(true, Ordering::Release);
    }
    fn status(&self, action: DialogProbeAction) -> Option<bool> {
        if self.failed.load(Ordering::Acquire) {
            return Some(false);
        }
        let id = self.assigned[action.index()].load(Ordering::Acquire);
        if id == -2 {
            return Some(false);
        }
        let reply = lock(&self.replies).get(&id).copied();
        if reply.is_some() {
            return reply;
        }
        if self.detached.load(Ordering::Acquire) {
            Some(false)
        } else {
            None
        }
    }
}
wrap_dev_tools_message_observer! {
    struct DialogObserver { expected_native:i32, results:Arc<Results>, }
    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, browser:Option<&mut Browser>, message_id:i32,
            success:i32, _result:Option<&[u8]>) {
            if browser.is_none_or(|browser|browser.identifier()!=self.expected_native) {
                self.results.failed.store(true,Ordering::Release);return;
            }
            // CEF can deliver this synchronously before execute_* returns the
            // actual (possibly adjusted) id. Cache first, associate afterwards.
            self.results.record(message_id,success!=0);
        }
        fn on_dev_tools_agent_detached(&self,_browser:Option<&mut Browser>) {
            self.results.detach();
        }
    }
}
pub(crate) struct DialogProbe {
    results: Arc<Results>,
    registration: Option<Registration>,
}
impl DialogProbe {
    /// Only called on CEF UI, outside registry locks.
    pub(crate) fn new(native: &Browser, host: &BrowserHost, audit: &NativeAudit) -> Self {
        let results = Arc::new(Results::default());
        let mut observer = DialogObserver::new(native.identifier(), results.clone());
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
    pub(crate) fn send(&self, host: &BrowserHost, action: DialogProbeAction, audit: &NativeAudit) {
        if self.registration.is_none()
            || self.results.assigned[action.index()].load(Ordering::Acquire) != 0
        {
            self.results.failed.store(true, Ordering::Release);
            return;
        }
        self.results.assigned[action.index()].store(-1, Ordering::Release);
        let method = CefString::from(action.method());
        let mut params = None;
        if action == DialogProbeAction::Reset {
            audit.record();
            params = dictionary_value_create();
            let Some(params) = params.as_mut() else {
                self.results.failed.store(true, Ordering::Release);
                return;
            };
            audit.record();
            if params.set_bool(Some(&CefString::from("accept")), 0) == 0 {
                self.results.failed.store(true, Ordering::Release);
                return;
            }
        }
        audit.record();
        let actual = host.execute_dev_tools_method(
            10001 + action.index() as i32,
            Some(&method),
            params.as_mut(),
        );
        self.results.assigned[action.index()]
            .store(if actual > 0 { actual } else { -2 }, Ordering::Release);
    }
    pub(crate) fn status(&self, action: DialogProbeAction) -> Option<bool> {
        self.results.status(action)
    }
    pub(crate) fn has_reference(&self) -> bool {
        self.registration.is_some()
    }
    /// UI retirement is explicit so an unfinished Runtime Drop cannot Release
    /// a native registration from an arbitrary owner sequence.
    pub(crate) fn retire(mut self) {
        drop(self.registration.take());
    }
}
impl Drop for DialogProbe {
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
    fn reply_can_precede_adjusted_id_assignment_and_detach_is_not_success() {
        let result = Results::default();
        result.assigned[0].store(-1, Ordering::Release);
        result.record(12000, true);
        assert_eq!(result.status(DialogProbeAction::Enable), None);
        result.assigned[0].store(12000, Ordering::Release);
        assert_eq!(result.status(DialogProbeAction::Enable), Some(true));
        result.detach();
        assert_eq!(result.status(DialogProbeAction::Reset), Some(false));
        result.assigned[1].store(12001, Ordering::Release);
        result.record(12001, true);
        assert_eq!(result.status(DialogProbeAction::Reset), Some(false));
        assert_eq!(result.status(DialogProbeAction::Enable), Some(false));
    }
}
