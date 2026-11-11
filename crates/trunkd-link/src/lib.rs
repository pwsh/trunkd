// SPDX-License-Identifier: GPL-3.0-or-later
//! The recorder link: an authenticated UDP channel between trunkd instances, so
//! recorders can hand each other grants and say what they are recording.
//!
//! The link is deliberately the dumbest thing that works. There is no session,
//! no handshake and no ordering guarantee: every datagram is independently
//! authenticated and independently useful. That is the same bargain the rest of
//! the recorder makes with its bounded drop-oldest queues — shed work, never
//! grow memory, never block the DSP thread.
//!
//! The one exception is [`LinkMsg::Grant`], where a lost datagram costs a
//! *call* rather than a stale advertisement. A grant is acknowledged by frame
//! sequence ([`LinkMsg::GrantAck`]) and retried a bounded number of times on
//! the link's own sender thread — 3 × 8 ms by default, inside the 100–300 ms a
//! P25 grant has before its voice starts. A redelivered grant is a no-op: the
//! replay cache drops it for the pipeline while the ack goes out again.
//! Everything else stays fire-and-forget.
//!
//! Replay protection is keyed on what the frame authenticates — the sender's
//! id, its frame counter and its send time — never on the UDP source address,
//! which anyone on the LAN can change (see [`replay`](crate::replay)). A grant
//! is acknowledged only once it has been *accepted*: delivered to the pipeline
//! now, or a retransmission of one that was. So "acked" on the sender's side
//! means "the peer's pipeline has it".
//!
//! ```text
//!   this recorder                                   a peer recorder
//! ┌───────────────────┐   Hello / ControlState   ┌───────────────────┐
//! │ coordinator ─▶ tx ┼──────── Grant ──────────▶│ ingest ─▶ route   │
//! │        thread ◀───┼──────── GrantAck ────────┤ peer table        │
//! │ calls thread ─────┼──────── Claim ──────────▶│                   │
//! │ ingest ◀──────────┼──────── Release ─────────┼───────────────────┘
//! └───────────────────┘        UDP :47123
//! ```
//!
//! [`wire`] is the protocol; this module is the socket, the peer table and the
//! receiver thread. Nothing here knows what a `GrantInfo` is: the recorder maps
//! its pipeline types onto [`LinkMsg`] at the edge.

#![deny(missing_docs)]

mod replay;
pub mod wire;

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use trunkd_core::chan::{self, BoundedReceiver, BoundedSender};
use trunkd_core::Metrics;

pub use replay::MAX_CLOCK_SKEW_MS;
pub use wire::{
    Frame, GrantFlags, LinkMsg, SiteAd, SiteKey, TunerAd, WireError, MAX_DATAGRAM, MIN_KEY_LEN,
};

use replay::{Nonce, ReplayCache, Verdict};

/// Depth of the receiver thread's channel into the pipeline. Link traffic is a
/// few messages a second; this is minutes of headroom, and drop-oldest past it.
pub const RX_DEPTH: usize = 1_024;

/// How long the receiver blocks in `recv_from` before looking at the stop flag.
///
/// The receiver blocks rather than polls: an idle link should cost four
/// wakeups a second, not five hundred. A quarter second is short enough that a
/// shutdown is not perceptibly delayed and long enough that the thread is
/// asleep essentially all of the time.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// Depth of the queue feeding the sender thread.
///
/// Grants and their acknowledgements only; a few a second at worst, so this is
/// minutes of headroom and drop-oldest past it (a grant nobody could send in
/// two hundred queue slots is long past being worth sending).
pub const TX_DEPTH: usize = 256;

/// Default number of times a `Grant` is retransmitted before the sender gives
/// up on hearing an ack for it.
pub const DEFAULT_GRANT_RETRIES: u32 = 3;

/// Default spacing between those retransmissions, in milliseconds.
pub const DEFAULT_GRANT_RETRY_MS: u64 = 8;

/// Ceiling on `grant_retries`, enforced by config validation.
///
/// The point of the retry is to beat one dropped datagram, not to wait: the
/// budget from grant to voice is 100–300 ms, and the default (3 × 8 ms) spends
/// under a tenth of it.
pub const MAX_GRANT_RETRIES: u32 = 8;

/// Ceiling on `grant_retry_ms`, same reasoning.
pub const MAX_GRANT_RETRY_MS: u64 = 100;

/// How often the sender thread looks for acks while a grant is outstanding.
///
/// One millisecond, and only while something is unacknowledged: the thread is
/// blocked in the queue for [`RECV_TIMEOUT`] the rest of the time, so an idle
/// link still costs four wakeups a second.
const ACK_POLL: Duration = Duration::from_millis(1);

/// How many grants may go unacknowledged in a row before a peer is written off
/// as one that does not acknowledge them at all.
const UNACKED_BEFORE_SILENT: u32 = 3;

/// How many recently abandoned frame sequences are remembered, so an ack that
/// arrives after the sender gave up still proves the peer is answering.
const RECENT_UNACKED: usize = 64;

/// The link's sockets, its peer list and its shared key.
///
/// Shared through an `Arc`: the send path is used from the coordinator and call
/// threads, the receive path runs on its own thread. They are separate sockets
/// so each can have the mode it needs — the receiver blocks in the kernel
/// between datagrams, and no caller of [`LinkNode::broadcast`] can ever be
/// parked by a full send buffer.
#[derive(Debug)]
pub struct LinkNode {
    /// The bound socket every peer sends to. Blocking, with [`RECV_TIMEOUT`],
    /// and read only by the receiver thread.
    rx: UdpSocket,
    /// A second, ephemeral socket used for every outgoing datagram, in
    /// non-blocking mode.
    ///
    /// Two sockets rather than one because the two modes cannot coexist on a
    /// single one: on Unix `O_NONBLOCK` lives on the open file description, so
    /// a `try_clone` shares it, and a receiver that blocks would force sends to
    /// block too. The alternative — one blocking socket with a short write
    /// timeout — would still let a pipeline thread wait, which is exactly what
    /// the send path must never do.
    ///
    /// The visible consequence is that a peer sees datagrams arriving from this
    /// host on an ephemeral port rather than the configured one. Nothing
    /// depends on that: a peer is authenticated by its key and named by the
    /// `instance` in its `Hello` and by the sender id every frame carries;
    /// the source address only says where to send an ack.
    tx: UdpSocket,
    key: Vec<u8>,
    /// This node's authenticated sender id, carried in every frame it sends
    /// (see [`wire`]). Random per bind, so a restarted recorder is a new
    /// sender whose frame counter may start again from zero.
    sender_id: u64,
    peers: Vec<SocketAddr>,
    seq: AtomicU32,
    metrics: Metrics,
    /// How hard a `Grant` is retried before the sender stops waiting for an ack.
    policy: GrantAckPolicy,
    /// Per-peer grant bookkeeping, read by the status report.
    grants: Mutex<HashMap<SocketAddr, PeerGrants>>,
    /// The queue into the sender thread: grants to deliver, and acks the
    /// receive thread picked up on the bound socket rather than the send one.
    tx_queue: BoundedSender<LinkMsg>,
    /// The other half, taken by [`LinkNode::spawn_receiver`] exactly once.
    tx_queue_rx: Mutex<Option<BoundedReceiver<LinkMsg>>>,
    /// Whether the sender thread is running. Until it is, a `Grant` goes out
    /// inline exactly as every other message does, unacknowledged — which is
    /// what a [`LinkNode`] built without threads (a test, `trunkd check`) wants.
    sender_running: AtomicBool,
}

/// How patiently a `Grant` is retried until the peer acknowledges it.
///
/// A grant is the one message where loss costs a call rather than a stale
/// advertisement, so it is the one message that is retried. Everything else
/// stays fire-and-forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantAckPolicy {
    /// Retransmissions after the first send. Zero turns the retry off; the ack
    /// is still expected and still counted.
    pub retries: u32,
    /// Spacing between retransmissions.
    pub spacing: Duration,
}

impl Default for GrantAckPolicy {
    fn default() -> Self {
        GrantAckPolicy {
            retries: DEFAULT_GRANT_RETRIES,
            spacing: Duration::from_millis(DEFAULT_GRANT_RETRY_MS),
        }
    }
}

impl GrantAckPolicy {
    /// A policy from the two config keys, clamped to what the link will do.
    pub fn new(retries: u32, retry_ms: u64) -> Self {
        GrantAckPolicy {
            retries: retries.min(MAX_GRANT_RETRIES),
            spacing: Duration::from_millis(retry_ms.clamp(1, MAX_GRANT_RETRY_MS)),
        }
    }
}

/// What has become of the grants sent to one peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrantStats {
    /// Grants handed to this peer (first transmissions only).
    pub sent: u64,
    /// Grants this peer acknowledged.
    pub acked: u64,
    /// Retransmissions sent to it.
    pub retried: u64,
    /// Grants it never acknowledged, after every retry was spent.
    pub unacked: u64,
    /// Whether it answers grants: `None` until the first one is resolved,
    /// `Some(false)` for a peer written off as an older build.
    pub acks: Option<bool>,
}

/// Per-peer state behind [`GrantStats`].
#[derive(Debug, Default)]
struct PeerGrants {
    stats: GrantStats,
    /// Consecutive grants with no ack, reset by any ack.
    unacked_run: u32,
    /// Whether "this peer does not ack grants" has already been logged.
    logged_silent: bool,
}

/// One grant in flight to one peer.
#[derive(Debug)]
struct Pending {
    peer: SocketAddr,
    /// Frame sequence, which is what the ack names.
    seq: u32,
    /// The exact datagram, retransmitted byte for byte so the receiver's replay
    /// window recognises the redelivery and drops it while acking it again.
    bytes: Vec<u8>,
    /// Retransmissions still allowed.
    left: u32,
    /// When the next retransmission (or the give-up) is due.
    due: Instant,
}

/// Failure to bring the link up. Anything after bind is counted, not returned.
#[derive(Debug)]
pub enum LinkError {
    /// The bind address or a peer address could not be resolved or bound.
    Io(std::io::Error),
    /// A peer address string resolved to nothing.
    NoAddress(String),
    /// The shared key is shorter than [`MIN_KEY_LEN`].
    WeakKey(usize),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Io(e) => write!(f, "link socket: {e}"),
            LinkError::NoAddress(a) => write!(f, "link peer {a:?} resolved to no address"),
            LinkError::WeakKey(n) => write!(
                f,
                "link key must be at least {MIN_KEY_LEN} characters (got {n})"
            ),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<std::io::Error> for LinkError {
    fn from(e: std::io::Error) -> Self {
        LinkError::Io(e)
    }
}

impl LinkNode {
    /// Bind `bind` and resolve `peers`.
    ///
    /// Peers are unicast addresses today. Multicast would be one more branch
    /// here (join the group, send to it) and nothing elsewhere: every message
    /// is already a self-contained broadcast to whoever can authenticate it.
    pub fn bind(
        bind: &str,
        peers: &[String],
        key: &str,
        metrics: Metrics,
    ) -> Result<Arc<LinkNode>, LinkError> {
        LinkNode::bind_with(bind, peers, key, metrics, GrantAckPolicy::default())
    }

    /// [`LinkNode::bind`] with an explicit grant retry policy (`[link]
    /// grant_retries` and `grant_retry_ms`).
    pub fn bind_with(
        bind: &str,
        peers: &[String],
        key: &str,
        metrics: Metrics,
        policy: GrantAckPolicy,
    ) -> Result<Arc<LinkNode>, LinkError> {
        if key.len() < MIN_KEY_LEN {
            return Err(LinkError::WeakKey(key.len()));
        }
        let rx = UdpSocket::bind(bind)?;
        // Blocking with a timeout: the receiver sleeps in the kernel between
        // datagrams and wakes four times a second to check the stop flag.
        rx.set_nonblocking(false)?;
        rx.set_read_timeout(Some(RECV_TIMEOUT))?;
        // The send socket takes the receive socket's address family and host,
        // so a link bound to one interface does not send from another.
        let local = rx.local_addr()?;
        let tx = UdpSocket::bind(SocketAddr::new(local.ip(), 0))?;
        tx.set_nonblocking(true)?;
        let mut resolved = Vec::with_capacity(peers.len());
        for p in peers {
            let addr = p
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| LinkError::NoAddress(p.clone()))?;
            resolved.push(addr);
        }
        let (tx_queue, tx_queue_rx) = chan::bounded::<LinkMsg>(TX_DEPTH);
        Ok(Arc::new(LinkNode {
            sender_id: new_sender_id(&local),
            rx,
            tx,
            key: key.as_bytes().to_vec(),
            peers: resolved,
            seq: AtomicU32::new(0),
            metrics,
            policy,
            grants: Mutex::new(HashMap::new()),
            tx_queue,
            tx_queue_rx: Mutex::new(Some(tx_queue_rx)),
            sender_running: AtomicBool::new(false),
        }))
    }

    /// The address peers send to (port 0 resolves to a real port here).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.rx.local_addr()
    }

    /// The peers this node sends to.
    pub fn peers(&self) -> &[SocketAddr] {
        &self.peers
    }

    /// Send `msg` to every peer.
    ///
    /// Never blocks and never fails upward: an unsendable datagram is counted
    /// (`link.dropped`) and forgotten, because the caller is a pipeline thread
    /// that must not wait on a network for anything.
    pub fn broadcast(&self, msg: &LinkMsg) {
        if self.peers.is_empty() {
            return;
        }
        // A grant goes to the sender thread, which is where the ack and retry
        // live; everything else is one datagram per peer, right here. Handing
        // it over is a bounded drop-oldest send, so the caller still never
        // waits and a wedged sender still costs grants rather than memory.
        if matches!(msg, LinkMsg::Grant { .. }) && self.sender_running.load(Ordering::Relaxed) {
            if self.tx_queue.send(msg.clone()) == chan::SendOutcome::Evicted {
                self.metrics.counter("link.dropped").inc();
            }
            return;
        }
        let seq = self.next_seq();
        let bytes = match self.frame(msg, seq) {
            Some(b) => b,
            None => return,
        };
        for peer in &self.peers {
            self.send_bytes(&bytes, *peer);
        }
    }

    /// The next frame sequence. It wraps at 2^32, far outside the receiver's
    /// 30 s replay window at any real frame rate.
    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// Frame and authenticate a message, counting the ones that will not fit.
    fn frame(&self, msg: &LinkMsg, seq: u32) -> Option<Vec<u8>> {
        let now = trunkd_core::Timestamp::now().unix_millis();
        match wire::encode(msg, &self.key, self.sender_id, seq, now) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::debug!(error = %e, "link message not sent");
                self.metrics.counter("link.dropped").inc();
                None
            }
        }
    }

    /// One non-blocking datagram, counted either way.
    fn send_bytes(&self, bytes: &[u8], peer: SocketAddr) {
        match self.tx.send_to(bytes, peer) {
            Ok(_) => {
                self.metrics.counter("link.tx").inc();
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                self.metrics.counter("link.dropped").inc();
            }
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "link send failed");
                self.metrics.counter("link.dropped").inc();
            }
        }
    }

    /// What has become of the grants sent to each configured peer, in the
    /// order the peers were configured. The status report's source.
    pub fn grant_stats(&self) -> Vec<(SocketAddr, GrantStats)> {
        let g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
        self.peers
            .iter()
            .map(|p| (*p, g.get(p).map(|s| s.stats).unwrap_or_default()))
            .collect()
    }

    /// The retry policy in force.
    pub fn grant_policy(&self) -> GrantAckPolicy {
        self.policy
    }

    /// Start the receiver thread.
    ///
    /// It authenticates and replay-checks every datagram itself, so the
    /// pipeline only ever sees messages that came from something holding the
    /// key, were sent inside the clock window, and have not been delivered
    /// before. The channel is bounded drop-oldest like every other queue in
    /// the recorder.
    pub fn spawn_receiver(
        self: &Arc<Self>,
        stop: Arc<AtomicBool>,
    ) -> (BoundedReceiver<(SocketAddr, LinkMsg)>, Vec<JoinHandle<()>>) {
        let (tx, rx) = chan::bounded::<(SocketAddr, LinkMsg)>(RX_DEPTH);
        let node = self.clone();
        let receiver = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("trunkd-link-rx".into())
                .spawn(move || node.receive_loop(tx, stop))
                .expect("spawn link receiver thread")
        };
        let mut threads = vec![receiver];
        // The sender thread is what makes a grant retriable; take the queue it
        // drains. A second call would find it gone, and the node would keep the
        // sender it already has.
        let queue = self
            .tx_queue_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(queue) = queue {
            let node = self.clone();
            self.sender_running.store(true, Ordering::Relaxed);
            threads.push(
                std::thread::Builder::new()
                    .name("trunkd-link-tx".into())
                    .spawn(move || node.send_loop(queue, stop))
                    .expect("spawn link sender thread"),
            );
        }
        (rx, threads)
    }

    /// The sender thread body: deliver grants, chase their acknowledgements.
    ///
    /// It owns the whole retry state machine, so no pipeline thread ever waits
    /// on one: publishing a grant is a bounded channel send, and everything
    /// after it — framing per peer, the retransmissions, the give-up — happens
    /// here. While nothing is outstanding the thread is asleep in the queue for
    /// [`RECV_TIMEOUT`]; while something is, it wakes every [`ACK_POLL`].
    fn send_loop(&self, queue: BoundedReceiver<LinkMsg>, stop: Arc<AtomicBool>) {
        let mut pending: Vec<Pending> = Vec::new();
        let mut recent_unacked: Vec<(u32, SocketAddr)> = Vec::new();
        let mut buf = [0u8; MAX_DATAGRAM];
        while !stop.load(Ordering::Relaxed) {
            let wait = if pending.is_empty() {
                RECV_TIMEOUT
            } else {
                ACK_POLL
            };
            if let Some(msg) = queue.recv_timeout(wait) {
                self.handle_outgoing(msg, &mut pending, &mut recent_unacked);
                while let Ok(msg) = queue.try_recv() {
                    self.handle_outgoing(msg, &mut pending, &mut recent_unacked);
                }
            }
            self.drain_acks(&mut buf, &mut pending, &mut recent_unacked);
            self.retry_due(&mut pending, &mut recent_unacked);
        }
    }

    /// One item off the sender queue: a grant to send, or an ack the *receive*
    /// thread picked up on the bound socket and forwarded here.
    fn handle_outgoing(
        &self,
        msg: LinkMsg,
        pending: &mut Vec<Pending>,
        recent: &mut Vec<(u32, SocketAddr)>,
    ) {
        match msg {
            LinkMsg::GrantAck { seq } => self.note_ack(seq, pending, recent),
            msg => self.send_grant(&msg, pending),
        }
    }

    /// Send one grant to every peer, each with its own frame sequence, and
    /// remember what is owed.
    ///
    /// A separate sequence per peer is what makes an ack unambiguous: the ack
    /// names a frame, and a frame went to exactly one peer.
    fn send_grant(&self, msg: &LinkMsg, pending: &mut Vec<Pending>) {
        for peer in &self.peers {
            let (retries, window) = {
                let mut g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
                let entry = g.entry(*peer).or_default();
                entry.stats.sent += 1;
                // A peer written off as an older build is sent the grant once,
                // but is still watched for the whole window: one ack and it is
                // back to being a peer that answers.
                if entry.stats.acks == Some(false) {
                    (0, self.policy.retries + 1)
                } else {
                    (self.policy.retries, 1)
                }
            };
            let seq = self.next_seq();
            let Some(bytes) = self.frame(msg, seq) else {
                return;
            };
            self.send_bytes(&bytes, *peer);
            pending.push(Pending {
                peer: *peer,
                seq,
                bytes,
                left: retries,
                due: Instant::now() + self.policy.spacing * window,
            });
        }
    }

    /// Read every ack waiting on the send socket.
    ///
    /// Acks land here rather than on the bound socket because a peer answers
    /// the address the grant came *from*, which is this node's ephemeral send
    /// port. Anything else that turns up is not this thread's business.
    fn drain_acks(
        &self,
        buf: &mut [u8; MAX_DATAGRAM],
        pending: &mut Vec<Pending>,
        recent: &mut Vec<(u32, SocketAddr)>,
    ) {
        loop {
            let (n, from) = match self.tx.recv_from(buf) {
                Ok(v) => v,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::debug!(error = %e, "link ack receive failed");
                    return;
                }
            };
            match wire::decode(&buf[..n], &self.key) {
                Ok(Frame {
                    msg: LinkMsg::GrantAck { seq },
                    ..
                }) => {
                    self.metrics.counter("link.grant_acks_rx").inc();
                    self.note_ack(seq, pending, recent);
                }
                Ok(_) => {}
                Err(e) => {
                    self.metrics.counter("link.auth_failed").inc();
                    tracing::debug!(peer = %from, error = %e, "link ack rejected");
                }
            }
        }
    }

    /// Retire the grant `seq` names, and remember that its peer answers.
    fn note_ack(&self, seq: u32, pending: &mut Vec<Pending>, recent: &mut Vec<(u32, SocketAddr)>) {
        let peer = match pending.iter().position(|p| p.seq == seq) {
            Some(i) => pending.remove(i).peer,
            // An ack that arrives after the sender gave up still proves the
            // peer is answering — slowly, or after a lost ack — so it counts.
            None => match recent.iter().position(|(s, _)| *s == seq) {
                Some(i) => recent.remove(i).1,
                None => return,
            },
        };
        let mut g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
        let entry = g.entry(peer).or_default();
        entry.stats.acked += 1;
        entry.unacked_run = 0;
        if entry.stats.acks != Some(true) {
            if entry.logged_silent {
                tracing::info!(peer = %peer, "peer acknowledges grants again");
            }
            entry.logged_silent = false;
            entry.stats.acks = Some(true);
        }
    }

    /// Retransmit what is due, and write off what has run out of attempts.
    fn retry_due(&self, pending: &mut Vec<Pending>, recent: &mut Vec<(u32, SocketAddr)>) {
        let now = Instant::now();
        let mut i = 0;
        while i < pending.len() {
            if pending[i].due > now {
                i += 1;
                continue;
            }
            if pending[i].left > 0 {
                pending[i].left -= 1;
                pending[i].due = now + self.policy.spacing;
                let (bytes, peer) = (pending[i].bytes.clone(), pending[i].peer);
                self.send_bytes(&bytes, peer);
                self.metrics.counter("link.grants_retried").inc();
                let mut g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
                g.entry(peer).or_default().stats.retried += 1;
                i += 1;
                continue;
            }
            let gone = pending.remove(i);
            recent.push((gone.seq, gone.peer));
            if recent.len() > RECENT_UNACKED {
                recent.remove(0);
            }
            self.metrics.counter("link.grants_unacked").inc();
            self.note_unacked(gone.peer);
        }
    }

    /// One grant this peer never answered.
    fn note_unacked(&self, peer: SocketAddr) {
        let mut g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
        let entry = g.entry(peer).or_default();
        entry.stats.unacked += 1;
        entry.unacked_run += 1;
        if entry.stats.acks == Some(true) || entry.unacked_run < UNACKED_BEFORE_SILENT {
            return;
        }
        entry.stats.acks = Some(false);
        if !entry.logged_silent {
            entry.logged_silent = true;
            tracing::info!(
                peer = %peer,
                "peer does not ack grants; sending them once until it says otherwise"
            );
        }
    }

    /// A peer said hello: give it the benefit of the doubt again.
    ///
    /// A restarted peer is a new build as often as not, and its hello is the
    /// only thing this node hears that is unambiguously "I am here now". The
    /// hello arrives from an ephemeral port, so it is matched to configured
    /// peers by host, which is exactly the granularity that matters — a box
    /// upgrades as a whole.
    fn note_hello(&self, from: IpAddr) {
        let mut g = self.grants.lock().unwrap_or_else(|p| p.into_inner());
        for peer in self.peers.iter().filter(|p| p.ip() == from) {
            if let Some(entry) = g.get_mut(peer) {
                if entry.stats.acks == Some(false) {
                    entry.stats.acks = None;
                    entry.unacked_run = 0;
                }
            }
        }
    }

    /// The receiver thread body: block, authenticate, replay-check, forward.
    fn receive_loop(&self, tx: BoundedSender<(SocketAddr, LinkMsg)>, stop: Arc<AtomicBool>) {
        let mut buf = [0u8; MAX_DATAGRAM];
        // Every nonce accepted inside the clock window, keyed by what the
        // frame authenticates rather than by where it came from. Bounded.
        let mut replay = ReplayCache::default();
        while !stop.load(Ordering::Relaxed) {
            match self.rx.recv_from(&mut buf) {
                Ok((n, from)) => self.accept(&buf[..n], from, &mut replay, &tx),
                // A read timeout: Unix reports it as `WouldBlock`, Windows as
                // `TimedOut`. Both mean "nothing arrived; look at the stop flag
                // and block again".
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "link receive failed; link is degraded");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// Authenticate one datagram and hand it on, counting every rejection.
    fn accept(
        &self,
        datagram: &[u8],
        from: SocketAddr,
        replay: &mut ReplayCache,
        tx: &BoundedSender<(SocketAddr, LinkMsg)>,
    ) {
        let frame = match wire::decode(datagram, &self.key) {
            Ok(f) => f,
            Err(e) => {
                self.metrics.counter("link.auth_failed").inc();
                tracing::debug!(peer = %from, error = %e, "link frame rejected");
                return;
            }
        };
        // An ack is for the sender thread, not the pipeline. It carries no
        // state a replay could corrupt (retiring a grant twice is a no-op), so
        // it is handed straight over.
        if let LinkMsg::GrantAck { .. } = frame.msg {
            self.metrics.counter("link.grant_acks_rx").inc();
            self.tx_queue.send(frame.msg);
            return;
        }
        // A grant is acknowledged only once it is *accepted* — delivered below,
        // or a retransmission of one that was — so the sender's "acked" means
        // the pipeline has it. A stale frame is never acked, and a duplicate
        // from an address other than the first delivery's (a replay, not a
        // retransmission) is not answered either: it would only be reflecting
        // acks at whoever the source claims to be.
        let wants_ack =
            matches!(frame.msg, LinkMsg::Grant { .. }) && frame.version >= wire::GRANT_ACK_VERSION;
        let now = trunkd_core::Timestamp::now().unix_millis();
        match replay.check(Nonce::of(&frame), frame.sent_ms, now, from) {
            Verdict::Stale { skew_ms } => {
                let n = self.metrics.counter("link.stale").add(1) + 1;
                // A peer whose clock is off by more than the window has every
                // frame refused: say so, but once per power of two.
                if n.is_power_of_two() {
                    tracing::warn!(
                        peer = %from,
                        skew_ms,
                        window_ms = MAX_CLOCK_SKEW_MS,
                        refused = n,
                        "link frame refused: its send time is outside the clock window \
                         (a replay, or a peer whose clock is not synchronised)"
                    );
                }
            }
            Verdict::Duplicate { same_source } => {
                self.metrics.counter("link.replay").inc();
                if wants_ack && same_source {
                    self.ack_grant(from, frame.seq);
                }
            }
            Verdict::Fresh => {
                if let LinkMsg::Hello { .. } = frame.msg {
                    self.note_hello(from.ip());
                }
                self.metrics.counter("link.rx").inc();
                // Drop-oldest: this frame is always queued (an older one may
                // be evicted for it), so it is delivered and may be acked.
                if tx.send((from, frame.msg)) == chan::SendOutcome::Evicted {
                    self.metrics.counter("link.dropped").inc();
                }
                if wants_ack {
                    self.ack_grant(from, frame.seq);
                }
            }
        }
    }

    /// Answer one grant, straight back to the address it came from.
    ///
    /// Sent from the receive thread on the non-blocking send socket: the ack is
    /// on the peer's latency path, so it must not wait for anything, and a lost
    /// one is answered again by the peer's next retransmission.
    fn ack_grant(&self, to: SocketAddr, seq: u32) {
        let ack = LinkMsg::GrantAck { seq };
        let frame_seq = self.next_seq();
        if let Some(bytes) = self.frame(&ack, frame_seq) {
            self.send_bytes(&bytes, to);
            self.metrics.counter("link.grant_acks_tx").inc();
        }
    }
}

/// A sender id for a node bound at `local`: random, so two recorders — or one
/// recorder before and after a restart — are never mistaken for each other.
///
/// Not a secret (it travels in clear, inside the tag), only unique: std's
/// per-process random hash keys, mixed with the clock, the process id and the
/// bound address, are ample for 64 bits of that without a new dependency.
fn new_sender_id(local: &SocketAddr) -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.write_u128(nanos);
    h.write_u32(std::process::id());
    h.write(local.to_string().as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::{GrantFlags, SiteAd, SiteKey};

    const KEY: &str = "a-sixteen-byte-key-or-longer";

    /// The sender id of a hand-built peer frame.
    const PEER_ID: u64 = 0x5EED_0000_0000_0042;

    fn now() -> i64 {
        trunkd_core::Timestamp::now().unix_millis()
    }

    fn node(peers: &[String]) -> Arc<LinkNode> {
        LinkNode::bind("127.0.0.1:0", peers, KEY, Metrics::new()).expect("binds")
    }

    fn join(threads: Vec<JoinHandle<()>>) {
        for t in threads {
            t.join().expect("link thread ends");
        }
    }

    /// Spin until `f` is true, or fail after two seconds. Every assertion here
    /// is about a thread doing something, and none of them is worth a sleep
    /// long enough to be reliable on a loaded machine.
    fn until(f: impl Fn() -> bool) {
        for _ in 0..2_000 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the link did not get there within two seconds");
    }

    fn stats_of(node: &LinkNode, peer: SocketAddr) -> GrantStats {
        node.grant_stats()
            .into_iter()
            .find(|(p, _)| *p == peer)
            .map(|(_, s)| s)
            .expect("the peer is configured")
    }

    fn wait_for(rx: &BoundedReceiver<(SocketAddr, LinkMsg)>) -> LinkMsg {
        for _ in 0..2_000 {
            if let Ok((_, m)) = rx.try_recv() {
                return m;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("nothing arrived on the link within two seconds");
    }

    fn grant(freq: u64) -> LinkMsg {
        LinkMsg::Grant {
            site: SiteKey {
                system: "example".into(),
                site: "central".into(),
                nac: 0x34F,
            },
            frequency_hz: freq,
            talkgroup: 1301,
            source: 7,
            flags: GrantFlags::default(),
            at_wall_ms: 1_700_000_000_000,
            seq: 1,
        }
    }

    #[test]
    fn a_short_key_is_refused_at_bind() {
        let e = LinkNode::bind("127.0.0.1:0", &[], "tooshort", Metrics::new());
        assert!(matches!(e.as_ref().err(), Some(LinkError::WeakKey(8))));
    }

    #[test]
    fn two_sockets_exchange_a_hello_and_a_grant() {
        let b = node(&[]);
        let b_addr = b.local_addr().expect("bound");
        let a = node(&[b_addr.to_string()]);
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        let hello = LinkMsg::Hello {
            instance: "recorder-a".into(),
            version: 1,
            sites: vec![SiteAd {
                system: "example".into(),
                site: "central".into(),
                nac: 0x34F,
                control_hz: 851_050_000,
                locked: true,
                tsbk_rate: 15,
                tuners: Vec::new(),
            }],
        };
        a.broadcast(&hello);
        assert_eq!(wait_for(&rx), hello);

        a.broadcast(&grant(852_137_500));
        assert_eq!(wait_for(&rx), grant(852_137_500));

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_frame_signed_with_another_key_never_reaches_the_pipeline() {
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        // An impostor with the right shape and the wrong key.
        let impostor = LinkNode::bind(
            "127.0.0.1:0",
            &[b_addr.to_string()],
            "a-totally-different-key",
            Metrics::new(),
        )
        .expect("binds");
        impostor.broadcast(&grant(852_137_500));

        // And a real one after it, so the test does not merely observe silence.
        let a = node(&[b_addr.to_string()]);
        a.broadcast(&grant(853_000_000));

        assert_eq!(
            wait_for(&rx),
            grant(853_000_000),
            "only the authentic frame"
        );
        assert_eq!(metrics.counter("link.auth_failed").get(), 1);
        assert_eq!(metrics.counter("link.rx").get(), 1);

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_replayed_datagram_is_dropped_and_counted() {
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        // Capture a real datagram, then send it twice from one source port.
        let bytes =
            wire::encode(&grant(852_137_500), KEY.as_bytes(), PEER_ID, 9, now()).expect("encodes");
        let attacker = UdpSocket::bind("127.0.0.1:0").expect("binds");
        attacker.send_to(&bytes, b_addr).expect("sent");
        assert_eq!(wait_for(&rx), grant(852_137_500));
        attacker.send_to(&bytes, b_addr).expect("sent again");

        for _ in 0..500 {
            if metrics.counter("link.replay").get() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(metrics.counter("link.replay").get(), 1);
        assert!(
            rx.try_recv().is_err(),
            "the replay never reached the pipeline"
        );

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn an_idle_receiver_still_stops_within_one_read_timeout() {
        // The receiver blocks in the kernel rather than polling, so the one
        // thing that has to be checked is that blocking does not cost a slow
        // shutdown: nothing is ever sent to this node, so every iteration is a
        // full timeout.
        let b = node(&[]);
        let stop = Arc::new(AtomicBool::new(false));
        let (_rx, threads) = b.spawn_receiver(stop.clone());
        std::thread::sleep(Duration::from_millis(50));

        let asked = std::time::Instant::now();
        stop.store(true, Ordering::Relaxed);
        join(threads);
        let took = asked.elapsed();

        assert!(
            took < RECV_TIMEOUT * 2,
            "an idle receiver took {took:?} to notice a stop"
        );
    }

    #[test]
    fn sending_never_waits_on_a_blocked_receiver() {
        // The send path is a separate, non-blocking socket. A caller pushing
        // datagrams at a peer that is doing nothing with them must not be
        // parked — not by the receiver's blocking read, and not by a full send
        // buffer once the peer stops draining.
        let b = node(&[]);
        let b_addr = b.local_addr().expect("bound");
        let a = node(&[b_addr.to_string()]);
        // B binds but never spawns a receiver: nothing drains its buffer.
        let started = std::time::Instant::now();
        for _ in 0..2_000 {
            a.broadcast(&grant(852_137_500));
        }
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(2),
            "2000 sends into an undrained peer took {took:?}; the send path blocked"
        );
        // Whatever the kernel refused was counted rather than waited on.
        assert_eq!(
            a.metrics.counter("link.tx").get() + a.metrics.counter("link.dropped").get(),
            2_000
        );
    }

    #[test]
    fn datagrams_arrive_from_the_senders_own_socket() {
        // Sends leave on an ephemeral port, so a peer sees this host at an
        // address that is not the one it was configured with. That is fine and
        // this pins it: the frame is accepted, and every frame from one sender
        // keys to the same address so the peer table and the acks still work
        // (replay protection no longer looks at the address at all).
        let b = node(&[]);
        let b_addr = b.local_addr().expect("bound");
        let a = node(&[b_addr.to_string()]);
        let a_bound = a.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        a.broadcast(&grant(852_137_500));
        a.broadcast(&grant(853_000_000));
        let mut seen = Vec::new();
        for _ in 0..2_000 {
            if let Ok((from, _)) = rx.try_recv() {
                seen.push(from);
                if seen.len() == 2 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
        join(threads);

        assert_eq!(seen.len(), 2, "both frames arrived");
        assert_eq!(seen[0], seen[1], "one sender is one peer address");
        assert_eq!(seen[0].ip(), a_bound.ip(), "and the host is A's");
        assert_ne!(
            seen[0].port(),
            a_bound.port(),
            "sends leave on their own port"
        );
    }

    #[test]
    fn a_grant_is_acknowledged_and_stops_being_retried() {
        let b = node(&[]);
        let b_addr = b.local_addr().expect("bound");
        let a = node(&[b_addr.to_string()]);
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, b_threads) = b.spawn_receiver(stop.clone());
        let (_a_rx, a_threads) = a.spawn_receiver(stop.clone());

        a.broadcast(&grant(852_137_500));
        assert_eq!(wait_for(&rx), grant(852_137_500));
        until(|| stats_of(&a, b_addr).acked == 1);

        let stats = stats_of(&a, b_addr);
        assert_eq!(stats.sent, 1);
        assert_eq!(stats.acked, 1);
        assert_eq!(stats.unacked, 0, "an acked grant is never given up on");
        assert_eq!(stats.acks, Some(true));
        assert_eq!(b.metrics.counter("link.grant_acks_tx").get(), 1);

        stop.store(true, Ordering::Relaxed);
        join(a_threads);
        join(b_threads);
    }

    #[test]
    fn a_grant_nobody_answers_is_retried_a_bounded_number_of_times() {
        let metrics = Metrics::new();
        // A peer that exists on the network and never says anything back: the
        // datagrams arrive, nothing reads them, no ack is ever sent.
        let deaf = UdpSocket::bind("127.0.0.1:0").expect("binds");
        let deaf_addr = deaf.local_addr().expect("bound");
        let a = LinkNode::bind(
            "127.0.0.1:0",
            &[deaf_addr.to_string()],
            KEY,
            metrics.clone(),
        )
        .expect("binds");
        let stop = Arc::new(AtomicBool::new(false));
        let (_rx, threads) = a.spawn_receiver(stop.clone());

        let started = std::time::Instant::now();
        a.broadcast(&grant(852_137_500));
        until(|| stats_of(&a, deaf_addr).unacked == 1);
        let took = started.elapsed();

        let stats = stats_of(&a, deaf_addr);
        assert_eq!(stats.sent, 1);
        assert_eq!(stats.acked, 0);
        assert_eq!(
            stats.retried,
            u64::from(DEFAULT_GRANT_RETRIES),
            "three retries and no more"
        );
        assert!(
            took < Duration::from_millis(300),
            "giving up took {took:?}; the retry budget is milliseconds"
        );
        assert_eq!(metrics.counter("link.grants_unacked").get(), 1);

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_peer_that_never_acks_is_written_off_and_stops_being_retried() {
        let deaf = UdpSocket::bind("127.0.0.1:0").expect("binds");
        let deaf_addr = deaf.local_addr().expect("bound");
        let a = node(&[deaf_addr.to_string()]);
        let stop = Arc::new(AtomicBool::new(false));
        let (_rx, threads) = a.spawn_receiver(stop.clone());

        for i in 1..=UNACKED_BEFORE_SILENT {
            a.broadcast(&grant(852_137_500));
            until(|| stats_of(&a, deaf_addr).unacked == u64::from(i));
        }
        let written_off = stats_of(&a, deaf_addr);
        assert_eq!(
            written_off.acks,
            Some(false),
            "an old build, as far as we know"
        );
        assert_eq!(
            written_off.retried,
            u64::from(UNACKED_BEFORE_SILENT * DEFAULT_GRANT_RETRIES)
        );

        // From here the grant is sent once and no longer chased.
        a.broadcast(&grant(853_000_000));
        until(|| stats_of(&a, deaf_addr).unacked == u64::from(UNACKED_BEFORE_SILENT) + 1);
        assert_eq!(
            stats_of(&a, deaf_addr).retried,
            written_off.retried,
            "a peer that does not ack is not retried at"
        );

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_redelivered_grant_is_acked_again_and_routed_once() {
        // The exact case a lost ack produces: the same datagram arrives twice.
        // The replay cache keeps the pipeline from seeing it twice, and the
        // ack goes out both times — otherwise the sender would keep retrying
        // something the receiver already has.
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        let bytes =
            wire::encode(&grant(852_137_500), KEY.as_bytes(), PEER_ID, 9, now()).expect("encodes");
        let sender = UdpSocket::bind("127.0.0.1:0").expect("binds");
        sender.send_to(&bytes, b_addr).expect("sent");
        assert_eq!(wait_for(&rx), grant(852_137_500));
        sender.send_to(&bytes, b_addr).expect("sent again");

        until(|| metrics.counter("link.grant_acks_tx").get() == 2);
        until(|| metrics.counter("link.replay").get() == 1);
        assert!(rx.try_recv().is_err(), "the redelivery routed nothing");
        assert_eq!(metrics.counter("link.rx").get(), 1);

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_captured_frame_resent_from_another_port_is_refused_and_not_acked() {
        // C4: the replay state used to be keyed by source address, so the same
        // datagram from a second port met fresh state and was routed again.
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        let bytes =
            wire::encode(&grant(852_137_500), KEY.as_bytes(), PEER_ID, 9, now()).expect("encodes");
        let peer = UdpSocket::bind("127.0.0.1:0").expect("binds");
        peer.send_to(&bytes, b_addr).expect("sent");
        assert_eq!(wait_for(&rx), grant(852_137_500));
        until(|| metrics.counter("link.grant_acks_tx").get() == 1);

        let attacker = UdpSocket::bind("127.0.0.1:0").expect("binds");
        attacker.send_to(&bytes, b_addr).expect("replayed");
        until(|| metrics.counter("link.replay").get() == 1);
        assert!(rx.try_recv().is_err(), "the replay routed nothing");
        assert_eq!(metrics.counter("link.rx").get(), 1);
        assert_eq!(
            metrics.counter("link.grant_acks_tx").get(),
            1,
            "a replay from somewhere else is not answered"
        );

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_frame_sent_outside_the_clock_window_is_refused_and_not_acked() {
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        // Captured over half a minute ago: authentic, and useless.
        let old = now() - MAX_CLOCK_SKEW_MS - 1_000;
        let stale =
            wire::encode(&grant(852_137_500), KEY.as_bytes(), PEER_ID, 3, old).expect("encodes");
        let peer = UdpSocket::bind("127.0.0.1:0").expect("binds");
        peer.send_to(&stale, b_addr).expect("sent");
        // And a current one after it, so the test observes more than silence.
        let fresh =
            wire::encode(&grant(853_000_000), KEY.as_bytes(), PEER_ID, 4, now()).expect("encodes");
        peer.send_to(&fresh, b_addr).expect("sent");

        assert_eq!(wait_for(&rx), grant(853_000_000), "only the current frame");
        until(|| metrics.counter("link.grant_acks_tx").get() == 1);
        assert_eq!(metrics.counter("link.stale").get(), 1);
        assert_eq!(metrics.counter("link.rx").get(), 1);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            metrics.counter("link.grant_acks_tx").get(),
            1,
            "the stale grant was never acked, so its sender never counts it delivered"
        );

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_retransmission_overtaken_by_a_later_frame_is_still_delivered() {
        // C5: grant seq 9's first copy is lost, and the sender's next frame
        // (seq 10) arrives before 9's retransmission. The old high-water mark
        // dropped 9 as a replay *after* acking it — counted delivered by the
        // sender, never seen by the pipeline.
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        let t = now();
        let nine =
            wire::encode(&grant(852_137_500), KEY.as_bytes(), PEER_ID, 9, t).expect("encodes");
        let ten =
            wire::encode(&grant(853_000_000), KEY.as_bytes(), PEER_ID, 10, t + 1).expect("encodes");
        let peer = UdpSocket::bind("127.0.0.1:0").expect("binds");
        peer.send_to(&ten, b_addr).expect("sent");
        assert_eq!(wait_for(&rx), grant(853_000_000));
        peer.send_to(&nine, b_addr).expect("retransmitted");
        assert_eq!(
            wait_for(&rx),
            grant(852_137_500),
            "the retransmission is new"
        );

        until(|| metrics.counter("link.grant_acks_tx").get() == 2);
        assert_eq!(metrics.counter("link.replay").get(), 0);
        assert_eq!(metrics.counter("link.rx").get(), 2);

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn two_nodes_never_share_a_sender_id() {
        let a = node(&[]);
        let b = node(&[]);
        assert_ne!(a.sender_id, b.sender_id);
    }

    #[test]
    fn an_older_peers_grant_is_routed_but_never_acked() {
        // A version-1 build cannot parse a `GrantAck`, so it is never sent one:
        // its grants route exactly as they always did.
        let metrics = Metrics::new();
        let b = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        let b_addr = b.local_addr().expect("bound");
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, threads) = b.spawn_receiver(stop.clone());

        let old = wire::encode_versioned(&grant(852_137_500), KEY.as_bytes(), None, 4, now(), 1)
            .expect("encodes as v1");
        let sender = UdpSocket::bind("127.0.0.1:0").expect("binds");
        sender.send_to(&old, b_addr).expect("sent");

        assert_eq!(wait_for(&rx), grant(852_137_500));
        assert_eq!(
            metrics.counter("link.grant_acks_tx").get(),
            0,
            "a v1 peer is never sent something it cannot parse"
        );

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_hello_gives_a_written_off_peer_another_chance() {
        let deaf = UdpSocket::bind("127.0.0.1:0").expect("binds");
        let deaf_addr = deaf.local_addr().expect("bound");
        let a = node(&[deaf_addr.to_string()]);
        let stop = Arc::new(AtomicBool::new(false));
        let (_rx, threads) = a.spawn_receiver(stop.clone());
        for i in 1..=UNACKED_BEFORE_SILENT {
            a.broadcast(&grant(852_137_500));
            until(|| stats_of(&a, deaf_addr).unacked == u64::from(i));
        }
        assert_eq!(stats_of(&a, deaf_addr).acks, Some(false));

        // The peer restarts on the new build and says hello from that host.
        let hello = LinkMsg::Hello {
            instance: "recorder-b".into(),
            version: u32::from(wire::PROTOCOL_VERSION),
            sites: Vec::new(),
        };
        let bytes = wire::encode(&hello, KEY.as_bytes(), PEER_ID, 1, now()).expect("encodes");
        deaf.send_to(&bytes, a.local_addr().expect("bound"))
            .expect("sent");

        until(|| stats_of(&a, deaf_addr).acks.is_none());

        stop.store(true, Ordering::Relaxed);
        join(threads);
    }

    #[test]
    fn a_node_with_no_peers_sends_nothing() {
        let metrics = Metrics::new();
        let a = LinkNode::bind("127.0.0.1:0", &[], KEY, metrics.clone()).expect("binds");
        a.broadcast(&grant(852_137_500));
        assert_eq!(metrics.counter("link.tx").get(), 0);
        assert_eq!(metrics.counter("link.dropped").get(), 0);
    }
}
