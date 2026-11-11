// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded channels with an explicit **drop-oldest** overflow policy.
//!
//! This is the anti-unbounded-queue primitive the whole application depends on
//! (ARCHITECTURE §2): every channel crossing a domain boundary is bounded, and
//! when a slow consumer lets it fill, the *oldest* item is evicted and counted
//! rather than the producer blocking or the queue growing without bound.
//!
//! The dropped count is a health signal — persistent growth means a consumer is
//! not keeping up.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};

pub use crossbeam_channel::RecvTimeoutError;

/// Sending half of a bounded drop-oldest channel.
///
/// Cloning is intentionally *not* provided: the drop-oldest policy assumes a
/// single logical producer per channel (the SPSC/MPSC boundaries in trunkd).
///
/// The sender holds a receiver clone purely to evict the oldest item when the
/// channel is full, so crossbeam alone would never report the real consumer as
/// gone. A separate `alive` flag, cleared when the [`BoundedReceiver`] drops,
/// gives that signal back.
pub struct BoundedSender<T> {
    tx: Sender<T>,
    /// A receiver clone used solely to evict the oldest item when full.
    evict: Receiver<T>,
    dropped: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}

/// Receiving half of a bounded drop-oldest channel.
pub struct BoundedReceiver<T> {
    rx: Receiver<T>,
    dropped: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}

impl<T> Drop for BoundedReceiver<T> {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

impl<T> std::fmt::Debug for BoundedSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedSender")
            .field("queued", &self.tx.len())
            .field("dropped", &self.dropped_count())
            .finish()
    }
}

impl<T> std::fmt::Debug for BoundedReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedReceiver")
            .field("queued", &self.rx.len())
            .field("dropped", &self.dropped_count())
            .finish()
    }
}

/// Create a bounded drop-oldest channel with room for `capacity` items.
///
/// `capacity` must be at least 1.
pub fn bounded<T>(capacity: usize) -> (BoundedSender<T>, BoundedReceiver<T>) {
    let cap = capacity.max(1);
    let (tx, rx) = crossbeam_channel::bounded(cap);
    let dropped = Arc::new(AtomicU64::new(0));
    let alive = Arc::new(AtomicBool::new(true));
    (
        BoundedSender {
            tx,
            evict: rx.clone(),
            dropped: dropped.clone(),
            alive: alive.clone(),
        },
        BoundedReceiver { rx, dropped, alive },
    )
}

/// Outcome of a [`BoundedSender::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The item was enqueued without evicting anything.
    Sent,
    /// The item was enqueued after evicting the oldest item.
    Evicted,
    /// The receiver is gone; the item was dropped.
    Disconnected,
}

impl<T> BoundedSender<T> {
    /// Send `value`, evicting and counting the oldest item if the channel is
    /// full. Never blocks and never grows the queue.
    pub fn send(&self, value: T) -> SendOutcome {
        if !self.alive.load(Ordering::Acquire) {
            return SendOutcome::Disconnected;
        }
        let mut value = value;
        let mut evicted = false;
        loop {
            match self.tx.try_send(value) {
                Ok(()) => {
                    return if evicted {
                        SendOutcome::Evicted
                    } else {
                        SendOutcome::Sent
                    };
                }
                Err(TrySendError::Full(returned)) => {
                    // Evict the oldest to make room, then retry.
                    if self.evict.try_recv().is_ok() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        evicted = true;
                    }
                    value = returned;
                }
                Err(TrySendError::Disconnected(_)) => return SendOutcome::Disconnected,
            }
        }
    }

    /// Send `value` only if there is room, never evicting anything.
    ///
    /// For a producer that has somewhere better than the floor to put an item
    /// the queue cannot take (the uplink spools it to disk): the item comes
    /// back in the error, and nothing is counted as dropped.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(value));
        }
        self.tx.try_send(value)
    }

    /// Number of items dropped by the drop-oldest policy so far.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl<T> BoundedReceiver<T> {
    /// Receive the next item, blocking until one is available or the sender is
    /// dropped.
    pub fn recv(&self) -> Option<T> {
        self.rx.recv().ok()
    }

    /// Receive the next item without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.rx.try_recv()
    }

    /// Receive the next item, waiting at most `timeout`.
    ///
    /// `None` means "nothing arrived in time" *or* "every sender is gone" —
    /// the callers that need to tell those apart hold their own stop flag, and
    /// a loop that wakes on a timeout is exactly what the link's sender thread
    /// wants: sleep in the kernel while idle, wake on the retry deadline.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<T> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// [`recv_timeout`](Self::recv_timeout) for a consumer that has periodic
    /// work of its own and must tell "nothing arrived in time" (do the work,
    /// wait again) from "every sender is gone and the queue is drained" (stop).
    pub fn recv_wait(&self, timeout: std::time::Duration) -> Result<T, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Number of items dropped by the drop-oldest policy so far.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_oldest_and_counts() {
        let (tx, rx) = bounded::<i32>(2);
        assert_eq!(tx.send(1), SendOutcome::Sent);
        assert_eq!(tx.send(2), SendOutcome::Sent);
        // Channel is now full [1, 2]; sending 3 evicts 1.
        assert_eq!(tx.send(3), SendOutcome::Evicted);
        // Sending 4 evicts 2.
        assert_eq!(tx.send(4), SendOutcome::Evicted);

        assert_eq!(rx.recv(), Some(3));
        assert_eq!(rx.recv(), Some(4));
        assert_eq!(tx.dropped_count(), 2);
        assert_eq!(rx.dropped_count(), 2);
    }

    #[test]
    fn no_eviction_when_consumer_keeps_up() {
        let (tx, rx) = bounded::<i32>(2);
        for i in 0..100 {
            assert_eq!(tx.send(i), SendOutcome::Sent);
            assert_eq!(rx.recv(), Some(i));
        }
        assert_eq!(tx.dropped_count(), 0);
    }

    #[test]
    fn try_send_hands_the_item_back_instead_of_evicting() {
        let (tx, rx) = bounded::<i32>(1);
        assert!(tx.try_send(1).is_ok());
        assert!(matches!(tx.try_send(2), Err(TrySendError::Full(2))));
        assert_eq!(tx.dropped_count(), 0, "nothing was evicted");
        assert_eq!(rx.recv(), Some(1));
        drop(rx);
        assert!(matches!(tx.try_send(3), Err(TrySendError::Disconnected(3))));
    }

    #[test]
    fn recv_wait_tells_a_timeout_from_a_closed_queue() {
        let (tx, rx) = bounded::<i32>(2);
        let wait = std::time::Duration::from_millis(1);
        assert_eq!(rx.recv_wait(wait), Err(RecvTimeoutError::Timeout));
        tx.send(7);
        drop(tx);
        assert_eq!(
            rx.recv_wait(wait),
            Ok(7),
            "what was queued is still drained"
        );
        assert_eq!(rx.recv_wait(wait), Err(RecvTimeoutError::Disconnected));
    }

    #[test]
    fn reports_disconnected_when_receiver_dropped() {
        let (tx, rx) = bounded::<i32>(2);
        drop(rx);
        assert_eq!(tx.send(1), SendOutcome::Disconnected);
    }
}
