//! Request-scoped IO decisions, including redirect, external-protocol refusal,
//! and origin-gated bounded request interception.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]
use crate::{client::BrowserClientState, policy_native::NativePolicy};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    custom_schemes::{
        InterceptAction, InterceptRequest, MAX_INTERCEPT_HEADERS, MAX_SCHEME_ASSET_BYTES,
        validate_intercept_header, validate_mime_type,
    },
    ports::PolicyAction,
};
use std::sync::{Arc, Mutex};

struct InterceptedResponse {
    status_code: u16,
    mime_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

pub(crate) fn browser_handler(
    state: Arc<BrowserClientState>,
    main_navigation: bool,
) -> ResourceRequestHandler {
    RuntimeResourcePolicy::new(
        state.policy.clone(),
        Some(state.browser),
        Some(state),
        main_navigation,
        Arc::new(Mutex::new(None)),
    )
}
pub(crate) fn context_handler(policy: Arc<NativePolicy>) -> ResourceRequestHandler {
    RuntimeResourcePolicy::new(policy, None, None, false, Arc::new(Mutex::new(None)))
}

wrap_resource_request_handler! {
    struct RuntimeResourcePolicy {
        policy: Arc<NativePolicy>,
        browser: Option<BrowserId>,
        state: Option<Arc<BrowserClientState>>,
        main_navigation: bool,
        intercepted_response: Arc<Mutex<Option<InterceptedResponse>>>,
    }
    impl ResourceRequestHandler {
        fn cookie_access_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
        ) -> Option<CookieAccessFilter> {
            self.policy
                .host_capabilities()
                .cookie_access
                .map(|policy| crate::profile_cookies::cookie_access_filter(self.browser, policy))
        }

        fn on_before_resource_load(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            request: Option<&mut Request>, _callback: Option<&mut Callback>) -> ReturnValue {
            let Some(request) = request else {
                self.policy.deny(self.browser, PolicyAction::Resource);
                return ReturnValue::CANCEL;
            };
            let url = CefString::from(&request.url()).to_string();
            let allowed = match self.state.as_ref() {
                Some(state) => state.allows_resource(&url, self.main_navigation),
                None => self.policy.rules.allows_network(&url),
            };
            if !allowed {
                self.policy.deny(self.browser, PolicyAction::Resource);
                return ReturnValue::CANCEL;
            }
            if let Some(interceptor) = self.policy.interceptor() {
                let method = CefString::from(&request.method()).to_string();
                match interceptor.intercept(&InterceptRequest {
                    url,
                    method,
                    is_main_frame: self.main_navigation,
                }) {
                    InterceptAction::Continue => {}
                    InterceptAction::Cancel => {
                        return ReturnValue::CANCEL;
                    }
                    InterceptAction::AddHeaders(headers) => {
                        for (name, value) in headers.into_iter().take(MAX_INTERCEPT_HEADERS) {
                            if validate_intercept_header(&name, &value) {
                                request.set_header_by_name(
                                    Some(&CefString::from(name.as_str())),
                                    Some(&CefString::from(value.as_str())),
                                    1,
                                );
                            }
                        }
                    }
                    InterceptAction::Respond {
                        status_code,
                        mime_type,
                        headers,
                        body,
                    } => {
                        if !(100..=599).contains(&status_code)
                            || !validate_mime_type(&mime_type)
                            || body.len() > MAX_SCHEME_ASSET_BYTES
                        {
                            return ReturnValue::CANCEL;
                        }
                        let clean_headers: Vec<(String, String)> = headers
                            .into_iter()
                            .take(MAX_INTERCEPT_HEADERS)
                            .filter(|(k, v)| validate_intercept_header(k, v))
                            .collect();
                        *self
                            .intercepted_response
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(InterceptedResponse {
                                status_code,
                                mime_type,
                                headers: clean_headers,
                                body,
                            });
                    }
                }
            }
            // cef-rs's default here is CANCEL, unlike the C++ default.
            ReturnValue::CONTINUE
        }

        fn resource_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            self.intercepted_response
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map(|resp| {
                    crate::scheme_handlers::memory_resource_handler(
                        resp.status_code,
                        resp.mime_type,
                        resp.headers,
                        resp.body,
                    )
                })
        }

        fn on_resource_redirect(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            _request: Option<&mut Request>, _response: Option<&mut Response>, new_url: Option<&mut CefString>) {
            if let Some(new_url) = new_url {
                if !self.policy.rules.allows_network(&new_url.to_string()) {
                    self.policy.deny(self.browser, PolicyAction::Redirect);
                    // Invalid strings are ignored by the fixed implementation.
                    // Use a valid non-network target; the restarted request is
                    // denied by this handler.
                    let _ = new_url.try_set("about:blank");
                }
            }
        }
        fn on_protocol_execution(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            _request: Option<&mut Request>, allow_os_execution: Option<&mut ::std::os::raw::c_int>) {
            if let Some(allow) = allow_os_execution { *allow = 0; }
            self.policy.deny(self.browser, PolicyAction::ExternalProtocol);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::CompiledPolicy;
    use rust_cef_core::contracts::TrustedContentPolicy;
    use rust_cef_core::custom_schemes::{
        CustomSchemeConfig, CustomSchemeOptions, ResourceInterceptor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestInterceptor {
        calls: AtomicUsize,
        mode: &'static str,
    }

    impl ResourceInterceptor for TestInterceptor {
        fn intercept(&self, _request: &InterceptRequest) -> InterceptAction {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                "respond" => InterceptAction::Respond {
                    status_code: 201,
                    mime_type: "application/json".into(),
                    headers: vec![
                        ("X-Custom".into(), "ok".into()),
                        ("Host".into(), "evil.test".into()),
                    ],
                    body: br#"{"intercepted":true}"#.to_vec(),
                },
                "cancel" => InterceptAction::Cancel,
                _ => InterceptAction::Continue,
            }
        }
    }

    #[test]
    fn interceptor_never_runs_for_disallowed_origin_and_serves_permitted_memory_stream() {
        let schemes = vec![CustomSchemeConfig {
            scheme: "app".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        }];
        let policy = NativePolicy::new(
            Arc::new(
                CompiledPolicy::compile_with_custom_schemes(
                    &TrustedContentPolicy {
                        allowed_origins: vec!["app://local".into()],
                        allow_embedded_data: false,
                    },
                    &schemes,
                )
                .unwrap(),
            ),
            std::sync::Weak::new(),
        );
        let interceptor = Arc::new(TestInterceptor {
            calls: AtomicUsize::new(0),
            mode: "respond",
        });
        policy.set_interceptor(Some(interceptor.clone()));

        let handler = context_handler(policy.clone());
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

        // Missing/disallowed request is rejected BEFORE invoking interceptor.
        assert_eq!(
            handler.on_before_resource_load(None, None, None, None),
            ReturnValue::CANCEL
        );
        assert_eq!(interceptor.calls.load(Ordering::SeqCst), 0);

        if let Some(mut req) = cef::request_create() {
            req.set_url(Some(&CefString::from("https://evil.invalid/steal")));
            assert_eq!(
                handler.on_before_resource_load(None, None, Some(&mut req), None),
                ReturnValue::CANCEL
            );
            assert_eq!(interceptor.calls.load(Ordering::SeqCst), 0);

            req.set_url(Some(&CefString::from("app://local/intercepted.json")));
            assert_eq!(
                handler.on_before_resource_load(None, None, Some(&mut req), None),
                ReturnValue::CONTINUE
            );
            assert_eq!(interceptor.calls.load(Ordering::SeqCst), 1);

            let resource = handler
                .resource_handler(None, None, Some(&mut req))
                .expect("responder created ResourceHandler");
            let mut buf = [0u8; 64];
            let mut n = 0;
            assert_eq!(resource.read(buf.as_mut_ptr(), 64, Some(&mut n), None), 1);
            assert_eq!(&buf[..n as usize], br#"{"intercepted":true}"#);
            assert_eq!(resource.read(buf.as_mut_ptr(), 64, Some(&mut n), None), 0);
        }
    }

    #[test]
    fn cookie_access_filter_is_absent_by_default_and_enforces_opt_in_policy() {
        use rust_cef_core::host_capabilities::{
            CookieAccessPolicy, CookieFilterDecision, CookieFilterRequest, HostCapabilities,
        };
        struct BlockSession;
        impl CookieAccessPolicy for BlockSession {
            fn can_send_cookie(&self, req: &CookieFilterRequest) -> CookieFilterDecision {
                if req.cookie_name == "blocked" {
                    CookieFilterDecision::Block
                } else {
                    CookieFilterDecision::Allow
                }
            }
            fn can_save_cookie(&self, req: &CookieFilterRequest) -> CookieFilterDecision {
                if req.cookie_name == "blocked" {
                    CookieFilterDecision::Block
                } else {
                    CookieFilterDecision::Allow
                }
            }
        }
        let policy = NativePolicy::new(
            Arc::new(
                CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec![],
                    allow_embedded_data: false,
                })
                .unwrap(),
            ),
            std::sync::Weak::new(),
        );
        let handler = context_handler(policy.clone());
        assert!(handler.cookie_access_filter(None, None, None).is_none());

        policy.set_host_capabilities(HostCapabilities {
            cookie_access: Some(Arc::new(BlockSession)),
            ..Default::default()
        });
        let filter = handler
            .cookie_access_filter(None, None, None)
            .expect("opt-in filter");
        let blocked = cef::Cookie {
            size: std::mem::size_of::<cef::sys::_cef_cookie_t>(),
            name: CefString::from("blocked"),
            ..Default::default()
        };
        let allowed = cef::Cookie {
            size: std::mem::size_of::<cef::sys::_cef_cookie_t>(),
            name: CefString::from("ok"),
            ..Default::default()
        };
        assert_eq!(filter.can_send_cookie(None, None, None, Some(&blocked)), 0);
        assert_eq!(
            filter.can_save_cookie(None, None, None, None, Some(&blocked)),
            0
        );
        assert_eq!(filter.can_send_cookie(None, None, None, Some(&allowed)), 1);
    }
}
