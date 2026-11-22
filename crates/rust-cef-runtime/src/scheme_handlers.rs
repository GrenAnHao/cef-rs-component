//! Confined profile custom-scheme factories and bounded memory/file resource handlers.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cef::*;
use rust_cef_core::custom_schemes::{
    CustomSchemeValidationError, MAX_SCHEME_TOTAL_MEMORY_BYTES, MemoryAsset, ProfileSchemeBinding,
    SchemeAssetSource, normalize_memory_asset_path, validate_profile_scheme_binding,
};
use rust_cef_core::ports::PolicyAction;
use url::Url;

use crate::policy_native::NativePolicy;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

pub(crate) fn mime_for_extension(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "application/octet-stream";
    };
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" | "log" => "text/plain",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

struct MemoryStreamState {
    status_code: i32,
    mime_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    offset: usize,
}

wrap_resource_handler! {
    pub(crate) struct BoundedMemoryResourceHandler {
        state: Arc<Mutex<Option<MemoryStreamState>>>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = guard.as_ref() {
                if let Some(response) = response {
                    response.set_status(state.status_code);
                    response.set_mime_type(Some(&CefString::from(state.mime_type.as_str())));
                    for (name, value) in &state.headers {
                        response.set_header_by_name(
                            Some(&CefString::from(name.as_str())),
                            Some(&CefString::from(value.as_str())),
                            1,
                        );
                    }
                }
                if let Some(len) = response_length {
                    *len = state.body.len() as i64;
                }
            } else {
                if let Some(response) = response {
                    response.set_status(404);
                    response.set_mime_type(Some(&CefString::from("text/plain")));
                }
                if let Some(len) = response_length {
                    *len = 0;
                }
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            if data_out.is_null() || bytes_to_read <= 0 {
                if let Some(read) = bytes_read {
                    *read = 0;
                }
                return 0;
            }
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(state) = guard.as_mut() else {
                if let Some(read) = bytes_read {
                    *read = 0;
                }
                return 0;
            };
            let remaining = state.body.len().saturating_sub(state.offset);
            let copy_len = remaining.min(bytes_to_read as usize);
            if copy_len == 0 {
                *guard = None;
                if let Some(read) = bytes_read {
                    *read = 0;
                }
                return 0;
            }
            // SAFETY: CEF guarantees `data_out` has `bytes_to_read` writable bytes
            // for this synchronous read invocation.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    state.body.as_ptr().add(state.offset),
                    data_out,
                    copy_len,
                );
            }
            state.offset += copy_len;
            if state.offset >= state.body.len() {
                state.body = Vec::new();
            }
            if let Some(read) = bytes_read {
                *read = copy_len as i32;
            }
            1
        }

        fn cancel(&self) {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }
    }
}

pub(crate) fn memory_resource_handler(
    status_code: u16,
    mime_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> ResourceHandler {
    BoundedMemoryResourceHandler::new(Arc::new(Mutex::new(Some(MemoryStreamState {
        status_code: i32::from(status_code),
        mime_type,
        headers,
        body,
        offset: 0,
    }))))
}

pub(crate) fn not_found_resource_handler() -> ResourceHandler {
    memory_resource_handler(404, "text/plain".into(), Vec::new(), b"Not Found".to_vec())
}

struct FileStreamState {
    file: File,
    mime_type: String,
    length: u64,
    offset: u64,
}

wrap_resource_handler! {
    struct ConfinedFileResourceHandler {
        state: Arc<Mutex<Option<FileStreamState>>>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = guard.as_ref() {
                if let Some(response) = response {
                    response.set_status(200);
                    response.set_mime_type(Some(&CefString::from(state.mime_type.as_str())));
                }
                if let Some(len) = response_length {
                    *len = i64::try_from(state.length).unwrap_or(-1);
                }
            } else {
                if let Some(response) = response {
                    response.set_status(404);
                    response.set_mime_type(Some(&CefString::from("text/plain")));
                }
                if let Some(len) = response_length {
                    *len = 0;
                }
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            if data_out.is_null() || bytes_to_read <= 0 {
                if let Some(read) = bytes_read {
                    *read = 0;
                }
                return 0;
            }
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(state) = guard.as_mut() else {
                if let Some(read) = bytes_read {
                    *read = 0;
                }
                return 0;
            };
            let max_chunk = (bytes_to_read as usize).min(256 * 1024);
            // SAFETY: CEF provides `data_out` valid for `bytes_to_read` bytes.
            let slice = unsafe { std::slice::from_raw_parts_mut(data_out, max_chunk) };
            match state.file.read(slice) {
                Ok(0) | Err(_) => {
                    *guard = None;
                    if let Some(read) = bytes_read {
                        *read = 0;
                    }
                    0
                }
                Ok(n) => {
                    state.offset = state.offset.saturating_add(n as u64);
                    if state.offset >= state.length {
                        *guard = None;
                    }
                    if let Some(read) = bytes_read {
                        *read = n as i32;
                    }
                    1
                }
            }
        }

        fn cancel(&self) {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfinedDirectoryAssetHandler {
    canonical_root: PathBuf,
    default_page: String,
}

impl ConfinedDirectoryAssetHandler {
    pub(crate) fn new(
        root: &Path,
        default_page: &str,
    ) -> Result<Self, CustomSchemeValidationError> {
        if !root.is_absolute() || root.to_string_lossy().contains('\0') {
            return Err(CustomSchemeValidationError::InvalidDirectoryRoot);
        }
        let meta = fs::symlink_metadata(root)
            .map_err(|_| CustomSchemeValidationError::InvalidDirectoryRoot)?;
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(CustomSchemeValidationError::InvalidDirectoryRoot);
        }
        let canonical_root = fs::canonicalize(root)
            .map_err(|_| CustomSchemeValidationError::InvalidDirectoryRoot)?;
        let canon_meta = fs::symlink_metadata(&canonical_root)
            .map_err(|_| CustomSchemeValidationError::InvalidDirectoryRoot)?;
        if !canon_meta.is_dir()
            || canon_meta.file_type().is_symlink()
            || (canon_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(CustomSchemeValidationError::InvalidDirectoryRoot);
        }
        Ok(Self {
            canonical_root,
            default_page: default_page.to_owned(),
        })
    }

    pub(crate) fn resolve_url_file(&self, raw_url: &str) -> Option<(PathBuf, &'static str)> {
        if raw_url.contains('\\') || raw_url.contains('\0') {
            return None;
        }
        // Reject raw `.` / `..` segments before `Url::parse` normalizes dot
        // segments away (e.g., `app://local/../index.html`).
        if let Some((_, after_scheme)) = raw_url.split_once("://") {
            let before_query_frag = after_scheme.split(['?', '#']).next().unwrap_or("");
            if let Some((_, raw_subpath)) = before_query_frag.split_once('/') {
                for raw_seg in raw_subpath.split('/') {
                    if raw_seg == "." || raw_seg == ".." {
                        return None;
                    }
                }
            }
        }
        let parsed = Url::parse(raw_url).ok()?;
        let raw_path = parsed.path();
        if raw_path.contains('\\') || raw_path.contains('\0') {
            return None;
        }
        let trimmed = raw_path.strip_prefix('/').unwrap_or(raw_path);
        let candidate = if trimmed.is_empty() {
            self.default_page.as_str()
        } else {
            trimmed
        };
        let lower = candidate.to_ascii_lowercase();
        if lower.contains("%5c")
            || lower.contains("%2f")
            || lower.contains("%00")
            || lower.contains("%3a")
        {
            return None;
        }
        let decoded = percent_decode_strict(candidate)?;
        if decoded.contains('\\') || decoded.contains('\0') || decoded.contains(':') {
            return None;
        }
        let mut current = self.canonical_root.clone();
        for segment in decoded.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return None;
            }
            current.push(segment);
            let meta = fs::symlink_metadata(&current).ok()?;
            if meta.file_type().is_symlink()
                || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return None;
            }
        }
        let canonical = fs::canonicalize(&current).ok()?;
        if !canonical.starts_with(&self.canonical_root) {
            return None;
        }
        let final_meta = fs::symlink_metadata(&canonical).ok()?;
        if !final_meta.is_file()
            || final_meta.file_type().is_symlink()
            || (final_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || final_meta.len() > MAX_SCHEME_TOTAL_MEMORY_BYTES as u64
        {
            return None;
        }
        let mime = mime_for_extension(&canonical);
        Some((canonical, mime))
    }

    fn open_handler(&self, raw_url: &str) -> ResourceHandler {
        let Some((path, mime)) = self.resolve_url_file(raw_url) else {
            return not_found_resource_handler();
        };
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            return not_found_resource_handler();
        };
        let Ok(meta) = file.metadata() else {
            return not_found_resource_handler();
        };
        if !meta.is_file() {
            return not_found_resource_handler();
        }
        ConfinedFileResourceHandler::new(Arc::new(Mutex::new(Some(FileStreamState {
            file,
            mime_type: mime.to_owned(),
            length: meta.len(),
            offset: 0,
        }))))
    }
}

fn percent_decode_strict(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[derive(Clone)]
enum AssetSourceRuntime {
    Memory(Arc<BTreeMap<String, MemoryAsset>>),
    Directory(Arc<ConfinedDirectoryAssetHandler>),
}

pub(crate) fn build_scheme_handler_factory(
    policy: Arc<NativePolicy>,
    binding: &ProfileSchemeBinding,
) -> Result<SchemeHandlerFactory, CustomSchemeValidationError> {
    validate_profile_scheme_binding(binding)?;
    let source = match &binding.source {
        SchemeAssetSource::Memory(map) => {
            let mut normalized = BTreeMap::new();
            for (k, v) in map {
                normalized.insert(normalize_memory_asset_path(k)?, v.clone());
            }
            AssetSourceRuntime::Memory(Arc::new(normalized))
        }
        SchemeAssetSource::Directory { root, default_page } => AssetSourceRuntime::Directory(
            Arc::new(ConfinedDirectoryAssetHandler::new(root, default_page)?),
        ),
    };
    Ok(ProfileAssetSchemeFactory::new(
        policy,
        binding.scheme.to_ascii_lowercase(),
        binding.domain.as_ref().map(|d| d.to_ascii_lowercase()),
        source,
    ))
}

wrap_scheme_handler_factory! {
    struct ProfileAssetSchemeFactory {
        policy: Arc<NativePolicy>,
        expected_scheme: String,
        expected_domain: Option<String>,
        source: AssetSourceRuntime,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let Some(request) = request else {
                self.policy.deny(None, PolicyAction::Resource);
                return Some(crate::policy_native::CancelNetworkResource::new());
            };
            let url = CefString::from(&request.url()).to_string();
            // Enforce TrustedContentPolicy first: custom schemes never bypass
            // allowed_origins.
            if !self.policy.rules.allows_network(&url) {
                self.policy.deny(None, PolicyAction::Resource);
                return Some(crate::policy_native::CancelNetworkResource::new());
            }
            let actual_scheme = scheme_name
                .map(ToString::to_string)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if actual_scheme != self.expected_scheme {
                return Some(not_found_resource_handler());
            }
            if let Some(expected_domain) = &self.expected_domain {
                let Ok(parsed) = Url::parse(&url) else {
                    return Some(not_found_resource_handler());
                };
                if !parsed
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(expected_domain))
                {
                    return Some(not_found_resource_handler());
                }
            }
            match &self.source {
                AssetSourceRuntime::Memory(assets) => {
                    let Ok(parsed) = Url::parse(&url) else {
                        return Some(not_found_resource_handler());
                    };
                    let raw_path = parsed.path().strip_prefix('/').unwrap_or(parsed.path());
                    let key = if raw_path.is_empty() {
                        "index.html".to_owned()
                    } else if let Ok(norm) = normalize_memory_asset_path(raw_path) {
                        norm
                    } else {
                        return Some(not_found_resource_handler());
                    };
                    if let Some(asset) = assets.get(&key) {
                        Some(memory_resource_handler(
                            asset.status_code,
                            asset.mime_type.clone(),
                            asset.headers.clone(),
                            asset.body.clone(),
                        ))
                    } else {
                        Some(not_found_resource_handler())
                    }
                }
                AssetSourceRuntime::Directory(dir) => Some(dir.open_handler(&url)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::CompiledPolicy;
    use rust_cef_core::contracts::TrustedContentPolicy;
    use rust_cef_core::custom_schemes::{CustomSchemeConfig, CustomSchemeOptions};

    #[test]
    fn directory_handler_serves_confined_files_and_blocks_traversal_and_junctions() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("gpui-cef-dir-confine-{nonce}"));
        let root = base.join("bundle");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("index.html"), b"<h1>ok</h1>").unwrap();
        fs::write(root.join("sub/app.js"), b"console.log(1);").unwrap();
        fs::write(outside.join("secret.txt"), b"TOP_SECRET").unwrap();

        // Create an NTFS junction inside `root` pointing outside `root` if supported.
        let junction = root.join("escape_junction");
        let junction_cmd = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &outside.to_string_lossy(),
            ])
            .output();

        let handler = ConfinedDirectoryAssetHandler::new(&root, "index.html").unwrap();
        let (idx_path, idx_mime) = handler.resolve_url_file("app://local/").unwrap();
        assert_eq!(idx_mime, "text/html");
        assert!(idx_path.ends_with("index.html"));

        let (js_path, js_mime) = handler.resolve_url_file("app://local/sub/app.js").unwrap();
        assert_eq!(js_mime, "application/javascript");
        assert!(js_path.ends_with("app.js"));

        for bad in [
            "app://local/../index.html",
            "app://local/./index.html",
            "app://local/sub/../index.html",
            "app://local/../outside/secret.txt",
            "app://local/%2e%2e/outside/secret.txt",
            "app://local/sub/%2e%2e/%2e%2e/outside/secret.txt",
            "app://local/sub\\..\\..\\outside\\secret.txt",
            "app://local/index.html:ads",
            "app://local/index.html%00.txt",
            "app://local/missing.html",
        ] {
            assert!(
                handler.resolve_url_file(bad).is_none(),
                "{bad:?} must be refused"
            );
        }

        if let Ok(out) = junction_cmd
            && out.status.success()
        {
            assert!(
                handler
                    .resolve_url_file("app://local/escape_junction/secret.txt")
                    .is_none(),
                "NTFS junction escape outside root must be blocked"
            );
        }

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn memory_handler_respects_policy_gate_first() {
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
        let mut assets = BTreeMap::new();
        assets.insert("index.html".into(), MemoryAsset::html(b"hi".to_vec()));
        let binding = ProfileSchemeBinding {
            scheme: "app".into(),
            domain: Some("local".into()),
            source: SchemeAssetSource::Memory(assets),
        };
        let factory = build_scheme_handler_factory(policy.clone(), &binding).unwrap();
        assert!(
            factory.create(None, None, None, None).is_some(),
            "denied missing request returns CancelNetworkResource"
        );
        assert_eq!(policy.snapshot().0[PolicyAction::Resource as usize], 1);
    }
}
