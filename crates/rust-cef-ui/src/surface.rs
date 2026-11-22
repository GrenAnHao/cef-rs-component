//! Framework-agnostic UI surface presentation pipeline.
//!
//! Any Rust UI framework (GPUI, Rune, egui, iced, slint, winit) can implement
//! [`SurfaceTarget`] and drive [`SurfacePipeline`] on its UI thread to consume
//! CEF VIEW and POPUP frames, manage viewport resizes, and route input/IME/drag
//! state without depending on GPUI or touching CEF internals.
use crate::{binding::WindowToken, file_drag::FileDragSession, input::InputLane};
use rust_cef_core::{
    contracts::{BrowserId, Viewport},
    mailbox::{FrameUpdate, SurfaceBounds},
    ports::{
        BrowserCommand, BrowserHandle, EngineError, FrameSource, OwnedFrame, PixelFormat,
        SurfaceKind, WindowBinding,
    },
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use rust_cef_runtime::attachment::BrowserAttachment;
use std::sync::Arc;

/// Summary of one [`SurfacePipeline::poll_frames`] turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfacePollReport {
    pub consumed_update: bool,
    pub view_presented: bool,
    pub popup_presented: bool,
    pub popup_hidden: bool,
    pub browser_closed: bool,
    pub stale_or_invalid_skipped: u64,
}

/// Errors returned while presenting frames to a [`SurfaceTarget`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfacePipelineError<E> {
    Retired,
    Target(E),
}

/// Plug-in rendering target implemented by each Rust UI framework adapter
/// (e.g. `rust-cef-gpui`, future `rust-cef-rune`, wgpu/egui/iced).
pub trait SurfaceTarget {
    type Error;

    /// Uploads or replaces the main browser VIEW frame (premultiplied BGRA8).
    fn present_view(&mut self, frame: Arc<OwnedFrame>) -> Result<(), Self::Error>;

    /// Uploads or replaces the active browser POPUP overlay (e.g. `<select>`).
    fn present_popup(
        &mut self,
        bounds: SurfaceBounds,
        frame: Arc<OwnedFrame>,
    ) -> Result<(), Self::Error>;

    /// Releases and hides any currently displayed POPUP overlay.
    fn hide_popup(&mut self) -> Result<(), Self::Error>;
}

/// Cohesive, UI-framework-independent controller for one attached Browser
/// surface: consumes VIEW/POPUP updates from [`FrameSource`], tracks surface
/// generations, and holds bounded input/drag/token state.
pub struct SurfacePipeline {
    browser: BrowserId,
    handle: BrowserHandle,
    frames: Arc<dyn FrameSource>,
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    _attachment: Option<BrowserAttachment>,
    token: WindowToken,
    input: InputLane,
    file_drag: Option<FileDragSession>,
    view_generation: u64,
    view_sequence: u64,
    popup_visible: bool,
    popup_generation: u64,
    popup_sequence: u64,
    popup_bounds: Option<SurfaceBounds>,
}

impl SurfacePipeline {
    /// Creates a new framework-agnostic surface pipeline from explicit
    /// `BrowserHandle` + `FrameSource` endpoints and a host window identifier.
    pub fn from_parts(
        browser: BrowserId,
        handle: BrowserHandle,
        frames: Arc<dyn FrameSource>,
        window_id: u64,
    ) -> Self {
        let token = WindowToken::new(WindowBinding {
            browser,
            window: window_id,
        });
        Self {
            browser,
            handle,
            frames,
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            _attachment: None,
            token,
            input: InputLane::default(),
            file_drag: None,
            view_generation: 0,
            view_sequence: 0,
            popup_visible: false,
            popup_generation: 0,
            popup_sequence: 0,
            popup_bounds: None,
        }
    }

    /// Creates a new framework-agnostic surface pipeline owning a one-shot
    /// [`BrowserAttachment`] from `Runtime::attach_browser`.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    pub fn new(attachment: BrowserAttachment, window_id: u64) -> Self {
        let browser = attachment.browser();
        let handle = attachment.handle().clone();
        let frames = attachment.frame_source().clone();
        let mut this = Self::from_parts(browser, handle, frames, window_id);
        this._attachment = Some(attachment);
        this
    }

    /// Creates a new offscreen surface pipeline, rejecting `NativeChildHwnd`
    /// attachments before consuming the attachment token.
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    pub fn try_new(
        attachment: BrowserAttachment,
        window_id: u64,
    ) -> Result<Self, SurfacePipelineError<()>> {
        if attachment.surface_mode().is_native_child_hwnd() {
            return Err(SurfacePipelineError::Target(()));
        }
        Ok(Self::new(attachment, window_id))
    }

    pub fn browser_id(&self) -> BrowserId {
        self.browser
    }

    pub fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub fn token(&self) -> &WindowToken {
        &self.token
    }

    pub fn input(&self) -> &InputLane {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut InputLane {
        &mut self.input
    }

    pub fn file_drag(&self) -> Option<&FileDragSession> {
        self.file_drag.as_ref()
    }

    pub fn file_drag_mut(&mut self) -> &mut Option<FileDragSession> {
        &mut self.file_drag
    }

    pub fn view_generation(&self) -> u64 {
        self.view_generation
    }

    pub fn view_sequence(&self) -> u64 {
        self.view_sequence
    }

    pub fn popup_visible(&self) -> bool {
        self.popup_visible
    }

    pub fn popup_bounds(&self) -> Option<SurfaceBounds> {
        self.popup_bounds
    }

    /// Submits a viewport resize command to CEF for this browser surface.
    pub fn resize(&self, viewport: Viewport) -> Result<(), EngineError> {
        if !self.token.is_live() {
            return Err(EngineError::Closed);
        }
        self.handle
            .submit(BrowserCommand::SetViewport(viewport))
            .map(|_| ())
    }

    /// Consumes a pending [`FrameUpdate`] (if any) from the attached
    /// [`FrameSource`] and forwards valid, newer VIEW/POPUP frames to `target`.
    pub fn poll_frames<T: SurfaceTarget>(
        &mut self,
        target: &mut T,
    ) -> Result<SurfacePollReport, SurfacePipelineError<T::Error>> {
        if !self.token.is_live() {
            return Err(SurfacePipelineError::Retired);
        }
        let Some(update) = self.frames.take_update() else {
            return Ok(SurfacePollReport::default());
        };
        self.apply_update(update, target)
    }

    /// Applies an explicit [`FrameUpdate`] snapshot (useful for resync).
    pub fn apply_update<T: SurfaceTarget>(
        &mut self,
        update: FrameUpdate,
        target: &mut T,
    ) -> Result<SurfacePollReport, SurfacePipelineError<T::Error>> {
        if !self.token.is_live() {
            return Err(SurfacePipelineError::Retired);
        }
        let mut report = SurfacePollReport {
            consumed_update: true,
            browser_closed: update.closed,
            ..Default::default()
        };

        if update.browser != self.browser {
            report.stale_or_invalid_skipped += 1;
            return Ok(report);
        }

        if let Some(frame) = update.view.frame {
            if self.accept_view(&frame, update.view.generation) {
                let (generation, seq) = (frame.info.surface_generation, frame.info.sequence);
                target
                    .present_view(frame)
                    .map_err(SurfacePipelineError::Target)?;
                self.view_generation = generation;
                self.view_sequence = seq;
                report.view_presented = true;
            } else {
                report.stale_or_invalid_skipped += 1;
            }
        }

        if !update.popup.visible || update.closed {
            if self.popup_visible {
                target.hide_popup().map_err(SurfacePipelineError::Target)?;
                self.popup_visible = false;
                self.popup_bounds = None;
                report.popup_hidden = true;
            }
        } else if let Some(frame) = update.popup.frame {
            let bounds = update.popup.bounds;
            if self.accept_popup(&frame, update.popup.generation)
                && bounds.width > 0
                && bounds.height > 0
            {
                let (generation, seq) = (frame.info.surface_generation, frame.info.sequence);
                target
                    .present_popup(bounds, frame)
                    .map_err(SurfacePipelineError::Target)?;
                self.popup_visible = true;
                self.popup_generation = generation;
                self.popup_sequence = seq;
                self.popup_bounds = Some(bounds);
                report.popup_presented = true;
            } else {
                report.stale_or_invalid_skipped += 1;
            }
        }

        Ok(report)
    }

    /// Retires this surface pipeline so no further frames or resizes are
    /// accepted for this window binding.
    pub fn retire(&mut self) -> bool {
        self.file_drag = None;
        self.popup_visible = false;
        self.popup_bounds = None;
        self.token.retire()
    }

    fn accept_view(&self, frame: &OwnedFrame, expected_generation: u64) -> bool {
        valid_frame(frame, self.browser, SurfaceKind::View, expected_generation)
            && (frame.info.surface_generation > self.view_generation
                || (frame.info.surface_generation == self.view_generation
                    && frame.info.sequence > self.view_sequence))
    }

    fn accept_popup(&self, frame: &OwnedFrame, expected_generation: u64) -> bool {
        valid_frame(frame, self.browser, SurfaceKind::Popup, expected_generation)
            && (frame.info.surface_generation > self.popup_generation
                || (frame.info.surface_generation == self.popup_generation
                    && frame.info.sequence > self.popup_sequence))
    }
}

fn valid_frame(
    frame: &OwnedFrame,
    expected_browser: BrowserId,
    expected_surface: SurfaceKind,
    expected_generation: u64,
) -> bool {
    let info = frame.info;
    info.browser == expected_browser
        && info.surface == expected_surface
        && info.surface_generation == expected_generation
        && info.format == PixelFormat::PremultipliedBgra8
        && info.width > 0
        && info.height > 0
        && info
            .width
            .checked_mul(info.height)
            .and_then(|px| px.checked_mul(4))
            .is_some_and(|expected| frame.pixels.as_slice().len() == expected as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{
            BrowserSnapshot, NavigationSnapshot, RendererSnapshot, RuntimeGeneration, SessionState,
        },
        frame_budget::FrameBudget,
        mailbox::SurfaceSnapshot,
        ports::{BrowserControl, CloseMode, CloseTicket, CommandId, FrameInfo},
    };
    use std::sync::Mutex;

    struct DummyControl {
        snapshot: BrowserSnapshot,
    }
    impl BrowserControl for DummyControl {
        fn snapshot(&self) -> BrowserSnapshot {
            self.snapshot.clone()
        }
        fn submit(&self, _: BrowserCommand) -> Result<CommandId, EngineError> {
            Ok(CommandId {
                browser: self.snapshot.browser,
                sequence: 1,
            })
        }
        fn request_close(&self, _: CloseMode) -> CloseTicket {
            CloseTicket::pending(self.snapshot.browser, 1).0
        }
        fn input_snapshot(&self) -> Result<rust_cef_core::input::InputSnapshot, EngineError> {
            Err(EngineError::NotReady)
        }
        fn ime_snapshot(&self) -> Result<rust_cef_core::ime::ImeSnapshot, EngineError> {
            Err(EngineError::NotReady)
        }
    }

    struct MockSource {
        update: Mutex<Option<FrameUpdate>>,
    }
    impl FrameSource for MockSource {
        fn latest(&self, _: SurfaceKind) -> Option<Arc<OwnedFrame>> {
            None
        }
        fn take_update(&self) -> Option<FrameUpdate> {
            self.update.lock().unwrap().take()
        }
        fn snapshot(&self) -> FrameUpdate {
            self.update.lock().unwrap().clone().unwrap()
        }
    }

    #[derive(Default)]
    struct MockTarget {
        views: Vec<(u64, u64)>,
        popups: Vec<(SurfaceBounds, u64, u64)>,
        hides: u64,
    }
    impl SurfaceTarget for MockTarget {
        type Error = ();
        fn present_view(&mut self, frame: Arc<OwnedFrame>) -> Result<(), ()> {
            self.views
                .push((frame.info.surface_generation, frame.info.sequence));
            Ok(())
        }
        fn present_popup(
            &mut self,
            bounds: SurfaceBounds,
            frame: Arc<OwnedFrame>,
        ) -> Result<(), ()> {
            self.popups
                .push((bounds, frame.info.surface_generation, frame.info.sequence));
            Ok(())
        }
        fn hide_popup(&mut self) -> Result<(), ()> {
            self.hides += 1;
            Ok(())
        }
    }

    fn make_frame(
        budget: &Arc<FrameBudget>,
        browser: BrowserId,
        surface: SurfaceKind,
        generation: u64,
        sequence: u64,
    ) -> Arc<OwnedFrame> {
        let pixels = budget.allocate(16).expect("budget alloc").freeze();
        Arc::new(OwnedFrame {
            info: FrameInfo {
                browser,
                surface,
                surface_generation: generation,
                sequence,
                width: 2,
                height: 2,
                format: PixelFormat::PremultipliedBgra8,
            },
            pixels,
        })
    }

    #[test]
    fn pipeline_presents_view_rejects_stale_and_tracks_popup_show_hide() {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
        let budget = Arc::new(FrameBudget::new(1024));
        let control = Arc::new(DummyControl {
            snapshot: BrowserSnapshot {
                browser,
                creation_accepted: true,
                state: SessionState::Alive,
                navigation: NavigationSnapshot::default(),
                renderer: RendererSnapshot::default(),
            },
        });
        let popup_bounds = SurfaceBounds {
            x: 10,
            y: 20,
            width: 80,
            height: 40,
        };
        let source = Arc::new(MockSource {
            update: Mutex::new(Some(FrameUpdate {
                browser,
                viewport: None,
                view: SurfaceSnapshot {
                    generation: 2,
                    bounds: SurfaceBounds {
                        x: 0,
                        y: 0,
                        width: 2,
                        height: 2,
                    },
                    visible: true,
                    pixel_width: 2,
                    pixel_height: 2,
                    frame: Some(make_frame(&budget, browser, SurfaceKind::View, 2, 5)),
                },
                popup: SurfaceSnapshot {
                    generation: 1,
                    bounds: popup_bounds,
                    visible: true,
                    pixel_width: 2,
                    pixel_height: 2,
                    frame: Some(make_frame(&budget, browser, SurfaceKind::Popup, 1, 1)),
                },
                closed: false,
            })),
        });
        let mut pipeline =
            SurfacePipeline::from_parts(browser, BrowserHandle::new(control), source.clone(), 42);
        let mut target = MockTarget::default();

        let first = pipeline.poll_frames(&mut target).unwrap();
        assert!(first.consumed_update);
        assert!(first.view_presented);
        assert!(first.popup_presented);
        assert_eq!(target.views, vec![(2, 5)]);
        assert_eq!(target.popups, vec![(popup_bounds, 1, 1)]);
        assert!(pipeline.popup_visible());

        // Stale view sequence must be skipped; hidden popup triggers hide_popup.
        *source.update.lock().unwrap() = Some(FrameUpdate {
            browser,
            viewport: None,
            view: SurfaceSnapshot {
                generation: 2,
                bounds: SurfaceBounds::default(),
                visible: true,
                pixel_width: 2,
                pixel_height: 2,
                frame: Some(make_frame(&budget, browser, SurfaceKind::View, 2, 4)),
            },
            popup: SurfaceSnapshot {
                generation: 1,
                bounds: SurfaceBounds::default(),
                visible: false,
                pixel_width: 0,
                pixel_height: 0,
                frame: None,
            },
            closed: false,
        });
        let second = pipeline.poll_frames(&mut target).unwrap();
        assert!(second.consumed_update);
        assert!(!second.view_presented);
        assert!(second.popup_hidden);
        assert_eq!(second.stale_or_invalid_skipped, 1);
        assert_eq!(target.hides, 1);
        assert!(!pipeline.popup_visible());

        assert!(pipeline.retire());
        assert_eq!(
            pipeline.poll_frames(&mut target),
            Err(SurfacePipelineError::Retired)
        );
    }
}
