// SPDX-License-Identifier: GPL-3.0-or-later
//! The keyup **push channel** (W6): a WebSocket that says "fetch now".
//!
//! The control loop already pulls keyup's desired state on a ~10 s poll. That
//! is the contract and it stays exactly as it is; this module only removes the
//! wait. One long-lived thread holds a WebSocket open to
//! `{keyup}/api/recorder/stream` and, when keyup says a new desired-state
//! version exists, sets a flag the control loop checks on its next 100 ms tick.
//!
//! # What the channel deliberately does not carry
//!
//! **No configuration.** A frame from keyup carries a *version number*, never a
//! tuner's config. The recorder answers it by making the same authenticated
//! desired-state GET the poll loop makes, through the same parse-and-apply path
//! with the same validation, caps and refusals. Three properties fall out:
//!
//! * The notification is **idempotent**. Two frames, a duplicate, or a frame
//!   for a version already applied all end in one GET that returns the newest
//!   document; nothing is ordered, replayed, or acknowledged.
//! * The push channel cannot **widen the trust boundary**. Everything keyup can
//!   change it could already change through the GET, so a hostile or buggy
//!   frame can at worst cause an extra HTTP request.
//! * Losing the channel loses **latency, not correctness**. The poll is the
//!   fallback and it is untouched, so a recorder whose stream is down behaves
//!   precisely as a recorder built before this module did.
//!
//! # The loop
//!
//! ```text
//! connect ──▶ send hello ──▶ read frames ──┐
//!    ▲                                     │ heartbeat missed twice,
//!    │                                     │ close, or I/O error
//!    └──── backoff 1s→2→4…capped ◀─────────┘
//! ```
//!
//! The socket carries a short read timeout (250 ms) rather than one sized to
//! the heartbeat: the two-missed-heartbeats rule is then enforced against the
//! clock, on a loop that also sends the client's own `ping`, notices the stop
//! flag inside a quarter second, and never sits in a blocking read while the
//! recorder is trying to shut down. A connection that lasted longer than
//! [`STABLE_CONNECTION`] resets the backoff, so an hourly blip does not leave a
//! healthy recorder reconnecting on a 30 s cadence forever.
//!
//! Two close codes are treated as answers rather than accidents: **4401** (the
//! API key is not accepted) backs off for a whole minute instead of hammering
//! keyup with a credential it has already rejected, and **4408** (no hello
//! arrived in time) is logged so a recorder that cannot get its first frame out
//! says so. Each is logged once at WARN and not repeated while it keeps
//! happening for the same reason.
//!
//! # Portability
//!
//! Nothing here is platform-specific. `wss://` is served by `rustls` with the
//! webpki trust anchors compiled in, so the Windows build needs no platform
//! certificate store and behaves identically to the Linux one.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tungstenite::http::Uri;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{ClientRequestBuilder, Message, WebSocket};

use crate::pipeline::control::RecorderControl;
use crate::pipeline::observations::rfc3339;

/// Ceiling on the TCP connect, so an unreachable keyup does not park the thread
/// for the operating system's own (minutes-long) timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Read/write timeout in force while the HTTP upgrade is in flight.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Socket read timeout once the connection is up: the granularity at which the
/// loop notices the stop flag and the heartbeat deadline.
const POLL: Duration = Duration::from_millis(250);

/// Write timeout for our own (tiny) frames.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Heartbeat cadence assumed until the server's `hello` states one.
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

/// Bounds on the server-stated heartbeat. A server asking for 0 s would spin
/// the loop; one asking for an hour would leave a dead connection undetected.
const MIN_HEARTBEAT: u64 = 1;
/// Upper bound on the server-stated heartbeat, in seconds.
const MAX_HEARTBEAT: u64 = 300;

/// First reconnect wait; it doubles from here.
const BACKOFF_START: Duration = Duration::from_secs(1);

/// How long a connection must last to be judged healthy and reset the backoff.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// Wait imposed after keyup rejects the API key ([`CLOSE_AUTH`]). Retrying a
/// credential keyup has just refused, on the ordinary backoff, is a way to get
/// an instance rate-limited for nothing.
const AUTH_BACKOFF: Duration = Duration::from_secs(60);

/// keyup's close code for "the bearer token is not accepted".
const CLOSE_AUTH: u16 = 4401;

/// keyup's close code for "no hello frame arrived inside the deadline".
const CLOSE_NO_HELLO: u16 = 4408;

/// Refuse a frame bigger than this. A legitimate one is a hundred bytes.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Granularity of the default backoff wait, so the stop flag is honoured
/// promptly even in the middle of a 60 s auth backoff.
const WAIT_SLICE: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Reported state
// ---------------------------------------------------------------------------

/// The `uplink.stream` object of the status POST and the panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamReport {
    /// Whether the push channel is configured to run at all.
    pub enabled: bool,
    /// Whether a connection is up right now.
    pub connected: bool,
    /// When the current connection was established (RFC 3339 UTC), or `None`
    /// while it is down.
    pub since: Option<String>,
    /// Connections after the first: how flaky the path to keyup is.
    pub reconnects: u64,
    /// When keyup last told this recorder to fetch.
    pub last_notify_at: Option<String>,
    /// Why the last connection ended, when one has.
    pub last_error: Option<String>,
}

impl StreamReport {
    /// The wire object.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "connected": self.connected,
            "since": self.since,
            "reconnects": self.reconnects,
            "last_notify_at": self.last_notify_at,
            "last_error": self.last_error,
        })
    }
}

/// The live state behind [`StreamReport`], shared with the status reporters.
///
/// Written only by the stream thread and read by the control and panel threads,
/// so the numbers are atomics and the one string sits behind a mutex that is
/// never held across an I/O call.
#[derive(Debug, Default)]
pub(crate) struct StreamState {
    enabled: AtomicBool,
    connected: AtomicBool,
    /// Unix ms the current connection came up; 0 while it is down.
    since_ms: AtomicI64,
    /// Successful connections, including the first.
    connects: AtomicU64,
    /// Unix ms of the last "fetch now" from keyup; 0 for never.
    last_notify_ms: AtomicI64,
    last_error: Mutex<Option<String>>,
}

impl StreamState {
    /// A state block for a recorder with the push channel `enabled` or not.
    pub(crate) fn new(enabled: bool) -> Arc<StreamState> {
        Arc::new(StreamState {
            enabled: AtomicBool::new(enabled),
            ..StreamState::default()
        })
    }

    /// Snapshot for a status POST or the panel.
    pub(crate) fn report(&self) -> StreamReport {
        StreamReport {
            enabled: self.enabled.load(Ordering::Relaxed),
            connected: self.connected.load(Ordering::Relaxed),
            since: stamp(self.since_ms.load(Ordering::Relaxed)),
            // The first connection is not a reconnect.
            reconnects: self.connects.load(Ordering::Relaxed).saturating_sub(1),
            last_notify_at: stamp(self.last_notify_ms.load(Ordering::Relaxed)),
            last_error: lock(&self.last_error).clone(),
        }
    }

    fn note_connected(&self) {
        self.connects.fetch_add(1, Ordering::Relaxed);
        self.since_ms.store(now_ms(), Ordering::Relaxed);
        self.connected.store(true, Ordering::Relaxed);
        *lock(&self.last_error) = None;
    }

    fn note_disconnected(&self, error: Option<String>) {
        self.connected.store(false, Ordering::Relaxed);
        self.since_ms.store(0, Ordering::Relaxed);
        if let Some(e) = error {
            *lock(&self.last_error) = Some(e);
        }
    }

    fn note_notify(&self) {
        self.last_notify_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// Unix ms as an RFC 3339 instant, or `None` for the 0 sentinel.
fn stamp(unix_ms: i64) -> Option<String> {
    (unix_ms != 0).then(|| rfc3339(unix_ms))
}

/// A poisoned status mutex must not take the recorder down: the data behind it
/// is a report, and a stale report beats a panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wall clock in Unix milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How the stream thread reaches keyup.
#[derive(Clone, Debug)]
pub(crate) struct StreamConfig {
    /// `ws(s)://…/api/recorder/stream`. Empty disables the thread.
    pub url: String,
    /// Bearer token (the upload token).
    pub api_key: String,
    /// This instance's stable id, echoed in the hello.
    pub instance_id: String,
    /// Ceiling on the reconnect backoff.
    pub reconnect_max: Duration,
}

/// An interruptible wait, injected so tests can assert *how long* the loop
/// wanted to back off without spending that long.
///
/// Returns `true` when the wait ran to completion and `false` when the stop
/// flag cut it short.
pub(crate) type WaitFn = Arc<dyn Fn(Duration, &AtomicBool) -> bool + Send + Sync>;

/// The production wait: sleep in [`WAIT_SLICE`] slices so a stopping recorder
/// does not have to sit out a 60 s auth backoff.
pub(crate) fn sleeping_wait() -> WaitFn {
    Arc::new(|d: Duration, stop: &AtomicBool| {
        let deadline = std::time::Instant::now() + d;
        loop {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(WAIT_SLICE));
        }
    })
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// Run the push channel until `stop` is set.
pub(crate) fn run(
    cfg: StreamConfig,
    state: Arc<StreamState>,
    recorder: Arc<RecorderControl>,
    fetch_now: Arc<AtomicBool>,
    metrics: trunkd_core::Metrics,
    stop: Arc<AtomicBool>,
) {
    run_with(
        cfg,
        state,
        recorder,
        fetch_now,
        metrics,
        stop,
        sleeping_wait(),
    );
}

/// [`run`] with the wait injected. The whole loop lives here so the tests
/// exercise the production path and not a simplified copy of it.
pub(crate) fn run_with(
    cfg: StreamConfig,
    state: Arc<StreamState>,
    recorder: Arc<RecorderControl>,
    fetch_now: Arc<AtomicBool>,
    metrics: trunkd_core::Metrics,
    stop: Arc<AtomicBool>,
    wait: WaitFn,
) {
    if cfg.url.is_empty() {
        tracing::debug!("keyup push channel disabled: no URL");
        return;
    }
    let notifies = metrics.counter("stream.notifies");
    let connects = metrics.counter("stream.connects");
    let failures = metrics.counter("stream.failures");
    let connected_gauge = metrics.gauge("stream.connected");
    let reconnects_gauge = metrics.gauge("stream.reconnects");

    let mut backoff = BACKOFF_START;
    // The close code last complained about, so a keyup that keeps refusing the
    // same key produces one WARN rather than one a minute.
    let mut warned: Option<u16> = None;

    while !stop.load(Ordering::Relaxed) {
        let started = std::time::Instant::now();
        // One bad frame must not end the push channel for the life of the
        // process; the control loop takes the same precaution.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session(
                &cfg,
                &state,
                &recorder,
                &fetch_now,
                &notifies,
                &connects,
                &connected_gauge,
                &reconnects_gauge,
                &stop,
            )
        }))
        .unwrap_or_else(|_| {
            tracing::error!("keyup push channel panicked; reconnecting");
            Err(Ended::Error("panicked".to_owned()))
        });
        let lasted = started.elapsed();
        connected_gauge.set(0);

        let mut auth = false;
        match &outcome {
            Ok(()) => {
                state.note_disconnected(None);
                break;
            }
            Err(Ended::Closed(code)) => {
                failures.inc();
                let code = *code;
                auth = code == CLOSE_AUTH;
                if warned != Some(code) {
                    warned = Some(code);
                    match code {
                        CLOSE_AUTH => tracing::warn!(
                            code,
                            "keyup refused the push channel's API key; backing off"
                        ),
                        CLOSE_NO_HELLO => tracing::warn!(
                            code,
                            "keyup closed the push channel: our hello did not arrive in time"
                        ),
                        _ => tracing::warn!(code, "keyup closed the push channel"),
                    }
                }
                state.note_disconnected(Some(format!("closed {code}")));
            }
            Err(Ended::Error(e)) => {
                failures.inc();
                warned = None;
                tracing::debug!(error = %e, "keyup push channel dropped; reconnecting");
                state.note_disconnected(Some(e.clone()));
            }
        }

        let this_wait = if auth { AUTH_BACKOFF } else { backoff };
        if !wait(this_wait, &stop) {
            break;
        }
        // A connection that stayed up is evidence the path is healthy: start
        // the next outage's progression from the bottom again.
        backoff = if lasted >= STABLE_CONNECTION {
            BACKOFF_START
        } else {
            (backoff * 2).min(cfg.reconnect_max.max(BACKOFF_START))
        };
    }
    state.note_disconnected(None);
    connected_gauge.set(0);
    tracing::debug!("keyup push channel stopped");
}

/// Why one connection ended.
#[derive(Debug)]
enum Ended {
    /// The server sent a close frame carrying this code.
    Closed(u16),
    /// Connect, handshake, heartbeat or I/O failure.
    Error(String),
}

/// Hold one connection open until it fails or `stop` is set.
///
/// `Ok(())` means the recorder is shutting down; every other end is an `Ended`.
#[allow(clippy::too_many_arguments)]
fn session(
    cfg: &StreamConfig,
    state: &StreamState,
    recorder: &RecorderControl,
    fetch_now: &AtomicBool,
    notifies: &trunkd_core::Metric,
    connects: &trunkd_core::Metric,
    connected_gauge: &trunkd_core::Metric,
    reconnects_gauge: &trunkd_core::Metric,
    stop: &AtomicBool,
) -> Result<(), Ended> {
    let mut ws = connect(cfg)?;
    state.note_connected();
    connects.inc();
    connected_gauge.set(1);
    reconnects_gauge.set(state.report().reconnects);
    tracing::info!(url = %cfg.url, "keyup push channel connected");

    // The hello has to be the first thing on the wire: keyup closes with 4408
    // if it does not arrive inside ten seconds.
    let hello = json!({
        "type": "hello",
        "instance": cfg.instance_id,
        "recorder_version": env!("CARGO_PKG_VERSION"),
        "desired_seen": recorder.desired_seen(),
    });
    send(&mut ws, &hello.to_string())?;

    let mut heartbeat = DEFAULT_HEARTBEAT;
    let mut last_rx = std::time::Instant::now();
    let mut last_tx = std::time::Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            // Best effort: a peer that has already gone away is not an error
            // worth reporting on the way out.
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }
        match ws.read() {
            Ok(msg) => {
                last_rx = std::time::Instant::now();
                match msg {
                    Message::Text(t) => {
                        if let Some(hb) =
                            handle_frame(t.as_str(), state, recorder, fetch_now, notifies)
                        {
                            heartbeat = hb;
                        }
                    }
                    Message::Close(frame) => {
                        let code = frame.map(|f| u16::from(f.code)).unwrap_or(1000);
                        // Answer the close so the peer sees a clean shutdown,
                        // then let the next read report ConnectionClosed.
                        let _ = ws.flush();
                        return Err(Ended::Closed(code));
                    }
                    // Binary payloads, pings and pongs: nothing this contract
                    // uses. tungstenite has already queued the pong for a ping.
                    _ => {}
                }
            }
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Err(Ended::Error("connection closed".to_owned()));
            }
            Err(e) => return Err(Ended::Error(e.to_string())),
        }

        let now = std::time::Instant::now();
        // Two missed heartbeats is the death test. A server that sends nothing
        // at all is indistinguishable from a half-open socket, which is exactly
        // the case a recorder behind NAT hits and the one this catches.
        if now.duration_since(last_rx) > heartbeat * 2 {
            return Err(Ended::Error(format!(
                "no frame for {}s (heartbeat {}s)",
                now.duration_since(last_rx).as_secs(),
                heartbeat.as_secs()
            )));
        }
        if now.duration_since(last_tx) >= heartbeat {
            send(&mut ws, r#"{"type":"ping"}"#)?;
            last_tx = now;
        }
        if let Err(e) = ws.flush() {
            if !would_block(&e) {
                return Err(Ended::Error(e.to_string()));
            }
        }
    }
}

/// Whether a tungstenite error is just "the socket had nothing to say".
fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io)
        if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Write one text frame and push it out.
fn send<S: Read + Write>(ws: &mut WebSocket<S>, text: &str) -> Result<(), Ended> {
    ws.send(Message::text(text.to_owned()))
        .map_err(|e| Ended::Error(e.to_string()))
}

/// Act on one server frame, returning a new heartbeat cadence when the frame
/// stated one.
///
/// Every unknown `type`, and every frame that is not JSON at all, is ignored:
/// keyup is free to grow the protocol without a recorder rollout.
fn handle_frame(
    text: &str,
    state: &StreamState,
    recorder: &RecorderControl,
    fetch_now: &AtomicBool,
    notifies: &trunkd_core::Metric,
) -> Option<Duration> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("push frame is not JSON; ignoring");
        return None;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("hello") => {
            let want = v.get("desired_version").and_then(Value::as_i64);
            // Only when keyup is ahead of us: a reconnect to a server with
            // nothing new must not cost a GET, and the recorder that has never
            // fetched anything wants one either way.
            let behind = match (want, recorder.desired_seen()) {
                (Some(w), Some(seen)) => w > seen,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if behind {
                notify(state, fetch_now, notifies, "hello");
            }
            v.get("heartbeat_s")
                .and_then(Value::as_u64)
                .map(|s| Duration::from_secs(s.clamp(MIN_HEARTBEAT, MAX_HEARTBEAT)))
        }
        Some("desired_state") => {
            // The version is a "go fetch" signal, not data: no comparison, no
            // parse, no state. A redelivery costs one GET that returns the
            // document the recorder already has.
            notify(state, fetch_now, notifies, "desired_state");
            None
        }
        // Liveness only: the read itself already reset the heartbeat deadline.
        Some("heartbeat") | Some("pong") => None,
        other => {
            tracing::debug!(kind = other.unwrap_or("(none)"), "ignoring push frame");
            None
        }
    }
}

/// Ask the control loop for an out-of-cycle desired-state GET.
fn notify(state: &StreamState, fetch_now: &AtomicBool, notifies: &trunkd_core::Metric, why: &str) {
    fetch_now.store(true, Ordering::Relaxed);
    state.note_notify();
    notifies.inc();
    tracing::debug!(why, "keyup pushed: fetching desired state now");
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

/// Open the TCP connection, run the upgrade, and hand back a socket whose read
/// timeout is the loop's polling granularity.
fn connect(cfg: &StreamConfig) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, Ended> {
    let uri: Uri = cfg
        .url
        .parse()
        .map_err(|e| Ended::Error(format!("bad stream URL: {e}")))?;
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        other => {
            return Err(Ended::Error(format!(
                "unsupported stream scheme {:?}",
                other.unwrap_or("")
            )))
        }
    };
    let host = uri
        .host()
        .ok_or_else(|| Ended::Error("stream URL has no host".to_owned()))?;
    // An IPv6 literal arrives bracketed in the URL and unbracketed in a socket
    // address.
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Ended::Error(format!("resolve {host}:{port}: {e}")))?
        .collect();
    let mut sock = None;
    let mut last = String::from("no address resolved");
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let sock = sock.ok_or_else(|| Ended::Error(format!("connect {host}:{port}: {last}")))?;
    let _ = sock.set_nodelay(true);
    // The upgrade is a blocking exchange, so it gets a timeout it can finish
    // inside; the loop's own 250 ms granularity is set once the socket is up.
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .and_then(|()| sock.set_write_timeout(Some(WRITE_TIMEOUT)))
        .map_err(|e| Ended::Error(format!("socket timeouts: {e}")))?;

    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {}", cfg.api_key));
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));

    let ws = if tls {
        tungstenite::client_tls_with_config(request, sock, Some(config), None)
            .map_err(|e| Ended::Error(format!("tls handshake: {e}")))?
            .0
    } else {
        tungstenite::client::client_with_config(request, MaybeTlsStream::Plain(sock), Some(config))
            .map_err(|e| Ended::Error(format!("handshake: {e}")))?
            .0
    };
    tcp_of(ws.get_ref())
        .ok_or_else(|| Ended::Error("no socket behind the websocket".to_owned()))?
        .set_read_timeout(Some(POLL))
        .map_err(|e| Ended::Error(format!("read timeout: {e}")))?;
    Ok(ws)
}

/// The `TcpStream` under a (possibly TLS-wrapped) websocket stream.
fn tcp_of(s: &MaybeTlsStream<TcpStream>) -> Option<&TcpStream> {
    match s {
        MaybeTlsStream::Plain(t) => Some(t),
        // Always present: `Cargo.toml` pins tungstenite's rustls feature on.
        MaybeTlsStream::Rustls(t) => Some(t.get_ref()),
        // `MaybeTlsStream` is `#[non_exhaustive]`; a variant we do not compile
        // support for cannot appear, but the match has to say so.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use trunkd_uplink::UplinkHold;

    /// A recorder-control block whose `desired_seen` starts wherever a test
    /// needs it.
    fn recorder(seen: Option<i64>) -> Arc<RecorderControl> {
        let cfg = crate::config::Config::default();
        let slot = crate::pipeline::link::LinkSlot::new(
            &cfg,
            &[],
            trunkd_core::Metrics::new(),
            "rec-1".to_owned(),
        );
        let rc = RecorderControl::new(
            Arc::new(UplinkHold::new(false)),
            crate::pipeline::eventarchive::ArchiveState::new(false),
            slot,
            trunkd_uplink::BroadcastifyState::new(true, &trunkd_core::Metrics::new()),
            trunkd_core::SkipList::empty(),
        );
        if let Some(v) = seen {
            rc.note_desired_seen(v);
        }
        rc
    }

    fn cfg(url: String) -> StreamConfig {
        StreamConfig {
            url,
            api_key: "kup_test".into(),
            instance_id: "rec-1".into(),
            reconnect_max: Duration::from_secs(30),
        }
    }

    /// What one mock connection saw and what the test wants done about it.
    struct Conn {
        ws: WebSocket<TcpStream>,
        /// The client's hello, already parsed.
        hello: Value,
        /// The `Authorization` header the upgrade carried.
        auth: Option<String>,
        /// The path the upgrade requested.
        path: String,
    }

    /// A mock keyup on 127.0.0.1. `handler` is called once per accepted
    /// connection with the client's hello already read.
    // tungstenite's handshake callback returns a whole HTTP response in its
    // `Err`, which clippy judges too large to be worth moving around. It is the
    // library's signature, not ours.
    #[allow(clippy::result_large_err)]
    fn mock<F>(handler: F) -> (String, Arc<AtomicU64>)
    where
        F: Fn(Conn) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepted = Arc::new(AtomicU64::new(0));
        let counter = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut auth = None;
                let mut path = String::new();
                let Ok(mut ws) = tungstenite::accept_hdr(
                    stream,
                    |req: &tungstenite::handshake::server::Request,
                     resp: tungstenite::handshake::server::Response| {
                        path = req.uri().path().to_owned();
                        auth = req
                            .headers()
                            .get("Authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        Ok(resp)
                    },
                ) else {
                    continue;
                };
                // The contract's first frame.
                let hello = match ws.read() {
                    Ok(Message::Text(t)) => {
                        serde_json::from_str::<Value>(t.as_str()).unwrap_or(Value::Null)
                    }
                    _ => Value::Null,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                handler(Conn {
                    ws,
                    hello,
                    auth,
                    path,
                });
            }
        });
        (
            format!("ws://127.0.0.1:{port}/api/recorder/stream"),
            accepted,
        )
    }

    /// A wait that never really waits, but reports every duration it was asked
    /// for down `tx` — the backoff schedule, observable in milliseconds of test
    /// time rather than minutes of real time.
    fn recording_wait() -> (WaitFn, mpsc::Receiver<Duration>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let wait: WaitFn = Arc::new(move |d: Duration, stop: &AtomicBool| {
            let _ = lock(&tx).send(d);
            !stop.load(Ordering::Relaxed)
        });
        (wait, rx)
    }

    /// [`recording_wait`], except that the `park_on`-th wait (1-based) does not
    /// return until `stop` is set. A test that reads the state a disconnect
    /// left behind needs this: with a wait that returns at once the loop is
    /// already reconnecting, and a successful reconnect clears `last_error`
    /// before the test can look at it. The duration is reported before
    /// parking, so receiving it means the loop is parked and the state frozen.
    fn parking_wait(park_on: usize) -> (WaitFn, mpsc::Receiver<Duration>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let calls = AtomicU64::new(0);
        let wait: WaitFn = Arc::new(move |d: Duration, stop: &AtomicBool| {
            let _ = lock(&tx).send(d);
            if calls.fetch_add(1, Ordering::SeqCst) + 1 == park_on as u64 {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            !stop.load(Ordering::Relaxed)
        });
        (wait, rx)
    }

    /// Spawn the stream thread; the caller joins through the returned handle.
    #[allow(clippy::type_complexity)]
    fn spawn(
        url: String,
        rec: Arc<RecorderControl>,
        wait: WaitFn,
    ) -> (
        std::thread::JoinHandle<()>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<StreamState>,
        trunkd_core::Metrics,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let fetch_now = Arc::new(AtomicBool::new(false));
        let state = StreamState::new(true);
        let metrics = trunkd_core::Metrics::new();
        let handle = {
            let (c, s, f, m, st) = (
                cfg(url),
                state.clone(),
                fetch_now.clone(),
                metrics.clone(),
                stop.clone(),
            );
            std::thread::spawn(move || run_with(c, s, rec, f, m, st, wait))
        };
        (handle, stop, fetch_now, state, metrics)
    }

    /// Block until `f` holds or the deadline passes.
    fn until(f: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn notifies(m: &trunkd_core::Metrics) -> u64 {
        m.counter("stream.notifies").get()
    }

    #[test]
    fn the_stream_endpoint_swaps_the_route_and_the_scheme() {
        use crate::pipeline::control::stream_url;
        assert_eq!(
            stream_url("http://192.0.2.60/api/call-upload"),
            "ws://192.0.2.60/api/recorder/stream"
        );
        assert_eq!(
            stream_url("https://keyup.example/api/call-upload"),
            "wss://keyup.example/api/recorder/stream"
        );
        // A base URL that is not the upload route simply gains the route.
        assert_eq!(
            stream_url("https://keyup.example"),
            "wss://keyup.example/api/recorder/stream"
        );
        // A trailing slash is trimmed, exactly as the sibling routes trim it.
        assert_eq!(
            stream_url("http://192.0.2.60/api/call-upload/"),
            "ws://192.0.2.60/api/recorder/stream"
        );
    }

    #[test]
    fn the_hello_carries_the_instance_the_version_and_the_bearer_token() {
        let (tx, rx) = mpsc::channel();
        let (url, _) = mock(move |c| {
            let _ = tx.send((c.hello.clone(), c.auth.clone(), c.path.clone()));
            // Hold the connection open so the client does not reconnect.
            std::thread::sleep(Duration::from_secs(2));
        });
        let (wait, _waits) = recording_wait();
        let (handle, stop, _fetch, _state, _m) = spawn(url, recorder(Some(7)), wait);

        let (hello, auth, path) = rx.recv_timeout(Duration::from_secs(10)).expect("hello");
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["instance"], "rec-1");
        assert_eq!(hello["recorder_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(hello["desired_seen"], 7);
        assert_eq!(auth.as_deref(), Some("Bearer kup_test"));
        assert_eq!(path, "/api/recorder/stream");

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn a_recorder_that_has_never_fetched_reports_a_null_desired_seen() {
        let (tx, rx) = mpsc::channel();
        let (url, _) = mock(move |c| {
            let _ = tx.send(c.hello.clone());
            std::thread::sleep(Duration::from_secs(2));
        });
        let (wait, _waits) = recording_wait();
        let (handle, stop, _f, _s, _m) = spawn(url, recorder(None), wait);
        let hello = rx.recv_timeout(Duration::from_secs(10)).expect("hello");
        assert!(hello["desired_seen"].is_null(), "{hello}");
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn a_hello_then_a_desired_state_asks_for_exactly_one_fetch_each() {
        let (url, _) = mock(|mut c| {
            // Ahead of the recorder's `desired_seen` (3): one fetch.
            let _ = c.ws.send(Message::text(
                r#"{"type":"hello","server_time":1,"heartbeat_s":15,"desired_version":9,"recorder_block_version":2}"#,
            ));
            // A push while connected: one more.
            let _ = c.ws.send(Message::text(
                r#"{"type":"desired_state","version":10,"recorder_block_version":2}"#,
            ));
            // Noise the recorder must ignore.
            let _ = c.ws.send(Message::text(r#"{"type":"heartbeat","ts":2}"#));
            let _ = c.ws.send(Message::text(r#"{"type":"pong"}"#));
            let _ =
                c.ws.send(Message::text(r#"{"type":"something_new","x":1}"#));
            let _ = c.ws.send(Message::text("not json at all"));
            let _ = c.ws.flush();
            std::thread::sleep(Duration::from_secs(3));
        });
        let (wait, _waits) = recording_wait();
        let (handle, stop, fetch, state, metrics) = spawn(url, recorder(Some(3)), wait);

        assert!(until(|| notifies(&metrics) >= 2), "two notifications");
        assert!(fetch.load(Ordering::Relaxed), "the control loop is woken");
        // And nothing beyond those two: the unknown and malformed frames are
        // ignored rather than treated as pushes.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(notifies(&metrics), 2);

        let report = state.report();
        assert!(report.enabled && report.connected, "{report:?}");
        assert_eq!(report.reconnects, 0);
        assert!(report.since.is_some() && report.last_notify_at.is_some());

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn a_hello_that_is_not_ahead_of_us_costs_no_fetch() {
        let (url, _) = mock(|mut c| {
            let _ = c.ws.send(Message::text(
                r#"{"type":"hello","heartbeat_s":15,"desired_version":3}"#,
            ));
            let _ = c.ws.flush();
            std::thread::sleep(Duration::from_secs(2));
        });
        let (wait, _waits) = recording_wait();
        let (handle, stop, fetch, _s, metrics) = spawn(url, recorder(Some(3)), wait);
        assert!(
            until(|| metrics.counter("stream.connects").get() >= 1),
            "connects"
        );
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(notifies(&metrics), 0);
        assert!(!fetch.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn two_missed_heartbeats_reconnect_on_a_doubling_backoff() {
        // heartbeat_s = 1, then silence: the client must give up after 2 s and
        // come back.
        let (url, accepted) = mock(|mut c| {
            let _ = c.ws.send(Message::text(
                r#"{"type":"hello","heartbeat_s":1,"desired_version":0}"#,
            ));
            let _ = c.ws.flush();
            std::thread::sleep(Duration::from_secs(6));
        });
        // Parked on the second backoff: a third connection would clear the
        // heartbeat error the end of this test reads.
        let (wait, waits) = parking_wait(2);
        let (handle, stop, _f, state, _m) = spawn(url, recorder(Some(0)), wait);

        // The first reconnect wait is the bottom of the progression.
        assert_eq!(
            waits
                .recv_timeout(Duration::from_secs(20))
                .expect("backoff"),
            Duration::from_secs(1)
        );
        // The second connection dies the same way, and the wait has doubled.
        assert_eq!(
            waits
                .recv_timeout(Duration::from_secs(20))
                .expect("backoff"),
            Duration::from_secs(2)
        );
        assert!(accepted.load(Ordering::Relaxed) >= 2, "reconnected");
        assert!(until(|| state.report().reconnects >= 1), "counted");
        assert!(
            state
                .report()
                .last_error
                .unwrap_or_default()
                .contains("no frame"),
            "the heartbeat miss is what is reported"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn an_auth_close_backs_off_a_whole_minute() {
        let (url, _) = mock(|mut c| {
            let _ = c.ws.close(Some(tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::Library(CLOSE_AUTH),
                reason: "bad key".into(),
            }));
            let _ = c.ws.flush();
        });
        // Parked on the first backoff, so the reconnect after it cannot clear
        // the error before it is read.
        let (wait, waits) = parking_wait(1);
        let (handle, stop, _f, state, _m) = spawn(url, recorder(Some(1)), wait);

        assert_eq!(
            waits
                .recv_timeout(Duration::from_secs(20))
                .expect("backoff"),
            AUTH_BACKOFF,
            "4401 waits a minute rather than a second"
        );
        assert_eq!(state.report().last_error.as_deref(), Some("closed 4401"));

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn an_ordinary_close_keeps_the_short_backoff() {
        let (url, _) = mock(|mut c| {
            let _ = c.ws.close(Some(tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::Library(CLOSE_NO_HELLO),
                reason: "no hello".into(),
            }));
            let _ = c.ws.flush();
        });
        let (wait, waits) = parking_wait(1);
        let (handle, stop, _f, state, _m) = spawn(url, recorder(Some(1)), wait);
        assert_eq!(
            waits
                .recv_timeout(Duration::from_secs(20))
                .expect("backoff"),
            BACKOFF_START
        );
        assert_eq!(state.report().last_error.as_deref(), Some("closed 4408"));
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn the_backoff_is_capped_at_the_configured_ceiling() {
        // Nothing is listening, so every attempt fails immediately.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let (wait, waits) = recording_wait();
        let mut c = cfg(format!("ws://127.0.0.1:{port}/api/recorder/stream"));
        c.reconnect_max = Duration::from_secs(4);
        let stop = Arc::new(AtomicBool::new(false));
        let state = StreamState::new(true);
        let metrics = trunkd_core::Metrics::new();
        let handle = {
            let (s, st, m) = (state.clone(), stop.clone(), metrics.clone());
            let rec = recorder(Some(0));
            let fetch = Arc::new(AtomicBool::new(false));
            std::thread::spawn(move || run_with(c, s, rec, fetch, m, st, wait))
        };
        let seen: Vec<Duration> = (0..5)
            .map(|_| {
                waits
                    .recv_timeout(Duration::from_secs(20))
                    .expect("backoff")
            })
            .collect();
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
        assert_eq!(
            seen,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(4),
                Duration::from_secs(4),
            ],
            "doubling, then held at the ceiling"
        );
        assert!(!state.report().connected);
    }

    #[test]
    fn the_stop_flag_ends_a_healthy_connection_promptly() {
        let (url, _) = mock(|mut c| {
            let _ = c.ws.send(Message::text(
                r#"{"type":"hello","heartbeat_s":15,"desired_version":0}"#,
            ));
            let _ = c.ws.flush();
            // Stay quiet and connected until the client goes away.
            std::thread::sleep(Duration::from_secs(30));
        });
        let (wait, _waits) = recording_wait();
        let (handle, stop, _f, state, metrics) = spawn(url, recorder(Some(0)), wait);
        assert!(
            until(|| metrics.counter("stream.connects").get() >= 1),
            "connects"
        );
        stop.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "the stop flag is honoured within 3 s");
        handle.join().expect("joins");
        let report = state.report();
        assert!(!report.connected && report.since.is_none(), "{report:?}");
    }

    #[test]
    fn an_empty_url_runs_nothing() {
        let (wait, waits) = recording_wait();
        let stop = Arc::new(AtomicBool::new(false));
        run_with(
            cfg(String::new()),
            StreamState::new(false),
            recorder(None),
            Arc::new(AtomicBool::new(false)),
            trunkd_core::Metrics::new(),
            stop,
            wait,
        );
        assert!(waits.try_recv().is_err(), "it never even backs off");
    }

    #[test]
    fn a_disabled_channel_reports_itself_disabled() {
        let r = StreamState::new(false).report();
        assert_eq!(
            serde_json::to_value(r.to_json()).expect("json"),
            json!({
                "enabled": false,
                "connected": false,
                "since": Value::Null,
                "reconnects": 0,
                "last_notify_at": Value::Null,
                "last_error": Value::Null,
            })
        );
    }
}
