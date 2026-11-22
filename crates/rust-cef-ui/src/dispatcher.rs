//! Minimal UI wake-up adapter for CEF worker/FFI notifications.
//!
//! CEF callbacks publish owned data through [`rust_cef_core::ports::FrameSink`]
//! and [`rust_cef_core::ports::BrowserEventSink`], then call
//! [`UiDispatcher::wake`]. Nothing here touches GPUI: the sender is `Send +
//! Sync`, and the receiving half is awaited by the GPUI foreground owner.
//!
//! Capacity is exactly one. An unbounded burst of event/frame notifications
//! collapses into a single pending wake-up; the consumer re-reads the latest
//! owned state after every wake instead of draining a queue. That is what makes
//! Threaded mode work without a manual pump or a polling ticker: the GUI
//! coroutine is woken by `Receiver::recv`, not by a timer.

use std::fmt;

use async_channel::{Receiver, Sender, TrySendError};
use rust_cef_core::ports::{EngineError, UiDispatcher};

/// Sending half handed to the Runtime as its `UiDispatcher`.
pub struct UiWakeSender {
    sender: Sender<()>,
}

/// Receiving half owned by the GPUI foreground coroutine.
pub struct UiWakeReceiver {
    receiver: Receiver<()>,
}

/// Creates a capacity-one wake channel.
pub fn channel() -> (UiWakeSender, UiWakeReceiver) {
    let (sender, receiver) = async_channel::bounded(1);
    (UiWakeSender { sender }, UiWakeReceiver { receiver })
}

impl UiWakeSender {
    /// Number of notifications not consumed yet: always 0 or 1.
    pub fn pending(&self) -> usize {
        self.sender.len()
    }
}

impl fmt::Debug for UiWakeSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiWakeSender")
            .field("pending", &self.sender.len())
            .field("closed", &self.sender.is_closed())
            .finish()
    }
}

impl UiDispatcher for UiWakeSender {
    fn wake(&self) -> Result<(), EngineError> {
        match self.sender.try_send(()) {
            Ok(()) => Ok(()),
            // A full slot already means "the host must look again"; the extra
            // notification is merged, not lost information.
            Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Closed(())) => Err(EngineError::Closed),
        }
    }
}

impl UiWakeReceiver {
    /// Waits until a wake-up is pending, or returns `false` once every sender is
    /// gone. `recv` is cancel-safe, so it can be raced against a timer branch.
    pub async fn recv(&self) -> bool {
        self.receiver.recv().await.is_ok()
    }

    /// Non-blocking check for the same coroutine after a timer branch fired.
    pub fn try_recv(&self) -> bool {
        self.receiver.try_recv().is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }

    /// Closes the channel so a later `wake()` reports `Closed` to the Runtime.
    pub fn close(&self) {
        self.receiver.close();
    }

    /// Borrowed handle for `select`-style waits alongside a timer.
    pub fn receiver(&self) -> &Receiver<()> {
        &self.receiver
    }
}

impl fmt::Debug for UiWakeReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiWakeReceiver")
            .field("pending", &self.receiver.len())
            .field("closed", &self.receiver.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
        thread,
        time::{Duration, Instant},
    };

    /// std-only block_on: the crate has no async runtime dependency of its own,
    /// and the GPUI foreground executor drives the real host.
    fn block_on<F: Future>(future: F) -> F::Output {
        struct ThreadWaker(thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => thread::park(),
            }
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn sender_and_receiver_are_send_and_sync() {
        assert_send_sync::<UiWakeSender>();
        assert_send_sync::<UiWakeReceiver>();
    }

    #[test]
    fn capacity_one_coalesces_a_burst_into_one_signal() {
        let (sender, receiver) = channel();
        for _ in 0..5 {
            assert_eq!(sender.wake(), Ok(()));
        }
        assert_eq!(sender.pending(), 1, "a burst must collapse to one wake-up");
        assert!(receiver.try_recv());
        assert!(!receiver.try_recv(), "no stale duplicate after the merge");
        assert_eq!(sender.pending(), 0);
    }

    #[test]
    fn wake_after_consumption_is_delivered_again() {
        let (sender, receiver) = channel();
        assert_eq!(sender.wake(), Ok(()));
        assert!(receiver.try_recv());
        assert_eq!(sender.wake(), Ok(()));
        assert!(receiver.try_recv());
    }

    #[test]
    fn dropped_receiver_reports_closed_not_success() {
        let (sender, receiver) = channel();
        drop(receiver);
        assert_eq!(sender.wake(), Err(EngineError::Closed));
    }

    #[test]
    fn closed_receiver_reports_closed_and_wakes_recv() {
        let (sender, receiver) = channel();
        receiver.close();
        assert_eq!(sender.wake(), Err(EngineError::Closed));
        // The GPUI owner must not wait forever once the runtime side is gone.
        assert!(!block_on(receiver.recv()));
    }

    #[test]
    fn wake_from_a_plain_thread_reaches_a_waiting_receiver() {
        let (sender, receiver) = channel();
        let worker = thread::spawn(move || {
            // No GPUI App/Window is borrowed on this thread.
            for _ in 0..8 {
                sender.wake().unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(block_on(async {
            loop {
                if receiver.recv().await {
                    break true;
                }
                if receiver.is_closed() || Instant::now() >= deadline {
                    break false;
                }
            }
        }));
        worker.join().unwrap();
    }
}
