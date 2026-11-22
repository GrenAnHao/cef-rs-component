//! Bounded refresh demands for CEF's OSR resize/capture stall (CEF #3826).
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ResizeRecovery {
    expected: Option<(u32, u32)>,
    // Retained after an acknowledgement so rapid targets cannot reset the cooldown.
    next_request: Option<Instant>,
    remaining: u8,
}

impl ResizeRecovery {
    pub fn resized(&mut self, width: i32, height: i32, scale: f32, now: Instant) {
        if width <= 1 || height <= 1 {
            self.expected = None;
            self.remaining = 0;
            return;
        }
        self.expected = Some((
            (width as f32 * scale).ceil() as u32,
            (height as f32 * scale).ceil() as u32,
        ));
        self.next_request = Some(self.next_request.map_or(now, |next| next.max(now)));
        self.remaining = 8;
    }

    /// Only complete, successfully copied frames may acknowledge a resize.
    pub fn frame_received(&mut self, width: u32, height: u32) {
        if self.expected == Some((width, height)) {
            self.expected = None;
            self.remaining = 0;
        }
    }

    /// Called by the existing CEF pump, never from a paint callback. Refresh
    /// demands bypass the stuck ordinary refresh path in affected Chromium
    /// versions. Retries stop on a matching frame or after a short bounded burst.
    pub fn take_request(&mut self, now: Instant) -> bool {
        let Some(next) = self.next_request else {
            return false;
        };
        if self.remaining == 0 || now < next {
            return false;
        }
        self.remaining -= 1;
        self.next_request = Some(now + Duration::from_millis(100));
        true
    }

    /// Requests still available for the current target. A new target resets
    /// this to 8; an acknowledgement or [`Self::cancel`] sets it to 0.
    pub fn remaining(&self) -> u8 {
        self.remaining
    }

    /// Current complete pixel target, if a recovery is still pending.
    pub fn expected(&self) -> Option<(u32, u32)> {
        self.expected
    }

    /// Abandons the current target without clearing the retry cooldown. Hiding
    /// and restoring a view must not allow an immediate extra refresh demand.
    pub fn cancel(&mut self) {
        self.expected = None;
        self.remaining = 0;
    }

    /// Whether the retry cooldown has elapsed. Runtime uses this to avoid
    /// declaring exhaustion before the final request has had its 100 ms
    /// response window.
    pub fn cooldown_elapsed(&self, now: Instant) -> bool {
        self.next_request.map_or(true, |next| now >= next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_resize_frame_requests_refresh_even_without_more_paints() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(773, 565, 1.5, now);
        assert!(recovery.take_request(now));
        assert!(!recovery.take_request(now + Duration::from_millis(50)));
        assert!(recovery.take_request(now + Duration::from_millis(100)));
    }

    #[test]
    fn old_frame_does_not_acknowledge_new_size_and_hidpi_rounds_up() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(773, 565, 1.5, now);
        recovery.frame_received(595, 213);
        assert!(recovery.take_request(now));
        recovery.frame_received(1159, 847);
        assert!(recovery.take_request(now + Duration::from_millis(100)));
        recovery.frame_received(1160, 848);
        assert!(!recovery.take_request(now + Duration::from_secs(1)));
    }

    #[test]
    fn fractional_scale_uses_cef_float_pixel_geometry() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(100, 200, 1.1, now);
        recovery.frame_received(110, 220);
        assert!(!recovery.take_request(now));
    }

    #[test]
    fn latest_resize_supersedes_previous_target() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(200, 100, 1.5, now);
        recovery.resized(800, 600, 1.5, now);
        recovery.frame_received(300, 150);
        assert!(recovery.take_request(now));
        recovery.frame_received(1200, 900);
        assert!(!recovery.take_request(now + Duration::from_secs(1)));
    }

    #[test]
    fn hidden_view_cancels_recovery_and_restore_rearms_it() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(800, 600, 1.0, now);
        recovery.resized(200, 1, 1.0, now);
        assert!(!recovery.take_request(now));
        recovery.resized(800, 600, 1.0, now);
        assert!(recovery.take_request(now));
    }

    #[test]
    fn rapid_resize_targets_and_acknowledgements_do_not_bypass_rate_limit() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(800, 600, 1.0, now);
        assert!(recovery.take_request(now));
        for step in 1..5 {
            let tick = now + Duration::from_millis(step * 20);
            recovery.frame_received(800 + step as u32 - 1, 600);
            recovery.resized(800 + step as i32, 600, 1.0, tick);
            assert!(
                !recovery.take_request(tick),
                "new target bypassed the cooldown"
            );
        }
        assert!(recovery.take_request(now + Duration::from_millis(100)));
    }

    #[test]
    fn permanent_failure_cannot_create_an_endless_repaint_loop() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(800, 600, 1.0, now);
        let requests = (0..100)
            .filter(|step| recovery.take_request(now + Duration::from_millis(step * 100)))
            .count();
        assert!(requests > 1, "must recover a dropped first refresh demand");
        assert!(
            requests <= 10,
            "must stop requesting refresh after a bounded retry window"
        );
        assert_eq!(
            requests, 8,
            "the bounded burst is exactly the 8-request budget"
        );
    }

    #[test]
    fn recovery_grants_exactly_eight_requests_at_least_100ms_apart() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(800, 600, 1.0, now);
        assert_eq!(recovery.remaining(), 8);
        assert_eq!(recovery.expected(), Some((800, 600)));
        assert!(recovery.take_request(now));
        assert!(!recovery.take_request(now + Duration::from_millis(99)));

        let granted: Vec<u32> = (0..100u32)
            .filter(|step| {
                recovery.take_request(now + Duration::from_millis(u64::from(*step) * 100))
            })
            .collect();
        assert_eq!(granted, (1..8u32).collect::<Vec<_>>());
        assert_eq!(recovery.remaining(), 0);
        assert!(
            recovery.expected().is_some(),
            "the target stays pending until a complete frame arrives"
        );
        assert!(recovery.cooldown_elapsed(now + Duration::from_millis(800)));
        assert!(
            !recovery.take_request(now + Duration::from_millis(800)),
            "a ninth request must never be granted"
        );
    }

    #[test]
    fn cancel_clears_target_but_keeps_the_retry_cooldown() {
        let now = Instant::now();
        let mut recovery = ResizeRecovery::default();
        recovery.resized(800, 600, 1.0, now);
        assert!(recovery.take_request(now));
        recovery.cancel();
        assert_eq!(recovery.expected(), None);
        assert_eq!(recovery.remaining(), 0);

        recovery.resized(800, 600, 1.0, now + Duration::from_millis(10));
        assert_eq!(recovery.remaining(), 8);
        assert!(!recovery.take_request(now + Duration::from_millis(99)));
        assert!(!recovery.cooldown_elapsed(now + Duration::from_millis(99)));
        assert!(recovery.take_request(now + Duration::from_millis(100)));
    }
}
