//! CEF UI-only native input dispatch; the ledger contains no text or CEF reference.
use crate::executor::NativeAudit;
use cef::*;
use rust_cef_core::{
    file_drop::{FileDragPhase, FileDragStage},
    input::{
        ActiveTouchTracker, BrowserTouchEvent, InputEvent, InputKind, InputSnapshot,
        MAX_PRESSED_KEYS, MouseButton, TouchEventType, TouchPointerType, Utf16Range,
    },
    ports::{EngineError, MouseModifiers},
};
use std::collections::{BTreeMap, BTreeSet};

/// The fixed CEF 146 Windows adapter ignores document replacement ranges.
/// Refuse them before a native call rather than report a false replacement.
pub(crate) fn validate_native_input(input: &InputEvent) -> Result<(), EngineError> {
    input.validate()?;
    if matches!(
        input.kind,
        InputKind::ReplaceText { .. }
            | InputKind::ImeSet {
                replacement: Some(_),
                ..
            }
    ) {
        return Err(EngineError::UnsupportedInputReplacement);
    }
    Ok(())
}

/// Guard file target coordinates against the current serialized DIP viewport.
/// This is not an atomic renderer/widget-liveness or dragover-acceptance ACK.
pub(crate) fn validate_file_drag_viewport(
    input: &InputEvent,
    viewport: rust_cef_core::contracts::Viewport,
) -> Result<(), EngineError> {
    let point = match &input.kind {
        InputKind::FileDragEnter { x, y, .. }
        | InputKind::FileDragOver { x, y, .. }
        | InputKind::FileDrop { x, y, .. } => Some((*x, *y)),
        _ => None,
    };
    if point.is_some_and(|(x, y)| x < 0 || y < 0 || x >= viewport.width || y >= viewport.height) {
        return Err(EngineError::InvalidRequest);
    }
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct NativeInput {
    buttons: BTreeSet<u8>,
    keys: BTreeMap<i32, (i32, bool, MouseModifiers)>,
    composition: bool,
    file_drag: FileDragPhase,
    touches: ActiveTouchTracker,
    last_pointer: (i32, i32),
    pub(crate) admitted: u64,
    pub(crate) executed: u64,
    pub(crate) rejected: u64,
    pub(crate) cleanup_failed: bool,
    pub(crate) last_native_error: Option<EngineError>,
}

impl NativeInput {
    pub(crate) fn snapshot(&self, epoch: u64, pending: bool) -> InputSnapshot {
        InputSnapshot {
            epoch,
            admitted: self.admitted,
            executed: self.executed,
            rejected: self.rejected,
            inflight: 0,
            native_execution_unconfirmed: false,
            pressed_buttons: self.buttons.len(),
            pressed_keys: self.keys.len(),
            composition_active: self.composition,
            file_drag_active: self.file_drag.is_active(),
            file_drag_phase: self.file_drag,
            cleanup_pending: pending,
            cleanup_failed: self.cleanup_failed,
            last_native_error: self.last_native_error.clone(),
            accepts_text: false,
        }
    }

    pub(crate) fn dispatch(
        &mut self,
        input: &InputEvent,
        host: &BrowserHost,
        audit: &NativeAudit,
    ) -> Result<(), EngineError> {
        validate_native_input(input)?;
        let event = |x, y, modifiers| MouseEvent { x, y, modifiers };
        match &input.kind {
            InputKind::FileDragEnter {
                paths,
                x,
                y,
                modifiers,
            } => {
                let next = self.file_drag.transition(FileDragStage::Enter)?;
                audit.record();
                let mut data =
                    cef::drag_data_create().ok_or(EngineError::Backend { code: None })?;
                audit.record();
                data.reset_file_contents();
                for path in paths.paths() {
                    let text = path.to_str().ok_or(EngineError::InvalidRequest)?;
                    audit.record();
                    data.add_file(Some(&CefString::from(text)), None);
                }
                audit.record();
                if data.is_file() == 0 {
                    return Err(EngineError::Backend { code: None });
                }
                self.file_drag = next;
                self.last_pointer = (*x, *y);
                audit.record();
                // Fixed CEF retains a CefRefPtr in its OSR delegate. Our local
                // wrapper never enters the shared/cloned NativeInput ledger.
                host.drag_target_drag_enter(
                    Some(&mut data),
                    Some(&event(*x, *y, flags(*modifiers))),
                    file_copy_mask(),
                );
            }
            InputKind::FileDragOver { x, y, modifiers } => {
                self.file_drag = self.file_drag.transition(FileDragStage::Over)?;
                self.last_pointer = (*x, *y);
                audit.record();
                host.drag_target_drag_over(
                    Some(&event(*x, *y, flags(*modifiers))),
                    file_copy_mask(),
                );
            }
            InputKind::FileDragLeave => self.cancel_file_drag(host, audit),
            InputKind::FileDrop { x, y, modifiers } => {
                self.file_drag = self.file_drag.transition(FileDragStage::Drop)?;
                self.last_pointer = (*x, *y);
                audit.record();
                host.drag_target_drop(Some(&event(*x, *y, flags(*modifiers))));
            }
            InputKind::MouseMove {
                x,
                y,
                leaving,
                modifiers,
            } => {
                self.last_pointer = (*x, *y);
                audit.record();
                host.send_mouse_move_event(
                    Some(&event(*x, *y, self.mouse_flags(*modifiers))),
                    i32::from(*leaving),
                );
            }
            InputKind::MouseButton {
                x,
                y,
                button,
                up,
                click_count,
                modifiers,
            } => {
                self.last_pointer = (*x, *y);
                let (id, native) = button_type(*button);
                if !*up {
                    self.buttons.insert(id);
                }
                let flags =
                    self.mouse_flags(*modifiers) & if *up { !button_flag(id) } else { u32::MAX };
                audit.record();
                host.send_mouse_click_event(
                    Some(&event(*x, *y, flags)),
                    native,
                    i32::from(*up),
                    i32::from(*click_count),
                );
                if *up {
                    self.buttons.remove(&id);
                }
            }
            InputKind::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                self.last_pointer = (*x, *y);
                audit.record();
                host.send_mouse_wheel_event(
                    Some(&event(*x, *y, self.mouse_flags(*modifiers))),
                    *delta_x,
                    *delta_y,
                );
            }
            InputKind::Key {
                windows_key_code,
                native_key_code,
                up,
                repeat,
                system,
                modifiers,
            } => {
                if !*up {
                    self.record_key(
                        *windows_key_code,
                        *native_key_code,
                        false,
                        *system,
                        *modifiers,
                    )?;
                }
                let key = KeyEvent {
                    size: KeyEvent::default().size,
                    type_: if *up {
                        KeyEventType::KEYUP
                    } else {
                        KeyEventType::RAWKEYDOWN
                    },
                    modifiers: flags(*modifiers)
                        | if *repeat {
                            rust_cef_core::keymap::EVENTFLAG_IS_REPEAT
                        } else {
                            0
                        },
                    windows_key_code: *windows_key_code,
                    native_key_code: *native_key_code,
                    is_system_key: i32::from(*system),
                    character: 0,
                    unmodified_character: 0,
                    focus_on_editable_field: 0,
                };
                audit.record();
                host.send_key_event(Some(&key));
                if *up {
                    self.record_key(
                        *windows_key_code,
                        *native_key_code,
                        true,
                        *system,
                        *modifiers,
                    )?;
                }
            }
            InputKind::Text(text) => {
                if !text.is_empty() {
                    audit.record();
                    // C++ takes a required CefRange reference. "Unspecified"
                    // means InvalidRange(), not a null C API pointer; retain
                    // the working old GPUI handler's (-1,-1) bit pattern.
                    host.ime_commit_text(
                        Some(&CefString::from(text.as_str())),
                        Some(&unspecified_ime_replacement_range()),
                        0,
                    );
                    self.composition = false;
                }
            }
            InputKind::ReplaceText { .. } => {
                // Also defend this branch if the validation above is refactored.
                return Err(EngineError::UnsupportedInputReplacement);
            }
            InputKind::ImeSet {
                text,
                selection,
                replacement,
            } => {
                let replacement = replacement
                    .map(range)
                    .unwrap_or_else(unspecified_ime_replacement_range);
                let selection = range(*selection);
                audit.record();
                host.ime_set_composition(
                    Some(&CefString::from(text.as_str())),
                    None,
                    Some(&replacement),
                    Some(&selection),
                );
                self.composition = true;
            }
            InputKind::ImeFinish { keep_selection } => {
                if self.composition {
                    audit.record();
                    host.ime_finish_composing_text(i32::from(*keep_selection));
                    self.composition = false;
                }
            }
            InputKind::ImeCancel => {
                if self.composition {
                    audit.record();
                    host.ime_cancel_composition();
                    self.composition = false;
                }
            }
        }
        self.executed = self.executed.saturating_add(1);
        Ok(())
    }

    fn record_key(
        &mut self,
        code: i32,
        native: i32,
        up: bool,
        system: bool,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        if up {
            self.keys.remove(&code);
        } else if !self.keys.contains_key(&code) {
            if self.keys.len() >= MAX_PRESSED_KEYS {
                return Err(EngineError::Backpressure);
            }
            self.keys.insert(code, (native, system, modifiers));
        }
        Ok(())
    }

    fn mouse_flags(&self, modifiers: MouseModifiers) -> u32 {
        flags(modifiers)
            | self
                .buttons
                .iter()
                .copied()
                .fold(0, |bits, id| bits | button_flag(id))
    }

    pub(crate) fn has_native_state(&self) -> bool {
        !self.keys.is_empty()
            || !self.buttons.is_empty()
            || self.composition
            || self.file_drag.is_active()
            || !self.touches.is_empty()
    }

    pub(crate) fn dispatch_touch(
        &mut self,
        touch: &BrowserTouchEvent,
        host: &BrowserHost,
        audit: &NativeAudit,
    ) -> Result<(), EngineError> {
        let normalized = self.touches.observe(touch)?;
        let cef_touch = to_cef_touch_event(&normalized);
        audit.record();
        host.send_touch_event(Some(&cef_touch));
        self.executed = self.executed.saturating_add(1);
        Ok(())
    }

    pub(crate) fn cancel_touches(&mut self, host: &BrowserHost, audit: &NativeAudit) {
        let cancelled = self.touches.drain_cancellations();
        for touch in cancelled {
            let cef_touch = to_cef_touch_event(&touch);
            audit.record();
            host.send_touch_event(Some(&cef_touch));
        }
    }

    fn cancel_file_drag(&mut self, host: &BrowserHost, audit: &NativeAudit) {
        if self.file_drag.is_active() {
            // Clear before a native call which can synchronously reenter.
            self.file_drag = FileDragPhase::Idle;
            audit.record();
            host.drag_target_drag_leave();
        }
    }

    /// Release the actual UI-executed pressed state, not merely submitted events.
    pub(crate) fn cleanup(&mut self, host: &BrowserHost, audit: &NativeAudit) {
        self.cancel_file_drag(host, audit);
        self.cancel_touches(host, audit);
        if self.composition {
            audit.record();
            host.ime_cancel_composition();
            self.composition = false;
        }
        for (&key_code, &(native_code, system, modifiers)) in &self.keys {
            let key = KeyEvent {
                size: KeyEvent::default().size,
                type_: KeyEventType::KEYUP,
                modifiers: flags(modifiers),
                windows_key_code: key_code,
                native_key_code: native_code,
                is_system_key: i32::from(system),
                character: 0,
                unmodified_character: 0,
                focus_on_editable_field: 0,
            };
            audit.record();
            host.send_key_event(Some(&key));
        }
        let (x, y) = self.last_pointer;
        self.keys.clear();
        for id in self.buttons.iter().copied().collect::<Vec<_>>() {
            let button = match id {
                0 => MouseButtonType::LEFT,
                1 => MouseButtonType::MIDDLE,
                _ => MouseButtonType::RIGHT,
            };
            audit.record();
            host.send_mouse_click_event(
                Some(&MouseEvent {
                    x,
                    y,
                    modifiers: self.mouse_flags(MouseModifiers::default()) & !button_flag(id),
                }),
                button,
                1,
                1,
            );
            self.buttons.remove(&id);
        }
        audit.record();
        host.send_capture_lost_event();
        audit.record();
        host.set_focus(0);
    }
}

fn file_copy_mask() -> DragOperationsMask {
    DragOperationsMask::from(cef::sys::cef_drag_operations_mask_t(1))
}
fn flags(m: MouseModifiers) -> u32 {
    rust_cef_core::keymap::modifier_flags(m.shift, m.control, m.alt, m.platform)
}
fn button_type(b: MouseButton) -> (u8, MouseButtonType) {
    match b {
        MouseButton::Left => (0, MouseButtonType::LEFT),
        MouseButton::Middle => (1, MouseButtonType::MIDDLE),
        MouseButton::Right => (2, MouseButtonType::RIGHT),
    }
}
fn button_flag(id: u8) -> u32 {
    match id {
        0 => rust_cef_core::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON,
        1 => rust_cef_core::keymap::EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        _ => rust_cef_core::keymap::EVENTFLAG_RIGHT_MOUSE_BUTTON,
    }
}
/// CEF's CefRange::InvalidRange(); None at our public input boundary must not
/// become a null pointer for the SDK's required const CefRange& argument.
fn unspecified_ime_replacement_range() -> Range {
    Range {
        from: u32::MAX,
        to: u32::MAX,
    }
}
fn range(r: Utf16Range) -> Range {
    Range {
        from: r.start,
        to: r.end,
    }
}

pub(crate) fn to_cef_touch_event(touch: &BrowserTouchEvent) -> cef::TouchEvent {
    let type_ = match touch.event_type {
        TouchEventType::Released => cef::TouchEventType::RELEASED,
        TouchEventType::Pressed => cef::TouchEventType::PRESSED,
        TouchEventType::Moved => cef::TouchEventType::MOVED,
        TouchEventType::Cancelled => cef::TouchEventType::CANCELLED,
    };
    let pointer_type = match touch.pointer_type {
        TouchPointerType::Touch => cef::PointerType::TOUCH,
        TouchPointerType::Mouse => cef::PointerType::MOUSE,
        TouchPointerType::Pen => cef::PointerType::PEN,
        TouchPointerType::Eraser => cef::PointerType::ERASER,
        TouchPointerType::Unknown => cef::PointerType::UNKNOWN,
    };
    cef::TouchEvent {
        id: touch.id,
        x: touch.x,
        y: touch.y,
        radius_x: touch.radius_x,
        radius_y: touch.radius_y,
        rotation_angle: touch.rotation_angle,
        pressure: touch.pressure,
        type_,
        modifiers: flags(touch.modifiers),
        pointer_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_target_coordinates_refuse_outside_or_empty_viewport_before_native_call() {
        let viewport = rust_cef_core::contracts::Viewport {
            width: 320,
            height: 240,
            scale: 1.5,
        };
        for (x, y, valid) in [
            (0, 0, true),
            (319, 239, true),
            (320, 20, false),
            (20, 240, false),
            (-1, 0, false),
            (i32::MAX, 0, false),
        ] {
            let event = InputEvent {
                epoch: 1,
                kind: InputKind::FileDragOver {
                    x,
                    y,
                    modifiers: MouseModifiers::default(),
                },
            };
            assert_eq!(validate_file_drag_viewport(&event, viewport).is_ok(), valid);
            assert_eq!(
                validate_file_drag_viewport(
                    &event,
                    rust_cef_core::contracts::Viewport {
                        width: 0,
                        ..viewport
                    }
                ),
                Err(EngineError::InvalidRequest)
            );
        }
        assert_eq!(
            validate_file_drag_viewport(
                &InputEvent {
                    epoch: 1,
                    kind: InputKind::FileDragLeave
                },
                viewport
            ),
            Ok(())
        );
    }

    #[test]
    fn file_drag_phase_remains_pure_and_participates_in_native_cleanup_gate() {
        let mut input = NativeInput::default();
        assert!(!input.has_native_state());
        assert!(!input.snapshot(7, false).file_drag_active);
        for phase in [FileDragPhase::Entered, FileDragPhase::Over] {
            input.file_drag = phase;
            assert!(input.has_native_state());
            assert!(input.snapshot(7, false).file_drag_active);
            let copy = input.clone();
            assert_eq!(copy.file_drag, phase);
        }
        input.file_drag = FileDragPhase::Idle;
        assert!(!input.has_native_state());
    }

    #[test]
    fn unspecified_native_ime_range_preserves_invalid_range_bit_pattern() {
        let replacement = unspecified_ime_replacement_range();
        assert_eq!(replacement.from, u32::MAX);
        assert_eq!(replacement.to, u32::MAX);
        assert_ne!(replacement.from, 0);
    }

    #[test]
    fn windows_refuses_osx_only_replacement_without_native_dispatch() {
        let replacement = Utf16Range {
            start: 5000,
            end: 5001,
        };
        for kind in [
            InputKind::ReplaceText {
                text: "x".into(),
                replacement,
            },
            InputKind::ImeSet {
                text: "x".into(),
                selection: Utf16Range { start: 0, end: 1 },
                replacement: Some(replacement),
            },
        ] {
            let event = InputEvent { epoch: 1, kind };
            assert_eq!(event.validate(), Ok(()));
            assert_eq!(
                validate_native_input(&event),
                Err(EngineError::UnsupportedInputReplacement)
            );
        }
        assert_eq!(
            validate_native_input(&InputEvent {
                epoch: 1,
                kind: InputKind::Text("x".into())
            }),
            Ok(())
        );
        assert_eq!(
            validate_native_input(&InputEvent {
                epoch: 1,
                kind: InputKind::ImeSet {
                    text: "x".into(),
                    selection: Utf16Range { start: 0, end: 1 },
                    replacement: None
                }
            }),
            Ok(())
        );
    }

    #[test]
    fn native_ledger_keeps_held_mouse_flags_and_repeated_key_at_capacity() {
        let mut input = NativeInput::default();
        input.buttons.insert(0);
        input.buttons.insert(2);
        assert_eq!(
            input.mouse_flags(MouseModifiers::default()),
            rust_cef_core::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON
                | rust_cef_core::keymap::EVENTFLAG_RIGHT_MOUSE_BUTTON
        );
        for code in 1..=MAX_PRESSED_KEYS as i32 {
            input
                .record_key(code, code, false, false, MouseModifiers::default())
                .unwrap();
        }
        input
            .record_key(1, 1, false, false, MouseModifiers::default())
            .unwrap();
        assert_eq!(
            input.record_key(100, 100, false, false, MouseModifiers::default()),
            Err(EngineError::Backpressure)
        );
        input
            .record_key(1, 1, true, false, MouseModifiers::default())
            .unwrap();
        input
            .record_key(100, 100, false, false, MouseModifiers::default())
            .unwrap();
        assert_eq!(input.snapshot(1, false).pressed_keys, MAX_PRESSED_KEYS);
    }

    #[test]
    fn touch_conversion_preserves_fields_and_tracks_native_state() {
        let touch = BrowserTouchEvent {
            id: 7,
            x: 120.5,
            y: 240.25,
            radius_x: 8.0,
            radius_y: 8.0,
            rotation_angle: 15.0,
            pressure: 1.4,
            event_type: TouchEventType::Pressed,
            modifiers: MouseModifiers {
                shift: true,
                control: false,
                alt: true,
                platform: false,
            },
            pointer_type: TouchPointerType::Pen,
        };
        let normalized = touch.validate_and_normalize().unwrap();
        let cef_touch = to_cef_touch_event(&normalized);
        assert_eq!(cef_touch.id, 7);
        assert_eq!(cef_touch.x, 120.5);
        assert_eq!(cef_touch.pressure, 1.0);
        assert_eq!(cef_touch.type_, cef::TouchEventType::PRESSED);
        assert_eq!(cef_touch.pointer_type, cef::PointerType::PEN);
        assert_eq!(
            cef_touch.modifiers,
            rust_cef_core::keymap::EVENTFLAG_SHIFT_DOWN | rust_cef_core::keymap::EVENTFLAG_ALT_DOWN
        );

        let mut input = NativeInput::default();
        assert!(!input.has_native_state());
        input.touches.observe(&touch).unwrap();
        assert!(input.has_native_state());
        let cancelled = input.touches.drain_cancellations();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].event_type, TouchEventType::Cancelled);
        assert!(!input.has_native_state());
    }
}
