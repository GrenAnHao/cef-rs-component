//! Pure trusted-content URL policy. Origins compile to canonical ASCII
//! scheme/host/port and match by exact `BTreeSet` membership, never
//! prefix/suffix. Exact data content approvals live separately per Browser.

use std::collections::BTreeSet;

use rust_cef_core::{contracts::TrustedContentPolicy, custom_schemes::CustomSchemeConfig};
use url::Url;

/// Configuration error; deliberately carries no raw URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyConfigError {
    EmptyOrigin,
    MalformedOrigin,
    UnsupportedScheme,
    CredentialsNotAllowed,
    NonRootPath,
    QueryOrFragment,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledPolicy {
    allowed_origins: BTreeSet<String>,
    custom_schemes: BTreeSet<String>,
    allow_any_http: bool,
    allow_any_https: bool,
    allow_embedded_data: bool,
}

impl CompiledPolicy {
    #[cfg(test)]
    pub(crate) fn compile(config: &TrustedContentPolicy) -> Result<Self, PolicyConfigError> {
        Self::compile_with_custom_schemes(config, &[])
    }

    pub(crate) fn compile_with_custom_schemes(
        config: &TrustedContentPolicy,
        custom: &[CustomSchemeConfig],
    ) -> Result<Self, PolicyConfigError> {
        let custom_schemes: BTreeSet<String> = custom
            .iter()
            .map(|c| c.scheme.to_ascii_lowercase())
            .collect();
        let mut allowed_origins = BTreeSet::new();
        let mut allow_any_http = false;
        let mut allow_any_https = false;
        for origin in &config.allowed_origins {
            if origin.trim() == origin && origin.eq_ignore_ascii_case("http://*") {
                allow_any_http = true;
                continue;
            }
            if origin.trim() == origin && origin.eq_ignore_ascii_case("https://*") {
                allow_any_https = true;
                continue;
            }
            allowed_origins.insert(compile_origin_with_schemes(origin, &custom_schemes)?);
        }
        Ok(Self {
            allowed_origins,
            custom_schemes,
            allow_any_http,
            allow_any_https,
            allow_embedded_data: config.allow_embedded_data,
        })
    }

    pub(crate) fn is_custom_scheme(&self, scheme: &str) -> bool {
        self.custom_schemes.contains(&scheme.to_ascii_lowercase())
    }

    pub(crate) fn canonical_origin_for(&self, url: &str) -> Option<String> {
        canonical_origin(url, &self.custom_schemes)
    }

    fn matches_compiled_origin(&self, canonical: &str) -> bool {
        self.allowed_origins.contains(canonical)
            || (self.allow_any_https && canonical.starts_with("https://"))
            || (self.allow_any_http && canonical.starts_with("http://"))
    }

    pub(crate) fn allows_canonical_origin(&self, origin: &str) -> bool {
        if self.allowed_origins.contains(origin) {
            return true;
        }
        if !(self.allow_any_https || self.allow_any_http) {
            return false;
        }
        canonical_origin(origin, &self.custom_schemes).as_deref() == Some(origin)
            && self.matches_compiled_origin(origin)
    }

    /// Subresource/network request: exact allowed HTTP(S), WebSocket (`ws:`/`wss:`),
    /// or custom scheme origin only.
    pub(crate) fn allows_network(&self, url: &str) -> bool {
        self.allows_target(url) || self.allows_websocket(url)
    }

    /// Host-selected navigation may approve embedded data content. Page paths
    /// may later revisit only URI bytes present in that Browser's approval set;
    /// this generic host decision must not be used as a page-wide data bypass.
    pub(crate) fn allows_host_navigation(&self, url: &str) -> bool {
        self.allows_page_navigation(url) || (self.allow_embedded_data && is_embedded_data_page(url))
    }

    /// Network navigation baseline; Browser-local approved data is checked by
    /// the adapter separately, never inherited by an unassociated context.
    pub(crate) fn allows_page_navigation(&self, url: &str) -> bool {
        self.allows_target(url)
    }

    pub(crate) fn is_open_web(&self) -> bool {
        self.allow_any_http && self.allow_any_https
    }

    pub(crate) fn allows_http_wildcard(&self) -> bool {
        self.allow_any_http
    }

    pub(crate) fn allows_https_wildcard(&self) -> bool {
        self.allow_any_https
    }

    /// Inline `data:` URLs (subframes, WebWorkers, WASM, inline scripts/styles/images)
    /// permitted under opt-in open-web (`http://*` + `https://*`) browsing with embedded
    /// data enabled. Strict allowlist policies retain `ApprovedDataPages` isolation.
    pub(crate) fn allows_open_web_data(&self, url: &str) -> bool {
        self.is_open_web()
            && self.allow_embedded_data
            && url.len() >= 6
            && !url.contains('\0')
            && url[..5].eq_ignore_ascii_case("data:")
            && url[5..].contains(',')
    }

    fn allows_websocket(&self, url: &str) -> bool {
        if !raw_target_ok(url) {
            return false;
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            return false;
        };
        let mapped = if scheme.eq_ignore_ascii_case("ws") {
            format!("http://{rest}")
        } else if scheme.eq_ignore_ascii_case("wss") {
            format!("https://{rest}")
        } else {
            return false;
        };
        parse_http(&mapped)
            .is_some_and(|parsed| self.matches_compiled_origin(&parsed.origin().ascii_serialization()))
    }

    /// Web-standard `blob:http://...` / `blob:https://...` URLs permitted only
    /// when the host selected open-web (`http://*` + `https://*`) browsing.
    pub(crate) fn allows_open_web_blob(&self, url: &str) -> bool {
        if !self.is_open_web() || !raw_target_ok(url) || url.len() < 5 {
            return false;
        }
        url[..5].eq_ignore_ascii_case("blob:") && self.allows_target(&url[5..])
    }

    /// Web-standard blank/srcdoc URLs permitted for subframes/subresources or
    /// JS-opened popups only under open-web browsing.
    pub(crate) fn allows_open_web_blank(
        &self,
        url: &str,
        allow_srcdoc: bool,
        allow_empty: bool,
    ) -> bool {
        if !self.is_open_web() {
            return false;
        }
        if allow_empty && url.is_empty() {
            return true;
        }
        raw_target_ok(url)
            && Url::parse(url).is_ok_and(|parsed| {
                parsed.scheme() == "about"
                    && parsed.cannot_be_a_base()
                    && parsed.host_str().is_none()
                    && (parsed.path() == "blank" || (allow_srcdoc && parsed.path() == "srcdoc"))
            })
    }

    /// Explicit internal-helper exception only; never a page/resource path.
    #[cfg(test)]
    pub(crate) fn allows_internal_about_blank(&self, url: &str) -> bool {
        raw_target_ok(url)
            && Url::parse(url).is_ok_and(|parsed| {
                parsed.scheme() == "about"
                    && parsed.cannot_be_a_base()
                    && parsed.host_str().is_none()
                    && parsed.path() == "blank"
                    && parsed.query().is_none()
                    && parsed.fragment().is_none()
            })
    }

    fn allows_target(&self, url: &str) -> bool {
        canonical_origin(url, &self.custom_schemes)
            .is_some_and(|origin| self.matches_compiled_origin(&origin))
    }
}

pub(crate) fn canonical_origin(raw: &str, custom_schemes: &BTreeSet<String>) -> Option<String> {
    if !raw_target_ok(raw) {
        return None;
    }
    let scheme_lower = raw.split_once("://")?.0.to_ascii_lowercase();
    if scheme_lower == "http" || scheme_lower == "https" {
        return parse_http(raw).map(|parsed| parsed.origin().ascii_serialization());
    }
    if !custom_schemes.contains(&scheme_lower) {
        return None;
    }
    let auth = authority(raw)?;
    if auth.contains('@') {
        return None;
    }
    let parsed = Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    if parsed.cannot_be_a_base()
        || host.is_empty()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let host_lower = host.to_ascii_lowercase();
    Some(match parsed.port() {
        Some(port) => format!("{scheme_lower}://{host_lower}:{port}"),
        None => format!("{scheme_lower}://{host_lower}"),
    })
}

fn compile_origin_with_schemes(
    raw: &str,
    custom_schemes: &BTreeSet<String>,
) -> Result<String, PolicyConfigError> {
    if raw.trim().is_empty() {
        return Err(PolicyConfigError::EmptyOrigin);
    }
    if raw.trim() != raw || raw.chars().any(|c| c.is_ascii_control()) || raw.contains('\\') {
        return Err(PolicyConfigError::MalformedOrigin);
    }
    let Some((scheme, _)) = raw.split_once(':') else {
        return Err(PolicyConfigError::MalformedOrigin);
    };
    if !valid_scheme(scheme) {
        return Err(PolicyConfigError::MalformedOrigin);
    }
    let scheme_lower = scheme.to_ascii_lowercase();
    let is_http = scheme_lower == "http" || scheme_lower == "https";
    if !is_http && !custom_schemes.contains(&scheme_lower) {
        return Err(PolicyConfigError::UnsupportedScheme);
    }
    // A nonempty explicit authority is required; the parser otherwise repairs
    // `http:host` or `http:///host` into a different origin.
    if authority(raw).is_none() {
        return Err(PolicyConfigError::MalformedOrigin);
    }
    let parsed = Url::parse(raw).map_err(|_| PolicyConfigError::MalformedOrigin)?;
    if parsed.cannot_be_a_base()
        || parsed
            .host_str()
            .is_none_or(|h| h.is_empty() || h.contains('*'))
    {
        return Err(PolicyConfigError::MalformedOrigin);
    }
    if has_credentials(raw, &parsed) {
        return Err(PolicyConfigError::CredentialsNotAllowed);
    }
    if is_http {
        if parsed.path() != "/" {
            return Err(PolicyConfigError::NonRootPath);
        }
    } else if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err(PolicyConfigError::NonRootPath);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(PolicyConfigError::QueryOrFragment);
    }
    if is_http {
        Ok(parsed.origin().ascii_serialization())
    } else {
        canonical_origin(raw, custom_schemes).ok_or(PolicyConfigError::MalformedOrigin)
    }
}

fn parse_http(value: &str) -> Option<Url> {
    if !raw_target_ok(value) {
        return None;
    }
    let scheme = value.split_once("://")?.0;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    if authority(value).is_none() {
        return None;
    }
    let parsed = Url::parse(value).ok()?;
    if parsed.cannot_be_a_base() || parsed.host_str().is_none() || has_credentials(value, &parsed) {
        return None;
    }
    Some(parsed)
}

/// Authority text before path/query/fragment, or `None` when absent.
fn authority(value: &str) -> Option<&str> {
    let rest = value.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    (!authority.is_empty()).then_some(authority)
}

fn valid_scheme(scheme: &str) -> bool {
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Reject text the URL parser would silently trim or rewrite.
fn raw_target_ok(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(|c| c.is_ascii_control())
        && !value.contains('\\')
        && !value.starts_with("//")
}

fn has_credentials(raw: &str, url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    // `http://@host` parses with empty userinfo, so inspect the raw authority.
    authority(raw).is_some_and(|authority| authority.contains('@'))
}

fn is_embedded_data_page(value: &str) -> bool {
    if !raw_target_ok(value) {
        return false;
    }
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    parsed.scheme() == "data"
        && parsed.cannot_be_a_base()
        && parsed.host_str().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed
            .path()
            .split_once(',')
            .is_some_and(|(_, payload)| !payload.is_empty())
}

/// Browser-local exact-content approvals, explicitly chosen by the host.
/// This authorizes revisiting identical URI bytes, not the source of each visit.
#[derive(Default)]
pub(crate) struct ApprovedDataPages(BTreeSet<String>);

impl ApprovedDataPages {
    pub(crate) fn approve_host(&mut self, policy: &CompiledPolicy, url: &str) {
        if policy.allows_host_navigation(url) && !policy.allows_network(url) {
            self.0.insert(url.to_owned());
        }
    }
    pub(crate) fn contains(&self, url: &str) -> bool {
        self.0.contains(url)
    }
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::PolicyConfigError as E;
    use super::*;

    fn policy(origins: &[&str], embedded: bool) -> CompiledPolicy {
        CompiledPolicy::compile(&TrustedContentPolicy {
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            allow_embedded_data: embedded,
        })
        .unwrap()
    }

    fn check(run: impl Fn(&str) -> bool, cases: &[(&str, bool)]) {
        for &(url, expected) in cases {
            assert_eq!(run(url), expected, "{url:?}");
        }
    }

    #[test]
    #[rustfmt::skip]
    fn canonical_match_default_ports_idna_and_no_suffixes() {
        let p = policy(&["HTTP://Example.COM:80/", "https://bücher.example:443"], false);
        let cases = [
            ("http://example.com/", true), ("http://EXAMPLE.COM:80/a?b#c", true),
            ("https://bücher.example/", true), ("https://xn--bcher-kva.example/path", true),
            ("http://example.com.evil/", false), ("http://sub.example.com/", false),
            ("http://example.com:8080/", false), ("https://example.com/", false),
            ("http://evil.com/", false), ("http://example.com@evil.com/", false),
        ];
        check(|u| p.allows_network(u), &cases);
        check(|u| p.allows_page_navigation(u), &cases);
        assert_eq!(p.allowed_origins.iter().cloned().collect::<Vec<_>>(),
            ["http://example.com", "https://xn--bcher-kva.example"]);
    }

    #[test]
    #[rustfmt::skip]
    fn malformed_targets_fail_closed() {
        let p = policy(&["http://example.com"], false);
        let cases = [
            ("", false), ("    ", false), (" http://example.com", false),
            ("http://example.com\n", false), ("http://example.com\\path", false),
            ("//example.com", false), ("http:example.com", false),
            ("http://user@example.com", false), ("http://@example.com", false),
            ("http://", false), ("http:///example.com", false),
            ("javascript:alert(1)", false), ("file:///etc/passwd", false),
            ("http://example.com/ok?q=1#frag", true),
        ];
        check(|u| p.allows_network(u), &cases);
        check(|u| p.allows_page_navigation(u), &cases);
    }

    #[test]
    #[rustfmt::skip]
    fn origin_config_rejects_bad_origins() {
        for (origin, err) in [
            ("", E::EmptyOrigin), ("   ", E::EmptyOrigin),
            ("//example.com", E::MalformedOrigin), ("http://", E::MalformedOrigin),
            ("http://exa\nmple.com", E::MalformedOrigin),
            ("http://example.com\\path", E::MalformedOrigin),
            ("ftp://example.com", E::UnsupportedScheme),
            ("data:text/html,x", E::UnsupportedScheme),
            ("http://user@example.com", E::CredentialsNotAllowed),
            ("http://@example.com", E::CredentialsNotAllowed),
            ("http://example.com/path", E::NonRootPath),
            ("http://example.com/?q", E::QueryOrFragment),
            ("http://example.com/#f", E::QueryOrFragment),
        ] {
            let config = TrustedContentPolicy {
                allowed_origins: vec![origin.to_string()],
                allow_embedded_data: false,
            };
            assert_eq!(CompiledPolicy::compile(&config).unwrap_err(), err, "{origin:?}");
        }
    }

    #[test]
    fn exact_data_content_approvals_are_browser_local_and_survive_revisits() {
        let policy = policy(&["https://allowed.test"], true);
        let mut first = ApprovedDataPages::default();
        let second = ApprovedDataPages::default();
        let uri = "data:text/html,host-approved";
        first.approve_host(&policy, uri);
        assert!(first.contains(uri) && first.contains(uri)); // Back/Reload do not consume approval.
        assert!(!first.contains("data:text/html,page-created"));
        assert!(!second.contains(uri));
        first.approve_host(&policy, "https://allowed.test/");
        assert!(!first.contains("https://allowed.test/"));
        first.clear();
        assert!(!first.contains(uri));
    }

    #[test]
    #[rustfmt::skip]
    fn data_and_about_only_on_explicit_host_paths() {
        let enabled = policy(&["http://example.com"], true);
        let disabled = policy(&["http://example.com"], false);
        for (url, ok) in [
            ("data:text/html,hello", true), ("data:,hello", true),
            ("data:text/html,", false), ("data:", false),
            ("datax:text/html,hello", false), ("data:text/html,hello\\world", false),
        ] {
            assert_eq!(enabled.allows_host_navigation(url), ok, "{url:?}");
            assert!(!enabled.allows_page_navigation(url), "{url:?}");
            assert!(!enabled.allows_network(url), "{url:?}");
            assert!(!disabled.allows_host_navigation(url), "{url:?}");
        }
        assert!(disabled.allows_host_navigation("http://example.com/"));
        assert!(enabled.allows_internal_about_blank("about:blank"));
        check(|u| enabled.allows_internal_about_blank(u),
            &[("about:blank#x", false), ("about:srcdoc", false)]);
        assert!(!enabled.allows_page_navigation("about:blank"));
        assert!(!enabled.allows_host_navigation("about:blank"));
    }

    #[test]
    fn custom_schemes_compile_canonical_origins_and_enforce_allowlist() {
        use rust_cef_core::custom_schemes::CustomSchemeOptions;
        let schemes = vec![CustomSchemeConfig {
            scheme: "app".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        }];
        let p = CompiledPolicy::compile_with_custom_schemes(
            &TrustedContentPolicy {
                allowed_origins: vec!["APP://LOCAL/".into()],
                allow_embedded_data: false,
            },
            &schemes,
        )
        .unwrap();
        assert!(p.is_custom_scheme("APP"));
        assert!(p.allows_network("app://local/index.html?x=1"));
        assert!(p.allows_page_navigation("APP://local/sub/a.js"));
        assert!(!p.allows_network("app://unlisted/index.html"));
        assert!(!p.allows_network("app://local:8443/index.html"));
        assert!(!p.allows_network("other://local/index.html"));
        assert_eq!(
            p.canonical_origin_for("app://LOCAL/index.html"),
            Some("app://local".into())
        );
    }

    #[test]
    fn allow_all_http_https_permits_valid_web_origins_and_keeps_fail_closed_guards() {
        let p = CompiledPolicy::compile(&TrustedContentPolicy::allow_all_http_https(true)).unwrap();
        for ok_url in [
            "https://cn.bing.com/search?q=test",
            "https://tse1-mm.cn.bing.net/th?id=OIP.123",
            "https://www.zhihu.com/question/1",
            "http://example.com:8080/a",
        ] {
            assert!(p.allows_network(ok_url), "{ok_url}");
            assert!(p.allows_page_navigation(ok_url), "{ok_url}");
            assert!(p.allows_host_navigation(ok_url), "{ok_url}");
            let origin = p.canonical_origin_for(ok_url).unwrap();
            assert!(p.allows_canonical_origin(&origin), "{origin}");
        }
        assert!(p.allows_host_navigation("data:text/html,ok"));
        assert!(!p.allows_page_navigation("data:text/html,ok"));

        for blocked in [
            "javascript:alert(1)",
            "file:///C:/Windows/win.ini",
            "ftp://example.com/",
            "https://user:pass@example.com/",
            "https://example.com\\path",
            "https://example.com\n",
            "",
        ] {
            assert!(!p.allows_network(blocked), "{blocked:?}");
            assert!(!p.allows_page_navigation(blocked), "{blocked:?}");
            assert!(!p.allows_canonical_origin(blocked), "{blocked:?}");
        }
        assert!(!p.allows_canonical_origin("https://evil.com/path"));

        for bad_wildcard in [
            "https://*.bing.com",
            "*://*",
            "https://*/foo",
            " https://*",
            "ftp://*",
        ] {
            let cfg = TrustedContentPolicy {
                allowed_origins: vec![bad_wildcard.to_string()],
                allow_embedded_data: false,
            };
            assert!(CompiledPolicy::compile(&cfg).is_err(), "{bad_wildcard:?}");
        }
    }

    #[test]
    fn open_web_permits_valid_blobs_and_blank_frames_while_strict_policy_denies_them() {
        let open = CompiledPolicy::compile(&TrustedContentPolicy::allow_all_http_https(true)).unwrap();
        let strict = policy(&["https://allowed.local"], true);

        assert!(open.is_open_web());
        assert!(!strict.is_open_web());

        assert!(open.allows_open_web_blob("blob:https://cn.bing.com/12345678-1234"));
        assert!(open.allows_open_web_blob("BLOB:http://127.0.0.1:8080/item"));
        assert!(!open.allows_open_web_blob("blob:null/1234"));
        assert!(!open.allows_open_web_blob("blob:file:///C:/secret"));
        assert!(!open.allows_open_web_blob("blob:https://user:pass@cn.bing.com/123"));
        assert!(!open.allows_open_web_blob("blob:https://cn.bing.com\\bad"));
        assert!(!strict.allows_open_web_blob("blob:https://allowed.local/1234"));

        assert!(open.allows_open_web_blank("about:blank", false, false));
        assert!(open.allows_open_web_blank("about:blank#frame", true, false));
        assert!(open.allows_open_web_blank("about:srcdoc", true, false));
        assert!(!open.allows_open_web_blank("about:srcdoc", false, false));
        assert!(open.allows_open_web_blank("", false, true));
        assert!(!open.allows_open_web_blank("", false, false));
        assert!(!open.allows_open_web_blank("about:settings", true, false));
        assert!(!open.allows_open_web_blank("javascript:alert(1)", true, true));

        assert!(!strict.allows_open_web_blank("about:blank", true, true));
        assert!(!strict.allows_open_web_blank("about:srcdoc", true, true));
        assert!(!strict.allows_open_web_blank("", true, true));

        // WebSockets match HTTP(S) origin rules, and open-web data: subresources stay isolated from strict policies.
        assert!(open.allows_network("wss://ws.example.com/socket"));
        assert!(open.allows_network("ws://localhost:8080/live"));
        assert!(!open.allows_page_navigation("wss://ws.example.com/socket"));
        assert!(!open.allows_network("wss://user:pass@ws.example.com/socket"));
        let strict_ws = policy(&["https://allowed.local:8443", "http://allowed.local"], false);
        assert!(strict_ws.allows_network("wss://allowed.local:8443/stream"));
        assert!(strict_ws.allows_network("ws://allowed.local/stream"));
        assert!(!strict_ws.allows_network("wss://allowed.local/stream"));
        assert!(!strict_ws.allows_network("wss://evil.local:8443/stream"));

        assert!(open.allows_open_web_data("data:text/javascript,console.log('ok')"));
        assert!(open.allows_open_web_data("data:text/html,<h1>hi</h1>\n<script>1\\n</script>"));
        assert!(!open.allows_open_web_data("data:text/html"));
        assert!(!strict.allows_open_web_data("data:text/javascript,console.log('ok')"));
    }
}
