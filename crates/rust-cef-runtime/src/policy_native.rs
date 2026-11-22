//! Immutable decisions and a browser-context network backstop. No host/UI waits
//! occur while choosing a URL policy result. This is not an untrusted sandbox.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use crate::{policy::CompiledPolicy, registry::UiRegistry};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId, custom_schemes::ResourceInterceptor, host_capabilities::HostCapabilities,
    ports::PolicyAction,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) const POLICY_ACTIONS: usize = 12;

pub(crate) struct NativePolicy {
    pub(crate) rules: Arc<CompiledPolicy>,
    registry: Weak<UiRegistry>,
    denied: [AtomicU64; POLICY_ACTIONS],
    without_browser: AtomicU64,
    factory_without_browser: AtomicU64,
    interceptor: RwLock<Option<Arc<dyn ResourceInterceptor>>>,
    host_capabilities: RwLock<HostCapabilities>,
    forbidden_roots: RwLock<Vec<PathBuf>>,
}

impl NativePolicy {
    pub(crate) fn new(rules: Arc<CompiledPolicy>, registry: Weak<UiRegistry>) -> Arc<Self> {
        Arc::new(Self {
            rules,
            registry,
            denied: std::array::from_fn(|_| AtomicU64::new(0)),
            without_browser: AtomicU64::new(0),
            factory_without_browser: AtomicU64::new(0),
            interceptor: RwLock::new(None),
            host_capabilities: RwLock::new(HostCapabilities::default()),
            forbidden_roots: RwLock::new(Vec::new()),
        })
    }

    pub(crate) fn set_interceptor(&self, interceptor: Option<Arc<dyn ResourceInterceptor>>) {
        let mut guard = self
            .interceptor
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = interceptor;
    }

    pub(crate) fn interceptor(&self) -> Option<Arc<dyn ResourceInterceptor>> {
        self.interceptor
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn clear_interceptor(&self) {
        self.set_interceptor(None);
    }

    pub(crate) fn set_host_capabilities(&self, capabilities: HostCapabilities) {
        let mut guard = self
            .host_capabilities
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = capabilities;
    }

    pub(crate) fn host_capabilities(&self) -> HostCapabilities {
        self.host_capabilities
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn clear_host_capabilities(&self) {
        self.set_host_capabilities(HostCapabilities::default());
    }

    pub(crate) fn add_forbidden_root(&self, root: PathBuf) {
        if root.as_os_str().is_empty() {
            return;
        }
        let mut guard = self
            .forbidden_roots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.iter().any(|existing| existing == &root) {
            guard.push(root);
        }
    }

    pub(crate) fn forbidden_roots(&self) -> Vec<PathBuf> {
        self.forbidden_roots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn deny(&self, browser: Option<BrowserId>, action: PolicyAction) {
        self.denied[action as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(browser) = browser {
            if let Some(registry) = self.registry.upgrade() {
                registry.note_policy_denial(browser, action);
            }
        } else {
            self.without_browser.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> ([u64; POLICY_ACTIONS], u64) {
        (
            std::array::from_fn(|index| self.denied[index].load(Ordering::Acquire)),
            self.without_browser.load(Ordering::Acquire),
        )
    }

    pub(crate) fn factory_without_browser(&self) -> u64 {
        self.factory_without_browser.load(Ordering::Acquire)
    }

    pub(crate) fn factory(self: &Arc<Self>) -> SchemeHandlerFactory {
        PolicySchemeFactory::new(self.clone())
    }
}

wrap_scheme_handler_factory! {
    struct PolicySchemeFactory { policy: Arc<NativePolicy>, }
    impl SchemeHandlerFactory {
        fn create(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            if browser.is_none() {
                self.policy.factory_without_browser.fetch_add(1, Ordering::Relaxed);
            }
            let allowed = request.is_some_and(|request| {
                self.policy.rules.allows_network(&CefString::from(&request.url()).to_string())
            });
            if allowed {
                // Null selects Chromium's normal network implementation; it is
                // NOT the denial result for a standard HTTP/HTTPS scheme.
                None
            } else {
                self.policy.deny(None, PolicyAction::Resource);
                Some(CancelNetworkResource::new())
            }
        }
    }
}

wrap_resource_handler! {
    pub(crate) struct CancelNetworkResource;
    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(handle_request) = handle_request { *handle_request = 1; }
            // Fixed SDK: false + handle_request=true cancels synchronously.
            // No URLLoader/network fallback and no borrowed/native state kept.
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_backstop_denial_cancels_instead_of_returning_default_network() {
        let rules = Arc::new(
            CompiledPolicy::compile(&rust_cef_core::contracts::TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: true,
            })
            .unwrap(),
        );
        let policy = NativePolicy::new(rules, Weak::new());
        let factory = policy.factory();
        // A missing request must fail closed with a real cancellation handler;
        // this is not a native URLLoader/network or service-worker proof.
        let handler = factory
            .create(None, None, None, None)
            .expect("deny requires a real handler");
        let mut handled = 0;
        assert_eq!(handler.open(None, Some(&mut handled), None), 0);
        assert_eq!(handled, 1);
        assert_eq!(policy.snapshot().0[PolicyAction::Resource as usize], 1);
    }
}
