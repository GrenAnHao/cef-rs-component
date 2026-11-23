//! Finite GPUI input ownership and one explicit backpressure retry slot.
//! The old complete-click candidate is retained solely as a diagnostic for
//! in-bounds left pairs; real gestures use ordered raw input commands.
use rust_cef_core::{
    input::{InputEvent, InputKind, InputResetReason, InputSnapshot},
    keymap,
    ports::{BrowserCommand, BrowserHandle, EngineError, MouseClick, MouseModifiers},
};
use std::num::NonZeroU8;

/// Actual GPUI callback/paint entry observations, never inferred from Runtime
/// admission, a final snapshot, or the OS message trace. No key/text is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCallback {
    KeyDown,
    KeyUp,
    ReplaceText,
    ReplaceAndMarkText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCallbackGate {
    WindowInactive,
    FocusNotOwned,
    BindingRetired,
    SnapshotUnavailable,
    CleanupUnavailable,
    TextNotAccepted,
    FocusCommandRejected,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputCallbackDiagnostics {
    pub key_down_entered: u64,
    pub key_up_entered: u64,
    pub replace_text_entered: u64,
    pub replace_and_mark_text_entered: u64,
    /// Calls made in paint while the original FocusHandle is focused. This
    /// does not prove GPUI installed a platform handler or dispatched WM_CHAR.
    pub focused_handler_paint_attempts: u64,
    pub key_down_rejected: u64,
    pub key_up_rejected: u64,
    pub replace_text_rejected: u64,
    pub replace_and_mark_text_rejected: u64,
    pub last_rejection: Option<(InputCallback, InputCallbackGate)>,
}

impl InputCallbackDiagnostics {
    fn enter(&mut self, source: InputCallback) {
        let count = match source {
            InputCallback::KeyDown => &mut self.key_down_entered,
            InputCallback::KeyUp => &mut self.key_up_entered,
            InputCallback::ReplaceText => &mut self.replace_text_entered,
            InputCallback::ReplaceAndMarkText => &mut self.replace_and_mark_text_entered,
        };
        *count = count.saturating_add(1);
    }

    fn reject(&mut self, source: InputCallback, gate: InputCallbackGate) {
        let count = match source {
            InputCallback::KeyDown => &mut self.key_down_rejected,
            InputCallback::KeyUp => &mut self.key_up_rejected,
            InputCallback::ReplaceText => &mut self.replace_text_rejected,
            InputCallback::ReplaceAndMarkText => &mut self.replace_and_mark_text_rejected,
        };
        *count = count.saturating_add(1);
        self.last_rejection = Some((source, gate));
    }
}

/// The snapshot is always queried from Runtime, not inferred from GPUI admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDiagnostics {
    pub snapshot: Result<InputSnapshot, EngineError>,
    pub pending: bool,
    pub rejected: u64,
    pub retry_attempts: u64,
    pub retry_accepted: u64,
    pub last_error: Option<EngineError>,
    pub callbacks: InputCallbackDiagnostics,
}

/// A bounded effect on GPUI's UTF-16 composition mirror. It travels with the
/// same owned event and is applied only after Runtime accepts that event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorEffect {
    Mark {
        text: String,
        selection: std::ops::Range<usize>,
    },
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputAdmission {
    Admitted(Option<MirrorEffect>),
    AdmittedAndReset,
    Deferred,
    Idle,
}

struct PendingInput {
    event: InputEvent,
    window: u64,
    accepted_click: bool,
    mirror: Option<MirrorEffect>,
    overflow: bool,
}

/// Local ownership never substitutes for the Runtime's native execution ledger.
/// A pending event retains its original window and epoch; it is never rebased.
pub struct InputLane {
    pending: Option<PendingInput>,
    rejected: u64,
    retry_attempts: u64,
    retry_accepted: u64,
    last_error: Option<EngineError>,
    accepted_clicks: u64,
    rejected_clicks: u64,
    buttons: [bool; 5],
    reset_armed: bool,
    callbacks: InputCallbackDiagnostics,
}

impl Default for InputLane {
    fn default() -> Self {
        Self {
            pending: None,
            rejected: 0,
            retry_attempts: 0,
            retry_accepted: 0,
            last_error: None,
            accepted_clicks: 0,
            rejected_clicks: 0,
            buttons: [false; 5],
            reset_armed: true,
            callbacks: InputCallbackDiagnostics::default(),
        }
    }
}

impl InputLane {
    pub fn diagnostics(&self, handle: &BrowserHandle) -> InputDiagnostics {
        InputDiagnostics {
            snapshot: handle.input_snapshot(),
            pending: self.pending.is_some(),
            rejected: self.rejected,
            retry_attempts: self.retry_attempts,
            retry_accepted: self.retry_accepted,
            last_error: self.last_error.clone(),
            callbacks: self.callbacks.clone(),
        }
    }
    pub fn note_callback_entered(&mut self, source: InputCallback) {
        self.callbacks.enter(source);
    }
    pub fn note_callback_rejected(&mut self, source: InputCallback, gate: InputCallbackGate) {
        self.callbacks.reject(source, gate);
    }
    pub fn note_focused_handler_paint_attempt(&mut self) {
        self.callbacks.focused_handler_paint_attempts = self
            .callbacks
            .focused_handler_paint_attempts
            .saturating_add(1);
    }
    pub fn accepted_clicks(&self) -> u64 {
        self.accepted_clicks
    }
    pub fn rejected_clicks(&self) -> u64 {
        self.rejected_clicks
    }
    pub fn mark_focus(&mut self) {
        self.reset_armed = true;
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn pending_mark(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            matches!(pending.mirror.as_ref(), Some(MirrorEffect::Mark { .. }))
        })
    }
    pub fn note_overflow(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.overflow = true;
            self.rejected = self.rejected.saturating_add(1);
            self.last_error = Some(EngineError::Backpressure);
        }
    }

    /// A focused GPUI callback lacked a usable Runtime input context.
    /// Cleanup already in progress owns the reset lane; do not post it again.
    pub fn reject_focus_unavailable(
        &mut self,
        handle: &BrowserHandle,
        error: EngineError,
        cleanup_in_progress: bool,
    ) {
        if self.pending.is_some() {
            self.note_overflow();
            return;
        }
        self.rejected = self.rejected.saturating_add(1);
        self.last_error = Some(error);
        if cleanup_in_progress {
            self.buttons = [false; 5];
            self.reset_armed = false;
        } else {
            self.abandon(handle, InputResetReason::Rejected);
        }
    }
    pub fn held_button(&self) -> bool {
        self.buttons.contains(&true)
    }
    pub fn owns_button(&self, button: rust_cef_core::input::MouseButton) -> bool {
        self.buttons[button_index(button)]
    }
    pub fn clear_held_buttons(&mut self) {
        self.buttons = [false; 5];
    }
    pub fn abandon(&mut self, handle: &BrowserHandle, reason: InputResetReason) {
        if self
            .pending
            .take()
            .is_some_and(|pending| pending.accepted_click)
        {
            self.rejected_clicks = self.rejected_clicks.saturating_add(1);
        }
        self.buttons = [false; 5];
        if !self.reset_armed {
            return;
        }
        self.reset_armed = false;
        if let Err(error) = handle.submit(BrowserCommand::ResetInput(reason)) {
            self.last_error = Some(error);
        }
    }
    fn reject(&mut self, handle: &BrowserHandle, error: EngineError) -> EngineError {
        self.rejected = self.rejected.saturating_add(1);
        self.last_error = Some(error.clone());
        self.abandon(handle, InputResetReason::Rejected);
        error
    }
    fn admitted(&mut self, event: &InputEvent, accepted_click: bool) {
        if let InputKind::MouseButton { button, up, .. } = &event.kind {
            self.buttons[button_index(*button)] = !*up;
        }
        if accepted_click {
            self.accepted_clicks = self.accepted_clicks.saturating_add(1);
        }
        // Passive hover/wheel samples do not create held native button/key/IME
        // state, so they must not arm spurious ResetInput commands on inactive windows.
        if !is_transient_pointer_input(&event.kind) {
            self.reset_armed = true;
        }
        self.last_error = None;
    }
    pub fn submit(
        &mut self,
        handle: &BrowserHandle,
        window: u64,
        kind: InputKind,
        accepted_click: bool,
        mirror: Option<MirrorEffect>,
    ) -> Result<InputAdmission, EngineError> {
        self.submit_with_epoch(handle, window, kind, accepted_click, mirror, None)
            .map(|(admission, _)| admission)
    }
    /// High-rate `MouseMove` fast path bypassing `input_snapshot()` and `Session`
    /// command-queue capacity while preserving causal order before discrete events.
    pub fn submit_mouse_move_fast(
        &mut self,
        handle: &BrowserHandle,
        x: i32,
        y: i32,
        leaving: bool,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        if self.pending.is_some() {
            self.last_error = Some(EngineError::Backpressure);
            return Err(EngineError::Backpressure);
        }
        match handle.submit_mouse_move_fast(x, y, leaving, modifiers) {
            Ok(()) => {
                self.last_error = None;
                Ok(())
            }
            Err(EngineError::Backpressure) => {
                self.last_error = Some(EngineError::Backpressure);
                Err(EngineError::Backpressure)
            }
            Err(error) => {
                if matches!(
                    error,
                    EngineError::NotReady | EngineError::Closing | EngineError::Closed
                ) {
                    self.last_error = Some(error.clone());
                    Err(error)
                } else {
                    Err(self.reject(handle, error))
                }
            }
        }
    }
    /// High-rate `MouseWheel` fast path bypassing `input_snapshot()` and `Session`
    /// command-queue capacity while preserving causal order before discrete events.
    pub fn submit_mouse_wheel_fast(
        &mut self,
        handle: &BrowserHandle,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        if self.pending.is_some() {
            self.last_error = Some(EngineError::Backpressure);
            return Err(EngineError::Backpressure);
        }
        match handle.submit_mouse_wheel_fast(x, y, delta_x, delta_y, modifiers) {
            Ok(()) => {
                self.last_error = None;
                Ok(())
            }
            Err(EngineError::Backpressure) => {
                self.last_error = Some(EngineError::Backpressure);
                Err(EngineError::Backpressure)
            }
            Err(error) => {
                if matches!(
                    error,
                    EngineError::NotReady | EngineError::Closing | EngineError::Closed
                ) {
                    self.last_error = Some(error.clone());
                    Err(error)
                } else {
                    Err(self.reject(handle, error))
                }
            }
        }
    }
    /// Returns the epoch of the exact owned event, not a later snapshot that a
    /// concurrent reset could already have advanced. This adds no pending slot.
    pub fn submit_with_epoch(
        &mut self,
        handle: &BrowserHandle,
        window: u64,
        kind: InputKind,
        accepted_click: bool,
        mirror: Option<MirrorEffect>,
        expected_epoch: Option<u64>,
    ) -> Result<(InputAdmission, u64), EngineError> {
        // A release retains the epoch of its own DOWN, even when a host on
        // another thread resets this Browser without changing GPUI focus.
        if expected_epoch
            .is_some_and(|epoch| handle.input_snapshot().is_ok_and(|s| s.epoch != epoch))
        {
            return Err(self.reject_old_epoch());
        }
        // No later discrete event may overtake the original event. Preserve
        // its one host-selected retry opportunity even when later input arrives.
        // Transient pointer streams (`MouseMove` / `MouseWheel`) simply drop the
        // redundant sample on backpressure so they never poison a held drag or
        // pending discrete event.
        if self.pending.is_some() {
            if is_transient_pointer_input(&kind) {
                self.last_error = Some(EngineError::Backpressure);
                return Err(EngineError::Backpressure);
            }
            self.note_overflow();
            return Err(EngineError::Backpressure);
        }
        let snapshot = handle
            .input_snapshot()
            .map_err(|error| self.reject(handle, error))?;
        if expected_epoch.is_some_and(|epoch| epoch != snapshot.epoch) {
            return Err(self.reject_old_epoch());
        }
        if snapshot.cleanup_pending || snapshot.cleanup_failed {
            if snapshot.cleanup_pending
                && !snapshot.cleanup_failed
                && is_transient_pointer_input(&kind)
            {
                self.last_error = Some(EngineError::Backpressure);
                return Err(EngineError::Backpressure);
            }
            return Err(self.reject(handle, EngineError::NotReady));
        }
        let event = InputEvent {
            epoch: snapshot.epoch,
            kind,
        };
        event
            .validate()
            .map_err(|error| self.reject(handle, error))?;
        match handle.submit(BrowserCommand::Input(event.clone())) {
            Ok(_) => {
                self.admitted(&event, accepted_click);
                Ok((InputAdmission::Admitted(mirror), event.epoch))
            }
            Err(EngineError::Backpressure) if is_transient_pointer_input(&event.kind) => {
                self.last_error = Some(EngineError::Backpressure);
                Err(EngineError::Backpressure)
            }
            Err(EngineError::Backpressure) if file_drag_input(&event.kind) => {
                // Platform file sessions cannot be replayed after Drop/Exited.
                // Preserve a newer epoch if reset raced the failed submission.
                if handle
                    .input_snapshot()
                    .is_ok_and(|s| s.epoch != event.epoch)
                {
                    return Err(self.reject_old_epoch());
                }
                Err(self.reject(handle, EngineError::Backpressure))
            }
            Err(EngineError::Backpressure) => {
                self.last_error = Some(EngineError::Backpressure);
                self.reset_armed = true;
                let epoch = event.epoch;
                self.pending = Some(PendingInput {
                    event,
                    window,
                    accepted_click,
                    mirror,
                    overflow: false,
                });
                Ok((InputAdmission::Deferred, epoch))
            }
            Err(error) => {
                if expected_epoch
                    .is_some_and(|epoch| handle.input_snapshot().is_ok_and(|s| s.epoch != epoch))
                {
                    return Err(self.reject_old_epoch());
                }
                if accepted_click {
                    self.rejected_clicks = self.rejected_clicks.saturating_add(1);
                }
                Err(self.reject(handle, error))
            }
        }
    }
    fn reject_old_epoch(&mut self) -> EngineError {
        self.rejected = self.rejected.saturating_add(1);
        self.last_error = Some(EngineError::NotReady);
        // Do not reset or overflow a newer epoch's gesture/pending record.
        EngineError::NotReady
    }
    /// Exactly one attempt, only at the host's explicit UI turn.
    pub fn retry(
        &mut self,
        handle: &BrowserHandle,
        window: u64,
    ) -> Result<InputAdmission, EngineError> {
        let Some(pending) = self.pending.take() else {
            return Ok(InputAdmission::Idle);
        };
        self.retry_attempts = self.retry_attempts.saturating_add(1);
        let snapshot = handle.input_snapshot().map_err(|error| {
            if pending.accepted_click {
                self.rejected_clicks = self.rejected_clicks.saturating_add(1);
            }
            self.reject(handle, error)
        })?;
        if pending.window != window
            || snapshot.epoch != pending.event.epoch
            || snapshot.cleanup_pending
            || snapshot.cleanup_failed
        {
            if pending.accepted_click {
                self.rejected_clicks = self.rejected_clicks.saturating_add(1);
            }
            return Err(self.reject(handle, EngineError::NotReady));
        }
        match handle.submit(BrowserCommand::Input(pending.event.clone())) {
            Ok(_) => {
                self.admitted(&pending.event, pending.accepted_click);
                self.retry_accepted = self.retry_accepted.saturating_add(1);
                if pending.overflow {
                    // The original command was admitted; its rejected successors
                    // cannot be replayed. Order a reset after that command.
                    self.abandon(handle, InputResetReason::Rejected);
                    if self.last_error.is_none() {
                        self.last_error = Some(EngineError::Backpressure);
                    }
                    Ok(InputAdmission::AdmittedAndReset)
                } else {
                    Ok(InputAdmission::Admitted(pending.mirror))
                }
            }
            Err(error) => {
                if pending.accepted_click {
                    self.rejected_clicks = self.rejected_clicks.saturating_add(1);
                }
                Err(self.reject(handle, error))
            }
        }
    }
}

fn file_drag_input(kind: &InputKind) -> bool {
    matches!(
        kind,
        InputKind::FileDragEnter { .. }
            | InputKind::FileDragOver { .. }
            | InputKind::FileDragLeave
            | InputKind::FileDrop { .. }
    )
}

/// High-frequency pointer motion (excluding pointer leave) and wheel events can
/// drop an individual backpressured sample without invalidating a held drag or
/// latching the discrete retry slot.
pub fn is_transient_pointer_input(kind: &InputKind) -> bool {
    matches!(
        kind,
        InputKind::MouseMove { leaving: false, .. } | InputKind::MouseWheel { .. }
    )
}

fn button_index(button: rust_cef_core::input::MouseButton) -> usize {
    match button {
        rust_cef_core::input::MouseButton::Left => 0,
        rust_cef_core::input::MouseButton::Middle => 1,
        rust_cef_core::input::MouseButton::Right => 2,
        rust_cef_core::input::MouseButton::Back => 3,
        rust_cef_core::input::MouseButton::Forward => 4,
    }
}

type Rect = (f32, f32, f32, f32);
#[derive(Clone, Copy, Debug)]
struct Start {
    window: u64,
    bounds: Rect,
    modifiers: MouseModifiers,
    count: NonZeroU8,
}
#[derive(Default)]
pub struct ClickGesture {
    start: Option<Start>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickError {
    InvalidGeometry,
    InvalidCount,
    WrongWindow { expected: u64, actual: u64 },
}
impl std::fmt::Display for ClickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGeometry => f.write_str("click has invalid content geometry"),
            Self::InvalidCount => f.write_str("click has a zero platform count"),
            Self::WrongWindow { expected, actual } => {
                write!(f, "click belongs to window {expected}, not {actual}")
            }
        }
    }
}
fn valid(bounds: Rect, position: (f32, f32)) -> bool {
    let (x, y, w, h) = bounds;
    [x, y, w, h, x + w, y + h, position.0, position.1]
        .into_iter()
        .all(f32::is_finite)
        && w > 0.
        && h > 0.
        && w < i32::MAX as f32
        && h < i32::MAX as f32
}
fn inside(bounds: Rect, position: (f32, f32)) -> bool {
    let (x, y, w, h) = bounds;
    position.0 >= x && position.0 < x + w && position.1 >= y && position.1 < y + h
}
impl ClickGesture {
    pub fn pending(&self) -> bool {
        self.start.is_some()
    }
    pub fn cancel(&mut self) {
        self.start = None;
    }
    pub fn begin(
        &mut self,
        window: u64,
        bounds: Rect,
        position: (f32, f32),
        modifiers: MouseModifiers,
        count: u8,
    ) -> Result<(), ClickError> {
        self.cancel();
        if !valid(bounds, position) {
            return Err(ClickError::InvalidGeometry);
        }
        let count = NonZeroU8::new(count).ok_or(ClickError::InvalidCount)?;
        if inside(bounds, position) {
            self.start = Some(Start {
                window,
                bounds,
                modifiers,
                count,
            });
        }
        Ok(())
    }
    pub fn finish(
        &mut self,
        window: u64,
        bounds: Rect,
        position: (f32, f32),
        modifiers: MouseModifiers,
    ) -> Result<Option<MouseClick>, ClickError> {
        let Some(start) = self.start.take() else {
            return Ok(None);
        };
        if window != start.window {
            return Err(ClickError::WrongWindow {
                expected: start.window,
                actual: window,
            });
        }
        if !valid(bounds, position) {
            return Err(ClickError::InvalidGeometry);
        }
        // This convenience API does not reproduce changing modifiers or a relayout
        // during a held gesture. Cancel rather than emit a semantically wrong click.
        if start.bounds != bounds || start.modifiers != modifiers || !inside(bounds, position) {
            return Ok(None);
        }
        let (x, y) = keymap::to_view_point(position, (bounds.0, bounds.1), (bounds.2, bounds.3))
            .ok_or(ClickError::InvalidGeometry)?;
        Ok(Some(MouseClick {
            x,
            y,
            modifiers,
            click_count: start.count,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_observations_saturate_without_storing_input_text() {
        let mut lane = InputLane::default();
        lane.callbacks.key_down_entered = u64::MAX;
        lane.callbacks.focused_handler_paint_attempts = u64::MAX;
        lane.callbacks.replace_text_rejected = u64::MAX;
        lane.note_callback_entered(InputCallback::KeyDown);
        lane.note_callback_entered(InputCallback::ReplaceText);
        lane.note_focused_handler_paint_attempt();
        lane.note_callback_rejected(
            InputCallback::ReplaceText,
            InputCallbackGate::TextNotAccepted,
        );
        lane.callbacks.replace_and_mark_text_rejected = u64::MAX;
        lane.note_callback_entered(InputCallback::ReplaceAndMarkText);
        assert_eq!(lane.callbacks.replace_and_mark_text_entered, 1);
        lane.note_callback_rejected(
            InputCallback::ReplaceAndMarkText,
            InputCallbackGate::CleanupUnavailable,
        );
        assert_eq!(lane.callbacks.replace_and_mark_text_rejected, u64::MAX);
        lane.note_callback_rejected(
            InputCallback::ReplaceText,
            InputCallbackGate::TextNotAccepted,
        );
        assert_eq!(lane.callbacks.key_down_entered, u64::MAX);
        assert_eq!(lane.callbacks.replace_text_entered, 1);
        assert_eq!(lane.callbacks.focused_handler_paint_attempts, u64::MAX);
        assert_eq!(lane.callbacks.replace_text_rejected, u64::MAX);
        assert_eq!(
            lane.callbacks.last_rejection,
            Some((
                InputCallback::ReplaceText,
                InputCallbackGate::TextNotAccepted
            ))
        );
    }

    #[test]
    fn complete_click_translates_window_dip_once_and_cannot_commit_twice() {
        let mut gesture = ClickGesture::default();
        let m = MouseModifiers {
            control: true,
            ..MouseModifiers::default()
        };
        gesture
            .begin(7, (200., 48., 100., 100.), (265., 88.), m, 2)
            .unwrap();
        assert!(gesture.pending());
        let click = gesture
            .finish(7, (200., 48., 100., 100.), (265., 89.), m)
            .unwrap()
            .unwrap();
        assert_eq!((click.x, click.y, click.click_count.get()), (65, 41, 2));
        assert_eq!(click.modifiers, m);
        assert!(!gesture.pending());
        assert_eq!(
            gesture.finish(7, (200., 48., 100., 100.), (265., 89.), m),
            Ok(None)
        );
    }
    #[test]
    fn outside_relayout_and_modifier_change_cancel_without_native_press() {
        for (bounds, position, mods) in [
            (
                (0., 48., 100., 100.),
                (101., 60.),
                MouseModifiers::default(),
            ),
            ((1., 48., 100., 100.), (20., 60.), MouseModifiers::default()),
            (
                (0., 48., 100., 100.),
                (20., 60.),
                MouseModifiers {
                    alt: true,
                    ..MouseModifiers::default()
                },
            ),
        ] {
            let mut gesture = ClickGesture::default();
            gesture
                .begin(
                    3,
                    (0., 48., 100., 100.),
                    (20., 60.),
                    MouseModifiers::default(),
                    1,
                )
                .unwrap();
            assert_eq!(gesture.finish(3, bounds, position, mods), Ok(None));
            assert!(!gesture.pending());
        }
        let mut gesture = ClickGesture::default();
        let bounds = (0., 48., 100., 100.);
        let mods = MouseModifiers::default();
        gesture.begin(3, bounds, (20., 60.), mods, 1).unwrap();
        assert_eq!(gesture.finish(3, bounds, (101., 60.), mods), Ok(None));
        gesture.begin(3, bounds, (101., 60.), mods, 1).unwrap();
        assert_eq!(gesture.finish(3, bounds, (20., 60.), mods), Ok(None));
        // The window activation observer uses this same cancellation path:
        // returning to the original window cannot revive its earlier down.
        gesture.begin(3, bounds, (20., 60.), mods, 1).unwrap();
        gesture.cancel();
        assert_eq!(gesture.finish(3, bounds, (20., 60.), mods), Ok(None));
    }
    #[test]
    fn foreign_window_and_invalid_geometry_fail_closed_and_clear_candidate() {
        let mut gesture = ClickGesture::default();
        let m = MouseModifiers::default();
        gesture
            .begin(3, (0., 48., 100., 100.), (20., 60.), m, 1)
            .unwrap();
        assert_eq!(
            gesture.finish(4, (0., 48., 100., 100.), (20., 60.), m),
            Err(ClickError::WrongWindow {
                expected: 3,
                actual: 4
            })
        );
        assert!(!gesture.pending());
        assert_eq!(
            gesture.begin(3, (0., 48., 100., 100.), (20., 60.), m, 0),
            Err(ClickError::InvalidCount)
        );
        for bad in [f32::NAN, f32::INFINITY, 0., -1.] {
            assert_eq!(
                gesture.begin(3, (0., 48., bad, 100.), (20., 60.), m, 1),
                Err(ClickError::InvalidGeometry)
            );
            assert!(!gesture.pending());
        }
    }

    use rust_cef_core::{
        contracts::{BrowserId, BrowserSnapshot, RuntimeGeneration},
        ports::{BrowserControl, CloseMode, CloseTicket, CommandId},
    };
    use std::sync::{Arc, Mutex};

    struct FakeControl(Mutex<FakeState>);
    struct FakeState {
        epoch: u64,
        failures: Vec<EngineError>,
        commands: Vec<BrowserCommand>,
        cleanup_pending: bool,
    }
    impl FakeControl {
        fn new(failures: Vec<EngineError>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(FakeState {
                epoch: 1,
                failures,
                commands: Vec::new(),
                cleanup_pending: false,
            })))
        }
    }
    fn browser_id() -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), 1)
    }
    impl BrowserControl for FakeControl {
        fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError> {
            let mut state = self.0.lock().unwrap();
            if matches!(&command, BrowserCommand::Input(_)) && !state.failures.is_empty() {
                return Err(state.failures.remove(0));
            }
            if matches!(&command, BrowserCommand::ResetInput(_)) {
                state.epoch += 1;
            }
            state.commands.push(command);
            Ok(CommandId {
                browser: browser_id(),
                sequence: state.commands.len() as u64,
            })
        }
        fn snapshot(&self) -> BrowserSnapshot {
            panic!("not used by input lane")
        }
        fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
            let state = self.0.lock().unwrap();
            Ok(InputSnapshot {
                epoch: state.epoch,
                admitted: 0,
                executed: 0,
                rejected: 0,
                inflight: 0,
                pressed_buttons: 0,
                pressed_keys: 0,
                composition_active: false,
                file_drag_active: false,
                file_drag_phase: rust_cef_core::file_drop::FileDragPhase::Idle,
                cleanup_pending: state.cleanup_pending,
                cleanup_failed: false,
                native_execution_unconfirmed: false,
                last_native_error: None,
                accepts_text: true,
            })
        }
        fn request_close(&self, _mode: CloseMode) -> CloseTicket {
            CloseTicket::pending(browser_id(), 1).0
        }
    }

    fn button(up: bool) -> InputKind {
        InputKind::MouseButton {
            x: 5,
            y: 9,
            button: rust_cef_core::input::MouseButton::Left,
            up,
            click_count: 1,
            modifiers: MouseModifiers::default(),
        }
    }

    #[test]
    fn platform_file_inputs_never_defer_after_backpressure() {
        let paths = rust_cef_core::file_drop::FileDropPaths::new(vec![std::path::PathBuf::from(
            "C:/owned/a.txt",
        )])
        .unwrap();
        for kind in [
            InputKind::FileDragEnter {
                paths,
                x: 1,
                y: 2,
                modifiers: MouseModifiers::default(),
            },
            InputKind::FileDragOver {
                x: 1,
                y: 2,
                modifiers: MouseModifiers::default(),
            },
            InputKind::FileDragLeave,
            InputKind::FileDrop {
                x: 1,
                y: 2,
                modifiers: MouseModifiers::default(),
            },
        ] {
            let control = FakeControl::new(vec![EngineError::Backpressure]);
            let handle = BrowserHandle::new(control.clone());
            let mut lane = InputLane::default();
            assert_eq!(
                lane.submit(&handle, 7, kind, false, None),
                Err(EngineError::Backpressure)
            );
            assert!(!lane.pending());
            assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::Idle));
            let state = control.0.lock().unwrap();
            assert_eq!(state.commands.len(), 1);
            assert!(matches!(
                state.commands[0],
                BrowserCommand::ResetInput(InputResetReason::Rejected)
            ));
        }
    }
    #[test]
    fn platform_file_cannot_overtake_or_replace_original_keyboard_pending() {
        let control = FakeControl::new(vec![EngineError::Backpressure]);
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        assert_eq!(
            lane.submit(&handle, 7, InputKind::Text("a".into()), false, None),
            Ok(InputAdmission::Deferred)
        );
        assert_eq!(
            lane.submit(&handle, 7, InputKind::FileDragLeave, false, None),
            Err(EngineError::Backpressure)
        );
        assert!(lane.pending());
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::AdmittedAndReset));
        let state = control.0.lock().unwrap();
        assert_eq!(state.commands.len(), 2);
        assert!(
            matches!(&state.commands[0], BrowserCommand::Input(InputEvent { kind: InputKind::Text(t), .. }) if t == "a")
        );
        assert!(matches!(
            state.commands[1],
            BrowserCommand::ResetInput(InputResetReason::Rejected)
        ));
    }
    #[test]
    fn stale_native_release_preserves_a_new_epoch_pending_input() {
        let control = FakeControl::new(vec![EngineError::Backpressure]);
        control.0.lock().unwrap().epoch = 9;
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        assert_eq!(
            lane.submit_with_epoch(&handle, 7, button(false), false, None, None),
            Ok((InputAdmission::Deferred, 9))
        );
        assert_eq!(
            lane.submit_with_epoch(&handle, 7, button(true), false, None, Some(8)),
            Err(EngineError::NotReady)
        );
        assert!(lane.pending());
        assert!(!lane.pending.as_ref().unwrap().overflow);
        assert!(control.0.lock().unwrap().commands.is_empty());
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::Admitted(None)));
        let state = control.0.lock().unwrap();
        assert_eq!(state.epoch, 9);
        assert!(matches!(
            &state.commands[..],
            [BrowserCommand::Input(InputEvent { epoch: 9, .. })]
        ));
    }
    #[test]
    fn explicit_retry_accepts_the_original_epoch_once_and_counts_up_only_then() {
        let control = FakeControl::new(vec![EngineError::Backpressure]);
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        assert_eq!(
            lane.submit(&handle, 7, button(true), true, None),
            Ok(InputAdmission::Deferred)
        );
        assert!(lane.pending());
        assert_eq!(lane.accepted_clicks(), 0);
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::Admitted(None)));
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::Idle));
        assert_eq!(lane.accepted_clicks(), 1);
        assert_eq!(lane.diagnostics(&handle).retry_attempts, 1);
        let state = control.0.lock().unwrap();
        assert!(matches!(
            &state.commands[..],
            [BrowserCommand::Input(InputEvent { epoch: 1, .. })]
        ));
    }

    #[test]
    fn pending_down_refuses_later_up_without_overtaking_and_requests_reset() {
        let control = FakeControl::new(vec![EngineError::Backpressure]);
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        assert_eq!(
            lane.submit(&handle, 7, button(false), false, None),
            Ok(InputAdmission::Deferred)
        );
        assert_eq!(
            lane.submit(&handle, 7, button(true), true, None),
            Err(EngineError::Backpressure)
        );
        assert!(lane.pending());
        assert_eq!(lane.accepted_clicks(), 0);
        assert!(control.0.lock().unwrap().commands.is_empty());
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::AdmittedAndReset));
        assert!(!lane.pending());
        assert!(matches!(
            &control.0.lock().unwrap().commands[..],
            [
                BrowserCommand::Input(InputEvent { epoch: 1, .. }),
                BrowserCommand::ResetInput(InputResetReason::Rejected)
            ]
        ));
    }

    #[test]
    fn one_utf16_text_commit_and_one_bounded_ime_update_use_owned_commands() {
        let control = FakeControl::new(Vec::new());
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        lane.submit(&handle, 7, InputKind::Text("中😀".to_owned()), false, None)
            .unwrap();
        lane.submit(
            &handle,
            7,
            InputKind::ImeSet {
                text: "中😀".to_owned(),
                selection: rust_cef_core::input::Utf16Range { start: 1, end: 3 },
                replacement: None,
            },
            false,
            None,
        )
        .unwrap();
        let state = control.0.lock().unwrap();
        assert_eq!(state.commands.len(), 2);
        assert!(
            matches!(&state.commands[0], BrowserCommand::Input(InputEvent {
            epoch: 1, kind: InputKind::Text(text) }) if text == "中😀")
        );
        assert!(
            matches!(&state.commands[1], BrowserCommand::Input(InputEvent {
            epoch: 1, kind: InputKind::ImeSet { selection, .. } }) if *selection == (rust_cef_core::input::Utf16Range { start: 1, end: 3 }))
        );
    }

    #[test]
    fn deferred_ime_mirror_is_delivered_only_on_admission_and_overflow_resets() {
        let mark = MirrorEffect::Mark {
            text: "😀".to_owned(),
            selection: 0..2,
        };
        let ime = InputKind::ImeSet {
            text: "😀".to_owned(),
            selection: rust_cef_core::input::Utf16Range { start: 0, end: 2 },
            replacement: Some(rust_cef_core::input::Utf16Range {
                start: 50_000,
                end: 50_001,
            }),
        };
        for overflow in [false, true] {
            let control = FakeControl::new(vec![EngineError::Backpressure]);
            let handle = BrowserHandle::new(control.clone());
            let mut lane = InputLane::default();
            assert_eq!(
                lane.submit(&handle, 7, ime.clone(), false, Some(mark.clone())),
                Ok(InputAdmission::Deferred)
            );
            assert!(control.0.lock().unwrap().commands.is_empty());
            if overflow {
                assert_eq!(
                    lane.submit(&handle, 7, InputKind::Text("x".into()), false, None),
                    Err(EngineError::Backpressure)
                );
                assert!(lane.pending());
                assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::AdmittedAndReset));
                assert!(matches!(
                    &control.0.lock().unwrap().commands[..],
                    [
                        BrowserCommand::Input(_),
                        BrowserCommand::ResetInput(InputResetReason::Rejected)
                    ]
                ));
            } else {
                assert_eq!(
                    lane.retry(&handle, 7),
                    Ok(InputAdmission::Admitted(Some(mark.clone())))
                );
                assert!(matches!(
                    &control.0.lock().unwrap().commands[..],
                    [BrowserCommand::Input(_)]
                ));
            }
        }
    }

    #[test]
    fn focused_input_snapshot_failure_is_counted_and_reset_only_once() {
        let control = FakeControl::new(Vec::new());
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        lane.reject_focus_unavailable(&handle, EngineError::NotReady, false);
        lane.reject_focus_unavailable(&handle, EngineError::NotReady, false);
        assert_eq!(lane.diagnostics(&handle).rejected, 2);
        assert_eq!(
            lane.diagnostics(&handle).last_error,
            Some(EngineError::NotReady)
        );
        assert!(matches!(
            &control.0.lock().unwrap().commands[..],
            [BrowserCommand::ResetInput(InputResetReason::Rejected)]
        ));
        control.0.lock().unwrap().cleanup_pending = true;
        lane.reject_focus_unavailable(&handle, EngineError::NotReady, true);
        assert_eq!(lane.diagnostics(&handle).rejected, 3);
        assert_eq!(control.0.lock().unwrap().commands.len(), 1);
    }

    #[test]
    fn unsupported_windows_replacement_preserves_typed_rejection_and_clears_slot() {
        let replacement = rust_cef_core::input::Utf16Range {
            start: 50_000,
            end: 50_001,
        };
        for kind in [
            InputKind::ReplaceText {
                text: "x".to_owned(),
                replacement,
            },
            InputKind::ImeSet {
                text: "x".to_owned(),
                selection: rust_cef_core::input::Utf16Range { start: 0, end: 1 },
                replacement: Some(replacement),
            },
        ] {
            let control = FakeControl::new(vec![EngineError::UnsupportedInputReplacement]);
            let handle = BrowserHandle::new(control.clone());
            let mut lane = InputLane::default();
            assert_eq!(
                lane.submit(&handle, 7, kind, false, Some(MirrorEffect::Clear)),
                Err(EngineError::UnsupportedInputReplacement)
            );
            assert_eq!(
                lane.diagnostics(&handle).last_error,
                Some(EngineError::UnsupportedInputReplacement)
            );
            assert_eq!(lane.diagnostics(&handle).rejected, 1);
            assert!(!lane.pending());
            assert!(matches!(
                &control.0.lock().unwrap().commands[..],
                [BrowserCommand::ResetInput(InputResetReason::Rejected)]
            ));
        }
    }

    #[test]
    fn replace_text_uses_document_range_independently_of_new_text() {
        let control = FakeControl::new(Vec::new());
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        assert_eq!(
            lane.submit(
                &handle,
                7,
                InputKind::ReplaceText {
                    text: String::new(),
                    replacement: rust_cef_core::input::Utf16Range {
                        start: 30_000,
                        end: 30_002
                    },
                },
                false,
                None
            ),
            Ok(InputAdmission::Admitted(None))
        );
        assert!(matches!(&control.0.lock().unwrap().commands[..],
            [BrowserCommand::Input(InputEvent { kind: InputKind::ReplaceText { text, replacement }, .. })]
            if text.is_empty() && replacement.start == 30_000 && replacement.end == 30_002));
    }

    #[test]
    fn rejected_retry_and_stale_epoch_never_rebase_the_owned_event() {
        for stale in [false, true] {
            let control = FakeControl::new(if stale {
                vec![EngineError::Backpressure]
            } else {
                vec![EngineError::Backpressure, EngineError::PostRejected]
            });
            let handle = BrowserHandle::new(control.clone());
            let mut lane = InputLane::default();
            lane.submit(&handle, 7, InputKind::Text("a".into()), false, None)
                .unwrap();
            if stale {
                control.0.lock().unwrap().epoch += 1;
            }
            assert_eq!(
                lane.retry(&handle, 7),
                Err(if stale {
                    EngineError::NotReady
                } else {
                    EngineError::PostRejected
                })
            );
            assert!(!lane.pending());
            assert_eq!(lane.diagnostics(&handle).retry_accepted, 0);
            assert!(matches!(
                &control.0.lock().unwrap().commands[..],
                [BrowserCommand::ResetInput(InputResetReason::Rejected)]
            ));
        }
    }

    #[test]
    fn transient_pointer_backpressure_drops_sample_without_poisoning_drag_or_pending() {
        let control = FakeControl::new(Vec::new());
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();

        // 1. Start a left drag (`MouseDown`).
        assert_eq!(
            lane.submit(&handle, 7, button(false), false, None),
            Ok(InputAdmission::Admitted(None))
        );
        assert!(lane.held_button());

        // 2. Next `MouseMove` hits queue backpressure: drop the redundant sample,
        //    keep `held_button()` intact, do NOT latch `pending`, do NOT emit ResetInput.
        control
            .0
            .lock()
            .unwrap()
            .failures
            .push(EngineError::Backpressure);
        let move_event = InputKind::MouseMove {
            x: 42,
            y: 42,
            leaving: false,
            modifiers: MouseModifiers::default(),
        };
        assert_eq!(
            lane.submit(&handle, 7, move_event, false, None),
            Err(EngineError::Backpressure)
        );
        assert!(!lane.pending());
        assert!(lane.held_button());

        // 3. Subsequent move + release succeed without an intervening ResetInput.
        assert_eq!(
            lane.submit(
                &handle,
                7,
                InputKind::MouseMove {
                    x: 120,
                    y: 42,
                    leaving: false,
                    modifiers: MouseModifiers::default(),
                },
                false,
                None
            ),
            Ok(InputAdmission::Admitted(None))
        );
        assert_eq!(
            lane.submit(&handle, 7, button(true), true, None),
            Ok(InputAdmission::Admitted(None))
        );
        assert!(!lane.held_button());
        assert!(
            !control
                .0
                .lock()
                .unwrap()
                .commands
                .iter()
                .any(|c| matches!(c, BrowserCommand::ResetInput(_)))
        );

        // 4. While a discrete event is pending retry, transient pointer/wheel samples
        //    must not mark `overflow = true`.
        control
            .0
            .lock()
            .unwrap()
            .failures
            .push(EngineError::Backpressure);
        assert_eq!(
            lane.submit(&handle, 7, InputKind::Text("a".into()), false, None),
            Ok(InputAdmission::Deferred)
        );
        assert_eq!(
            lane.submit(
                &handle,
                7,
                InputKind::MouseWheel {
                    x: 10,
                    y: 10,
                    delta_x: 0,
                    delta_y: -120,
                    modifiers: MouseModifiers::default(),
                },
                false,
                None,
            ),
            Err(EngineError::Backpressure)
        );
        assert_eq!(lane.retry(&handle, 7), Ok(InputAdmission::Admitted(None)));
    }

    #[test]
    fn transient_pointer_does_not_arm_reset_and_drops_cleanly_while_cleanup_pending() {
        let control = FakeControl::new(Vec::new());
        let handle = BrowserHandle::new(control.clone());
        let mut lane = InputLane::default();
        // Consume the initial construction arming, then verify passive hover
        // does not re-arm ResetInput on later FocusLost.
        lane.abandon(&handle, InputResetReason::FocusLost);
        control.0.lock().unwrap().commands.clear();

        // Hover/wheel does not arm ResetInput.
        assert_eq!(
            lane.submit(
                &handle,
                7,
                InputKind::MouseMove {
                    x: 10,
                    y: 20,
                    leaving: false,
                    modifiers: MouseModifiers::default(),
                },
                false,
                None,
            ),
            Ok(InputAdmission::Admitted(None))
        );
        lane.abandon(&handle, InputResetReason::FocusLost);
        assert!(
            !control
                .0
                .lock()
                .unwrap()
                .commands
                .iter()
                .any(|c| matches!(c, BrowserCommand::ResetInput(_)))
        );

        // During cleanup_pending, transient pointer drops cleanly without incrementing rejected.
        {
            let mut inner = control.0.lock().unwrap();
            inner.cleanup_pending = true;
            inner.commands.clear();
        }
        assert_eq!(
            lane.submit(
                &handle,
                7,
                InputKind::MouseMove {
                    x: 15,
                    y: 25,
                    leaving: false,
                    modifiers: MouseModifiers::default(),
                },
                false,
                None,
            ),
            Err(EngineError::Backpressure)
        );
        assert_eq!(lane.diagnostics(&handle).rejected, 0);
        assert!(control.0.lock().unwrap().commands.is_empty());
    }
}
