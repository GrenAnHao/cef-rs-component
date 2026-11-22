//! UI-neutral session state machine for asynchronous web outbound dragging.

use rust_cef_core::outbound_drag::{
    DragOperationMask, DragSessionId, OutboundDragFinish, OutboundDragStart,
};

/// UI-thread state tracker for one browser's single in-flight outbound drag.
#[derive(Debug, Default)]
pub struct OutboundDragState {
    pending: Option<OutboundDragStart>,
    in_flight: Option<DragSessionId>,
    in_flight_origin: Option<(i32, i32)>,
    last_cursor: Option<DragOperationMask>,
}

impl OutboundDragState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a new outbound drag start, returning an immediate cancel finish if
    /// an earlier drag is already in flight or if an older pending drag is displaced.
    pub fn enqueue(&mut self, start: OutboundDragStart) -> Option<OutboundDragFinish> {
        if self.in_flight.is_some() {
            return Some(OutboundDragFinish {
                drag_id: start.drag_id,
                x: start.x,
                y: start.y,
                operation: DragOperationMask::NONE,
            });
        }
        let displaced = self.pending.replace(start).map(|old| OutboundDragFinish {
            drag_id: old.drag_id,
            x: old.x,
            y: old.y,
            operation: DragOperationMask::NONE,
        });
        displaced
    }

    pub fn update_cursor(&mut self, op: DragOperationMask) {
        if self.is_active() {
            self.last_cursor = Some(op);
        }
    }

    pub fn last_cursor(&self) -> Option<DragOperationMask> {
        self.last_cursor
    }

    /// Promotes a queued outbound drag to in-flight unless another drag is already running.
    pub fn take_pending(&mut self) -> Option<OutboundDragStart> {
        if self.in_flight.is_some() {
            return None;
        }
        let start = self.pending.take()?;
        self.in_flight = Some(start.drag_id);
        self.in_flight_origin = Some((start.x, start.y));
        Some(start)
    }

    /// Marks the active in-flight drag finished when `drag_id` matches.
    pub fn finish_in_flight(
        &mut self,
        drag_id: DragSessionId,
        x: i32,
        y: i32,
        operation: DragOperationMask,
    ) -> Option<OutboundDragFinish> {
        if self.in_flight != Some(drag_id) {
            return None;
        }
        self.in_flight = None;
        self.in_flight_origin = None;
        self.last_cursor = None;
        Some(OutboundDragFinish {
            drag_id,
            x,
            y,
            operation,
        })
    }

    /// Cancels all pending/in-flight outbound drag sessions and clears cursor state.
    pub fn cancel_all(&mut self) -> Vec<OutboundDragFinish> {
        let mut finishes = Vec::new();
        if let Some(drag_id) = self.in_flight.take() {
            let (x, y) = self.in_flight_origin.take().unwrap_or((0, 0));
            finishes.push(OutboundDragFinish {
                drag_id,
                x,
                y,
                operation: DragOperationMask::NONE,
            });
        } else {
            self.in_flight_origin = None;
        }
        if let Some(start) = self.pending.take() {
            finishes.push(OutboundDragFinish {
                drag_id: start.drag_id,
                x: start.x,
                y: start.y,
                operation: DragOperationMask::NONE,
            });
        }
        self.last_cursor = None;
        finishes
    }

    pub fn is_active(&self) -> bool {
        self.pending.is_some() || self.in_flight.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};
    use rust_cef_core::outbound_drag::DraggedPayload;

    fn sample_start(seq: u64, x: i32, y: i32) -> OutboundDragStart {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 7);
        OutboundDragStart {
            drag_id: DragSessionId {
                browser,
                sequence: seq,
            },
            payload: DraggedPayload::bounded(Some("drag"), None, None, Vec::new()),
            allowed_ops: DragOperationMask::COPY,
            x,
            y,
        }
    }

    #[test]
    fn enqueue_take_finish_and_stale_rejection() {
        let mut state = OutboundDragState::new();
        assert!(!state.is_active());

        let s1 = sample_start(1, 10, 20);
        assert_eq!(state.enqueue(s1.clone()), None);
        assert!(state.is_active());

        state.update_cursor(DragOperationMask::COPY);
        assert_eq!(state.last_cursor(), Some(DragOperationMask::COPY));

        let taken = state.take_pending().expect("pending should promote");
        assert_eq!(taken.drag_id, s1.drag_id);
        assert_eq!(state.take_pending(), None);

        // New start while one is in flight is rejected immediately.
        let s2 = sample_start(2, 30, 40);
        let rejected = state
            .enqueue(s2.clone())
            .expect("concurrent drag must reject");
        assert_eq!(rejected.drag_id, s2.drag_id);
        assert_eq!(rejected.operation, DragOperationMask::NONE);

        // Wrong drag_id cannot finish the active session.
        assert_eq!(
            state.finish_in_flight(s2.drag_id, 0, 0, DragOperationMask::COPY),
            None
        );
        assert!(state.is_active());

        // Matching drag_id completes cleanly and clears state.
        let done = state
            .finish_in_flight(s1.drag_id, 55, 66, DragOperationMask::COPY)
            .expect("matching drag should finish");
        assert_eq!(done.drag_id, s1.drag_id);
        assert_eq!(done.operation, DragOperationMask::COPY);
        assert_eq!(state.last_cursor(), None);
        assert!(!state.is_active());
    }

    #[test]
    fn displacing_pending_and_cancel_all_emit_cancel_finishes() {
        let mut state = OutboundDragState::new();
        let s1 = sample_start(1, 12, 34);
        let s2 = sample_start(2, 56, 78);

        assert_eq!(state.enqueue(s1.clone()), None);
        let displaced = state
            .enqueue(s2.clone())
            .expect("second pending must displace first");
        assert_eq!(displaced.drag_id, s1.drag_id);
        assert_eq!(displaced.operation, DragOperationMask::NONE);

        let taken = state.take_pending().unwrap();
        assert_eq!(taken.drag_id, s2.drag_id);
        state.update_cursor(DragOperationMask::MOVE);

        let cancelled = state.cancel_all();
        assert_eq!(
            cancelled,
            vec![OutboundDragFinish {
                drag_id: s2.drag_id,
                x: 56,
                y: 78,
                operation: DragOperationMask::NONE,
            }]
        );
        assert!(!state.is_active());
        assert_eq!(state.last_cursor(), None);
    }
}
