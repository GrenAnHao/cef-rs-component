//! Framework-agnostic Rust UI integration pipeline for CEF (`rust-cef-ui`).
//!
//! This crate has **zero dependency on GPUI, Rune, or any other UI toolkit**.
//! It bundles the reusable UI-thread pipelines that sit between `rust-cef-core`
//! / `rust-cef-runtime` and a concrete rendering backend (`rust-cef-gpui`,
//! future `rust-cef-rune`, winit/wgpu, egui, iced, etc.):
//!
//! - [`surface`]: VIEW/POPUP frame consumption, generation checks, resize and
//!   [`SurfaceTarget`] plug-in trait.
//! - [`binding`]: Window binding tokens and paint-stamp revision fencing.
//! - [`dispatcher`]: Coalesced async UI wake channel (`UiWakeSender`/`UiWakeReceiver`).
//! - [`input`]: Bounded input lane with backpressure retry and composition mirror.
//! - [`file_drag`]: Bounded inbound file-drop session state machine.
//! - [`ime_geometry`]: CEF character-bounds to client-DIP IME anchor projection.
//! - [`find_inbox`] / [`source_inbox`]: UI-neutral bounded Find & ViewSource inboxes.
//! - [`win32_menu`] (Windows x64): Native Win32 `CreatePopupMenu`/`TrackPopupMenuEx` host.
pub mod binding;
pub mod dispatcher;
pub mod file_drag;
pub mod find_inbox;
pub mod ime_geometry;
pub mod input;
pub mod native_slot;
pub mod outbound_drag;
pub mod source_inbox;
pub mod surface;

pub use binding::{BindingError, PaintStamp, WindowToken};
pub use dispatcher::{UiWakeReceiver, UiWakeSender, channel};
pub use file_drag::FileDragSession;
pub use input::{
    ClickError, ClickGesture, InputAdmission, InputCallback, InputCallbackDiagnostics,
    InputCallbackGate, InputDiagnostics, InputLane, MirrorEffect,
};
pub use native_slot::{NativeSlotController, NativeSlotDiagnostics, NativeSlotError};
pub use outbound_drag::OutboundDragState;
pub use rust_cef_core::mailbox::SurfaceBounds;
pub use rust_cef_core::{
    accessibility, contracts, file_drop, ime, input as core_input, menu,
    outbound_drag as core_outbound_drag, ports, url_request,
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use rust_cef_runtime::{
    ContextMenuConfig, ContextMenuRegistration, ContextMenuStatus, FindDiagnostics,
    SourceViewDiagnostics, SpellingConfig, SpellingFailure, SpellingStatus,
    UrlRequestAuthChallenge, UrlRequestAuthResolver, UrlRequestState, UrlRequestTicket, win32_menu,
};
pub use rust_cef_runtime::{
    app, deployment, executor, policy,
    runtime::{self, Runtime, RuntimeServices, ShutdownStatus, ShutdownTicket, StartError},
};
pub use surface::{SurfacePipeline, SurfacePipelineError, SurfacePollReport, SurfaceTarget};

/// Non-default test-only surface for explicit validation builds.
#[cfg(all(target_os = "windows", target_arch = "x86_64", feature = "validation"))]
pub mod test_support {
    pub use rust_cef_runtime::test_support::{FrameProbeControl, FrameProbeState};
}
