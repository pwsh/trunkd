// SPDX-License-Identifier: GPL-3.0-or-later
//! Replay protection for received frames.
//!
//! The link used to keep "the last sequence accepted" per UDP source address.
//! That had two holes:
//!
//! * the source address is not authenticated, so a captured datagram resent
//!   from another port (or another spoofed address) met fresh state and was
//!   accepted — a stale `Grant` sent a tuner to a dead frequency, a replayed
//!   `Claim` made a peer skip a call;
//! * a single high-water mark rejects a grant *retransmission* whose original
//!   was lost once any later frame from the same sender arrived first — and the
//!   ack had already gone out, so the sender counted it delivered.
//!
//! What replaces it is keyed only by authenticated facts:
//!
//! * a frame's `sent_ms` must be within [`MAX_CLOCK_SKEW_MS`] of this host's
//!   clock, so nothing older than the window can be replayed at all;
//! * inside the window every accepted frame's nonce is remembered — `(sender
//!   id, seq)` for a frame that carries a sender id, the frame's own tag for
//!   one from an older build that does not — and a second frame with the same
//!   nonce is a duplicate, whatever address it came from.
//!
//! Nonces are remembered individually rather than as a high-water mark, so
//! frames may arrive in any order: a retransmission whose original was lost is
//! new, and accepted, however many later frames overtook it.
//!
//! The cache is bounded. It never fills in practice (a peer sends a few frames
//! a second; the cap is minutes of that), and if it ever does, the oldest nonce
//! is forgotten and everything sent at or before it is refused from then on —
//! a frame that can no longer be proven fresh is treated as stale.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;

use crate::wire::{Frame, TAG_LEN};

/// How far a frame's `sent_ms` may be from this host's clock, either way.
///
/// Two recorders are expected to be NTP-synchronised to well under a second;
/// thirty seconds is slack for a clock that has not synced yet, and short
/// enough that a captured frame is useless half a minute later.
pub const MAX_CLOCK_SKEW_MS: i64 = 30_000;

/// Most nonces remembered at once.
pub(crate) const MAX_REMEMBERED: usize = 16_384;

/// What names one frame, for the purpose of recognising it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Nonce {
    /// A frame from a sender that names itself: its id and frame counter.
    Sender {
        /// The sender's authenticated id.
        id: u64,
        /// The sender's frame counter.
        seq: u32,
    },
    /// A frame from an older build: the exact datagram, by its tag.
    Frame([u8; TAG_LEN]),
}

impl Nonce {
    /// The nonce of a decoded frame.
    pub(crate) fn of(frame: &Frame) -> Nonce {
        match frame.sender {
            Some(id) => Nonce::Sender { id, seq: frame.seq },
            None => Nonce::Frame(frame.tag),
        }
    }
}

/// The receiver's decision about one authenticated frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Never seen: deliver it.
    Fresh,
    /// Already accepted once. `same_source` says whether this copy came from
    /// the address the accepted one did — a genuine retransmission — rather
    /// than from somewhere else (a replay).
    Duplicate {
        /// Whether the address matches the first delivery's.
        same_source: bool,
    },
    /// `sent_ms` is outside the clock window (or behind what the cache can
    /// still vouch for). `skew_ms` is `sent_ms - now`.
    Stale {
        /// How far the frame's clock was from ours.
        skew_ms: i64,
    },
}

/// One remembered nonce: where its first delivery came from.
#[derive(Debug, Clone, Copy)]
struct Seen {
    from: SocketAddr,
}

/// The nonces accepted inside the clock window.
#[derive(Debug)]
pub(crate) struct ReplayCache {
    seen: HashMap<Nonce, Seen>,
    /// Insertion order, for expiry and for the cap.
    order: VecDeque<(Nonce, i64)>,
    /// Frames sent at or before this are refused: the cache had to forget
    /// nonces this recent to stay bounded.
    floor_ms: i64,
    cap: usize,
}

impl Default for ReplayCache {
    fn default() -> Self {
        ReplayCache::with_capacity(MAX_REMEMBERED)
    }
}

impl ReplayCache {
    /// A cache that remembers at most `cap` nonces.
    pub(crate) fn with_capacity(cap: usize) -> Self {
        ReplayCache {
            seen: HashMap::new(),
            order: VecDeque::new(),
            floor_ms: i64::MIN,
            cap: cap.max(1),
        }
    }

    /// Nonces currently remembered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.seen.len()
    }

    /// Judge one frame, and remember it if it is fresh.
    pub(crate) fn check(
        &mut self,
        nonce: Nonce,
        sent_ms: i64,
        now_ms: i64,
        from: SocketAddr,
    ) -> Verdict {
        self.expire(now_ms);
        let skew_ms = sent_ms.saturating_sub(now_ms);
        if skew_ms.abs() > MAX_CLOCK_SKEW_MS || sent_ms <= self.floor_ms {
            return Verdict::Stale { skew_ms };
        }
        if let Some(seen) = self.seen.get(&nonce) {
            return Verdict::Duplicate {
                same_source: seen.from == from,
            };
        }
        self.seen.insert(nonce, Seen { from });
        self.order.push_back((nonce, sent_ms));
        while self.order.len() > self.cap {
            if let Some((old, old_ms)) = self.order.pop_front() {
                self.seen.remove(&old);
                self.floor_ms = self.floor_ms.max(old_ms);
            }
        }
        Verdict::Fresh
    }

    /// Forget nonces the clock window already refuses.
    fn expire(&mut self, now_ms: i64) {
        let oldest = now_ms.saturating_sub(MAX_CLOCK_SKEW_MS);
        while let Some(&(nonce, sent_ms)) = self.order.front() {
            if sent_ms >= oldest {
                break;
            }
            self.order.pop_front();
            self.seen.remove(&nonce);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000_000;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 30], port))
    }

    fn n(id: u64, seq: u32) -> Nonce {
        Nonce::Sender { id, seq }
    }

    #[test]
    fn a_frame_is_fresh_once_and_a_duplicate_from_anywhere_after() {
        let mut c = ReplayCache::default();
        assert_eq!(c.check(n(1, 9), NOW, NOW, addr(5000)), Verdict::Fresh);
        assert_eq!(
            c.check(n(1, 9), NOW, NOW + 5, addr(5000)),
            Verdict::Duplicate { same_source: true },
            "a retransmission from the same socket"
        );
        assert_eq!(
            c.check(n(1, 9), NOW, NOW + 10, addr(5001)),
            Verdict::Duplicate { same_source: false },
            "the same frame from another port is a replay, not a new peer"
        );
    }

    #[test]
    fn frames_may_arrive_in_any_order() {
        // The original of seq 9 was lost; seq 10 overtook its
        // retransmission. Both are new.
        let mut c = ReplayCache::default();
        assert_eq!(c.check(n(1, 10), NOW, NOW, addr(1)), Verdict::Fresh);
        assert_eq!(c.check(n(1, 9), NOW - 3, NOW + 1, addr(1)), Verdict::Fresh);
        assert_eq!(
            c.check(n(1, 9), NOW - 3, NOW + 2, addr(1)),
            Verdict::Duplicate { same_source: true }
        );
    }

    #[test]
    fn two_senders_with_the_same_counter_do_not_collide() {
        let mut c = ReplayCache::default();
        assert_eq!(c.check(n(1, 1), NOW, NOW, addr(1)), Verdict::Fresh);
        assert_eq!(c.check(n(2, 1), NOW, NOW, addr(1)), Verdict::Fresh);
    }

    #[test]
    fn a_frame_outside_the_clock_window_is_stale() {
        let mut c = ReplayCache::default();
        let old = NOW - MAX_CLOCK_SKEW_MS - 1;
        assert_eq!(
            c.check(n(1, 1), old, NOW, addr(1)),
            Verdict::Stale {
                skew_ms: -MAX_CLOCK_SKEW_MS - 1
            }
        );
        let ahead = NOW + MAX_CLOCK_SKEW_MS + 1;
        assert!(matches!(
            c.check(n(1, 2), ahead, NOW, addr(1)),
            Verdict::Stale { .. }
        ));
        assert_eq!(c.len(), 0, "nothing stale is remembered");
    }

    #[test]
    fn a_nonce_is_forgotten_only_once_the_window_refuses_it() {
        let mut c = ReplayCache::default();
        assert_eq!(c.check(n(1, 1), NOW, NOW, addr(1)), Verdict::Fresh);
        // Just inside the window: still remembered, still a duplicate.
        assert!(matches!(
            c.check(n(1, 1), NOW, NOW + MAX_CLOCK_SKEW_MS, addr(1)),
            Verdict::Duplicate { .. }
        ));
        // Past it: forgotten, and refused by the clock instead.
        assert!(matches!(
            c.check(n(1, 1), NOW, NOW + MAX_CLOCK_SKEW_MS + 1, addr(1)),
            Verdict::Stale { .. }
        ));
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn a_full_cache_refuses_what_it_can_no_longer_vouch_for() {
        let mut c = ReplayCache::with_capacity(4);
        for seq in 0..6u32 {
            let sent = NOW + i64::from(seq);
            assert_eq!(c.check(n(1, seq), sent, NOW, addr(1)), Verdict::Fresh);
        }
        assert_eq!(c.len(), 4, "bounded");
        // Seq 0 and 1 were forgotten to stay bounded; replaying them is
        // refused rather than accepted as new.
        assert!(matches!(
            c.check(n(1, 1), NOW + 1, NOW, addr(1)),
            Verdict::Stale { .. }
        ));
        assert_eq!(c.check(n(1, 6), NOW + 6, NOW, addr(1)), Verdict::Fresh);
    }

    #[test]
    fn an_older_builds_frames_are_told_apart_by_their_tag() {
        let mut c = ReplayCache::default();
        let a = Nonce::Frame([1; TAG_LEN]);
        let b = Nonce::Frame([2; TAG_LEN]);
        assert_eq!(c.check(a, NOW, NOW, addr(1)), Verdict::Fresh);
        assert_eq!(c.check(b, NOW, NOW, addr(1)), Verdict::Fresh);
        assert!(matches!(
            c.check(a, NOW, NOW, addr(2)),
            Verdict::Duplicate { same_source: false }
        ));
    }
}
