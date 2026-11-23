//! Owned, bounded input passed across the CEF UI boundary. No native references.
use crate::ports::{EngineError, MouseModifiers};

pub const MAX_INPUT_TEXT_UTF16: usize = 4096;
pub const MAX_PRESSED_KEYS: usize = 64;
pub const MAX_ACTIVE_TOUCH_POINTS: usize = 32;
/// Maximum indexed existing-document UTF-16 position (independent of new text).
pub const MAX_INPUT_RANGE_UTF16: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchEventType {
    Released,
    Pressed,
    Moved,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TouchPointerType {
    #[default]
    Touch,
    Mouse,
    Pen,
    Eraser,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserTouchEvent {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub radius_x: f32,
    pub radius_y: f32,
    pub rotation_angle: f32,
    pub pressure: f32,
    pub event_type: TouchEventType,
    pub modifiers: MouseModifiers,
    pub pointer_type: TouchPointerType,
}

impl BrowserTouchEvent {
    pub fn validate_and_normalize(&self) -> Result<Self, EngineError> {
        if self.id < 0
            || !self.x.is_finite()
            || !self.y.is_finite()
            || !self.radius_x.is_finite()
            || !self.radius_y.is_finite()
            || !self.rotation_angle.is_finite()
            || !self.pressure.is_finite()
            || self.radius_x < 0.0
            || self.radius_y < 0.0
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut normalized = *self;
        normalized.pressure = normalized.pressure.clamp(0.0, 1.0);
        Ok(normalized)
    }
}

/// Tracks active touch points so focus loss/reset/close can cancel open contacts.
#[derive(Clone, Debug, Default)]
pub struct ActiveTouchTracker {
    active: std::collections::BTreeMap<i32, BrowserTouchEvent>,
}

impl ActiveTouchTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, event: &BrowserTouchEvent) -> Result<BrowserTouchEvent, EngineError> {
        let normalized = event.validate_and_normalize()?;
        match normalized.event_type {
            TouchEventType::Pressed => {
                if !self.active.contains_key(&normalized.id)
                    && self.active.len() >= MAX_ACTIVE_TOUCH_POINTS
                {
                    return Err(EngineError::Backpressure);
                }
                self.active.insert(normalized.id, normalized);
            }
            TouchEventType::Moved => {
                if self.active.contains_key(&normalized.id)
                    || self.active.len() < MAX_ACTIVE_TOUCH_POINTS
                {
                    self.active.insert(normalized.id, normalized);
                } else {
                    return Err(EngineError::Backpressure);
                }
            }
            TouchEventType::Released | TouchEventType::Cancelled => {
                self.active.remove(&normalized.id);
            }
        }
        Ok(normalized)
    }

    pub fn drain_cancellations(&mut self) -> Vec<BrowserTouchEvent> {
        let entries = std::mem::take(&mut self.active);
        entries
            .into_values()
            .map(|point| BrowserTouchEvent {
                event_type: TouchEventType::Cancelled,
                pressure: 0.0,
                ..point
            })
            .collect()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utf16Range {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputResetReason {
    FocusLost,
    Rejected,
    Closing,
    Draining,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    MouseMove {
        x: i32,
        y: i32,
        leaving: bool,
        modifiers: MouseModifiers,
    },
    MouseButton {
        x: i32,
        y: i32,
        button: MouseButton,
        up: bool,
        click_count: u8,
        modifiers: MouseModifiers,
    },
    MouseWheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: MouseModifiers,
    },
    /// RAWKEYDOWN/KEYUP only; character events are exclusively Text/IME.
    Key {
        windows_key_code: i32,
        native_key_code: i32,
        up: bool,
        repeat: bool,
        system: bool,
        modifiers: MouseModifiers,
    },
    Text(String),
    /// Replace an existing document UTF-16 range; empty text deletes it.
    ReplaceText {
        text: String,
        replacement: Utf16Range,
    },
    ImeSet {
        text: String,
        selection: Utf16Range,
        replacement: Option<Utf16Range>,
    },
    ImeFinish {
        keep_selection: bool,
    },
    ImeCancel,
    /// File-only COPY target. Host advances enter/over/drop across UI rounds;
    /// issued Over is not a renderer dragover acceptance acknowledgment.
    FileDragEnter {
        paths: crate::file_drop::FileDropPaths,
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
    },
    FileDragOver {
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
    },
    FileDragLeave,
    FileDrop {
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputEvent {
    /// Host-observed epoch from `input_snapshot`; stale epochs are refused.
    pub epoch: u64,
    pub kind: InputKind,
}

impl InputEvent {
    pub fn validate(&self) -> Result<(), EngineError> {
        match &self.kind {
            InputKind::FileDragEnter { paths, x, y, .. } => {
                paths.validate()?;
                if *x < 0 || *y < 0 {
                    return Err(EngineError::InvalidRequest);
                }
            }
            InputKind::FileDragOver { x, y, .. } | InputKind::FileDrop { x, y, .. }
                if *x < 0 || *y < 0 =>
            {
                return Err(EngineError::InvalidRequest);
            }
            InputKind::Text(text)
            | InputKind::ReplaceText { text, .. }
            | InputKind::ImeSet { text, .. } => {
                let len = text.encode_utf16().count();
                if len > MAX_INPUT_TEXT_UTF16 {
                    return Err(EngineError::InvalidRequest);
                }
                if let InputKind::ReplaceText { replacement, .. } = &self.kind {
                    if !valid_range(*replacement, MAX_INPUT_RANGE_UTF16) {
                        return Err(EngineError::InvalidRequest);
                    }
                }
                if let InputKind::ImeSet {
                    selection,
                    replacement,
                    ..
                } = &self.kind
                {
                    // Replacement indexes existing document/composition text, not new text.
                    if !valid_range(*selection, len)
                        || replacement.is_some_and(|r| !valid_range(r, MAX_INPUT_RANGE_UTF16))
                    {
                        return Err(EngineError::InvalidRequest);
                    }
                }
            }
            InputKind::MouseButton { click_count, .. } if *click_count == 0 => {
                return Err(EngineError::InvalidRequest);
            }
            InputKind::Key {
                windows_key_code, ..
            } if !(1..=255).contains(windows_key_code) => return Err(EngineError::InvalidRequest),
            _ => {}
        }
        Ok(())
    }
}

fn valid_range(range: Utf16Range, len: usize) -> bool {
    range.start <= range.end && range.end as usize <= len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_selection_and_replacement_are_distinct_bounded_spaces() {
        let event = InputEvent {
            epoch: 1,
            kind: InputKind::ImeSet {
                text: "😀".into(),
                selection: Utf16Range { start: 0, end: 2 },
                replacement: Some(Utf16Range { start: 10, end: 12 }),
            },
        };
        assert_eq!(event.validate(), Ok(()));
        let mut invalid = event.clone();
        if let InputKind::ImeSet { selection, .. } = &mut invalid.kind {
            selection.end = 3;
        }
        assert_eq!(invalid.validate(), Err(EngineError::InvalidRequest));
        assert_eq!(
            InputEvent {
                epoch: 1,
                kind: InputKind::Text("x".repeat(MAX_INPUT_TEXT_UTF16 + 1))
            }
            .validate(),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            InputEvent {
                epoch: 1,
                kind: InputKind::ReplaceText {
                    text: String::new(),
                    replacement: Utf16Range { start: 8, end: 20 },
                }
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            InputEvent {
                epoch: 1,
                kind: InputKind::ReplaceText {
                    text: String::new(),
                    replacement: Utf16Range { start: 20, end: 8 },
                }
            }
            .validate(),
            Err(EngineError::InvalidRequest)
        );
    }

    #[test]
    fn touch_validation_pressure_clamping_and_active_cancellation() {
        let valid = BrowserTouchEvent {
            id: 1,
            x: 12.5,
            y: 34.5,
            radius_x: 5.0,
            radius_y: 6.0,
            rotation_angle: 0.0,
            pressure: 1.8,
            event_type: TouchEventType::Pressed,
            modifiers: MouseModifiers::default(),
            pointer_type: TouchPointerType::Touch,
        };
        let normalized = valid.validate_and_normalize().unwrap();
        assert_eq!(normalized.pressure, 1.0);

        let negative_id = BrowserTouchEvent { id: -1, ..valid };
        assert_eq!(
            negative_id.validate_and_normalize(),
            Err(EngineError::InvalidRequest)
        );
        let nan_coord = BrowserTouchEvent {
            x: f32::NAN,
            ..valid
        };
        assert_eq!(
            nan_coord.validate_and_normalize(),
            Err(EngineError::InvalidRequest)
        );
        let neg_radius = BrowserTouchEvent {
            radius_x: -0.1,
            ..valid
        };
        assert_eq!(
            neg_radius.validate_and_normalize(),
            Err(EngineError::InvalidRequest)
        );

        let mut tracker = ActiveTouchTracker::new();
        for id in 0..MAX_ACTIVE_TOUCH_POINTS as i32 {
            tracker.observe(&BrowserTouchEvent { id, ..valid }).unwrap();
        }
        assert_eq!(tracker.active_count(), MAX_ACTIVE_TOUCH_POINTS);
        assert_eq!(
            tracker.observe(&BrowserTouchEvent {
                id: MAX_ACTIVE_TOUCH_POINTS as i32 + 1,
                ..valid
            }),
            Err(EngineError::Backpressure)
        );

        tracker
            .observe(&BrowserTouchEvent {
                id: 0,
                event_type: TouchEventType::Released,
                ..valid
            })
            .unwrap();
        assert_eq!(tracker.active_count(), MAX_ACTIVE_TOUCH_POINTS - 1);

        let cancelled = tracker.drain_cancellations();
        assert_eq!(cancelled.len(), MAX_ACTIVE_TOUCH_POINTS - 1);
        assert!(
            cancelled
                .iter()
                .all(|ev| ev.event_type == TouchEventType::Cancelled && ev.pressure == 0.0)
        );
        assert!(tracker.is_empty());
    }
}

/// No text content or native reference is exposed by this diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSnapshot {
    pub epoch: u64,
    pub admitted: u64,
    pub executed: u64,
    pub rejected: u64,
    pub inflight: usize,
    /// Native UI operation is executing; pressed counts are the last confirmed snapshot.
    pub native_execution_unconfirmed: bool,
    pub pressed_buttons: usize,
    pub pressed_keys: usize,
    pub composition_active: bool,
    /// Last confirmed UI-side file gesture state, not a page/drop effect ACK.
    pub file_drag_active: bool,
    /// Last confirmed issued stage; still NOT a renderer acceptance ACK.
    pub file_drag_phase: crate::file_drop::FileDragPhase,
    pub cleanup_pending: bool,
    pub cleanup_failed: bool,
    pub last_native_error: Option<EngineError>,
    pub accepts_text: bool,
}
