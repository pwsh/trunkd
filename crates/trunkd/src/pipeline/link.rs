// SPDX-License-Identifier: GPL-3.0-or-later
//! The recorder link, wired into the pipeline.
//!
//! [`trunkd_link`] is the protocol; this is where it meets the recorder. The
//! whole idea is that "two tuners in one process" and "two recorders on two
//! boxes" are the same mechanism: a front that follows a control channel
//! produces [`GrantMsg`]s, and the [`SiteCoordinator`] hands each to whichever
//! tuner can record it. The link simply widens the set of tuners the
//! coordinator can reach, and the set of control channels it hears.
//!
//! ```text
//!  control front ─▶ grant bus ─▶ coordinator ─┬─▶ local voice tuner
//!                                             └─▶ LinkHandle::publish ─▶ peers
//!
//!  peers ─▶ link rx thread ─▶ ingest ─▶ SiteCoordinator::route_remote ─▶ tuner
//! ```
//!
//! Three rules shape everything here:
//!
//! * **Nothing on a DSP thread waits on the network.** Publishing is one
//!   non-blocking `send_to`; a datagram that will not go is counted and
//!   forgotten. The grant a peer sends arrives on a bounded drop-oldest
//!   channel, exactly like every other queue in the recorder.
//! * **A peer can offer work, never demand it.** A remote grant is routed only
//!   to a tuner whose `remote_grants` says yes and whose bands cover it.
//! * **Two recordings beat none.** Deduplication is against this recorder's own
//!   grants only; `defer_to_claims` (off by default) is the switch for
//!   operators short of tuners rather than short of disk, because keyup already
//!   picks a best source among several copies of one call.
//!
//! What is published is the [`GrantMsg`] stream: grants this recorder is *not*
//! recording itself. A dedicated `control` front produces one for every grant
//! it sees, which is the topology the link is for; an `auto` front publishes
//! the ones outside its own bands. A call a local tuner takes is announced with
//! a [`LinkMsg::Claim`] instead, which is what tells a peer it is covered.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use trunkd_core::chan::BoundedReceiver;
use trunkd_core::{Metrics, Timestamp};
use trunkd_link::{GrantAckPolicy, GrantFlags, LinkMsg, LinkNode, SiteAd, SiteKey, TunerAd};
use trunkd_p25::event::CallKind;
use trunkd_p25::GrantInfo;

use crate::config::Config;
use crate::pipeline::coordinator::{GrantMsg, GrantPublisher, SiteCoordinator};
use crate::pipeline::{SiteSpec, ORIGIN_NONE};

/// How long a local grant suppresses an identical remote one.
///
/// A grant repeats on the control channel while a call runs, and two recorders
/// hearing the same site are a couple of hundred milliseconds apart at worst;
/// two seconds covers the skew without hiding a genuine second call on the same
/// frequency, which cannot start that soon anyway.
const DEDUPE_MS: i64 = 2_000;

/// How long a peer's claim counts as current, for `defer_to_claims` and for the
/// status report. Same reasoning as [`DEDUPE_MS`]: a claim is refreshed by the
/// peer's own traffic, and a stale one must not park a frequency forever.
const CLAIM_TTL_MS: i64 = 2_000;

/// A peer is alive if heard within this many hello intervals.
const ALIVE_INTERVALS: u64 = 3;

/// How long a silent peer is remembered.
///
/// A peer sends from an ephemeral port (see `LinkNode`), so a restart arrives
/// as a *new* address rather than the old one coming back. Without an expiry
/// the peer table would accumulate one dead entry per peer restart; ten
/// minutes is far beyond any hello interval an operator would set, so a peer
/// that is merely quiet is never forgotten while it is up.
const PEER_TTL_MS: i64 = 10 * 60 * 1_000;

/// Cap on remembered claims per peer, so a peer that never sends `Release`
/// (a crash, a lost datagram) cannot grow this recorder's memory.
const MAX_CLAIMS_PER_PEER: usize = 64;

/// Cap on remembered local grants for deduplication.
const MAX_RECENT_LOCAL: usize = 256;

/// Cap on the peer table.
///
/// Entries are keyed by source address, and a restarted peer arrives from a
/// new ephemeral port, so the table is bounded by more than [`PEER_TTL_MS`]:
/// past this many, the peer heard from longest ago is forgotten. Far more
/// recorders than any deployment shares a key with.
const MAX_PEERS: usize = 64;

/// Oldest a peer's grant may be, by its own `at_wall_ms`, and still be routed.
///
/// Voice follows a grant by 100–300 ms and the link's retries are spent within
/// a second, so a grant this old describes a frequency whose call has almost
/// certainly moved on — following it sends a tuner to a dead channel. The
/// frame itself already had to pass the link's clock window; this is the
/// tighter, grant-specific bound, with room for two recorders' clocks (and
/// stream clocks) disagreeing by a few seconds.
const MAX_REMOTE_GRANT_AGE_MS: i64 = 10_000;

/// One tuner this recorder runs, as the link advertises it.
#[derive(Debug, Clone)]
pub(crate) struct LinkTuner {
    /// The tuner key (`[[device]] id`).
    pub(crate) key: String,
    /// Role name as configured.
    pub(crate) role: String,
    /// Centre frequency, 0 when it has none.
    pub(crate) center_hz: u64,
    /// Capture span in Hz.
    pub(crate) span_hz: u64,
    /// Inclusive downlink ranges it records.
    pub(crate) bands: Vec<(u64, u64)>,
    /// Indices into [`crate::pipeline::Shared::sites`] this tuner serves.
    pub(crate) sites: Vec<usize>,
}

/// What one peer has told us about itself.
#[derive(Debug, Clone)]
struct PeerState {
    instance: String,
    last_seen_ms: i64,
    /// When this peer last said hello, as distinct from last said *anything*.
    /// The hello is the periodic keepalive, so it is the one that answers "is
    /// the other end still there" rather than "was there traffic". Zero until
    /// the first hello: a peer discovered by a grant has not introduced itself.
    last_hello_ms: i64,
    sites: Vec<SiteAd>,
    grants_rx: u64,
    claims: Vec<RemoteClaim>,
}

/// A call a peer says it is recording.
#[derive(Debug, Clone)]
struct RemoteClaim {
    site: SiteKey,
    frequency_hz: u64,
    talkgroup: u16,
    tuner: String,
    at_wall_ms: i64,
}

/// A grant this recorder published, kept just long enough to recognise a peer
/// echoing the same one back.
#[derive(Debug, Clone, Copy)]
struct LocalGrant {
    site: usize,
    frequency_hz: u64,
    talkgroup: u16,
    at_wall_ms: i64,
}

/// Everything the link mutates, behind one lock.
///
/// One mutex rather than several: it is taken a handful of times a second on
/// threads that are allowed to wait, never on a DSP thread, and one lock is one
/// fewer ordering to reason about.
#[derive(Debug, Default)]
struct LinkState {
    peers: HashMap<SocketAddr, PeerState>,
    recent_local: Vec<LocalGrant>,
    /// Frequency and talkgroup of each open local call, so its end can be
    /// released to peers by call id alone.
    open_calls: HashMap<(usize, u64), (u64, u16)>,
    /// The last control state announced per site, so `ControlState` goes out on
    /// a transition rather than every cycle.
    announced: HashMap<usize, (u64, bool)>,
}

/// The live link: the socket, what the peers have said, and what we have said.
#[derive(Debug)]
pub(crate) struct LinkHandle {
    node: Arc<LinkNode>,
    instance: String,
    /// `(system, site)` labels per site index, for building and matching keys.
    sites: Vec<SiteSpec>,
    tuners: Vec<LinkTuner>,
    metrics: Metrics,
    publish_grants: bool,
    accept_remote_grants: bool,
    defer_to_claims: bool,
    hello_interval_s: u64,
    bind: String,
    grant_seq: AtomicU64,
    state: Mutex<LinkState>,
}

/// The threads the link runs, held for an orderly shutdown.
pub(crate) struct LinkThreads {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl LinkThreads {
    /// Stop the link threads and wait for them.
    pub(crate) fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

impl LinkHandle {
    /// Build the link from one `[link]` block and a prepared tuner list, or
    /// `None` when it is disabled (or cannot bind).
    ///
    /// A bind failure is logged and swallowed: a recorder whose link cannot
    /// come up must still record, exactly as it did before the link existed.
    ///
    /// The form a rebuild uses: the tuner advertisements and the site table are
    /// properties of the TOML devices, which keyup's link settings never touch,
    /// so a rebuild reuses the ones computed at startup and only the `[link]`
    /// block moves.
    pub(crate) fn start_from(
        link: &crate::config::Link,
        sites: &[SiteSpec],
        tuners: &[LinkTuner],
        metrics: Metrics,
        instance: String,
    ) -> Option<Arc<LinkHandle>> {
        if !link.enabled {
            return None;
        }
        if link.key.trim().len() < trunkd_link::MIN_KEY_LEN {
            tracing::error!(
                min = trunkd_link::MIN_KEY_LEN,
                "recorder link has no usable key; running without it"
            );
            return None;
        }
        let node = match LinkNode::bind_with(
            &link.bind,
            &link.peers,
            &link.key,
            metrics.clone(),
            GrantAckPolicy::new(link.grant_retries, link.grant_retry_ms),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, bind = %link.bind, "recorder link could not start; running without it");
                return None;
            }
        };
        tracing::info!(
            bind = %link.bind,
            peers = link.peers.len(),
            instance = %instance,
            "recorder link up"
        );
        Some(Arc::new(LinkHandle {
            node,
            instance,
            sites: sites.to_vec(),
            tuners: tuners.to_vec(),
            metrics,
            publish_grants: link.publish_grants,
            accept_remote_grants: link.accept_remote_grants,
            defer_to_claims: link.defer_to_claims,
            hello_interval_s: link.hello_interval_s.max(1),
            bind: link.bind.clone(),
            grant_seq: AtomicU64::new(0),
            state: Mutex::new(LinkState::default()),
        }))
    }

    /// Start the receive, ingest and hello threads.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        coord: Option<Arc<SiteCoordinator>>,
        stop: Arc<AtomicBool>,
    ) -> LinkThreads {
        let (rx, mut threads) = self.node.spawn_receiver(stop.clone());
        let ingest = {
            let me = self.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("trunkd-link".into())
                .spawn(move || me.ingest_loop(rx, coord, stop))
                .expect("spawn link ingest thread")
        };
        let hello = {
            let me = self.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("trunkd-link-hello".into())
                .spawn(move || me.hello_loop(stop))
                .expect("spawn link hello thread")
        };
        threads.push(ingest);
        threads.push(hello);
        LinkThreads { stop, threads }
    }

    /// The site key for a local site index.
    fn site_key(&self, site: usize, nac: u16) -> Option<SiteKey> {
        let spec = self.sites.get(site)?;
        Some(SiteKey {
            system: spec.system_label.clone(),
            site: spec.site_label.clone(),
            nac,
        })
    }

    /// Current NAC for a site, from the same gauge the status POST reads.
    fn nac(&self, site: usize) -> u16 {
        self.sites
            .get(site)
            .map(|s| {
                self.metrics
                    .gauge(&format!("{}.nac", s.metric_prefix))
                    .get() as u16
            })
            .unwrap_or(0)
    }

    /// Which local site a peer's key names, matched on labels first and NAC
    /// second.
    ///
    /// Labels are how two operators who agree on names find each other; the NAC
    /// is how two who do not still do, because the site itself broadcasts it.
    fn match_site(&self, key: &SiteKey) -> Option<usize> {
        if let Some(i) = self.sites.iter().position(|s| {
            s.system_label.eq_ignore_ascii_case(&key.system)
                && s.site_label.eq_ignore_ascii_case(&key.site)
        }) {
            return Some(i);
        }
        if key.nac == 0 {
            return None;
        }
        (0..self.sites.len()).find(|&i| self.nac(i) == key.nac)
    }

    /// Announce a call a local tuner has started recording.
    pub(crate) fn on_call_started(
        &self,
        site: usize,
        call_id: u64,
        frequency_hz: u64,
        talkgroup: u16,
        tuner: &str,
    ) {
        let Some(key) = self.site_key(site, self.nac(site)) else {
            return;
        };
        if let Ok(mut st) = self.state.lock() {
            st.open_calls
                .insert((site, call_id), (frequency_hz, talkgroup));
        }
        self.node.broadcast(&LinkMsg::Claim {
            site: key,
            frequency_hz,
            talkgroup,
            tuner: tuner.to_owned(),
            at_wall_ms: Timestamp::now().unix_millis(),
        });
        self.metrics.counter("link.claims_tx").inc();
    }

    /// Announce that a local call has ended.
    pub(crate) fn on_call_ended(&self, site: usize, call_id: u64) {
        let open = self
            .state
            .lock()
            .ok()
            .and_then(|mut st| st.open_calls.remove(&(site, call_id)));
        let (Some((frequency_hz, _)), Some(key)) = (open, self.site_key(site, self.nac(site)))
        else {
            return;
        };
        self.node.broadcast(&LinkMsg::Release {
            site: key,
            frequency_hz,
            at_wall_ms: Timestamp::now().unix_millis(),
        });
        self.metrics.counter("link.releases_tx").inc();
    }

    /// Build this recorder's `Hello`, trimmed to one datagram.
    fn hello(&self) -> LinkMsg {
        let sites: Vec<SiteAd> = self
            .sites
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                let p = &spec.metric_prefix;
                let control_hz = self.metrics.gauge(&format!("{p}.control_hz")).get();
                SiteAd {
                    system: spec.system_label.clone(),
                    site: spec.site_label.clone(),
                    nac: self.nac(i),
                    control_hz,
                    locked: control_hz != 0,
                    tsbk_rate: gauge_f64(&self.metrics, &format!("{p}.tsbk_rate")) as u16,
                    tuners: self
                        .tuners
                        .iter()
                        .filter(|t| t.sites.contains(&i))
                        .map(|t| TunerAd {
                            key: t.key.clone(),
                            role: t.role.clone(),
                            center_hz: t.center_hz,
                            span_hz: t.span_hz,
                            bands: t.bands.clone(),
                        })
                        .collect(),
                }
            })
            .collect();
        let mut msg = LinkMsg::Hello {
            instance: self.instance.clone(),
            version: u32::from(trunkd_link::wire::PROTOCOL_VERSION),
            sites,
        };
        if trunkd_link::wire::fit_hello(&mut msg) {
            self.metrics.counter("link.hello_truncated").inc();
        }
        msg
    }

    /// Say hello on the configured cadence, and announce control-lock changes
    /// as they happen rather than waiting for the next hello.
    fn hello_loop(&self, stop: Arc<AtomicBool>) {
        let interval = Duration::from_secs(self.hello_interval_s);
        let mut next = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if std::time::Instant::now() >= next {
                next = std::time::Instant::now() + interval;
                self.node.broadcast(&self.hello());
            }
            self.announce_control_changes();
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Send a `ControlState` for every site whose lock or frequency changed.
    fn announce_control_changes(&self) {
        for (i, spec) in self.sites.iter().enumerate() {
            let p = &spec.metric_prefix;
            let control_hz = self.metrics.gauge(&format!("{p}.control_hz")).get();
            let locked = control_hz != 0;
            let changed = match self.state.lock() {
                Ok(mut st) => {
                    st.announced.insert(i, (control_hz, locked)) != Some((control_hz, locked))
                }
                Err(_) => false,
            };
            if !changed {
                continue;
            }
            let Some(key) = self.site_key(i, self.nac(i)) else {
                continue;
            };
            self.node.broadcast(&LinkMsg::ControlState {
                site: key,
                control_hz,
                locked,
                tsbk_rate: gauge_f64(&self.metrics, &format!("{p}.tsbk_rate")) as u16,
            });
        }
    }

    /// Drain the receive channel until asked to stop.
    fn ingest_loop(
        &self,
        rx: BoundedReceiver<(SocketAddr, LinkMsg)>,
        coord: Option<Arc<SiteCoordinator>>,
        stop: Arc<AtomicBool>,
    ) {
        // Blocks until a message arrives and wakes only to look at `stop`; a
        // 2 ms poll was ~500 wakeups a second (review 2026-09-22 §3.4).
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_wait(Duration::from_millis(100)) {
                Ok((from, msg)) => self.ingest(from, msg, coord.as_deref()),
                Err(trunkd_core::chan::RecvTimeoutError::Timeout) => {}
                Err(trunkd_core::chan::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Handle one authenticated message from `from`.
    pub(crate) fn ingest(&self, from: SocketAddr, msg: LinkMsg, coord: Option<&SiteCoordinator>) {
        let now = Timestamp::now().unix_millis();
        match msg {
            LinkMsg::Hello {
                instance, sites, ..
            } => {
                self.with_peer(from, now, |p| {
                    p.instance = instance;
                    p.sites = sites;
                    p.last_hello_ms = now;
                });
            }
            LinkMsg::ControlState {
                site,
                control_hz,
                locked,
                tsbk_rate,
            } => {
                self.with_peer(from, now, |p| {
                    if let Some(ad) = p
                        .sites
                        .iter_mut()
                        .find(|a| a.system == site.system && a.site == site.site)
                    {
                        ad.control_hz = control_hz;
                        ad.locked = locked;
                        ad.tsbk_rate = tsbk_rate;
                        ad.nac = site.nac;
                    }
                });
            }
            LinkMsg::Claim {
                site,
                frequency_hz,
                talkgroup,
                tuner,
                at_wall_ms,
            } => {
                self.metrics.counter("link.claims_rx").inc();
                self.with_peer(from, now, |p| {
                    p.claims
                        .retain(|c| !(c.frequency_hz == frequency_hz && c.site.site == site.site));
                    p.claims.push(RemoteClaim {
                        site,
                        frequency_hz,
                        talkgroup,
                        tuner,
                        at_wall_ms,
                    });
                    if p.claims.len() > MAX_CLAIMS_PER_PEER {
                        p.claims.remove(0);
                    }
                });
            }
            LinkMsg::Release {
                site, frequency_hz, ..
            } => {
                self.metrics.counter("link.releases_rx").inc();
                self.with_peer(from, now, |p| {
                    p.claims
                        .retain(|c| !(c.frequency_hz == frequency_hz && c.site.site == site.site));
                });
            }
            // Acknowledgements never reach the pipeline: `LinkNode` consumes
            // them on the receive thread and hands them to its sender.
            LinkMsg::GrantAck { .. } => {}
            LinkMsg::Grant {
                site,
                frequency_hz,
                talkgroup,
                source,
                flags,
                at_wall_ms,
                ..
            } => {
                self.with_peer(from, now, |p| p.grants_rx += 1);
                self.ingest_grant(
                    site,
                    frequency_hz,
                    talkgroup,
                    source,
                    flags,
                    at_wall_ms,
                    now,
                    coord,
                );
            }
        }
    }

    /// Route a peer's grant, or count the reason it was not routed.
    #[allow(clippy::too_many_arguments)]
    fn ingest_grant(
        &self,
        site: SiteKey,
        frequency_hz: u64,
        talkgroup: u16,
        source: u32,
        flags: GrantFlags,
        at_wall_ms: i64,
        now: i64,
        coord: Option<&SiteCoordinator>,
    ) {
        if !self.accept_remote_grants {
            self.metrics.counter("link.grant_ignored").inc();
            return;
        }
        let age_ms = now.saturating_sub(at_wall_ms);
        if age_ms > MAX_REMOTE_GRANT_AGE_MS {
            let n = self.metrics.counter("link.grant_stale").add(1) + 1;
            if n.is_power_of_two() {
                tracing::warn!(
                    %site,
                    frequency_hz,
                    talkgroup,
                    age_ms,
                    refused = n,
                    "a peer's grant is too old to follow (a delayed frame, or a peer \
                     whose clock is behind this one's); not routing it"
                );
            }
            return;
        }
        let Some(index) = self.match_site(&site) else {
            // A peer serving a site this recorder does not: normal on a shared
            // key, and worth counting so an operator can see a misconfigured
            // pairing rather than wondering why nothing routes.
            self.metrics.counter("link.grant_site_unknown").inc();
            return;
        };
        if self.is_duplicate(index, frequency_hz, talkgroup, now) {
            self.metrics.counter("link.grant_dup").inc();
            return;
        }
        if self.defer_to_claims && self.peer_claims(&site, frequency_hz, now) {
            self.metrics.counter("link.grant_deferred").inc();
            return;
        }
        let Some(coord) = coord else {
            self.metrics.counter("link.grant_unroutable").inc();
            return;
        };
        self.metrics.counter("link.grants_rx").inc();
        coord.route_remote(&GrantMsg {
            site: index,
            origin_device: ORIGIN_NONE,
            frequency_hz,
            info: GrantInfo {
                kind: if flags.has(GrantFlags::PRIVATE) {
                    CallKind::UnitToUnit
                } else {
                    CallKind::Group
                },
                talkgroup,
                target: u32::from(talkgroup),
                source,
            },
            at_wall_ms,
        });
    }

    /// Whether this recorder published the same grant within [`DEDUPE_MS`].
    fn is_duplicate(&self, site: usize, frequency_hz: u64, talkgroup: u16, now: i64) -> bool {
        let Ok(mut st) = self.state.lock() else {
            return false;
        };
        st.recent_local
            .retain(|g| now.saturating_sub(g.at_wall_ms) <= DEDUPE_MS);
        st.recent_local
            .iter()
            .any(|g| g.site == site && g.frequency_hz == frequency_hz && g.talkgroup == talkgroup)
    }

    /// Whether a peer has a current claim on this frequency.
    fn peer_claims(&self, site: &SiteKey, frequency_hz: u64, now: i64) -> bool {
        let Ok(st) = self.state.lock() else {
            return false;
        };
        st.peers.values().any(|p| {
            p.claims.iter().any(|c| {
                c.frequency_hz == frequency_hz
                    && c.site.site.eq_ignore_ascii_case(&site.site)
                    && now.saturating_sub(c.at_wall_ms) <= CLAIM_TTL_MS
            })
        })
    }

    /// Update (or create) a peer entry, stamping it as heard from.
    fn with_peer(&self, from: SocketAddr, now: i64, f: impl FnOnce(&mut PeerState)) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        // Forget peers that stopped talking long ago, so a peer restarting on a
        // new source port replaces its predecessor instead of joining it.
        if !st.peers.contains_key(&from) {
            st.peers
                .retain(|_, p| now.saturating_sub(p.last_seen_ms) <= PEER_TTL_MS);
            // Still full of live-looking entries: make room by forgetting the
            // one heard from longest ago, so the table (which the status body
            // lists in full) stays bounded whatever arrives.
            while st.peers.len() >= MAX_PEERS {
                let Some(oldest) = st
                    .peers
                    .iter()
                    .min_by_key(|(_, p)| p.last_seen_ms)
                    .map(|(a, _)| *a)
                else {
                    break;
                };
                st.peers.remove(&oldest);
            }
        }
        let peer = st.peers.entry(from).or_insert_with(|| PeerState {
            instance: String::new(),
            last_seen_ms: now,
            last_hello_ms: 0,
            sites: Vec::new(),
            grants_rx: 0,
            claims: Vec::new(),
        });
        peer.last_seen_ms = now;
        f(peer);
    }

    /// The `link` block of the status POST and the panel's `/api/status`.
    ///
    /// Built here rather than in `control.rs` so the status writer keeps
    /// knowing nothing about the link beyond "ask it for its JSON".
    pub(crate) fn status_json(&self) -> Value {
        let now = Timestamp::now().unix_millis();
        let alive_ms = (self.hello_interval_s * ALIVE_INTERVALS * 1_000) as i64;
        let peers: Vec<Value> = match self.state.lock() {
            Ok(st) => {
                let mut v: Vec<Value> = st
                    .peers
                    .iter()
                    .map(|(addr, p)| {
                        let age = now.saturating_sub(p.last_seen_ms);
                        json!({
                            "instance": p.instance,
                            "addr": addr.to_string(),
                            "alive": age <= alive_ms,
                            "last_seen_s": age / 1_000,
                            // `null`, not 0: a peer that has sent grants but
                            // never a hello has no age to report, and 0 would
                            // read as "just now".
                            "last_hello_s": match p.last_hello_ms {
                                0 => Value::Null,
                                at => json!(now.saturating_sub(at) / 1_000),
                            },
                            "sites": p.sites.iter().map(site_ad_json).collect::<Vec<_>>(),
                            "grants_rx": p.grants_rx,
                            "grants_tx": self.metrics.counter("link.grants_tx").get(),
                            "claims": p.claims.iter()
                                .filter(|c| now.saturating_sub(c.at_wall_ms) <= CLAIM_TTL_MS)
                                .map(|c| json!({
                                    "site": format!("{}/{}", c.site.system, c.site.site),
                                    "frequency_hz": c.frequency_hz,
                                    "talkgroup": c.talkgroup,
                                    "tuner": c.tuner,
                                    "age_s": now.saturating_sub(c.at_wall_ms) / 1_000,
                                }))
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                // Stable order: a status body that reshuffles every cycle is
                // unreadable in a diff and unusable in a test.
                v.sort_by_key(|p| p["addr"].as_str().unwrap_or_default().to_owned());
                attach_grant_stats(&mut v, self.node.grant_stats());
                v
            }
            Err(_) => Vec::new(),
        };
        let counter = |n: &str| self.metrics.counter(n).get();
        json!({
            "enabled": true,
            "instance": self.instance,
            "bind": self.bind,
            "peers": peers,
            "counters": {
                "rx": counter("link.rx"),
                "tx": counter("link.tx"),
                "dropped": counter("link.dropped"),
                "auth_failed": counter("link.auth_failed"),
                "replay": counter("link.replay"),
                "stale": counter("link.stale"),
                "grant_stale": counter("link.grant_stale"),
                "grants_tx": counter("link.grants_tx"),
                "grants_rx": counter("link.grants_rx"),
                "grant_dup": counter("link.grant_dup"),
                "grant_deferred": counter("link.grant_deferred"),
                "grant_site_unknown": counter("link.grant_site_unknown"),
                "claims_tx": counter("link.claims_tx"),
                "claims_rx": counter("link.claims_rx"),
                "grants_retried": counter("link.grants_retried"),
                "grants_unacked": counter("link.grants_unacked"),
                "grant_acks_tx": counter("link.grant_acks_tx"),
                "grant_acks_rx": counter("link.grant_acks_rx"),
            },
        })
    }
}

/// The `link` block for a recorder with no link: one flag, so a reader can tell
/// "off" from "this build has never heard of the link".
pub(crate) fn disabled_status_json() -> Value {
    json!({ "enabled": false })
}

impl GrantPublisher for LinkHandle {
    fn publish(&self, grant: &GrantMsg) {
        if !self.publish_grants {
            return;
        }
        let Some(site) = self.site_key(grant.site, self.nac(grant.site)) else {
            return;
        };
        if let Ok(mut st) = self.state.lock() {
            st.recent_local.push(LocalGrant {
                site: grant.site,
                frequency_hz: grant.frequency_hz,
                talkgroup: grant.info.talkgroup,
                at_wall_ms: grant.at_wall_ms,
            });
            if st.recent_local.len() > MAX_RECENT_LOCAL {
                st.recent_local.remove(0);
            }
        }
        let flags = GrantFlags::default().with(
            GrantFlags::PRIVATE,
            !matches!(grant.info.kind, CallKind::Group),
        );
        self.node.broadcast(&LinkMsg::Grant {
            site,
            frequency_hz: grant.frequency_hz,
            talkgroup: grant.info.talkgroup,
            source: grant.info.source,
            flags,
            at_wall_ms: grant.at_wall_ms,
            seq: self.grant_seq.fetch_add(1, Ordering::Relaxed),
        });
        self.metrics.counter("link.grants_tx").inc();
    }
}

/// Fold each configured peer's grant ack counters into the peer it belongs to.
///
/// The peer table is keyed on the address a peer's datagrams *come from*, which
/// is its ephemeral send port; the grant counters are keyed on the address this
/// recorder *sends to*, which is its configured one. The two are the same host
/// and never the same port, so they are matched by host, each set of counters
/// claimed by at most one peer entry. A peer that has never been sent a grant
/// (or one whose host does not match anything configured) reports zeros and a
/// null `acks_grants`, which is exactly what "nothing is known yet" means.
fn attach_grant_stats(peers: &mut [Value], stats: Vec<(SocketAddr, trunkd_link::GrantStats)>) {
    let mut unclaimed = stats;
    for peer in peers.iter_mut() {
        let ip = peer["addr"]
            .as_str()
            .and_then(|a| a.parse::<SocketAddr>().ok())
            .map(|a| a.ip());
        let s = ip
            .and_then(|ip| unclaimed.iter().position(|(p, _)| p.ip() == ip))
            .map(|i| unclaimed.remove(i).1)
            .unwrap_or_default();
        peer["grants_sent"] = json!(s.sent);
        peer["grants_acked"] = json!(s.acked);
        peer["grants_retried"] = json!(s.retried);
        peer["grants_unacked"] = json!(s.unacked);
        peer["acks_grants"] = json!(s.acks);
    }
}

/// A peer's site advertisement, as JSON.
fn site_ad_json(a: &SiteAd) -> Value {
    json!({
        "system": a.system,
        "site": a.site,
        "nac": a.nac,
        "control_hz": a.control_hz,
        "locked": a.locked,
        "tsbk_rate": a.tsbk_rate,
        "tuners": a.tuners.iter().map(|t| json!({
            "key": t.key,
            "role": t.role,
            "center_hz": t.center_hz,
            "span_hz": t.span_hz,
            "bands": t.bands.iter().map(|(lo, hi)| json!([lo, hi])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// Read a gauge that holds an `f64` bit pattern (the metrics registry stores
/// every value as `u64`).
fn gauge_f64(metrics: &Metrics, name: &str) -> f64 {
    let raw = metrics.gauge(name).get();
    if raw == 0 {
        0.0
    } else {
        f64::from_bits(raw)
    }
}

/// The tuner advertisements for this config, in config order.
fn link_tuners(cfg: &Config, sites: &[SiteSpec]) -> Vec<LinkTuner> {
    // Site index here is the position among *enabled* sites, which is what
    // `Shared::sites` and every message's `site` field use.
    let enabled: Vec<(usize, &crate::config::Site)> =
        cfg.site.iter().filter(|s| s.enabled).enumerate().collect();
    cfg.device
        .iter()
        .map(|d| LinkTuner {
            key: d.id.clone(),
            role: role_name(d.role).to_owned(),
            center_hz: d.center_frequency.unwrap_or(0),
            span_hz: u64::from(d.sample_rate),
            bands: crate::config::band_ranges(d.center_frequency, d.sample_rate, &d.bands),
            sites: enabled
                .iter()
                .filter(|(i, s)| s.device_ids().contains(&d.id.as_str()) && *i < sites.len())
                .map(|(i, _)| *i)
                .collect(),
        })
        .collect()
}

/// The wire name of a device role.
fn role_name(role: crate::config::DeviceRole) -> &'static str {
    match role {
        crate::config::DeviceRole::Auto => "auto",
        crate::config::DeviceRole::Control => "control",
        crate::config::DeviceRole::Voice => "voice",
        crate::config::DeviceRole::Data => "data",
    }
}

impl LinkHandle {
    /// The socket address actually bound, which differs from the configured
    /// `bind` when that named port 0.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        self.node.local_addr().ok()
    }
}

// ---------------------------------------------------------------------------
// The swappable slot
// ---------------------------------------------------------------------------

/// The live link, behind one swappable handle.
///
/// Everything that talks to the link — the call thread, the coordinator, the
/// control thread and the panel — holds this rather than a [`LinkHandle`], so
/// keyup can change the `[link]` settings of a running recorder: the slot stops
/// the old sockets and threads, builds a new handle from the new config, and
/// swaps it in. No tuner is touched and no call is interrupted; a call that was
/// claimed over the old link simply stops being claimed over the new one.
///
/// The slot exists even when the link is off (`handle()` is then `None`), which
/// is what lets a box whose TOML has no `[link]` at all be given one by keyup.
pub(crate) struct LinkSlot {
    /// The site table and tuner advertisements, fixed for the process: they
    /// come from the TOML devices, which the link settings never change.
    sites: Vec<SiteSpec>,
    tuners: Vec<LinkTuner>,
    metrics: Metrics,
    instance: String,
    /// The running link, or `None` when it is off (or failed to bind).
    handle: std::sync::RwLock<Option<Arc<LinkHandle>>>,
    /// The threads of the running link, taken and shut down on a rebuild.
    threads: Mutex<Option<LinkThreads>>,
    /// The coordinator remote grants are routed into, remembered so a rebuilt
    /// link is wired to the same one.
    coord: Mutex<Option<Arc<SiteCoordinator>>>,
    /// The effective `[link]` config: the TOML block, with keyup's managed
    /// fields laid over it.
    config: Mutex<crate::config::Link>,
    /// Whether [`LinkSlot::start`] has run. Before it has, a config change is
    /// only recorded — the first build then uses it, which is how a prefetched
    /// desired state reaches the link's *first* bind.
    started: AtomicBool,
}

impl LinkSlot {
    /// A slot seeded from the TOML `[link]` block. Nothing is bound until
    /// [`LinkSlot::start`].
    pub(crate) fn new(
        cfg: &Config,
        sites: &[SiteSpec],
        metrics: Metrics,
        instance: String,
    ) -> Arc<LinkSlot> {
        Arc::new(LinkSlot {
            sites: sites.to_vec(),
            tuners: link_tuners(cfg, sites),
            metrics,
            instance,
            handle: std::sync::RwLock::new(None),
            threads: Mutex::new(None),
            coord: Mutex::new(None),
            config: Mutex::new(cfg.link.clone()),
            started: AtomicBool::new(false),
        })
    }

    /// A slot that is off and has no config to turn on: the shape a test (or a
    /// fixture with no `[link]` at all) needs.
    #[cfg(test)]
    pub(crate) fn disabled() -> Arc<LinkSlot> {
        Arc::new(LinkSlot {
            sites: Vec::new(),
            tuners: Vec::new(),
            metrics: Metrics::new(),
            instance: String::new(),
            handle: std::sync::RwLock::new(None),
            threads: Mutex::new(None),
            coord: Mutex::new(None),
            config: Mutex::new(crate::config::Link::default()),
            started: AtomicBool::new(false),
        })
    }

    /// The running link, if there is one.
    pub(crate) fn handle(&self) -> Option<Arc<LinkHandle>> {
        self.handle
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The effective `[link]` config.
    pub(crate) fn config(&self) -> crate::config::Link {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Bind and spawn from the effective config, and remember `coord` for any
    /// later rebuild. A no-op when the link is disabled.
    pub(crate) fn start(&self, coord: Option<Arc<SiteCoordinator>>) {
        *self.coord.lock().unwrap_or_else(|p| p.into_inner()) = coord;
        self.started.store(true, Ordering::Relaxed);
        self.build();
    }

    /// Replace the effective config, rebuilding the link when it has started
    /// and the config actually moved. Returns whether anything changed.
    pub(crate) fn set_config(&self, next: crate::config::Link) -> bool {
        {
            let mut cur = self.config.lock().unwrap_or_else(|p| p.into_inner());
            if *cur == next {
                return false;
            }
            *cur = next;
        }
        if self.started.load(Ordering::Relaxed) {
            self.rebuild();
        }
        true
    }

    /// Stop what is running and build again from the effective config.
    ///
    /// The old threads are joined *before* the new socket is bound, so the two
    /// never hold the same UDP port — a rebuild that only changes `peers` would
    /// otherwise fail to bind and leave the recorder linkless.
    fn rebuild(&self) {
        self.stop_threads();
        *self.handle.write().unwrap_or_else(|p| p.into_inner()) = None;
        self.build();
        tracing::info!(
            enabled = self.config().enabled,
            "recorder link rebuilt from keyup's settings"
        );
    }

    /// Build and spawn from the effective config (nothing running).
    fn build(&self) {
        let cfg = self.config();
        let Some(handle) = LinkHandle::start_from(
            &cfg,
            &self.sites,
            &self.tuners,
            self.metrics.clone(),
            self.instance.clone(),
        ) else {
            return;
        };
        let coord = self.coord.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let threads = handle.spawn(coord, Arc::new(AtomicBool::new(false)));
        *self.threads.lock().unwrap_or_else(|p| p.into_inner()) = Some(threads);
        *self.handle.write().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    }

    fn stop_threads(&self) {
        let threads = self
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(t) = threads {
            t.shutdown();
        }
    }

    /// Stop the link for good (recorder teardown).
    pub(crate) fn shutdown(&self) {
        self.stop_threads();
        *self.handle.write().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// The `link` block of the status POST: the running link's own JSON, or the
    /// disabled marker.
    pub(crate) fn status_json(&self) -> Value {
        match self.handle() {
            Some(h) => h.status_json(),
            None => disabled_status_json(),
        }
    }

    /// Announce a call a local tuner started, when the link is up.
    pub(crate) fn on_call_started(
        &self,
        site: usize,
        call_id: u64,
        frequency_hz: u64,
        talkgroup: u16,
        tuner: &str,
    ) {
        if let Some(h) = self.handle() {
            h.on_call_started(site, call_id, frequency_hz, talkgroup, tuner);
        }
    }

    /// Release a call a local tuner finished, when the link is up.
    pub(crate) fn on_call_ended(&self, site: usize, call_id: u64) {
        if let Some(h) = self.handle() {
            h.on_call_ended(site, call_id);
        }
    }

    /// How many local calls the running link still holds a claim for.
    #[cfg(test)]
    pub(crate) fn open_claims(&self) -> usize {
        self.handle().map_or(0, |h| {
            h.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .open_calls
                .len()
        })
    }
}

impl GrantPublisher for LinkSlot {
    fn publish(&self, grant: &GrantMsg) {
        if let Some(h) = self.handle() {
            h.publish(grant);
        }
    }
}

/// A short, non-reversible fingerprint of a link key: the first 8 hex of its
/// SHA-256.
///
/// Echoed to keyup so an operator can see that two peers hold the *same* key
/// without the key itself ever leaving the box. `None` for an unset key.
pub(crate) fn key_fingerprint(key: &str) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some(
        crate::pipeline::eventarchive::sha256_hex(key.as_bytes())
            .chars()
            .take(8)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Device, DeviceRole, Link, Site};
    use crate::pipeline::coordinator::{RouteState, TrafficCmd, TunerRoute};
    use trunkd_core::chan::{self, BoundedReceiver, BoundedSender};

    const KEY: &str = "a-sixteen-byte-key-or-longer";

    fn site_spec(system: &str, site: &str, i: usize) -> SiteSpec {
        SiteSpec {
            system_label: system.to_owned(),
            site_label: site.to_owned(),
            control_frequencies: vec![851_050_000],
            metric_prefix: format!("site.{i}"),
            display: format!("{system}/{site}"),
        }
    }

    /// A config with one site and one tuner of `role`, and the link on.
    fn config(
        role: DeviceRole,
        bind: &str,
        peers: &[String],
        remote_grants: Option<bool>,
    ) -> Config {
        let mut cfg = Config {
            link: Link {
                enabled: true,
                bind: bind.to_owned(),
                peers: peers.to_vec(),
                key: KEY.to_owned(),
                ..Link::default()
            },
            ..Config::default()
        };
        let mut d = Device {
            id: "t0".to_owned(),
            ..serde_json::from_str::<Device>(
                r#"{"id":"t0","sample_rate":8000000,"center_frequency":854000000}"#,
            )
            .expect("a device with only its required fields")
        };
        d.role = role;
        d.remote_grants = remote_grants;
        cfg.device = vec![d];
        cfg.site = vec![Site {
            system: "example".to_owned(),
            site: "central".to_owned(),
            ..serde_json::from_str::<Site>(
                r#"{"system":"example","site":"central","device":"t0","control_frequencies":[851050000]}"#,
            )
            .expect("a site with only its required fields")
        }];
        cfg
    }

    fn handle(cfg: &Config, metrics: Metrics) -> Arc<LinkHandle> {
        let sites = [site_spec("example", "central", 0)];
        LinkHandle::start_from(
            &cfg.link,
            &sites,
            &link_tuners(cfg, &sites),
            metrics,
            "rec-a".to_owned(),
        )
        .expect("the link starts")
    }

    /// A coordinator with one tuner covering the whole 800 MHz band.
    fn coordinator(
        accepts_remote: bool,
        metrics: Metrics,
    ) -> (Arc<SiteCoordinator>, BoundedReceiver<TrafficCmd>) {
        let (tx, rx): (BoundedSender<TrafficCmd>, _) = chan::bounded(16);
        let route = TunerRoute {
            device_index: 0,
            state: RouteState::new(true, vec![(851_000_000, 866_000_000)], accepts_remote),
            tx: Arc::new(tx),
        };
        (
            Arc::new(SiteCoordinator::new(vec![vec![route]], metrics)),
            rx,
        )
    }

    fn grant_msg(freq: u64, talkgroup: u16) -> GrantMsg {
        GrantMsg {
            site: 0,
            origin_device: 0,
            frequency_hz: freq,
            info: GrantInfo {
                kind: CallKind::Group,
                talkgroup,
                target: u32::from(talkgroup),
                source: 99,
            },
            at_wall_ms: Timestamp::now().unix_millis(),
        }
    }

    fn remote_grant(system: &str, site: &str, nac: u16, freq: u64, talkgroup: u16) -> LinkMsg {
        LinkMsg::Grant {
            site: SiteKey {
                system: system.to_owned(),
                site: site.to_owned(),
                nac,
            },
            frequency_hz: freq,
            talkgroup,
            source: 7,
            flags: GrantFlags::default(),
            at_wall_ms: Timestamp::now().unix_millis(),
            seq: 1,
        }
    }

    fn peer() -> SocketAddr {
        "192.0.2.30:47123".parse().expect("a literal address")
    }

    fn followed(rx: &BoundedReceiver<TrafficCmd>) -> Option<u64> {
        match rx.try_recv() {
            Ok(TrafficCmd::Follow { frequency_hz, .. }) => Some(frequency_hz),
            _ => None,
        }
    }

    #[test]
    fn a_peers_grant_is_routed_to_a_local_tuner_that_covers_it() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), Some(852_137_500));
        assert_eq!(metrics.counter("link.grants_rx").get(), 1);
        // The peer is known from its grant alone; it has never introduced
        // itself, so the panel's "last hello" column has nothing to show.
        assert_eq!(
            link.status_json()["peers"][0]["last_hello_s"],
            Value::Null,
            "a hello age is not invented from other traffic"
        );
    }

    /// A private grant crosses the recorder link with its kind, so a peer that
    /// records it records a private call and not a group call on talkgroup 0.
    ///
    /// What does **not** cross is the called radio: `LinkMsg::Grant` carries a
    /// 16-bit talkgroup and no 24-bit TO, and widening it would be a wire
    /// break for every peer. A peer therefore records the audio and the
    /// talker; the pair is on the control-channel recorder's own log.
    #[test]
    fn a_private_grant_crosses_the_link_as_a_private_grant() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());

        let mut msg = remote_grant("example", "central", 0x34F, 852_137_500, 0);
        if let LinkMsg::Grant { flags, .. } = &mut msg {
            *flags = GrantFlags::default().with(GrantFlags::PRIVATE, true);
        }
        link.ingest(peer(), msg, Some(&coord));

        match rx.try_recv() {
            Ok(TrafficCmd::Follow {
                frequency_hz, info, ..
            }) => {
                assert_eq!(frequency_hz, 852_137_500);
                assert_eq!(info.kind, CallKind::UnitToUnit);
                assert_eq!(info.talkgroup, 0, "a private call has no talkgroup");
            }
            other => panic!("expected a private follow, got {other:?}"),
        }
    }

    /// And the publisher sets that flag: a local private grant offered to the
    /// peers is marked private, a group grant is not.
    #[test]
    fn publishing_marks_a_private_grant_private() {
        let metrics = Metrics::new();
        let link = handle(
            &config(
                DeviceRole::Control,
                "127.0.0.1:0",
                &["127.0.0.1:9".to_owned()],
                None,
            ),
            metrics.clone(),
        );
        let mut g = grant_msg(852_137_500, 0);
        g.info.kind = CallKind::UnitToUnit;
        g.info.target = 9_000_007;
        link.publish(&g);
        link.publish(&grant_msg(852_537_500, 1301));
        assert_eq!(metrics.counter("link.grants_tx").get(), 2);
    }

    #[test]
    fn a_tuner_that_refuses_remote_grants_is_never_handed_one() {
        // The tuner covers the frequency and is enabled; only `remote_grants`
        // stands between it and the call, and that has to be enough.
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(false, metrics.clone());

        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), None);
        assert_eq!(
            metrics.counter("coordinator.out_of_band_grants").get(),
            1,
            "and it is counted, not silently dropped"
        );
    }

    #[test]
    fn a_grant_this_recorder_just_published_is_not_recorded_twice() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());

        // We heard it first and told the peers; the peer heard the same grant
        // and told us. One call, one recording.
        link.publish(&grant_msg(852_137_500, 1301));
        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), None);
        assert_eq!(metrics.counter("link.grant_dup").get(), 1);

        // A different talkgroup on the same frequency is a different call.
        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1302),
            Some(&coord),
        );
        assert_eq!(followed(&rx), Some(852_137_500));
    }

    #[test]
    fn defer_to_claims_leaves_a_call_a_peer_already_has() {
        let metrics = Metrics::new();
        let mut cfg = config(DeviceRole::Voice, "127.0.0.1:0", &[], None);
        cfg.link.defer_to_claims = true;
        let link = handle(&cfg, metrics.clone());
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            LinkMsg::Claim {
                site: SiteKey {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                },
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                tuner: "peer-voice".into(),
                at_wall_ms: Timestamp::now().unix_millis(),
            },
            Some(&coord),
        );
        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), None, "the peer said it has this one");
        assert_eq!(metrics.counter("link.grant_deferred").get(), 1);
        assert_eq!(metrics.counter("link.claims_rx").get(), 1);
    }

    #[test]
    fn the_same_call_is_recorded_twice_by_default() {
        // The default is deliberately *not* to defer: keyup picks a best source
        // among copies, and a suppressed call the peer then loses is gone.
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            LinkMsg::Claim {
                site: SiteKey {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                },
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                tuner: "peer-voice".into(),
                at_wall_ms: Timestamp::now().unix_millis(),
            },
            Some(&coord),
        );
        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), Some(852_137_500));
        assert_eq!(metrics.counter("link.grant_deferred").get(), 0);
    }

    #[test]
    fn a_grant_for_a_site_this_recorder_does_not_serve_is_counted_and_dropped() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            remote_grant("other", "remote", 0x111, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), None);
        assert_eq!(metrics.counter("link.grant_site_unknown").get(), 1);
        assert_eq!(metrics.counter("link.grants_rx").get(), 0);
    }

    #[test]
    fn a_site_named_differently_by_a_peer_is_matched_on_its_nac() {
        // Two operators, two config files, one site. The NAC is the site's own
        // idea of who it is, so it settles the argument.
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        metrics.gauge("site.0.nac").set(0x34F);
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            remote_grant("COUNTY-P25", "north-site", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), Some(852_137_500));
    }

    #[test]
    fn accept_remote_grants_off_ignores_a_peers_grant_entirely() {
        let metrics = Metrics::new();
        let mut cfg = config(DeviceRole::Voice, "127.0.0.1:0", &[], None);
        cfg.link.accept_remote_grants = false;
        let link = handle(&cfg, metrics.clone());
        let (coord, rx) = coordinator(true, metrics.clone());

        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 852_137_500, 1301),
            Some(&coord),
        );

        assert_eq!(followed(&rx), None);
        assert_eq!(metrics.counter("link.grant_ignored").get(), 1);
    }

    #[test]
    fn publish_grants_off_sends_nothing() {
        let metrics = Metrics::new();
        let mut cfg = config(DeviceRole::Control, "127.0.0.1:0", &[], None);
        cfg.link.publish_grants = false;
        let link = handle(&cfg, metrics.clone());
        link.publish(&grant_msg(852_137_500, 1301));
        assert_eq!(metrics.counter("link.grants_tx").get(), 0);
    }

    /// Two trunkd *processes* on one box are two link nodes on loopback ports.
    /// This is that, minus the fork: A follows the control channel and records
    /// nothing; B has the voice tuner. A grant crosses a real UDP socket, is
    /// authenticated, and lands as a `Follow` on B's tuner queue.
    #[test]
    fn a_grant_crosses_two_processes_on_one_box() {
        let b_metrics = Metrics::new();
        let b = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            b_metrics.clone(),
        );
        let b_addr = b.local_addr().expect("bound");

        let a_metrics = Metrics::new();
        let a = handle(
            &config(
                DeviceRole::Control,
                "127.0.0.1:0",
                &[b_addr.to_string()],
                None,
            ),
            a_metrics.clone(),
        );

        let (coord, rx) = coordinator(true, b_metrics.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let threads = b.spawn(Some(coord), stop);

        // A's control front publishes a grant it cannot record itself.
        a.publish(&grant_msg(852_137_500, 1301));

        let mut got = None;
        for _ in 0..2_000 {
            if let Some(f) = followed(&rx) {
                got = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        threads.shutdown();

        assert_eq!(got, Some(852_137_500), "B's voice tuner took A's grant");
        assert_eq!(a_metrics.counter("link.grants_tx").get(), 1);
        assert_eq!(b_metrics.counter("link.grants_rx").get(), 1);
    }

    #[test]
    fn a_peers_grant_acks_are_counted_and_reported_per_peer() {
        // The whole ack path end to end: A publishes a grant, B's link thread
        // answers it, and A's status block says so against the peer entry for
        // B's host.
        let b_metrics = Metrics::new();
        let b = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            b_metrics.clone(),
        );
        let b_addr = b.local_addr().expect("bound");
        let a_metrics = Metrics::new();
        let a = handle(
            &config(
                DeviceRole::Control,
                "127.0.0.1:0",
                &[b_addr.to_string()],
                None,
            ),
            a_metrics.clone(),
        );
        let (coord, _rx) = coordinator(true, b_metrics.clone());
        let b_threads = b.spawn(Some(coord), Arc::new(AtomicBool::new(false)));
        let a_threads = a.spawn(None, Arc::new(AtomicBool::new(false)));

        a.publish(&grant_msg(852_137_500, 1301));
        for _ in 0..2_000 {
            if a_metrics.counter("link.grant_acks_rx").get() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // B is in A's peer table by the usual route: it said hello.
        a.ingest(
            "127.0.0.1:47123".parse().expect("an address"),
            LinkMsg::Hello {
                instance: "rec-b".into(),
                version: u32::from(trunkd_link::wire::PROTOCOL_VERSION),
                sites: Vec::new(),
            },
            None,
        );
        let v = a.status_json();
        a_threads.shutdown();
        b_threads.shutdown();

        assert_eq!(b_metrics.counter("link.grant_acks_tx").get(), 1, "B acked");
        assert_eq!(
            a_metrics.counter("link.grant_acks_rx").get(),
            1,
            "A heard it"
        );
        let p = &v["peers"][0];
        assert_eq!(p["grants_sent"], 1);
        assert_eq!(p["grants_acked"], 1);
        assert_eq!(p["grants_unacked"], 0);
        assert_eq!(p["acks_grants"], true);
        assert_eq!(v["counters"]["grants_unacked"], 0);
    }

    #[test]
    fn a_peer_nothing_is_known_about_reports_no_ack_state() {
        // The peer table is keyed on where datagrams come *from* and the ack
        // counters on where they are sent *to*; a peer entry that matches no
        // configured peer says "nothing known" rather than inventing zeros'
        // worth of confidence.
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            Metrics::new(),
        );
        link.ingest(
            peer(),
            LinkMsg::Hello {
                instance: "rec-b".into(),
                version: 1,
                sites: Vec::new(),
            },
            None,
        );
        let p = &link.status_json()["peers"][0];
        assert_eq!(p["grants_sent"], 0);
        assert_eq!(p["grants_retried"], 0);
        assert_eq!(p["acks_grants"], Value::Null);
    }

    #[test]
    fn the_status_block_reports_peers_claims_and_counters() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        link.ingest(
            peer(),
            LinkMsg::Hello {
                instance: "rec-b".into(),
                version: 1,
                sites: vec![SiteAd {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                    control_hz: 851_050_000,
                    locked: true,
                    tsbk_rate: 15,
                    tuners: vec![TunerAd {
                        key: "t9".into(),
                        role: "control".into(),
                        center_hz: 854_000_000,
                        span_hz: 8_000_000,
                        bands: vec![(851_000_000, 866_000_000)],
                    }],
                }],
            },
            None,
        );
        link.ingest(
            peer(),
            LinkMsg::Claim {
                site: SiteKey {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                },
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                tuner: "t9".into(),
                at_wall_ms: Timestamp::now().unix_millis(),
            },
            None,
        );

        let v = link.status_json();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["instance"], "rec-a");
        let p = &v["peers"][0];
        assert_eq!(p["instance"], "rec-b");
        assert_eq!(p["addr"], "192.0.2.30:47123");
        assert_eq!(p["alive"], true);
        assert_eq!(p["last_hello_s"], 0, "it said hello just now");
        assert_eq!(p["sites"][0]["site"], "central");
        assert_eq!(p["sites"][0]["tuners"][0]["key"], "t9");
        assert_eq!(p["claims"][0]["frequency_hz"], 852_137_500u64);
        assert_eq!(p["claims"][0]["tuner"], "t9");
        assert_eq!(v["counters"]["claims_rx"], 1);
        assert_eq!(disabled_status_json()["enabled"], false);
    }

    #[test]
    fn a_release_clears_a_peers_claim() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let claim = LinkMsg::Claim {
            site: SiteKey {
                system: "example".into(),
                site: "central".into(),
                nac: 0x34F,
            },
            frequency_hz: 852_137_500,
            talkgroup: 1301,
            tuner: "t9".into(),
            at_wall_ms: Timestamp::now().unix_millis(),
        };
        link.ingest(peer(), claim, None);
        link.ingest(
            peer(),
            LinkMsg::Release {
                site: SiteKey {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                },
                frequency_hz: 852_137_500,
                at_wall_ms: Timestamp::now().unix_millis(),
            },
            None,
        );
        let v = link.status_json();
        assert_eq!(v["peers"][0]["claims"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn a_peer_that_went_quiet_long_ago_is_forgotten_when_a_new_one_appears() {
        // A restarted peer arrives on a new source port. The old entry must go
        // rather than sit in the status report forever claiming to be a peer.
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let hello = |instance: &str| LinkMsg::Hello {
            instance: instance.to_owned(),
            version: 1,
            sites: Vec::new(),
        };
        let old: SocketAddr = "192.0.2.30:51000".parse().expect("a literal address");
        let new: SocketAddr = "192.0.2.30:52000".parse().expect("a literal address");

        link.ingest(old, hello("rec-b"), None);
        // Age the first entry past the expiry by hand: the alternative is a
        // ten-minute test.
        if let Ok(mut st) = link.state.lock() {
            if let Some(p) = st.peers.get_mut(&old) {
                p.last_seen_ms -= PEER_TTL_MS + 1_000;
            }
        }
        link.ingest(new, hello("rec-b"), None);

        let v = link.status_json();
        let peers = v["peers"].as_array().expect("an array");
        assert_eq!(peers.len(), 1, "the dead entry went with the restart");
        assert_eq!(peers[0]["addr"], "192.0.2.30:52000");
    }

    #[test]
    fn a_peers_grant_too_old_to_follow_is_counted_and_not_routed() {
        // C4: a grant that is authentic but old (delayed, or captured and
        // resent inside the frame window) would send a tuner to a frequency
        // whose call is long over.
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let (coord, rx) = coordinator(true, metrics.clone());
        let mut old = remote_grant("example", "central", 0x34F, 852_137_500, 1301);
        if let LinkMsg::Grant { at_wall_ms, .. } = &mut old {
            *at_wall_ms -= MAX_REMOTE_GRANT_AGE_MS + 1_000;
        }
        link.ingest(peer(), old, Some(&coord));
        assert_eq!(followed(&rx), None, "an old grant is not followed");
        assert_eq!(metrics.counter("link.grant_stale").get(), 1);
        assert_eq!(metrics.counter("link.grants_rx").get(), 0);
        assert_eq!(link.status_json()["counters"]["grant_stale"], 1);

        // A current one still is.
        link.ingest(
            peer(),
            remote_grant("example", "central", 0x34F, 853_000_000, 1301),
            Some(&coord),
        );
        assert_eq!(followed(&rx), Some(853_000_000));
    }

    #[test]
    fn the_peer_table_is_bounded_whatever_arrives() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        for port in 0..(MAX_PEERS as u16 + 20) {
            let from = SocketAddr::from(([192, 0, 2, 30], 40_000 + port));
            link.ingest(
                from,
                LinkMsg::Hello {
                    instance: format!("rec-{port}"),
                    version: 1,
                    sites: Vec::new(),
                },
                None,
            );
        }
        let v = link.status_json();
        let peers = v["peers"].as_array().expect("an array");
        assert_eq!(peers.len(), MAX_PEERS);
        let newest = format!("192.0.2.30:{}", 40_000 + MAX_PEERS as u16 + 19);
        assert!(
            peers.iter().any(|p| p["addr"] == newest.as_str()),
            "the newest peer is kept; the oldest made room"
        );
    }

    #[test]
    fn a_quiet_peer_that_is_still_up_is_kept() {
        let metrics = Metrics::new();
        let link = handle(
            &config(DeviceRole::Voice, "127.0.0.1:0", &[], None),
            metrics.clone(),
        );
        let hello = LinkMsg::Hello {
            instance: "rec-b".into(),
            version: 1,
            sites: Vec::new(),
        };
        link.ingest(
            "192.0.2.30:51000".parse().expect("a literal address"),
            hello.clone(),
            None,
        );
        link.ingest(
            "192.0.2.31:51000".parse().expect("a literal address"),
            hello,
            None,
        );
        assert_eq!(
            link.status_json()["peers"].as_array().map(Vec::len),
            Some(2),
            "two live peers are two peers"
        );
    }

    #[test]
    fn a_disabled_link_builds_nothing() {
        let mut cfg = config(DeviceRole::Voice, "127.0.0.1:0", &[], None);
        cfg.link.enabled = false;
        assert!(LinkHandle::start_from(
            &cfg.link,
            &[site_spec("example", "central", 0)],
            &[],
            Metrics::new(),
            "x".into()
        )
        .is_none());
    }

    // -- the slot ---------------------------------------------------------

    #[test]
    fn a_slot_binds_nothing_until_it_is_started() {
        // Which is what lets the desired-state prefetch reach the *first* bind:
        // a box whose TOML has no `[link]` can be given one, key included, by
        // keyup before a single datagram is sent.
        let mut cfg = config(DeviceRole::Voice, "127.0.0.1:0", &[], None);
        cfg.link.enabled = false;
        let slot = LinkSlot::new(
            &cfg,
            &[site_spec("example", "central", 0)],
            Metrics::new(),
            "rec-a".to_owned(),
        );
        assert!(slot.handle().is_none());
        assert_eq!(slot.status_json()["enabled"], false);

        // keyup turns it on before start: nothing binds yet, and the config is
        // what the first build will use.
        let mut want = slot.config();
        want.enabled = true;
        want.bind = "127.0.0.1:0".to_owned();
        want.key = KEY.to_owned();
        assert!(slot.set_config(want));
        assert!(slot.handle().is_none(), "still not started");

        slot.start(None);
        let up = slot.handle().expect("the first build uses keyup's config");
        assert!(up.local_addr().is_some());
        assert_eq!(slot.status_json()["enabled"], true);
        slot.shutdown();
        assert!(slot.handle().is_none());
    }

    #[test]
    fn a_config_change_rebuilds_the_link_in_place() {
        let cfg = config(DeviceRole::Voice, "127.0.0.1:0", &[], None);
        let slot = LinkSlot::new(
            &cfg,
            &[site_spec("example", "central", 0)],
            Metrics::new(),
            "rec-a".to_owned(),
        );
        slot.start(None);
        let first = slot.handle().expect("running");
        let first_addr = first.local_addr().expect("bound");

        // An identical config is not a rebuild: the handle is the same object.
        assert!(!slot.set_config(cfg.link.clone()));
        assert!(Arc::ptr_eq(&first, &slot.handle().expect("still running")));

        // A changed one is: new socket, new threads, same slot. Nothing here
        // touches a tuner — the call thread and the coordinator hold the slot,
        // not the handle, so they keep running through it.
        let mut want = slot.config();
        want.peers = vec!["127.0.0.1:59999".to_owned()];
        want.defer_to_claims = true;
        assert!(slot.set_config(want));
        let second = slot.handle().expect("rebuilt");
        assert!(!Arc::ptr_eq(&first, &second));
        assert_ne!(
            second.local_addr().expect("bound"),
            first_addr,
            "a rebuild rebinds; the old socket is gone first"
        );
        assert!(second.defer_to_claims);
        assert_eq!(slot.config().peers.len(), 1);

        // And keyup can switch it off entirely.
        let mut off = slot.config();
        off.enabled = false;
        assert!(slot.set_config(off));
        assert!(slot.handle().is_none());
        assert_eq!(slot.status_json()["enabled"], false);
        slot.shutdown();
    }

    #[test]
    fn a_key_fingerprint_identifies_a_shared_secret_without_revealing_it() {
        let a = key_fingerprint(KEY).expect("a key has one");
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, key_fingerprint(KEY).expect("stable"));
        assert_ne!(a, key_fingerprint("another-sixteen-plus-key").expect("hm"));
        assert!(!KEY.contains(&a), "and it is not a piece of the key");
        assert_eq!(key_fingerprint("   "), None, "an unset key has none");
    }

    #[test]
    fn remote_grants_defaults_follow_the_tuner_role() {
        let dev = |role: DeviceRole, over: Option<bool>| {
            let cfg = config(role, "127.0.0.1:0", &[], over);
            cfg.device[0].accepts_remote_grants()
        };
        assert!(
            dev(DeviceRole::Voice, None),
            "voice exists to be handed work"
        );
        assert!(dev(DeviceRole::Data, None));
        assert!(
            !dev(DeviceRole::Auto, None),
            "auto has its own control channel"
        );
        assert!(!dev(DeviceRole::Control, None));
        assert!(
            dev(DeviceRole::Auto, Some(true)),
            "the operator can say otherwise"
        );
        assert!(!dev(DeviceRole::Voice, Some(false)));
    }
}
