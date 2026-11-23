//! 9.1 core: per-Browser CPU frame budget, VIEW/POPUP mailbox and legacy facade.
//!
//! These tests exercise the real paths used by software CEF callbacks:
//! `capture_token -> copy_frame -> publish`, the shared CPU ledger, and the
//! merged wake handshake. No native Browser or GPUI is involved.

use std::sync::Arc;

use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration, Viewport},
    frame_budget::{BudgetStats, DEFAULT_CPU_FRAME_BUDGET, FrameBudget, FrameError},
    frame_store::FrameStore,
    mailbox::{FrameMailbox, FramePublication, SurfaceBounds},
    ports::{FrameSource, SurfaceKind},
};

const FRAME_2X2: [[u8; 4]; 4] = [
    [1, 2, 3, 255],
    [4, 5, 6, 255],
    [7, 8, 9, 255],
    [10, 11, 12, 255],
];

fn browser(local: u64) -> BrowserId {
    BrowserId::new(RuntimeGeneration::new(1), local)
}

fn bounds(width: i32, height: i32) -> SurfaceBounds {
    SurfaceBounds {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn configured_for(id: BrowserId) -> FrameMailbox {
    let mailbox = FrameMailbox::new(id);
    mailbox.configure_view(bounds(2, 2), 2, 2).unwrap();
    // Hosts drain the initial configuration before exposing the mailbox.
    assert!(mailbox.take_update().is_some());
    mailbox
}

fn configured() -> FrameMailbox {
    configured_for(browser(1))
}

fn flat_2x2() -> Vec<u8> {
    FRAME_2X2.iter().flatten().copied().collect()
}

fn publish_view(mailbox: &FrameMailbox, pixels: [[u8; 4]; 4]) -> FramePublication {
    let token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let bytes: Vec<u8> = pixels.iter().flatten().copied().collect();
    let frame = mailbox.copy_frame(&token, &bytes).unwrap();
    mailbox.publish(frame).unwrap()
}

fn publish_popup(mailbox: &FrameMailbox, pixel: [u8; 4]) -> FramePublication {
    let token = mailbox.capture_token(SurfaceKind::Popup, 1, 1).unwrap();
    let frame = mailbox.copy_frame(&token, &pixel).unwrap();
    mailbox.publish(frame).unwrap()
}

#[test]
fn applied_viewport_scale_revokes_popup_atomically_for_identical_pixel_targets() {
    let mailbox = FrameMailbox::new(browser(1));
    let a = Viewport {
        width: 1,
        height: 1,
        scale: 1.1,
    };
    let b = Viewport {
        width: 1,
        height: 1,
        scale: 1.2,
    };
    mailbox
        .configure_view_with_scale(bounds(1, 1), 2, 2, a.scale)
        .unwrap();
    mailbox.configure_popup(bounds(1, 1), 2, 2).unwrap();
    mailbox.set_popup_visible(true).unwrap();
    let view = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let popup = mailbox.capture_token(SurfaceKind::Popup, 2, 2).unwrap();
    let view_frame = mailbox.copy_frame(&view, &flat_2x2()).unwrap();
    let popup_frame = mailbox.copy_frame(&popup, &flat_2x2()).unwrap();
    mailbox.publish(view_frame).unwrap();
    mailbox.publish(popup_frame).unwrap();
    let old = mailbox.snapshot();
    assert_eq!(old.viewport, Some(a));
    assert!(old.popup.frame.is_some());
    let stale_view = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let stale_popup = mailbox.capture_token(SurfaceKind::Popup, 2, 2).unwrap();
    drop(mailbox.take_update());
    assert!(
        mailbox
            .configure_view_with_scale(bounds(1, 1), 2, 2, b.scale)
            .unwrap()
            .is_some()
    );
    let current = mailbox.snapshot();
    assert_eq!(current.viewport, Some(b));
    assert_eq!(current.view.generation, old.view.generation + 1);
    assert_eq!(current.popup.generation, old.popup.generation + 1);
    assert!(current.popup.visible);
    assert!(current.popup.frame.is_none());
    assert!(current.view.frame.as_ref().unwrap().info.surface_generation < current.view.generation);
    assert!(matches!(
        mailbox.copy_frame(&stale_view, &flat_2x2()),
        Err(FrameError::StaleGeneration)
    ));
    assert!(matches!(
        mailbox.copy_frame(&stale_popup, &flat_2x2()),
        Err(FrameError::StaleGeneration)
    ));
    assert!(
        mailbox
            .configure_view_with_scale(bounds(1, 1), 2, 2, b.scale)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        mailbox.configure_view_with_scale(bounds(1, 1), 2, 2, f32::NAN),
        Err(FrameError::InvalidDimensions)
    ));
    assert_eq!(mailbox.snapshot().viewport, Some(b));
    mailbox.configure_view(bounds(1, 1), 2, 2).unwrap();
    assert_eq!(
        mailbox.snapshot().viewport,
        None,
        "legacy pixels do not imply an exact scale"
    );
}

#[test]
fn default_budget_is_256_mib_per_browser_and_clones_share_one_ledger() {
    assert_eq!(DEFAULT_CPU_FRAME_BUDGET, 256 * 1024 * 1024);
    let budget = FrameBudget::default();
    assert_eq!(budget.stats().limit, DEFAULT_CPU_FRAME_BUDGET);

    let clone = budget.clone();
    let buffer = clone.allocate(64).unwrap();
    assert_eq!(buffer.as_slice().len(), 64);
    assert_eq!(budget.stats().used, 64);
    assert_eq!(clone.stats().used, 64);
    drop(buffer);
    assert_eq!(budget.stats().used, 0);

    // A separate FrameBudget is a separate ledger.
    let other = FrameBudget::default();
    let _held = budget.allocate(32).unwrap();
    assert_eq!(other.stats().used, 0);
}

#[test]
fn budget_reserves_before_allocating_and_rejects_over_limit() {
    let budget = FrameBudget::new(128);
    let first = budget.allocate(64).unwrap();
    let second = budget.allocate(64).unwrap();
    assert_eq!(budget.allocate(1).unwrap_err(), FrameError::BudgetExceeded);
    let stats = budget.stats();
    assert_eq!(
        (stats.limit, stats.used, stats.peak, stats.rejections),
        (128, 128, 128, 1)
    );
    drop(first);
    assert_eq!(budget.stats().used, 64);
    let third = budget.allocate(64).unwrap();
    assert_eq!(budget.stats().used, 128);
    drop((second, third));
    assert_eq!(budget.stats().used, 0);
    assert_eq!(budget.stats().peak, 128);
}

#[test]
fn exhausted_total_budget_rejects_a_copy_without_dropping_the_good_slot() {
    let mailbox = FrameMailbox::with_budget(browser(2), FrameBudget::new(16));
    mailbox.configure_view(bounds(2, 2), 2, 2).unwrap();
    // Drain the configuration wake so the publication below is observable.
    let _ = mailbox.take_update();
    let publication = publish_view(&mailbox, FRAME_2X2);
    assert!(publication.wake.is_some());
    let first = mailbox.latest(SurfaceKind::View).unwrap();

    let token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    assert_eq!(
        mailbox.copy_frame(&token, &flat_2x2()).unwrap_err(),
        FrameError::BudgetExceeded
    );
    assert!(Arc::ptr_eq(
        &first,
        &mailbox.latest(SurfaceKind::View).unwrap()
    ));
    let stats: BudgetStats = mailbox.budget().stats();
    assert_eq!(stats.used, 16);
    assert_eq!(stats.rejections, 1);
}

#[test]
fn view_and_popup_have_independent_slots_generations_and_sequences() {
    let mailbox = configured();
    mailbox.set_popup_visible(true).unwrap();
    mailbox
        .configure_popup(
            SurfaceBounds {
                x: 5,
                y: 6,
                width: 1,
                height: 1,
            },
            1,
            1,
        )
        .unwrap();
    let _ = mailbox.take_update();

    let view = publish_view(&mailbox, FRAME_2X2);
    let popup = publish_popup(&mailbox, [9, 8, 7, 127]);
    assert_eq!(view.frame.info.surface, SurfaceKind::View);
    assert_eq!(popup.frame.info.surface, SurfaceKind::Popup);
    assert_eq!(view.frame.info.sequence, 1);
    assert_eq!(popup.frame.info.sequence, 1, "sequences are per surface");
    assert_eq!(view.frame.info.width, 2);
    assert_eq!(popup.frame.info.width, 1);

    let update = mailbox.take_update().unwrap();
    assert!(!update.closed);
    assert_eq!(update.browser, browser(1));
    assert_eq!(update.view.frame.as_ref().unwrap().info, view.frame.info);
    assert_eq!(update.popup.frame.as_ref().unwrap().info, popup.frame.info);
    assert_eq!((update.popup.bounds.x, update.popup.bounds.y), (5, 6));
    assert!(update.view.visible && update.popup.visible);
    assert!(mailbox.take_update().is_none(), "take_update acknowledges");
}

#[test]
fn older_generation_capture_and_publication_are_rejected() {
    let mailbox = configured();
    let token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let frame = mailbox.copy_frame(&token, &flat_2x2()).unwrap();

    // Resize while the copy is in flight.
    mailbox.configure_view(bounds(3, 2), 3, 2).unwrap();
    assert_eq!(
        mailbox.publish(frame).unwrap_err(),
        FrameError::StaleGeneration
    );
    assert!(mailbox.latest(SurfaceKind::View).is_none());
    // A late callback with the previous size is stale, not a geometry error.
    assert_eq!(
        mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap_err(),
        FrameError::StaleGeneration
    );
    assert!(mailbox.capture_token(SurfaceKind::View, 3, 2).is_ok());
}

#[test]
fn unchanged_configuration_is_silent_but_resize_keeps_a_stale_frame() {
    let mailbox = configured();
    let publication = publish_view(&mailbox, FRAME_2X2);
    assert!(publication.wake.is_some());
    let good = mailbox.latest(SurfaceKind::View).unwrap();
    let _ = mailbox.take_update();

    assert!(
        mailbox
            .configure_view(bounds(2, 2), 2, 2)
            .unwrap()
            .is_none()
    );
    let generation = mailbox.snapshot().view.generation;
    let wake = mailbox.configure_view(bounds(4, 4), 4, 4).unwrap();
    assert!(wake.is_some());
    let update = mailbox.take_update().unwrap();
    assert!(update.view.generation > generation);
    let retained = update.view.frame.as_ref().unwrap();
    assert!(Arc::ptr_eq(retained, &good));
    // The retained frame is usable but explicitly stale.
    assert_ne!(retained.info.surface_generation, update.view.generation);
    assert_eq!(retained.info.surface_generation, update.view.generation - 1);
}

#[test]
fn popup_resize_or_hide_clears_the_slot_and_view_hide_gates_it() {
    let mailbox = configured();
    publish_view(&mailbox, FRAME_2X2);
    // Consume the VIEW wake so the popup change below issues its own ticket.
    let _ = mailbox.take_update();
    mailbox.set_popup_visible(true).unwrap();
    mailbox.configure_popup(bounds(1, 1), 1, 1).unwrap();
    let _ = mailbox.take_update();
    assert!(publish_popup(&mailbox, [9, 8, 7, 127]).wake.is_some());
    assert!(mailbox.latest(SurfaceKind::Popup).is_some());

    // A popup resize must not resurrect old pixels.
    mailbox.configure_popup(bounds(3, 3), 3, 3).unwrap();
    assert!(mailbox.latest(SurfaceKind::Popup).is_none());
    let _ = mailbox.take_update();

    mailbox.configure_popup(bounds(1, 1), 1, 1).unwrap();
    publish_popup(&mailbox, [1, 1, 1, 255]);
    assert!(mailbox.latest(SurfaceKind::Popup).is_some());
    // Popup publication armed a wake; consume it before toggling visibility.
    let _ = mailbox.take_update();

    // Hiding the popup clears its slot and leaves the VIEW frame alone.
    let view_generation = mailbox.snapshot().view.generation;
    assert!(mailbox.set_popup_visible(false).unwrap().is_some());
    assert!(mailbox.latest(SurfaceKind::Popup).is_none());
    assert_eq!(mailbox.snapshot().view.generation, view_generation);
    assert!(mailbox.latest(SurfaceKind::View).is_some());

    // A hidden view cannot show a popup.
    let _ = mailbox.set_visible(false).unwrap();
    assert_eq!(
        mailbox.set_popup_visible(true).unwrap_err(),
        FrameError::Hidden
    );
    let _ = mailbox.set_visible(true).unwrap();
    mailbox.set_popup_visible(true).unwrap();
    assert!(
        mailbox.capture_token(SurfaceKind::Popup, 1, 1).is_ok(),
        "re-showing a configured popup target is capturable again"
    );
}

#[test]
fn hiding_the_view_keeps_a_stale_view_frame_and_forces_popup_hidden() {
    let mailbox = configured();
    mailbox.set_popup_visible(true).unwrap();
    mailbox.configure_popup(bounds(1, 1), 1, 1).unwrap();
    publish_popup(&mailbox, [9, 8, 7, 127]);
    publish_view(&mailbox, FRAME_2X2);
    let _ = mailbox.take_update();

    let view_generation = mailbox.snapshot().view.generation;
    let popup_generation = mailbox.snapshot().popup.generation;
    assert!(mailbox.set_visible(false).unwrap().is_some());
    let update = mailbox.take_update().unwrap();
    assert!(!update.view.visible);
    assert!(!update.popup.visible);
    assert!(update.view.generation > view_generation);
    assert!(update.popup.generation > popup_generation);
    let view_frame = update.view.frame.as_ref().unwrap();
    assert_ne!(view_frame.info.surface_generation, update.view.generation);
    assert!(update.popup.frame.is_none(), "POPUP is retired");
    assert_eq!(
        mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap_err(),
        FrameError::Hidden
    );
    assert_eq!(
        mailbox
            .set_visible(false)
            .unwrap()
            .map(|ticket| ticket.id()),
        None,
        "unchanged visibility does not wake"
    );
}

#[test]
fn renderer_invalidation_keeps_view_but_retires_popup_and_tokens() {
    let mailbox = configured();
    mailbox.set_popup_visible(true).unwrap();
    mailbox.configure_popup(bounds(1, 1), 1, 1).unwrap();
    publish_popup(&mailbox, [9, 8, 7, 127]);
    publish_view(&mailbox, FRAME_2X2);
    let view_frame = mailbox.latest(SurfaceKind::View).unwrap();
    let _ = mailbox.take_update();
    let stale_token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();

    assert!(mailbox.invalidate(SurfaceKind::View).unwrap().is_some());
    let update = mailbox.take_update().unwrap();
    assert!(Arc::ptr_eq(
        update.view.frame.as_ref().unwrap(),
        &view_frame
    ));
    assert!(update.popup.frame.is_none());
    assert_ne!(
        update.view.frame.as_ref().unwrap().info.surface_generation,
        update.view.generation
    );
    assert_eq!(
        mailbox.copy_frame(&stale_token, &flat_2x2()).unwrap_err(),
        FrameError::StaleGeneration
    );
}

#[test]
fn layout_invalidation_revokes_tokens_but_keeps_the_popup_visible() {
    let mailbox = configured();
    mailbox.set_popup_visible(true).unwrap();
    mailbox.configure_popup(bounds(1, 1), 1, 1).unwrap();
    publish_popup(&mailbox, [9, 8, 7, 127]);
    publish_view(&mailbox, FRAME_2X2);
    let _ = mailbox.take_update();

    // Pixel extents are unchanged (a DPI cell round-trip can keep 3x3), so
    // configure_view/configure_popup alone would be a no-op.
    let view_token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let popup_token = mailbox.capture_token(SurfaceKind::Popup, 1, 1).unwrap();
    assert!(mailbox.invalidate_layout().unwrap().is_some());

    let update = mailbox.take_update().unwrap();
    assert!(
        update.popup.visible,
        "layout invalidation must not hide POPUP"
    );
    assert!(update.view.visible);
    assert!(update.view.frame.is_some(), "last good VIEW is retained");
    assert!(update.popup.frame.is_none(), "old POPUP pixels are retired");
    assert_eq!(update.view.bounds, bounds(2, 2));
    assert_eq!(update.popup.bounds, bounds(1, 1));

    // Both surfaces' old tokens are revoked while their targets stay valid.
    assert_eq!(
        mailbox.copy_frame(&view_token, &flat_2x2()).unwrap_err(),
        FrameError::StaleGeneration
    );
    assert_eq!(
        mailbox
            .copy_frame(&popup_token, &[9, 8, 7, 127])
            .unwrap_err(),
        FrameError::StaleGeneration
    );
    assert!(publish_view(&mailbox, FRAME_2X2).wake.is_some());
    // The successful VIEW publish armed a wake; consume it so the POPUP
    // publish is independently observable.
    let _ = mailbox.take_update();
    assert!(publish_popup(&mailbox, [1, 2, 3, 127]).wake.is_some());
    assert!(!mailbox.snapshot().closed);
}

#[test]
fn close_is_terminal_for_the_same_mailbox_and_clears_both_slots() {
    let mailbox = Arc::new(configured());
    let source = mailbox.clone();
    publish_view(&mailbox, FRAME_2X2);
    let _ = mailbox.take_update();
    let held = mailbox.latest(SurfaceKind::View).unwrap();

    let ticket = mailbox.close().expect("first close wakes once");
    assert_eq!(ticket.browser(), browser(1));
    assert!(mailbox.close().is_none(), "close is idempotent");
    assert!(mailbox.is_closed());
    assert!(mailbox.latest(SurfaceKind::View).is_none());
    // A detached consumer Arc is still readable; the slot itself is sealed.
    assert_eq!(
        held.cpu_pixels().unwrap().as_slice(),
        flat_2x2().as_slice()
    );

    let terminal = source.take_update().unwrap();
    assert!(terminal.closed);
    assert!(terminal.view.frame.is_none() && terminal.popup.frame.is_none());
    assert!(source.take_update().is_none());

    assert_eq!(
        mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap_err(),
        FrameError::Closed
    );
    assert_eq!(
        mailbox.publish((*held).clone()).unwrap_err(),
        FrameError::Closed
    );
}

#[test]
fn wake_is_merged_until_take_update_and_failed_wake_keeps_it_pending() {
    let mailbox = configured();
    let first = publish_view(&mailbox, FRAME_2X2);
    let ticket = first.wake.expect("first publication arms a wake");
    assert_ne!(ticket.id(), 0);

    // Further changes while armed do not issue another ticket.
    assert!(publish_view(&mailbox, FRAME_2X2).wake.is_none());
    assert!(mailbox.set_popup_visible(true).unwrap().is_none());

    // A failed host wake disarms the exact ticket but keeps the update dirty.
    assert!(mailbox.wake_failed(ticket));
    let retry = publish_view(&mailbox, FRAME_2X2);
    let retry_ticket = retry.wake.expect("dirty remains wakeable");
    assert_ne!(retry_ticket.id(), ticket.id());
    // The already-consumed ticket cannot disarm the newer wake.
    assert!(!mailbox.wake_failed(ticket));
    assert!(mailbox.take_update().is_some());
    assert!(mailbox.take_update().is_none());
    // take_update cleared armed state, so a new publication wakes again.
    assert!(publish_view(&mailbox, FRAME_2X2).wake.is_some());
}

#[test]
fn snapshot_and_latest_are_queries_not_acknowledgements() {
    let mailbox = configured();
    let _ = mailbox.take_update();
    publish_view(&mailbox, FRAME_2X2);

    let snap = mailbox.snapshot();
    assert!(snap.view.frame.is_some());
    assert!(!snap.closed);
    assert!(mailbox.take_update().is_some(), "wake was still pending");
    assert!(mailbox.latest(SurfaceKind::View).is_some());
    assert!(mailbox.take_update().is_none());
    assert_eq!(mailbox.browser(), browser(1));
    assert_eq!(mailbox.budget().stats().used, 16);
}

#[test]
fn capture_tokens_are_single_use_and_sequences_never_reset() {
    let mailbox = configured();
    let token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    assert_eq!(token.sequence(), 1);
    assert_eq!(token.generation(), mailbox.snapshot().view.generation);
    let frame = mailbox.copy_frame(&token, &flat_2x2()).unwrap();
    assert_eq!(
        mailbox.copy_frame(&token, &flat_2x2()).unwrap_err(),
        FrameError::OutOfOrder,
        "a copied token cannot be reused"
    );
    mailbox.publish(frame).unwrap();

    let second = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    assert_eq!(second.sequence(), 2, "generation changes do not reset it");
    let frame = mailbox.copy_frame(&second, &flat_2x2()).unwrap();
    mailbox.publish(frame).unwrap();
    assert_eq!(mailbox.latest(SurfaceKind::View).unwrap().info.sequence, 2);

    // Re-publishing an already accepted sequence is rejected.
    let mut duplicate = (*mailbox.latest(SurfaceKind::View).unwrap()).clone();
    duplicate.info.sequence = 2;
    assert_eq!(
        mailbox.publish(duplicate).unwrap_err(),
        FrameError::OutOfOrder
    );
}

#[test]
fn foreign_browser_or_budget_pixels_are_rejected() {
    let first = configured_for(browser(7));
    let second = configured_for(browser(7));
    let token = first.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let frame = first.copy_frame(&token, &flat_2x2()).unwrap();
    // Same Browser identity, different ledger: not chargeable here.
    assert_eq!(
        second.publish(frame).unwrap_err(),
        FrameError::WrongIdentity
    );

    let other_browser = configured_for(browser(8));
    let token = first.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let frame = first.copy_frame(&token, &flat_2x2()).unwrap();
    assert_eq!(
        other_browser.publish(frame).unwrap_err(),
        FrameError::WrongIdentity
    );
}

#[test]
fn retained_owner_is_refunded_only_after_its_real_drop() {
    let budget = FrameBudget::new(1024);
    let buffer = budget.allocate(16).unwrap();
    let owner = buffer.into_owner(|pixels| Arc::new(pixels));
    assert_eq!(owner.len(), 16);
    assert_eq!(budget.stats().used, 16, "charge survives owner creation");

    let clone = owner.clone();
    drop(owner);
    // A second strong reference blocks try_unwrap; the charge stays.
    assert_eq!(budget.stats().used, 16);
    drop(clone);
    // The ledger's unique Arc is now unwrappable and fully dropped.
    assert_eq!(budget.stats().used, 0);
}

#[test]
fn owner_destructor_must_finish_before_another_allocation_can_use_its_quota() {
    use std::{
        sync::{Mutex, mpsc},
        time::Duration,
    };
    struct Owner {
        pixels: Vec<u8>,
        started: mpsc::SyncSender<usize>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.started.send(self.pixels.len());
            // Bounded even if an assertion on the consumer side fails. The Vec
            // fields are still live throughout this destructor's body.
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
    }
    let budget = FrameBudget::new(16);
    let (started, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let owner = budget.allocate(16).unwrap().into_owner(|pixels| {
        Arc::new(Owner {
            pixels,
            started,
            release: Mutex::new(released),
        })
    });
    let weak = Arc::downgrade(&owner);
    drop(owner);
    let reaper_budget = budget.clone();
    let reaper = std::thread::spawn(move || reaper_budget.stats());
    assert_eq!(entered.recv_timeout(Duration::from_secs(5)).unwrap(), 16);
    assert!(
        weak.upgrade().is_none(),
        "zero strong count is not completed destruction"
    );
    assert_eq!(
        budget.stats().used,
        16,
        "opaque destructor runs outside ledger locks"
    );
    assert_eq!(budget.allocate(1).unwrap_err(), FrameError::BudgetExceeded);
    release.send(()).unwrap();
    assert_eq!(reaper.join().unwrap().used, 0);
    assert_eq!(budget.stats().used, 0);
}

#[test]
fn failed_owner_build_refunds_the_same_ledger() {
    let budget = FrameBudget::new(1024);
    let buffer = budget.allocate(8).unwrap();
    let failure: Result<Arc<Vec<u8>>, FrameError> = buffer.try_into_owner(|pixels| {
        assert_eq!(pixels.len(), 8);
        // The closure must actually drop the moved Vec before returning Err;
        // the narrowed error type cannot carry the charged bytes out.
        drop(pixels);
        Err(FrameError::InvalidDimensions)
    });
    assert_eq!(failure.unwrap_err(), FrameError::InvalidDimensions);
    assert_eq!(budget.stats().used, 0);
    assert_eq!(budget.stats().rejections, 0);
    // The ledger is immediately reusable.
    let reused = budget.allocate(8).unwrap();
    assert_eq!(budget.stats().used, 8);
    drop(reused);
    assert_eq!(budget.stats().used, 0);
}

#[test]
fn copy_buffer_makes_an_exclusive_copy_charged_to_the_same_ledger() {
    let mailbox = configured();
    publish_view(&mailbox, FRAME_2X2);
    let budget = mailbox.budget();
    assert_eq!(budget.stats().used, 16);

    let frame = mailbox.latest(SurfaceKind::View).unwrap();
    let copy = frame.cpu_pixels().unwrap().copy_buffer().unwrap();
    assert_eq!(copy.as_slice(), flat_2x2().as_slice());
    assert_eq!(budget.stats().used, 32);
    drop(copy);
    assert_eq!(budget.stats().used, 16);
}

#[test]
fn legacy_frame_store_budgeted_buffer_and_fingerprint() {
    let store = FrameStore::default();
    let mut buffer = store.allocate(2, 1).unwrap();
    buffer
        .as_mut_slice()
        .copy_from_slice(&[0, 0, 255, 255, 255, 0, 0, 255]);
    assert!(store.publish_buffer(2, 1, buffer));
    let frame = store.latest().unwrap();
    assert_eq!((frame.width, frame.height, frame.sequence), (2, 1, 1));
    assert_eq!(frame.bgra.as_slice(), &[0, 0, 255, 255, 255, 0, 0, 255]);
    assert!(store.fingerprint().is_some());
    assert_eq!(store.budget().stats().used, 8);

    assert!(store.publish_bgra(2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]));
    assert_eq!(store.sequence(), 2);
    assert_eq!(
        store.latest().unwrap().bgra.as_slice(),
        &[1, 2, 3, 4, 5, 6, 7, 8]
    );
}

#[test]
fn legacy_frame_store_rejects_when_total_budget_cannot_hold_slot_and_copy() {
    let store = FrameStore::with_budget(FrameBudget::new(4));
    assert!(store.publish_bgra(1, 1, &[1, 2, 3, 4]));
    // The latest slot still charges 4, so a new frame cannot be allocated.
    assert!(!store.publish_bgra(1, 1, &[5, 6, 7, 8]));
    assert_eq!(store.sequence(), 1);
    assert_eq!(store.latest().unwrap().bgra.as_slice(), &[1, 2, 3, 4]);
    assert!(store.budget().stats().rejections >= 1);
}

#[test]
fn frame_source_trait_exposes_the_same_mailbox_without_producer_state() {
    let mailbox = Arc::new(configured());
    publish_view(&mailbox, FRAME_2X2);
    let source: Arc<dyn FrameSource> = mailbox.clone();

    let frame = source.latest(SurfaceKind::View).unwrap();
    assert_eq!(frame.info.sequence, 1);
    let update = source.take_update().unwrap();
    assert_eq!(update.browser, browser(1));
    assert_eq!(update.view.pixel_width, 2);
    assert_eq!(update.popup.pixel_height, 0);
    assert!(!source.snapshot().closed);
    assert!(source.take_update().is_none());
}

#[test]
fn gpu_shared_texture_triple_buffer_tracker_protects_presenting_and_latest() {
    use rust_cef_core::ports::{
        GpuSharedTextureFrame, SharedTextureSlotTracker, TextureFormat,
    };

    let mailbox = configured();
    let tracker = Arc::new(SharedTextureSlotTracker::new());
    let keepalives = [Arc::new(()), Arc::new(()), Arc::new(()), Arc::new(())];

    let make_gpu = |slot: usize, generation: u64| GpuSharedTextureFrame {
        pool_id: 42,
        shared_handle: (slot as isize + 1) * 0x100,
        is_nt_handle: true,
        keyed_mutex_key: Some(0),
        width: 2,
        height: 2,
        format: TextureFormat::Bgra8888,
        frame_seq: 0,
        slot_index: slot,
        generation,
        resource_keepalive: Arc::downgrade(&keepalives[slot]),
        handle_owner: Arc::new(()),
        lease: tracker.lease(slot),
    };

    // Frame 1 -> slot 0 -> mailbox latest_ready = 0
    let s0 = tracker.acquire_write_slot().unwrap();
    assert_eq!(s0, 0);
    let t0 = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let pub0 = mailbox.publish_gpu_frame(&t0, make_gpu(s0, 1)).unwrap();
    assert_eq!(tracker.latest_ready_slot(), Some(0));
    assert_eq!(pub0.frame.gpu_texture().unwrap().frame_seq, 1);

    // Consumer presents slot 0: slot 0 is pinned while presented
    tracker.mark_presenting(s0);
    let presenting_frame = pub0.frame.clone();
    drop(pub0);
    assert_eq!(tracker.presenting_slot(), Some(0));
    assert_eq!(tracker.latest_ready_slot(), None);

    // Producer writes slot 1 -> mailbox latest_ready = 1
    let s1 = tracker.acquire_write_slot().unwrap();
    assert_eq!(s1, 1);
    let t1 = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let pub1 = mailbox.publish_gpu_frame(&t1, make_gpu(s1, 1)).unwrap();
    drop(pub1);
    assert_eq!(tracker.latest_ready_slot(), Some(1));

    // Producer overwrites mailbox before UI wakes: acquires slot 2, drops slot 1's lease!
    let s2 = tracker.acquire_write_slot().unwrap();
    assert_eq!(s2, 2);
    let t2 = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let pub2 = mailbox.publish_gpu_frame(&t2, make_gpu(s2, 1)).unwrap();
    drop(pub2);
    assert_eq!(tracker.latest_ready_slot(), Some(2));
    assert_eq!(tracker.consumer_ref_count(1), 0);

    // Quad buffering yields free slot 3 first, then wraps safely back to slot 1 while UI presents slot 0.
    let s3 = tracker.acquire_write_slot().unwrap();
    assert_eq!(s3, 3);
    let t3 = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
    let pub3 = mailbox.publish_gpu_frame(&t3, make_gpu(s3, 1)).unwrap();
    drop(pub3);
    assert_eq!(tracker.latest_ready_slot(), Some(3));
    assert_eq!(tracker.consumer_ref_count(2), 0);

    let s_next = tracker.acquire_write_slot().unwrap();
    assert_eq!(s_next, 1);
    drop(presenting_frame);
}
