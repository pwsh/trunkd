// SPDX-License-Identifier: GPL-3.0-or-later
//! The site coordinator: cross-tuner grant routing (multi-tuner core, W1).
//!
//! In a single-tuner run this thread has nothing to do: an `auto` device
//! follows its own grants inline on the DSP thread, exactly as before, and the
//! coordinator sees a grant only when the front could not record it. It earns
//! its keep when a site is served by more than one tuner — a dedicated
//! `control` front and one or more `voice` fronts, or an `auto` front whose
//! band partition hands some frequencies to a sibling — and when a *peer*
//! recorder offers a grant over the link.
//!
//! It is built for every run all the same, because the link is keyup's to turn
//! on at any moment and a recorder with no bus has nowhere to put an arriving
//! peer's grant. Idle it is one sleeping thread.
//!
//! ```text
//!   control/auto DSP thread                coordinator thread            voice DSP thread
//! ┌────────────────────────┐  GrantBus  ┌──────────────────────┐  cmd  ┌──────────────────┐
//! │ SiteEngine (control)   ├───────────▶│ route by span,       ├──────▶│ SiteEngine.follow│
//! │  → SiteEvent::GrantOut │  bounded   │ local tuner first    │bounded│  (traffic grabber)│
//! └────────────────────────┘  drop+cnt  └──────────────────────┘drop+cnt└──────────────────┘
//! ```
//!
//! Both hops are bounded drop-oldest channels, like every other queue in the
//! recorder: a wedged consumer costs grants (counted) rather than memory. The
//! `TrunkingTracker` never crosses a thread — it stays on the control DSP
//! thread; only the `Send` [`GrantInfo`] and periodic channel-plan snapshots do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use trunkd_core::chan::{BoundedReceiver, BoundedSender};
use trunkd_core::Metrics;
use trunkd_p25::iden::IdenBand;
use trunkd_p25::GrantInfo;

/// Depth of the control→coordinator grant bus. Grants run at most a few per
/// second even on a busy site; this is many seconds of headroom.
pub(crate) const GRANT_BUS_DEPTH: usize = 1_024;

/// Depth of each coordinator→voice-tuner command queue.
pub(crate) const TRAFFIC_CMD_DEPTH: usize = 1_024;

/// A grant a control/auto front could not (or should not) record itself,
/// published for the coordinator to route.
#[derive(Debug, Clone)]
pub(crate) struct GrantMsg {
    /// Which site (index into `Shared::sites`).
    pub(crate) site: usize,
    /// The device that published it, so the coordinator can prefer a co-located
    /// voice front when spans overlap ("local tuner preferred").
    pub(crate) origin_device: usize,
    /// Downlink frequency granted.
    pub(crate) frequency_hz: u64,
    /// Grant metadata (the only per-call state that crosses the thread).
    pub(crate) info: GrantInfo,
    /// Wall-clock time of the grant on the control tuner.
    pub(crate) at_wall_ms: i64,
}

/// What the coordinator tells a voice/data tuner to do. Carried on a bounded
/// drop-oldest queue drained by the tuner's DSP thread each block.
#[derive(Debug, Clone)]
pub(crate) enum TrafficCmd {
    /// Start following a granted call on `frequency_hz`.
    Follow {
        /// Which site on the receiving tuner.
        site: usize,
        /// Downlink frequency.
        frequency_hz: u64,
        /// Grant metadata.
        info: GrantInfo,
        /// Control-tuner wall-clock time of the grant.
        at_wall_ms: i64,
    },
    /// Refresh the metadata of a call already being followed (a talker change,
    /// a late patch). Applied like a follow. Seam for the keyup control loop —
    /// drained today, produced by that later task.
    #[allow(dead_code)]
    Update {
        /// Which site.
        site: usize,
        /// Downlink frequency.
        frequency_hz: u64,
        /// Fresh metadata.
        info: GrantInfo,
        /// Control-tuner wall-clock time.
        at_wall_ms: i64,
    },
    /// Stop following `frequency_hz` (the keyup control loop, later). Seam:
    /// drained today, produced by that later task.
    #[allow(dead_code)]
    Release {
        /// Which site.
        site: usize,
        /// Downlink frequency.
        frequency_hz: u64,
        /// Control-tuner wall-clock time.
        at_wall_ms: i64,
    },
    /// A refreshed channel plan (IDEN band map) for the receiving tuner's local
    /// tracker, so channel-referenced grants resolve the same way the control
    /// tuner sees them. Seam: drained today, produced by that later task.
    #[allow(dead_code)]
    ChannelPlan {
        /// Which site.
        site: usize,
        /// The site's bands.
        bands: Vec<IdenBand>,
    },
}

/// What one tuner will accept right now: whether it is recording at all, and
/// the inclusive downlink ranges it takes — its bands clamped to its usable
/// capture span, possibly several disjoint ones.
///
/// Shared with the device factory rather than baked into the route, because
/// keyup can push a new `bands` list or an `enabled = false` at run time: the
/// factory republishes both on every (re)build, so the routing table changes at
/// exactly the moment the rebuilt `DeviceUnit` starts honouring them. Empty
/// ranges mean the tuner accepts nothing (no usable capture, or bands that miss
/// it), and a disabled tuner accepts nothing whatever its ranges say.
#[derive(Debug)]
pub(crate) struct RouteState {
    /// Whether the tuner records at all (keyup's `enabled`). Read on the
    /// routing hot path before the mutex is even taken.
    enabled: AtomicBool,
    /// The ranges it accepts when it is enabled.
    ranges: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Whether it takes grants a *peer* recorder heard (`[[device]]
    /// remote_grants`, or keyup's push of it). Live for the same reason the
    /// other two are: it is republished on every device (re)build, so a keyup
    /// change lands with the restart that adopts it.
    accepts_remote: AtomicBool,
}

impl RouteState {
    pub(crate) fn new(
        enabled: bool,
        ranges: Vec<(u64, u64)>,
        accepts_remote: bool,
    ) -> Arc<RouteState> {
        Arc::new(RouteState {
            enabled: AtomicBool::new(enabled),
            ranges: std::sync::Mutex::new(ranges),
            accepts_remote: AtomicBool::new(accepts_remote),
        })
    }

    /// Republish what this tuner accepts, on a device (re)build. Both fields
    /// together, in one call: a rebuild is the moment the unit adopts a config,
    /// and routing must never see half of one. Poison is ignored the way the
    /// desired-state registry ignores it — routing must not stop because some
    /// other thread panicked.
    pub(crate) fn publish(&self, enabled: bool, ranges: Vec<(u64, u64)>, accepts_remote: bool) {
        *self.ranges.lock().unwrap_or_else(|p| p.into_inner()) = ranges;
        self.accepts_remote.store(accepts_remote, Ordering::Relaxed);
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Whether this tuner takes grants a peer recorder heard.
    pub(crate) fn accepts_remote(&self) -> bool {
        self.accepts_remote.load(Ordering::Relaxed)
    }

    /// Whether this tuner is recording and `freq` falls in one of its ranges.
    pub(crate) fn covers(&self, freq: u64) -> bool {
        self.enabled.load(Ordering::Relaxed)
            && self
                .ranges
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|(lo, hi)| freq >= *lo && freq <= *hi)
    }
}

/// One routable tuner: what it will accept, and the queue to reach it.
pub(crate) struct TunerRoute {
    /// Config index of the device, for the local-preference tie-break.
    pub(crate) device_index: usize,
    /// What this tuner accepts, live (see [`RouteState`]) — including whether
    /// it takes grants a *peer* recorder heard (`[[device]] remote_grants`).
    /// Local grants ignore that; only [`SiteCoordinator::route_remote`]
    /// consults it, so a link cannot pull a tuner off work the operator gave
    /// it.
    pub(crate) state: Arc<RouteState>,
    /// The tuner's command queue.
    pub(crate) tx: Arc<BoundedSender<TrafficCmd>>,
}

impl TunerRoute {
    fn covers(&self, freq: u64) -> bool {
        self.state.covers(freq)
    }
}

/// Somewhere a routed grant is also announced — the recorder link, when it is
/// running. A trait rather than a direct dependency so the coordinator keeps
/// knowing nothing about sockets, and so tests can watch what it publishes.
pub(crate) trait GrantPublisher: Send + Sync {
    /// Announce a grant this recorder could not record itself.
    fn publish(&self, grant: &GrantMsg);
}

/// Deterministic routing table: per site, the tuners that can record its
/// traffic, in config order. The set of tuners is fixed at startup (a tuner's
/// site membership is TOML); whether each is recording, and what it accepts,
/// is live.
pub(crate) struct SiteCoordinator {
    /// `routes[site]` are the voice/data tuners for that site.
    routes: Vec<Vec<TunerRoute>>,
    metrics: Metrics,
}

impl SiteCoordinator {
    pub(crate) fn new(routes: Vec<Vec<TunerRoute>>, metrics: Metrics) -> Self {
        SiteCoordinator { routes, metrics }
    }

    /// Route one grant to the first enabled tuner whose span covers it, the
    /// publishing device's own tuner preferred. A tuner keyup has disabled
    /// covers nothing, so its grants fall through to whatever other route can
    /// serve them; a grant no tuner covers is dropped and counted (per coarse
    /// band, plus the global out-of-band counter) — never queued.
    pub(crate) fn route(&self, g: &GrantMsg) {
        self.route_with(g, false)
    }

    /// Route a grant a *peer* recorder heard, to a tuner that accepts them.
    ///
    /// Identical to [`SiteCoordinator::route`] except that a tuner whose
    /// `remote_grants` is off is invisible: it keeps whatever the local control
    /// channel gives it and is never pulled onto a peer's traffic. There is no
    /// local preference to apply — the grant came from another box, so
    /// `origin_device` matches nothing.
    pub(crate) fn route_remote(&self, g: &GrantMsg) {
        self.route_with(g, true)
    }

    fn route_with(&self, g: &GrantMsg, remote: bool) {
        let Some(routes) = self.routes.get(g.site) else {
            self.count_unrouted(g.frequency_hz);
            return;
        };
        let eligible =
            |r: &&TunerRoute| (!remote || r.state.accepts_remote()) && r.covers(g.frequency_hz);
        // Local preference: try a route on the publishing device first, then the
        // rest in config order. Deterministic for a given config.
        let local = routes
            .iter()
            .find(|r| r.device_index == g.origin_device && eligible(r));
        let chosen = local.or_else(|| routes.iter().find(eligible));
        match chosen {
            Some(route) => {
                let outcome = route.tx.send(TrafficCmd::Follow {
                    site: g.site,
                    frequency_hz: g.frequency_hz,
                    info: g.info,
                    at_wall_ms: g.at_wall_ms,
                });
                self.metrics.counter("coordinator.grants_routed").inc();
                if outcome == trunkd_core::chan::SendOutcome::Evicted {
                    self.metrics.counter("coordinator.cmd_queue_dropped").inc();
                }
            }
            None => self.count_unrouted(g.frequency_hz),
        }
    }

    fn count_unrouted(&self, freq: u64) {
        self.metrics
            .counter(&format!(
                "coordinator.unrouted_grants.{}",
                band_label_for(freq)
            ))
            .inc();
        self.metrics.counter("coordinator.out_of_band_grants").inc();
    }
}

/// Coarse band label a frequency falls in, for the per-band unrouted counter.
pub(crate) fn band_label_for(freq: u64) -> &'static str {
    match freq {
        136_000_000..=174_000_000 => "vhf",
        380_000_000..=520_000_000 => "uhf",
        758_000_000..=805_999_999 => "700",
        806_000_000..=869_000_000 => "800",
        896_000_000..=941_000_000 => "900",
        _ => "other",
    }
}

/// Drain the grant bus and route each grant until the bus disconnects (every
/// publisher dropped) or the recorder is stopping.
///
/// Waits on the bus with a timeout rather than blocking on `recv` forever, so a
/// stop is noticed within [`STOP_POLL`] even when no grant is in flight, and a
/// disconnected bus (all control fronts gone at shutdown) ends the thread on
/// its own.
pub(crate) fn run(
    rx: BoundedReceiver<GrantMsg>,
    coord: Arc<SiteCoordinator>,
    stop: Arc<AtomicBool>,
    link: Option<Arc<dyn GrantPublisher>>,
) {
    use trunkd_core::chan::RecvTimeoutError;
    loop {
        // Blocks until a grant arrives (no added latency) and wakes only to
        // look at `stop`: it used to poll every 2 ms, ~480 wakeups a second
        // on an idle recorder (review 2026-09-22 §3.4, T-C3).
        match rx.recv_wait(STOP_POLL) {
            Ok(g) => {
                // Peers hear about it before it is routed: publishing is a
                // non-blocking datagram, and a peer whose tuner is free should
                // not wait on this recorder's queueing to learn of the call.
                if let Some(link) = &link {
                    link.publish(&g);
                }
                coord.route(&g);
            }
            Err(RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// How often an idle coordinator looks at its stop flag.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::ORIGIN_NONE;
    use trunkd_core::chan;

    #[allow(clippy::type_complexity)]
    fn route(device_index: usize, lo: u64, hi: u64) -> (TunerRoute, BoundedReceiver<TrafficCmd>) {
        let (tx, rx) = chan::bounded::<TrafficCmd>(16);
        (
            TunerRoute {
                device_index,
                state: RouteState::new(true, vec![(lo, hi)], true),
                tx: Arc::new(tx),
            },
            rx,
        )
    }

    fn grant(site: usize, origin: usize, freq: u64) -> GrantMsg {
        GrantMsg {
            site,
            origin_device: origin,
            frequency_hz: freq,
            info: GrantInfo::default(),
            at_wall_ms: 0,
        }
    }

    fn followed_freq(rx: &BoundedReceiver<TrafficCmd>) -> Option<u64> {
        match rx.try_recv() {
            Ok(TrafficCmd::Follow { frequency_hz, .. }) => Some(frequency_hz),
            _ => None,
        }
    }

    #[test]
    fn a_grant_goes_to_the_tuner_whose_span_covers_it() {
        let (a, a_rx) = route(0, 851_000_000, 856_000_000);
        let (b, b_rx) = route(1, 856_000_001, 866_000_000);
        let coord = SiteCoordinator::new(vec![vec![a, b]], Metrics::new());

        coord.route(&grant(0, 0, 852_137_500)); // in A's span
        coord.route(&grant(0, 0, 858_987_500)); // in B's span

        assert_eq!(followed_freq(&a_rx), Some(852_137_500));
        assert_eq!(followed_freq(&b_rx), Some(858_987_500));
        assert_eq!(followed_freq(&a_rx), None, "A got only its own grant");
        assert_eq!(followed_freq(&b_rx), None, "B got only its own grant");
    }

    #[test]
    fn a_grant_no_tuner_covers_is_counted_not_queued() {
        let (a, a_rx) = route(0, 851_000_000, 856_000_000);
        let metrics = Metrics::new();
        let coord = SiteCoordinator::new(vec![vec![a]], metrics.clone());

        coord.route(&grant(0, 0, 935_000_000)); // 900 MHz, outside A

        assert_eq!(followed_freq(&a_rx), None, "nothing queued");
        assert_eq!(metrics.counter("coordinator.unrouted_grants.900").get(), 1);
        assert_eq!(metrics.counter("coordinator.out_of_band_grants").get(), 1);
    }

    #[test]
    fn overlapping_spans_prefer_the_publishing_devices_own_tuner() {
        // Two voice tuners both cover the frequency; the one on the publishing
        // device wins, deterministically.
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        let (b, b_rx) = route(1, 851_000_000, 866_000_000);
        let coord = SiteCoordinator::new(vec![vec![a, b]], Metrics::new());

        coord.route(&grant(0, 1, 858_987_500)); // published by device 1

        assert_eq!(followed_freq(&a_rx), None);
        assert_eq!(followed_freq(&b_rx), Some(858_987_500));
    }

    #[test]
    fn republished_ranges_reroute_without_rebuilding_the_table() {
        // What a keyup `bands` push does: the tuner restarts, its factory
        // republishes its ranges, and the coordinator routes by the new ones
        // immediately. Without this the route table would keep the boot-time
        // TOML partition and the pushed bands would change nothing.
        let (a, a_rx) = route(0, 851_000_000, 856_000_000);
        let (b, b_rx) = route(1, 856_000_001, 866_000_000);
        let a_state = a.state.clone();
        let coord = SiteCoordinator::new(vec![vec![a, b]], Metrics::new());

        coord.route(&grant(0, 0, 858_987_500));
        assert_eq!(followed_freq(&b_rx), Some(858_987_500), "B's band today");

        // A now covers the whole span (bands cleared); it is first in config
        // order, so it wins the frequency B used to take.
        a_state.publish(true, vec![(851_000_000, 866_000_000)], true);
        coord.route(&grant(0, 0, 858_987_500));
        assert_eq!(followed_freq(&a_rx), Some(858_987_500));
        assert_eq!(followed_freq(&b_rx), None);

        // Emptied ranges (a tuner that can record nothing) route nowhere.
        a_state.publish(true, Vec::new(), true);
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), None);
    }

    #[test]
    fn several_disjoint_ranges_are_all_covered() {
        let (tx, rx) = chan::bounded::<TrafficCmd>(16);
        let r = TunerRoute {
            device_index: 0,
            state: RouteState::new(
                true,
                vec![(851_000_000, 853_000_000), (860_000_000, 866_000_000)],
                true,
            ),
            tx: Arc::new(tx),
        };
        let coord = SiteCoordinator::new(vec![vec![r]], Metrics::new());
        coord.route(&grant(0, 0, 852_137_500));
        coord.route(&grant(0, 0, 858_987_500)); // in the gap
        coord.route(&grant(0, 0, 861_000_000));
        assert_eq!(followed_freq(&rx), Some(852_137_500));
        assert_eq!(followed_freq(&rx), Some(861_000_000));
        assert_eq!(followed_freq(&rx), None, "the gap is not recorded");
    }

    #[test]
    fn a_peers_grant_only_reaches_a_tuner_that_accepts_them() {
        // `route_remote` is `route` plus one gate. A tuner with
        // `remote_grants = false` covers the frequency and is enabled, and
        // still never sees it — while the sibling that accepts them does.
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        let (b, b_rx) = route(1, 851_000_000, 866_000_000);
        a.state
            .publish(true, vec![(851_000_000, 866_000_000)], false);
        let metrics = Metrics::new();
        let coord = SiteCoordinator::new(vec![vec![a, b]], metrics.clone());

        // A local grant still takes A first, in config order: the gate applies
        // to the link and to nothing else.
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), Some(852_137_500));

        coord.route_remote(&grant(0, ORIGIN_NONE, 852_137_500));
        assert_eq!(followed_freq(&a_rx), None, "A refuses a peer's grant");
        assert_eq!(followed_freq(&b_rx), Some(852_137_500));
    }

    #[test]
    fn a_peers_grant_no_accepting_tuner_covers_is_counted() {
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        a.state
            .publish(true, vec![(851_000_000, 866_000_000)], false);
        let metrics = Metrics::new();
        let coord = SiteCoordinator::new(vec![vec![a]], metrics.clone());

        coord.route_remote(&grant(0, ORIGIN_NONE, 852_137_500));

        assert_eq!(followed_freq(&a_rx), None);
        assert_eq!(metrics.counter("coordinator.out_of_band_grants").get(), 1);
    }

    #[test]
    fn a_disabled_tuner_is_skipped() {
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        a.state
            .publish(false, vec![(851_000_000, 866_000_000)], true);
        let metrics = Metrics::new();
        let coord = SiteCoordinator::new(vec![vec![a]], metrics.clone());

        coord.route(&grant(0, 0, 852_137_500));

        assert_eq!(followed_freq(&a_rx), None);
        assert_eq!(metrics.counter("coordinator.out_of_band_grants").get(), 1);
    }

    #[test]
    fn disabling_a_tuner_hands_its_grants_to_a_sibling_and_re_enabling_takes_them_back() {
        // keyup disables one tuner of a pair. Its grants must fall to whoever
        // else can serve them rather than being queued to a unit that is idling
        // and will never drain them — and come back when it is re-enabled.
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        let (b, b_rx) = route(1, 851_000_000, 866_000_000);
        let (a_state, b_state) = (a.state.clone(), b.state.clone());
        let metrics = Metrics::new();
        let coord = SiteCoordinator::new(vec![vec![a, b]], metrics.clone());

        // Config order: A takes it while both are up.
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), Some(852_137_500));

        // A is disabled: B serves the same frequency.
        a_state.publish(false, vec![(851_000_000, 866_000_000)], true);
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), None, "nothing queued to an idle unit");
        assert_eq!(followed_freq(&b_rx), Some(852_137_500));

        // Both disabled: counted as unrouted, never queued.
        b_state.publish(false, vec![(851_000_000, 866_000_000)], true);
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), None);
        assert_eq!(followed_freq(&b_rx), None);
        assert_eq!(metrics.counter("coordinator.out_of_band_grants").get(), 1);
        assert_eq!(metrics.counter("coordinator.unrouted_grants.800").get(), 1);

        // A comes back: it is first in config order, so it takes them again.
        a_state.publish(true, vec![(851_000_000, 866_000_000)], true);
        coord.route(&grant(0, 0, 852_137_500));
        assert_eq!(followed_freq(&a_rx), Some(852_137_500));
        assert_eq!(followed_freq(&b_rx), None);
    }

    #[test]
    fn a_disabled_tuner_that_is_the_publishers_own_is_not_preferred() {
        // Local preference must not resurrect a disabled tuner: the grant goes
        // to the sibling that can actually record it.
        let (a, a_rx) = route(0, 851_000_000, 866_000_000);
        let (b, b_rx) = route(1, 851_000_000, 866_000_000);
        let b_state = b.state.clone();
        let coord = SiteCoordinator::new(vec![vec![a, b]], Metrics::new());

        b_state.publish(false, vec![(851_000_000, 866_000_000)], true);
        coord.route(&grant(0, 1, 858_987_500)); // published by device 1 (B)

        assert_eq!(followed_freq(&b_rx), None);
        assert_eq!(followed_freq(&a_rx), Some(858_987_500));
    }

    /// Review 2026-09-22 §3.4 (T-C3): an idle coordinator sleeps. It used to
    /// poll every 2 ms — 482 wakeups a second measured on the idle laptop
    /// recorder — and now blocks on the bus, waking only to look at `stop`.
    /// A grant still reaches its tuner at once, and a stop ends the thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_idle_coordinator_sleeps_and_still_routes_at_once() {
        fn voluntary_switches(tid: i32) -> u64 {
            let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status"))
                .expect("the thread's status");
            status
                .lines()
                .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|v| v.trim().parse().ok())
                .expect("a switch count")
        }

        let (a, a_rx) = route(0, 851_000_000, 856_000_000);
        let coord = Arc::new(SiteCoordinator::new(vec![vec![a]], Metrics::new()));
        let (tx, rx) = trunkd_core::chan::bounded::<GrantMsg>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = {
            let (coord, stop) = (coord.clone(), stop.clone());
            std::thread::spawn(move || {
                // SAFETY: `gettid` has no preconditions.
                tid_tx.send(unsafe { libc::gettid() }).expect("tid");
                run(rx, coord, stop, None);
            })
        };
        let tid = tid_rx.recv().expect("the coordinator's tid");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let before = voluntary_switches(tid);
        std::thread::sleep(std::time::Duration::from_secs(1));
        let wakeups = voluntary_switches(tid) - before;
        eprintln!("idle coordinator: {wakeups} wakeups in 1 s");
        assert!(wakeups <= 30, "{wakeups} wakeups in an idle second");

        let sent = std::time::Instant::now();
        tx.send(grant(0, 0, 852_137_500));
        let followed = loop {
            if let Some(freq) = followed_freq(&a_rx) {
                break freq;
            }
            assert!(
                sent.elapsed() < std::time::Duration::from_secs(1),
                "never routed"
            );
            std::thread::yield_now();
        };
        assert_eq!(followed, 852_137_500);

        stop.store(true, Ordering::Relaxed);
        thread.join().expect("the coordinator stops");
    }
}
