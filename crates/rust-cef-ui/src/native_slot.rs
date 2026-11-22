//! Framework-agnostic controller for one Win32 alloy child `HWND` browser slot.
//!
//! Zero dependency on GPUI or any other UI toolkit: any host integration
//! (`rust-cef-gpui`, future toolkits, or headless/Win32 probes) can drive
//! [`NativeSlotController`] to validate parent `HWND` affinity, deduplicate
//! physical-pixel `PixelRect` and visibility updates, retry cleanly across
//! `Creating -> Alive` (`EngineError::NotReady`), forward parent move/resize
//! notifications, and guarantee orderly hide-before-close retirement.
use std::{error::Error, fmt, num::NonZeroUsize};

use rust_cef_core::{
    contracts::{BrowserId, BrowserSurfaceMode, NativeChildHwndConfig, PixelRect},
    ports::{BrowserCommand, BrowserHandle, CloseMode, EngineError, WindowBinding},
};
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use rust_cef_runtime::attachment::BrowserAttachment;

use crate::binding::WindowToken;

/// Rejection returned when constructing or driving [`NativeSlotController`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSlotError {
    BrowserMismatch {
        expected: BrowserId,
        actual: BrowserId,
    },
    NotNativeChildHwnd,
    ParentHwndMismatch {
        expected: NonZeroUsize,
        actual: NonZeroUsize,
    },
    Retired,
    InvalidRect,
}

impl fmt::Display for NativeSlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BrowserMismatch { expected, actual } => write!(
                f,
                "browser identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::NotNativeChildHwnd => {
                f.write_str("browser attachment is configured for Offscreen, not NativeChildHwnd")
            }
            Self::ParentHwndMismatch { expected, actual } => write!(
                f,
                "native child parent HWND mismatch: expected 0x{:x}, got 0x{:x}",
                expected.get(),
                actual.get()
            ),
            Self::Retired => f.write_str("native child HWND slot binding is retired"),
            Self::InvalidRect => f.write_str("native child HWND PixelRect is out of bounds"),
        }
    }
}

impl Error for NativeSlotError {}

/// Snapshot of host-to-runtime child `HWND` synchronization state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSlotDiagnostics {
    pub parent_hwnd: NonZeroUsize,
    pub last_bounds: Option<PixelRect>,
    pub visible: bool,
    pub bounds_commands_sent: u64,
    pub visibility_commands_sent: u64,
    pub move_notifications_sent: u64,
    pub focus_commands_sent: u64,
    pub retired: bool,
    pub last_error: Option<EngineError>,
}

/// UI-framework-neutral controller for one `BrowserSurfaceMode::NativeChildHwnd` browser.
pub struct NativeSlotController {
    browser: BrowserId,
    handle: BrowserHandle,
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    _attachment: Option<BrowserAttachment>,
    parent_hwnd: NonZeroUsize,
    token: WindowToken,
    desired_bounds: Option<PixelRect>,
    sent_bounds: Option<PixelRect>,
    desired_visible: bool,
    sent_visible: Option<bool>,
    bounds_commands_sent: u64,
    visibility_commands_sent: u64,
    move_notifications_sent: u64,
    focus_commands_sent: u64,
    last_error: Option<EngineError>,
}

impl NativeSlotController {
    pub fn from_parts(
        browser: BrowserId,
        handle: BrowserHandle,
        config: NativeChildHwndConfig,
        actual_parent_hwnd: NonZeroUsize,
        window_id: u64,
    ) -> Result<Self, NativeSlotError> {
        let actual_browser = handle.snapshot().browser;
        if actual_browser != browser {
            return Err(NativeSlotError::BrowserMismatch {
                expected: browser,
                actual: actual_browser,
            });
        }
        if config.parent_hwnd != actual_parent_hwnd {
            return Err(NativeSlotError::ParentHwndMismatch {
                expected: config.parent_hwnd,
                actual: actual_parent_hwnd,
            });
        }
        if config.initial_bounds.validate().is_err() {
            return Err(NativeSlotError::InvalidRect);
        }
        let token = WindowToken::new(WindowBinding {
            browser,
            window: window_id,
        });
        Ok(Self {
            browser,
            handle,
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            _attachment: None,
            parent_hwnd: actual_parent_hwnd,
            token,
            desired_bounds: Some(config.initial_bounds),
            sent_bounds: None,
            desired_visible: true,
            sent_visible: None,
            bounds_commands_sent: 0,
            visibility_commands_sent: 0,
            move_notifications_sent: 0,
            focus_commands_sent: 0,
            last_error: None,
        })
    }

    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    pub fn from_attachment(
        attachment: BrowserAttachment,
        actual_parent_hwnd: NonZeroUsize,
        window_id: u64,
    ) -> Result<Self, NativeSlotError> {
        let BrowserSurfaceMode::NativeChildHwnd(config) = attachment.surface_mode() else {
            return Err(NativeSlotError::NotNativeChildHwnd);
        };
        let mut controller = Self::from_parts(
            attachment.browser(),
            attachment.handle().clone(),
            config,
            actual_parent_hwnd,
            window_id,
        )?;
        controller._attachment = Some(attachment);
        Ok(controller)
    }

    pub fn set_bounds(&mut self, rect: PixelRect) -> Result<bool, NativeSlotError> {
        if !self.token.is_live() {
            return Err(NativeSlotError::Retired);
        }
        if rect.validate().is_err() {
            return Err(NativeSlotError::InvalidRect);
        }
        self.desired_bounds = Some(rect);
        self.sync()
    }

    pub fn set_visible(&mut self, visible: bool) -> Result<bool, NativeSlotError> {
        if !self.token.is_live() {
            return Err(NativeSlotError::Retired);
        }
        self.desired_visible = visible;
        self.sync()
    }

    /// Reconciles desired geometry/visibility with CEF, deduplicating identical updates and
    /// preserving desired state across transient `EngineError::NotReady` while `Creating`.
    pub fn sync(&mut self) -> Result<bool, NativeSlotError> {
        if !self.token.is_live() {
            return Err(NativeSlotError::Retired);
        }
        let mut sent_any = false;
        let effective_visible =
            self.desired_visible && self.desired_bounds.is_none_or(|rect| !rect.is_empty());

        if !effective_visible {
            if self.sent_visible != Some(false) {
                match self.handle.submit(BrowserCommand::SetVisible(false)) {
                    Ok(_) => {
                        self.sent_visible = Some(false);
                        self.visibility_commands_sent =
                            self.visibility_commands_sent.saturating_add(1);
                        self.last_error = None;
                        sent_any = true;
                    }
                    Err(EngineError::NotReady) => return Ok(sent_any),
                    Err(error) => {
                        self.last_error = Some(error);
                        return Ok(sent_any);
                    }
                }
            }
            return Ok(sent_any);
        }

        if let Some(rect) = self.desired_bounds
            && self.sent_bounds != Some(rect)
        {
            match self.handle.submit(BrowserCommand::SetNativeBounds(rect)) {
                Ok(_) => {
                    self.sent_bounds = Some(rect);
                    self.bounds_commands_sent = self.bounds_commands_sent.saturating_add(1);
                    self.last_error = None;
                    sent_any = true;
                }
                Err(EngineError::NotReady) => return Ok(sent_any),
                Err(error) => {
                    self.last_error = Some(error);
                    return Ok(sent_any);
                }
            }
        }

        if self.sent_visible != Some(true) {
            match self.handle.submit(BrowserCommand::SetVisible(true)) {
                Ok(_) => {
                    self.sent_visible = Some(true);
                    self.visibility_commands_sent = self.visibility_commands_sent.saturating_add(1);
                    self.last_error = None;
                    sent_any = true;
                }
                Err(EngineError::NotReady) => return Ok(sent_any),
                Err(error) => {
                    self.last_error = Some(error);
                    return Ok(sent_any);
                }
            }
        }

        Ok(sent_any)
    }

    pub fn notify_parent_moved(&mut self) -> Result<bool, NativeSlotError> {
        if !self.token.is_live() {
            return Err(NativeSlotError::Retired);
        }
        match self.handle.submit(BrowserCommand::NotifyParentMoved) {
            Ok(_) => {
                self.move_notifications_sent = self.move_notifications_sent.saturating_add(1);
                self.last_error = None;
                Ok(true)
            }
            Err(EngineError::NotReady) => Ok(false),
            Err(error) => {
                self.last_error = Some(error);
                Ok(false)
            }
        }
    }

    pub fn set_focus(&mut self, focused: bool) -> Result<bool, NativeSlotError> {
        if !self.token.is_live() {
            return Err(NativeSlotError::Retired);
        }
        match self.handle.submit(BrowserCommand::SetFocus(focused)) {
            Ok(_) => {
                self.focus_commands_sent = self.focus_commands_sent.saturating_add(1);
                self.last_error = None;
                Ok(true)
            }
            Err(EngineError::NotReady) => Ok(false),
            Err(error) => {
                self.last_error = Some(error);
                Ok(false)
            }
        }
    }

    pub fn retire(&mut self) {
        if !self.token.is_live() {
            return;
        }
        self.token.retire();
        let _ = self.handle.submit(BrowserCommand::SetVisible(false));
        self.handle.request_close(CloseMode::Graceful);
    }

    pub fn browser_id(&self) -> BrowserId {
        self.browser
    }

    pub fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub fn parent_hwnd(&self) -> NonZeroUsize {
        self.parent_hwnd
    }

    pub fn token(&self) -> &WindowToken {
        &self.token
    }

    pub fn is_live(&self) -> bool {
        self.token.is_live()
    }

    pub fn diagnostics(&self) -> NativeSlotDiagnostics {
        let effective_visible = self.token.is_live()
            && self.sent_visible.unwrap_or(self.desired_visible)
            && self.desired_bounds.is_none_or(|rect| !rect.is_empty());
        NativeSlotDiagnostics {
            parent_hwnd: self.parent_hwnd,
            last_bounds: self.sent_bounds.or(self.desired_bounds),
            visible: effective_visible,
            bounds_commands_sent: self.bounds_commands_sent,
            visibility_commands_sent: self.visibility_commands_sent,
            move_notifications_sent: self.move_notifications_sent,
            focus_commands_sent: self.focus_commands_sent,
            retired: !self.token.is_live(),
            last_error: self.last_error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{BrowserSnapshot, MAX_NATIVE_BOUND, RuntimeGeneration, SessionState},
        ports::{BrowserControl, CloseMode, CloseTicket, CommandId},
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    struct StubControl {
        browser: BrowserId,
        ready: AtomicBool,
        next_seq: AtomicU64,
        commands: Mutex<Vec<BrowserCommand>>,
        closes: Mutex<Vec<CloseMode>>,
    }

    impl StubControl {
        fn new(browser: BrowserId, ready: bool) -> Arc<Self> {
            Arc::new(Self {
                browser,
                ready: AtomicBool::new(ready),
                next_seq: AtomicU64::new(1),
                commands: Mutex::new(Vec::new()),
                closes: Mutex::new(Vec::new()),
            })
        }
    }

    impl BrowserControl for StubControl {
        fn snapshot(&self) -> BrowserSnapshot {
            BrowserSnapshot {
                browser: self.browser,
                creation_accepted: self.ready.load(Ordering::Acquire),
                state: if self.ready.load(Ordering::Acquire) {
                    SessionState::Alive
                } else {
                    SessionState::Creating
                },
                navigation: Default::default(),
                renderer: Default::default(),
            }
        }

        fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError> {
            if !self.ready.load(Ordering::Acquire) {
                return Err(EngineError::NotReady);
            }
            self.commands.lock().unwrap().push(command);
            let sequence = self.next_seq.fetch_add(1, Ordering::Relaxed);
            Ok(CommandId {
                browser: self.browser,
                sequence,
            })
        }

        fn request_close(&self, mode: CloseMode) -> CloseTicket {
            self.closes.lock().unwrap().push(mode);
            CloseTicket::pending(self.browser, 1).0
        }
    }

    fn browser_id(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(7), local)
    }

    fn cfg(parent: usize) -> NativeChildHwndConfig {
        NativeChildHwndConfig {
            parent_hwnd: NonZeroUsize::new(parent).unwrap(),
            initial_bounds: PixelRect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            activate_on_creation: false,
        }
    }

    #[test]
    fn rejects_parent_hwnd_and_browser_id_mismatch_and_invalid_rect() {
        let id = browser_id(1);
        let stub = StubControl::new(id, true);
        let handle = BrowserHandle::new(stub.clone());

        assert_eq!(
            NativeSlotController::from_parts(
                id,
                handle.clone(),
                cfg(0x1000),
                NonZeroUsize::new(0x2000).unwrap(),
                1,
            )
            .err(),
            Some(NativeSlotError::ParentHwndMismatch {
                expected: NonZeroUsize::new(0x1000).unwrap(),
                actual: NonZeroUsize::new(0x2000).unwrap(),
            })
        );

        assert_eq!(
            NativeSlotController::from_parts(
                browser_id(2),
                handle.clone(),
                cfg(0x1000),
                NonZeroUsize::new(0x1000).unwrap(),
                1,
            )
            .err(),
            Some(NativeSlotError::BrowserMismatch {
                expected: browser_id(2),
                actual: id,
            })
        );

        let mut slot = NativeSlotController::from_parts(
            id,
            handle,
            cfg(0x1000),
            NonZeroUsize::new(0x1000).unwrap(),
            1,
        )
        .unwrap();
        assert_eq!(
            slot.set_bounds(PixelRect {
                x: 0,
                y: 0,
                width: MAX_NATIVE_BOUND + 1,
                height: 100,
            }),
            Err(NativeSlotError::InvalidRect)
        );
        assert!(stub.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn deduplicates_bounds_and_converges_after_not_ready() {
        let id = browser_id(2);
        let stub = StubControl::new(id, false);
        let mut slot = NativeSlotController::from_parts(
            id,
            BrowserHandle::new(stub.clone()),
            cfg(0x1000),
            NonZeroUsize::new(0x1000).unwrap(),
            1,
        )
        .unwrap();

        let rect = PixelRect {
            x: 16,
            y: 32,
            width: 800,
            height: 600,
        };
        assert_eq!(slot.set_bounds(rect), Ok(false));
        assert!(stub.commands.lock().unwrap().is_empty());
        assert_eq!(slot.diagnostics().last_error, None);

        stub.ready.store(true, Ordering::Release);
        assert_eq!(slot.sync(), Ok(true));
        assert_eq!(slot.set_bounds(rect), Ok(false));
        assert_eq!(
            *stub.commands.lock().unwrap(),
            vec![
                BrowserCommand::SetNativeBounds(rect),
                BrowserCommand::SetVisible(true),
            ]
        );
        let diag = slot.diagnostics();
        assert_eq!(diag.bounds_commands_sent, 1);
        assert_eq!(diag.visibility_commands_sent, 1);
        assert_eq!(diag.last_bounds, Some(rect));
        assert!(diag.visible);
    }

    #[test]
    fn hides_on_zero_bounds_restores_on_non_empty_and_retires_cleanly() {
        let id = browser_id(3);
        let stub = StubControl::new(id, true);
        let mut slot = NativeSlotController::from_parts(
            id,
            BrowserHandle::new(stub.clone()),
            cfg(0x1000),
            NonZeroUsize::new(0x1000).unwrap(),
            1,
        )
        .unwrap();

        let initial = PixelRect {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        assert_eq!(slot.set_bounds(initial), Ok(true));
        assert_eq!(
            slot.set_bounds(PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
            Ok(true)
        );
        assert!(!slot.diagnostics().visible);

        assert_eq!(slot.set_bounds(initial), Ok(true));
        assert!(slot.diagnostics().visible);

        assert_eq!(slot.notify_parent_moved(), Ok(true));
        assert_eq!(slot.set_focus(true), Ok(true));
        slot.retire();
        assert!(!slot.is_live());
        assert_eq!(slot.set_visible(true), Err(NativeSlotError::Retired));
        assert_eq!(slot.notify_parent_moved(), Err(NativeSlotError::Retired));
        assert_eq!(slot.set_focus(false), Err(NativeSlotError::Retired));

        let commands = stub.commands.lock().unwrap();
        assert_eq!(
            *commands,
            vec![
                BrowserCommand::SetNativeBounds(initial),
                BrowserCommand::SetVisible(true),
                BrowserCommand::SetVisible(false),
                BrowserCommand::SetVisible(true),
                BrowserCommand::NotifyParentMoved,
                BrowserCommand::SetFocus(true),
                BrowserCommand::SetVisible(false),
            ]
        );
        assert_eq!(*stub.closes.lock().unwrap(), vec![CloseMode::Graceful]);
    }
}
