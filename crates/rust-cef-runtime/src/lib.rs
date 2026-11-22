//! CEF process/deployment adapter, independent of GPUI.
//! Runtime/Browser lifecycle is added by the dedicated native integration tasks.
pub mod accessibility;
pub mod app;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod attachment;
mod before_unload;
mod build_stamp;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod capture;
pub mod client;
pub mod deployment;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod devtools;
#[cfg(feature = "test-support")]
mod dialog_probe;
pub mod executor;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod find;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use find::FindDiagnostics;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod headless_cdp;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod host_requests;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use headless_cdp::CdpDiagnostics;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod pdf_export;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use pdf_export::{PdfExportStatus, PdfExportTicket};
mod input;
#[cfg(feature = "test-support")]
pub mod input_fixture;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod menu_bridge;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod menu_context;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod menu_model;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod menu_native;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod source_view;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod spelling;
#[cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]
pub mod spelling_probe;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use menu_bridge::{ContextMenuConfig, ContextMenuRegistration, ContextMenuStatus};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use source_view::SourceViewDiagnostics;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use spelling::{SpellingConfig, SpellingFailure, SpellingStatus};
#[cfg(feature = "test-support")]
pub mod find_probe_report;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod helper_app;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod js_bridge_native;
#[cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]
pub mod menu_bridge_fixture;
#[cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]
mod menu_probe;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub(crate) mod native_hwnd;
#[cfg(feature = "test-support")]
pub mod navigation_fixture;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod outbound_drag;
pub mod policy;
mod policy_handlers;
mod policy_native;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod print_operations;
mod print_provider;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod profile_cookies;
#[cfg(feature = "test-support")]
pub mod profile_fixture;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod profile_flush;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use profile_cookies::{
    CookieMutationStatus, CookieMutationTicket, CookieVisitStatus, CookieVisitTicket,
    ProfileActionStatus, ProfileActionTicket,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod profile_paths;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod profiles;
pub mod registry;
#[cfg(feature = "test-support")]
mod renderer_probe;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod resize;
mod resource_policy;
pub mod runtime;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod scheme_handlers;
#[cfg(feature = "test-support")]
pub mod source_probe_report;
pub mod subprocess;
#[cfg(feature = "test-support")]
pub mod test_support;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod url_request;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod win32_menu;

pub use profiles::ProfileSchemeTicket;
pub use rust_cef_core::{
    accessibility as accessibility_contracts, contracts, custom_schemes, js_bridge,
    outbound_drag as outbound_drag_contracts, ports, url_request as url_request_contracts,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use url_request::{
    UrlRequestAuthChallenge, UrlRequestAuthResolver, UrlRequestState, UrlRequestTicket,
};
