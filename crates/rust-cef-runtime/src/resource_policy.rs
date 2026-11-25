//! Request-scoped IO decisions, including redirect, external-protocol refusal,
//! and origin-gated bounded request interception.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]
use crate::{client::BrowserClientState, policy_native::NativePolicy};
use cef::*;
use rust_cef_core::{
    contracts::{
        BrowserId, ResourceLoadCompleteInfo, ResourceResponseInfo, truncate_utf8_bytes,
    },
    custom_schemes::{
        InterceptAction, InterceptRequest, MAX_INTERCEPT_HEADERS, MAX_INTERCEPT_POST_BODY_BYTES,
        MAX_SCHEME_ASSET_BYTES, ResourceInterceptor, ResponseBodyFilter, ResponseFilterStep,
        validate_intercept_header, validate_mime_type,
    },
    host_capabilities::{CookieAccessPolicy, ProtocolExecutionDecision},
    ports::{BrowserEvent, EXT_EVENT_RESOURCE_OBSERVE, PolicyAction},
};
use std::sync::{Arc, Mutex};

struct InterceptedResponse {
    status_code: u16,
    mime_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn extract_post_body(request: &Request) -> Option<Vec<u8>> {
    let post_data = request.post_data()?;
    let count = post_data.element_count().min(32);
    if count == 0 {
        return None;
    }
    let mut elements = vec![None; count];
    post_data.elements(Some(&mut elements));
    let mut body = Vec::new();
    for elem in elements.into_iter().flatten() {
        if elem.get_type() != PostdataelementType::BYTES {
            continue;
        }
        let len = elem.bytes_count();
        if len == 0 {
            continue;
        }
        let take = len.min(MAX_INTERCEPT_POST_BODY_BYTES.saturating_sub(body.len()));
        if take == 0 {
            break;
        }
        let mut chunk = vec![0u8; take];
        let copied = elem.bytes(take, chunk.as_mut_ptr());
        chunk.truncate(copied.min(take));
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_INTERCEPT_POST_BODY_BYTES {
            break;
        }
    }
    (!body.is_empty()).then_some(body)
}

fn build_response_info(
    request: Option<&mut Request>,
    response: Option<&mut Response>,
    is_main_frame: bool,
) -> ResourceResponseInfo {
    let url = request
        .as_ref()
        .map(|r| truncate_utf8_bytes(&CefString::from(&r.url()).to_string(), 2048))
        .unwrap_or_default();
    let method = request
        .as_ref()
        .map(|r| truncate_utf8_bytes(&CefString::from(&r.method()).to_string(), 32))
        .unwrap_or_default();
    let status_code = response.as_ref().map_or(0, |r| r.status());
    let mime_type = response
        .as_ref()
        .map(|r| truncate_utf8_bytes(&CefString::from(&r.mime_type()).to_string(), 128))
        .unwrap_or_default();
    ResourceResponseInfo {
        url,
        method,
        is_main_frame,
        status_code,
        mime_type,
    }
}

wrap_response_filter! {
    struct RuntimeResponseFilter {
        inner: Arc<Mutex<Box<dyn ResponseBodyFilter>>>,
    }

    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            i32::from(guard.init())
        }

        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let input_slice: &[u8] = data_in.as_deref().map_or(&[], Vec::as_slice);
            let empty_out: &mut [u8] = &mut [];
            let output_slice: &mut [u8] = data_out.map_or(empty_out, Vec::as_mut_slice);
            // CRITICAL: `cef-rs` copies back `vec_data_out[..vec_data_out.len()]` into
            // CEF's output buffer, so `data_out` length must never be shrunk/cleared.
            let (read, written, step) = {
                let mut guard = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.filter(input_slice, output_slice)
            };
            let clamped_read = read.min(input_slice.len());
            let clamped_written = written.min(output_slice.len());
            if let Some(out_read) = data_in_read {
                *out_read = clamped_read;
            }
            if let Some(out_written) = data_out_written {
                *out_written = clamped_written;
            }
            match step {
                ResponseFilterStep::NeedMoreData => ResponseFilterStatus::NEED_MORE_DATA,
                ResponseFilterStep::Done => ResponseFilterStatus::DONE,
                ResponseFilterStep::Error => ResponseFilterStatus::ERROR,
            }
        }
    }
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
        false,
        Arc::new(Mutex::new(None)),
    )
}
pub(crate) fn context_handler(policy: Arc<NativePolicy>) -> ResourceRequestHandler {
    RuntimeResourcePolicy::new(policy, None, None, false, false, Arc::new(Mutex::new(None)))
}
pub(crate) fn popup_handler(
    policy: Arc<NativePolicy>,
    owner: BrowserId,
    main_navigation: bool,
) -> ResourceRequestHandler {
    RuntimeResourcePolicy::new(
        policy,
        Some(owner),
        None,
        main_navigation,
        main_navigation,
        Arc::new(Mutex::new(None)),
    )
}

wrap_resource_request_handler! {
    struct RuntimeResourcePolicy {
        policy: Arc<NativePolicy>,
        browser: Option<BrowserId>,
        state: Option<Arc<BrowserClientState>>,
        main_navigation: bool,
        allow_about_blank_nav: bool,
        intercepted_response: Arc<Mutex<Option<InterceptedResponse>>>,
    }
    impl ResourceRequestHandler {
        fn cookie_access_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
        ) -> Option<CookieAccessFilter> {
            self.effective_cookie_access()
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
                None => {
                    self.policy.rules.allows_network(&url)
                        || self.policy.rules.allows_open_web_blob(&url)
                        || self.policy.rules.allows_open_web_data(&url)
                        || (!self.main_navigation
                            && self.policy.rules.allows_open_web_blank(&url, true, false))
                        || (self.allow_about_blank_nav && url == "about:blank")
                }
            };
            if !allowed {
                self.policy.deny(self.browser, PolicyAction::Resource);
                return ReturnValue::CANCEL;
            }
            if let Some(interceptor) = self.effective_interceptor() {
                let method = CefString::from(&request.method()).to_string();
                let post_body = extract_post_body(request);
                match interceptor.intercept(&InterceptRequest {
                    url,
                    method,
                    is_main_frame: self.main_navigation,
                    post_body,
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
                let target = new_url.to_string();
                let allowed = match self.state.as_ref() {
                    Some(state) => state.allows_resource(&target, self.main_navigation),
                    None => {
                        self.policy.rules.allows_network(&target)
                            || self.policy.rules.allows_open_web_blob(&target)
                            || self.policy.rules.allows_open_web_data(&target)
                            || (!self.main_navigation
                                && self.policy.rules.allows_open_web_blank(&target, true, false))
                    }
                };
                if !allowed {
                    self.policy.deny(self.browser, PolicyAction::Redirect);
                    // Invalid strings are ignored by the fixed implementation.
                    // Use a valid non-network target; the restarted request is
                    // denied by this handler.
                    let _ = new_url.try_set("about:blank");
                }
            }
        }

        fn on_resource_response(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> ::std::os::raw::c_int {
            let interceptor = self.effective_interceptor();
            let wants_event = self
                .state
                .as_ref()
                .is_some_and(|s| s.wants_extended_event(EXT_EVENT_RESOURCE_OBSERVE));
            if interceptor.is_none() && !wants_event {
                return 0;
            }
            let info = build_response_info(request, response, self.main_navigation);
            if let Some(interceptor) = interceptor {
                interceptor.on_resource_response(&info);
            }
            if wants_event && let Some(state) = self.state.as_ref() {
                state.navigation_event(BrowserEvent::ResourceResponseObserved(info));
            }
            0
        }

        fn resource_response_filter(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> Option<ResponseFilter> {
            let interceptor = self.effective_interceptor()?;
            let info = build_response_info(request, response, self.main_navigation);
            let body_filter = interceptor.response_filter(&info)?;
            Some(RuntimeResponseFilter::new(Arc::new(Mutex::new(body_filter))))
        }

        fn on_resource_load_complete(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
            status: UrlrequestStatus,
            received_content_length: i64,
        ) {
            let interceptor = self.effective_interceptor();
            let wants_event = self
                .state
                .as_ref()
                .is_some_and(|s| s.wants_extended_event(EXT_EVENT_RESOURCE_OBSERVE));
            if interceptor.is_none() && !wants_event {
                return;
            }
            let base = build_response_info(request, response, self.main_navigation);
            let complete = ResourceLoadCompleteInfo {
                url: base.url,
                method: base.method,
                is_main_frame: base.is_main_frame,
                status_code: base.status_code,
                ok: status == UrlrequestStatus::SUCCESS,
                received_content_length,
            };
            if let Some(interceptor) = interceptor {
                interceptor.on_resource_load_complete(&complete);
            }
            if wants_event && let Some(state) = self.state.as_ref() {
                state.navigation_event(BrowserEvent::ResourceLoadCompleted(complete));
            }
        }

        fn on_protocol_execution(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            request: Option<&mut Request>, allow_os_execution: Option<&mut ::std::os::raw::c_int>) {
            let url = request
                .as_ref()
                .map(|r| CefString::from(&r.url()).to_string())
                .unwrap_or_default();
            if let Some(state) = self.state.as_ref()
                && let Some(filter) = state.routing.protocol_execution_filter()
                && !url.is_empty()
                && filter(state.browser, &url) == ProtocolExecutionDecision::AllowOsExecution
            {
                if let Some(allow) = allow_os_execution {
                    *allow = 1;
                }
                return;
            }
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
            self.policy.deny(self.browser, PolicyAction::ExternalProtocol);
        }
    }
}

impl RuntimeResourcePolicy {
    fn effective_interceptor(&self) -> Option<Arc<dyn ResourceInterceptor>> {
        self.state
            .as_ref()
            .and_then(|s| s.effective_resource_interceptor())
            .or_else(|| self.policy.interceptor())
    }

    fn effective_cookie_access(&self) -> Option<Arc<dyn CookieAccessPolicy>> {
        self.state
            .as_ref()
            .and_then(|s| s.effective_host_capabilities().cookie_access)
            .or_else(|| self.policy.host_capabilities().cookie_access)
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

    #[test]
    fn open_web_popup_handler_permits_blobs_and_subframe_blanks_without_opening_strict_policy() {
        let owner = BrowserId::new(rust_cef_core::contracts::RuntimeGeneration::new(1), 1);
        let open_policy = NativePolicy::new(
            Arc::new(
                CompiledPolicy::compile(&TrustedContentPolicy::allow_all_http_https(true))
                    .unwrap(),
            ),
            std::sync::Weak::new(),
        );
        let strict_policy = NativePolicy::new(
            Arc::new(
                CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec!["https://allowed.local".into()],
                    allow_embedded_data: true,
                })
                .unwrap(),
            ),
            std::sync::Weak::new(),
        );

        let open_sub = popup_handler(open_policy.clone(), owner, false);
        let strict_sub = popup_handler(strict_policy, owner, false);
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        if let Some(mut req) = cef::request_create() {
            for url in [
                "blob:https://cn.bing.com/subresource",
                "about:blank",
                "about:srcdoc",
            ] {
                req.set_url(Some(&CefString::from(url)));
                assert_eq!(
                    open_sub.on_before_resource_load(None, None, Some(&mut req), None),
                    ReturnValue::CONTINUE,
                    "open_sub url={url}"
                );
                assert_eq!(
                    strict_sub.on_before_resource_load(None, None, Some(&mut req), None),
                    ReturnValue::CANCEL,
                    "strict_sub url={url}"
                );
            }
            req.set_url(Some(&CefString::from("blob:null/subresource")));
            assert_eq!(
                open_sub.on_before_resource_load(None, None, Some(&mut req), None),
                ReturnValue::CANCEL
            );
        }
    }

    #[test]
    fn response_filter_rewrites_bytes_without_shrinking_output_buffer() {
        struct UppercaseFilter;
        impl ResponseBodyFilter for UppercaseFilter {
            fn filter(&mut self, input: &[u8], output: &mut [u8]) -> (usize, usize, ResponseFilterStep) {
                let n = input.len().min(output.len());
                for (dst, &src) in output[..n].iter_mut().zip(&input[..n]) {
                    *dst = src.to_ascii_uppercase();
                }
                (n, n, ResponseFilterStep::Done)
            }
        }
        struct FilterInterceptor;
        impl ResourceInterceptor for FilterInterceptor {
            fn intercept(&self, _request: &InterceptRequest) -> InterceptAction {
                InterceptAction::Continue
            }
            fn response_filter(&self, _info: &ResourceResponseInfo) -> Option<Box<dyn ResponseBodyFilter>> {
                Some(Box::new(UppercaseFilter))
            }
        }

        let policy = NativePolicy::new(
            Arc::new(
                CompiledPolicy::compile(&TrustedContentPolicy {
                    allowed_origins: vec!["https://*".into()],
                    allow_embedded_data: false,
                })
                .unwrap(),
            ),
            std::sync::Weak::new(),
        );
        policy.set_interceptor(Some(Arc::new(FilterInterceptor)));
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        let handler = context_handler(policy);
        let filter = handler
            .resource_response_filter(None, None, None, None)
            .expect("filter provided");
        assert_eq!(filter.init_filter(), 1);
        let mut data_in = b"hello cef".to_vec();
        let mut read = 0usize;
        let mut data_out = vec![0u8; 16];
        let mut written = 0usize;
        let status = filter.filter(
            Some(&mut data_in),
            Some(&mut read),
            Some(&mut data_out),
            Some(&mut written),
        );
        assert_eq!(status, ResponseFilterStatus::DONE);
        assert_eq!(read, 9);
        assert_eq!(written, 9);
        assert_eq!(data_out.len(), 16);
        assert_eq!(&data_out[..9], b"HELLO CEF");
    }
}
