//! Explicit GPUI-CEF-COMPONENT integration boundary.
//!
//! The host owns its Application, windows and Runtime. Start the Runtime with
//! explicit services, create a Browser, then obtain its one-shot attachment via
//! `Runtime::attach_browser` and construct [`CefBrowser`] in the existing window.
//! The foreground owner consumes [`dispatcher`] wakes; CEF callbacks never
//! borrow GPUI. Runtime pump/final shutdown remain the host's responsibility.
//! No Application, process exit, automatic CEF initialization or global locator
//! is installed by the view.

pub use rust_cef_core::input::{
    BrowserTouchEvent, InputEvent, InputKind, InputResetReason, InputSnapshot,
    MouseButton as InputMouseButton, TouchEventType, TouchPointerType, Utf16Range,
};
pub use rust_cef_core::{
    accessibility as accessibility_contracts, contracts, custom_schemes, file_drop, find,
    host_capabilities, ime, js_bridge, outbound_drag as outbound_drag_contracts, ports, print,
    source_view as source, url_request as url_request_contracts,
};

pub use rust_cef_ui::{
    binding, dispatcher, file_drag, ime_geometry, input, native_slot, outbound_drag, surface,
};

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use rust_cef_runtime::attachment::{AttachmentError, BrowserAttachment};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use rust_cef_runtime::runtime::{Runtime, RuntimeServices};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use rust_cef_runtime::{attachment, runtime};

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod composition;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod find_view;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod menu;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod menu_presenter;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod print_confirmation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod source_view;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use composition::{
    CompositionError, DispatchFailure, DispatchFailureKind, DispatchReport, InputRetryReport,
    UiComposition,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use find_view::{FindServices, FindUiError, FindUiStatus, FindUiUpdate};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use menu::{
    ContextMenuServices, MenuCleanup, MenuCleanupStatus, MenuDispatchReport, MenuFocusRegistration,
    MenuServiceError, MenuServiceStatus, MenuUiUpdate,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use menu_presenter::{
    DefaultMenuPresenter, MenuBounds, MenuCommandBounds, MenuPresenterFailure, MenuPresenterLayout,
    MenuPresenterReply, MenuPresenterStatus, init_default_menu_ui,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod accessibility;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod native_touch;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod ole_drag;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use rust_cef_runtime::{
    CdpDiagnostics, ContextMenuConfig, ContextMenuRegistration, ContextMenuStatus,
    CookieMutationStatus, CookieMutationTicket, CookieVisitStatus, CookieVisitTicket,
    FindDiagnostics, PdfExportStatus, PdfExportTicket, ProfileActionStatus, ProfileActionTicket,
    SpellingConfig, SpellingFailure, SpellingStatus, UrlRequestAuthChallenge,
    UrlRequestAuthResolver, UrlRequestState, UrlRequestTicket, win32_menu,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use source_view::{SourceUiError, SourceUiStatus, SourceViewOptions, SourceViewServices};

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod native_ime;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use native_ime::{NativeImeDiagnostics, NativeImeFailure};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod native_keyboard;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use native_keyboard::{
    HostShortcutPolicy, NativeKeyboardDiagnostics, NativeKeyboardError, NativeKeyboardFailure,
    NativeKeyboardServices,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use native_touch::{NativeTouchDiagnostics, NativeTouchError, NativeTouchServices};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use ole_drag::IntegratedPumpCallback;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub mod native_view;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use native_view::{
    NativeCefBrowser, NativeEvalCompletedCallback, NativeJsMessageCallback, NativeShortcutCallback,
    NativeTakeFocusCallback, NativeViewDiagnostics, window_hwnd,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod view;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use input::{InputCallback, InputCallbackDiagnostics, InputCallbackGate, InputDiagnostics};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use view::{
    CefBrowser, EvalCompletedCallback, JsMessageCallback, OutboundDragCallback, PopupDiagnostics,
    ShortcutCallback, TakeFocusCallback, ViewDiagnostics, ViewError, cursor_style_for,
};

/// Test-only frame-probe surface for validation builds; never a stable API.
///
/// Compiled only when the non-default `validation` feature is enabled on
/// Windows x64. It forwards the runtime's existing `test-support` probe types
/// so a validation host can observe frame probes without depending on
/// `rust-cef-runtime` directly. Expect names, shapes and signatures here to
/// change without notice and expect nothing in this module in a default build.
#[cfg(all(feature = "validation", target_os = "windows", target_arch = "x86_64"))]
pub mod test_support {
    //! Validation-only re-exports of runtime frame and pressure probes.
    pub use crate::view::ViewPaintSample;
    pub use rust_cef_core::{
        menu::{ContextMenuRequestId, ContextMenuResponse, MenuLimits},
        menu_context::MenuContextLimits,
        menu_lease::MenuLeaseOwner,
    };
    pub use rust_cef_runtime::menu_bridge_fixture::{
        BridgeDelivery, MenuBridgeFixture, fixture_url as menu_bridge_fixture_url,
    };
    pub use rust_cef_runtime::test_support::{
        CommandPressure, CookieFlushMarkerProbe, FrameProbeControl, FrameProbeState,
        MANUAL_PUMP_SAMPLE_CAPACITY, NativeFrameRejection, PRESSURE_EVENT_CAPACITY,
        PRESSURE_FIXTURE_URL, PRESSURE_SOURCE_FIXTURE_URL, PRESSURE_TITLE_PREFIX,
        ProbeBrowserEvent, ProbeDxgiAdapter, TimingDistribution, VIEW_COPY_SAMPLE_CAPACITY,
        WindowMenuCut, WindowPeerFlush, WindowPrintCut, probe_dxgi_adapters, probe_frame_sha256,
    };
}

#[cfg(all(
    test,
    feature = "validation",
    target_os = "windows",
    target_arch = "x86_64"
))]
mod validation_probe_contract {
    use super::Runtime;
    use super::test_support::{FrameProbeControl, FrameProbeState};
    use rust_cef_core::contracts::BrowserId;
    use rust_cef_core::ports::EngineError;

    /// Compile-time only: binds the re-exported probe types to the public
    /// `Runtime` probe method. No Runtime is built, no CEF is initialized and
    /// no fixture/native resource is touched.
    #[test]
    fn probe_types_and_public_methods_connect() {
        let control: fn(&Runtime, BrowserId, FrameProbeControl) -> Result<(), EngineError> =
            Runtime::probe_frame_control;
        let state: fn(&Runtime, BrowserId) -> Option<FrameProbeState> = Runtime::probe_frame_state;
        let applied: fn(&FrameProbeState) -> u64 = |state| state.applied;
        let pending: fn(&FrameProbeState) -> bool = |state| state.incomplete_pending;
        let drop_view: fn(FrameProbeControl) -> bool =
            |control| matches!(control, FrameProbeControl::DropView(_));
        let _ = (control, state, applied, pending, drop_view);
    }
}
