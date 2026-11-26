//! Immutable decisions and a browser-context network backstop. No host/UI waits
//! occur while choosing a URL policy result. This is not an untrusted sandbox.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use crate::{policy::CompiledPolicy, registry::UiRegistry};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    custom_schemes::ResourceInterceptor,
    host_capabilities::HostCapabilities,
    js_bridge::{MAX_JS_JSON_DEPTH, MAX_JS_PAYLOAD_BYTES, validate_bounded_json},
    ports::{EngineError, PolicyAction},
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub(crate) const POLICY_ACTIONS: usize = 12;
pub const MAX_JS_REPOSITORY_GLOBAL_NAME_BYTES: usize = 128;
pub const MAX_JS_REPOSITORY_OBJECTS: usize = 64;
pub const MAX_JS_REPOSITORY_METHODS_PER_OBJECT: usize = 256;
pub const MAX_JS_REPOSITORY_PROPERTIES_PER_OBJECT: usize = 128;
pub const MAX_JS_REPOSITORY_DEPTH: usize = 4;

/// Naming style for generated JS repository API methods on custom global objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsRepositoryMethodStyle {
    /// Follow CefSharp's convention (`JavascriptBindingSettings.cs:71-73`):
    /// lowercase-initial global names expose camelCase methods; uppercase-initial names expose PascalCase.
    #[default]
    AutoByGlobalName,
    CamelCase,
    PascalCase,
}

/// Pre-registered property descriptor for a bound JS repository object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsRepositoryPropertyDescriptor {
    pub name: String,
    /// Validated bounded JSON literal (or encoded `Date` marker) for initial value.
    pub value_json: String,
    pub readonly: bool,
}

/// Descriptor for an object pre-registered into the renderer V8 repository bootstrap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsRepositoryObjectDescriptor {
    pub name: String,
    pub is_async: bool,
    pub methods: Vec<String>,
    pub properties: Vec<JsRepositoryPropertyDescriptor>,
    pub sub_objects: Vec<JsRepositoryObjectDescriptor>,
}

/// Opt-in runtime configuration for renderer-side JS Object Repository bootstrap.
///
/// Mirrors CefSharp's `JavascriptBindingSettings`
/// (`D:\System Dir\Downloads\CefSharp\CefSharp\JavascriptBinding\JavascriptBindingSettings.cs:14-140`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsRepositorySettings {
    pub enabled: bool,
    /// Custom global object name on `window`/`globalThis`.
    /// When `None`, preserves default aliases (`window['cef-rs-gpui']`, `window.CefRsGpui`, `window.cefRsGpui`).
    pub global_object_name: Option<String>,
    pub method_style: JsRepositoryMethodStyle,
    /// Automatically bind pre-registered objects when a V8 context is created.
    pub legacy_binding_enabled: bool,
    pub objects: Vec<JsRepositoryObjectDescriptor>,
}

impl Default for JsRepositorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            global_object_name: None,
            method_style: JsRepositoryMethodStyle::AutoByGlobalName,
            legacy_binding_enabled: false,
            objects: Vec::new(),
        }
    }
}

pub(crate) fn validate_js_repository_name(
    name: &str,
    allow_hyphen: bool,
) -> Result<(), EngineError> {
    if name.is_empty() || name.len() > MAX_JS_REPOSITORY_GLOBAL_NAME_BYTES {
        return Err(EngineError::InvalidRequest);
    }
    if matches!(name, "__proto__" | "constructor" | "prototype") {
        return Err(EngineError::InvalidRequest);
    }
    let bytes = name.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return Err(EngineError::InvalidRequest);
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || (allow_hyphen && b == b'-'))
    {
        return Err(EngineError::InvalidRequest);
    }
    Ok(())
}

fn validate_object_descriptor(
    desc: &JsRepositoryObjectDescriptor,
    depth: usize,
) -> Result<(), EngineError> {
    if depth >= MAX_JS_REPOSITORY_DEPTH {
        return Err(EngineError::InvalidRequest);
    }
    validate_js_repository_name(&desc.name, false)?;
    if desc.methods.len() > MAX_JS_REPOSITORY_METHODS_PER_OBJECT
        || desc.properties.len() > MAX_JS_REPOSITORY_PROPERTIES_PER_OBJECT
        || desc.sub_objects.len() > MAX_JS_REPOSITORY_OBJECTS
    {
        return Err(EngineError::InvalidRequest);
    }
    for method in &desc.methods {
        validate_js_repository_name(method, false)?;
    }
    for prop in &desc.properties {
        validate_js_repository_name(&prop.name, false)?;
        validate_bounded_json(&prop.value_json, MAX_JS_PAYLOAD_BYTES, MAX_JS_JSON_DEPTH)
            .map_err(|_| EngineError::InvalidRequest)?;
    }
    for sub in &desc.sub_objects {
        validate_object_descriptor(sub, depth + 1)?;
    }
    Ok(())
}

impl JsRepositorySettings {
    pub fn validate(&self) -> Result<(), EngineError> {
        if let Some(ref name) = self.global_object_name {
            validate_js_repository_name(name, true)?;
        }
        if self.objects.len() > MAX_JS_REPOSITORY_OBJECTS {
            return Err(EngineError::InvalidRequest);
        }
        for obj in &self.objects {
            validate_object_descriptor(obj, 0)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn uses_camel_case_methods(&self) -> bool {
        match self.method_style {
            JsRepositoryMethodStyle::CamelCase => true,
            JsRepositoryMethodStyle::PascalCase => false,
            JsRepositoryMethodStyle::AutoByGlobalName => self
                .global_object_name
                .as_deref()
                .is_some_and(|name| name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())),
        }
    }
}

pub(crate) struct NativePolicy {
    pub(crate) rules: Arc<CompiledPolicy>,
    registry: Weak<UiRegistry>,
    denied: [AtomicU64; POLICY_ACTIONS],
    without_browser: AtomicU64,
    factory_without_browser: AtomicU64,
    interceptor: RwLock<Option<Arc<dyn ResourceInterceptor>>>,
    host_capabilities: RwLock<HostCapabilities>,
    forbidden_roots: RwLock<Vec<PathBuf>>,
    allow_chrome_internal_urls: AtomicBool,
    js_bridge_allow_subframes: AtomicBool,
    next_browser_legacy_binding: AtomicBool,
    js_repository_settings: RwLock<Option<JsRepositorySettings>>,
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
            allow_chrome_internal_urls: AtomicBool::new(false),
            js_bridge_allow_subframes: AtomicBool::new(false),
            next_browser_legacy_binding: AtomicBool::new(false),
            js_repository_settings: RwLock::new(None),
        })
    }

    pub(crate) fn set_js_repository_settings(
        &self,
        settings: Option<JsRepositorySettings>,
    ) -> Result<(), EngineError> {
        if let Some(ref cfg) = settings {
            cfg.validate()?;
        }
        let mut guard = self
            .js_repository_settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = settings;
        Ok(())
    }

    pub(crate) fn js_repository_settings(&self) -> Option<JsRepositorySettings> {
        self.js_repository_settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_allow_chrome_internal_urls(&self, enabled: bool) {
        self.allow_chrome_internal_urls
            .store(enabled, Ordering::Release);
    }

    pub(crate) fn allows_chrome_internal_url(&self, url: &str) -> bool {
        self.allow_chrome_internal_urls.load(Ordering::Acquire)
            && crate::policy::allows_chrome_internal_url_syntax(url)
    }

    pub(crate) fn set_js_bridge_allow_subframes(&self, enabled: bool) {
        self.js_bridge_allow_subframes
            .store(enabled, Ordering::Release);
    }

    pub(crate) fn js_bridge_allow_subframes(&self) -> bool {
        self.js_bridge_allow_subframes.load(Ordering::Acquire)
    }

    pub(crate) fn set_next_browser_legacy_binding(&self, enabled: bool) {
        self.next_browser_legacy_binding
            .store(enabled, Ordering::Release);
    }

    pub(crate) fn take_next_browser_legacy_binding(&self) -> bool {
        self.next_browser_legacy_binding
            .swap(false, Ordering::AcqRel)
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
