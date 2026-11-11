// SPDX-License-Identifier: GPL-3.0-or-later
//! The control panel's HTTP transport: accept, read one request, answer it,
//! close — every step on a clock.
//!
//! The panel used to run on `tiny_http`'s server. Its request/response
//! *types* are still what the routes are written against (they are fine), but
//! its connection layer cannot be made safe for a recorder that must not be
//! hung by a browser tab:
//!
//! * it never sets a socket timeout and exposes no socket to set one on, so a
//!   client that stops reading (or advertises a zero window) blocks the thread
//!   writing to it for as long as the TCP stack keeps the connection — and
//!   with one request thread, that is the whole panel, and on Windows the
//!   health monitor then restarts the recorder;
//! * it keeps connections alive and parses pipelined requests, and a request
//!   queued behind a response that never ends (the live audio stream) waits
//!   in `respond` forever;
//! * an HTTP/1.0 client makes it buffer an endless body to learn its length.
//!
//! So this module owns the sockets, and keeps the rules small enough to read:
//!
//! * **One request per connection**, answered with `Connection: close`.
//!   Nothing is ever pipelined, so nothing can queue behind a stream. A panel
//!   polled every two seconds does not miss keep-alive.
//! * **A thread per connection, and a cap on them** ([`Limits::max_in_flight`]).
//!   The accept loop never reads a request or writes a response; past the cap
//!   a connection gets a canned 503 (into an empty send buffer, under a
//!   100 ms timeout) and is closed. A live stream gives its slot back once it
//!   is streaming: streams have their own cap in the monitor hub.
//! * **Deadlines, not just timeouts.** The request head and body must arrive
//!   within [`Limits::read_timeout`] in total (a client trickling a byte a
//!   second does not get to reset the clock), and a response must be written
//!   within [`Limits::write_timeout`] in total. A stream chunk that cannot be
//!   written within [`Limits::stream_write_timeout`] ends the stream. A socket
//!   operation that times out is never retried: Winsock leaves such a socket in
//!   an indeterminate state, so a timeout is the end of the connection.
//! * **Bodies are read before routing**, bounded by [`Limits::max_body`], so a
//!   handler never waits on the network.
//!
//! Every response also carries `X-Frame-Options: DENY` and
//! `Content-Security-Policy: frame-ancestors 'none'`: nothing has any business
//! framing a recorder's control panel.

use std::io::{self, Cursor, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Response, StatusCode};

/// A plain buffered response: every route but the live stream answers one.
pub(crate) type Resp = Response<Cursor<Vec<u8>>>;

/// How often the accept loop looks for a new connection (and at the stop
/// flag) while idle. The listener is non-blocking so a stop is prompt.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// How long a closed-for-writing connection is drained before it is dropped,
/// so a client whose request was refused unread still gets the refusal rather
/// than a reset.
const LINGER: Duration = Duration::from_millis(500);

/// The most a lingering connection will read and throw away.
const LINGER_BYTES: usize = 64 * 1024;

/// How long the over-capacity 503 may take to write. It goes into the
/// empty send buffer of a connection that was accepted a moment ago, so it
/// never needs this long; the bound is for the accept loop's sake.
const BUSY_WRITE: Duration = Duration::from_millis(100);

/// The most headers one request may carry.
const MAX_HEADERS: usize = 100;

/// The transport's bounds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Connections being read or answered at once, streams excepted.
    pub max_in_flight: usize,
    /// The whole request — head and body — must arrive within this.
    pub read_timeout: Duration,
    /// A whole buffered response must be written within this.
    pub write_timeout: Duration,
    /// One chunk of a stream must be written within this.
    pub stream_write_timeout: Duration,
    /// Request line plus headers, in bytes.
    pub max_head: usize,
    /// Request body, in bytes. The largest body any route reads.
    pub max_body: usize,
}

impl Limits {
    /// What the panel runs with.
    pub(crate) const PANEL: Limits = Limits {
        max_in_flight: 16,
        read_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(15),
        stream_write_timeout: Duration::from_secs(10),
        max_head: 16 * 1024,
        max_body: 64 * 1024,
    };
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// One parsed request, its body already read.
///
/// Shaped like `tiny_http::Request` (the accessors the routes use have the
/// same names) so the routing code reads the same as it always has.
pub(crate) struct Request {
    method: Method,
    url: String,
    http11: bool,
    headers: Vec<Header>,
    remote_addr: Option<SocketAddr>,
    body: Cursor<Vec<u8>>,
}

impl Request {
    /// The request method.
    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    /// The request target, path and query, exactly as sent.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Every header, in the order sent.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.headers
    }

    /// The peer, when known.
    pub(crate) fn remote_addr(&self) -> Option<&SocketAddr> {
        self.remote_addr.as_ref()
    }

    /// Whether the client spoke HTTP/1.1 (and so can take a chunked body).
    pub(crate) fn is_http11(&self) -> bool {
        self.http11
    }

    /// The first value of header `name`, if present.
    pub(crate) fn header(&self, name: &'static str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str())
    }

    /// The body, for reading.
    pub(crate) fn as_reader(&mut self) -> &mut dyn Read {
        &mut self.body
    }
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// The connection a handler answers on. A handler calls [`Conn::respond`] or
/// [`Conn::stream`] once; a handler that does neither gets a 500 sent for it.
pub(crate) struct Conn {
    sock: TcpStream,
    limits: Limits,
    stop: Arc<AtomicBool>,
    head_only: bool,
    answered: bool,
    /// A write failed or timed out: the connection is dropped, not drained.
    broken: bool,
    slot: Option<Slot>,
}

impl Conn {
    /// Write a buffered response, bounded by [`Limits::write_timeout`].
    /// A second call is ignored.
    pub(crate) fn respond(&mut self, response: Resp) {
        if self.answered {
            return;
        }
        self.answered = true;
        let bytes = render(response, self.head_only);
        let deadline = Instant::now() + self.limits.write_timeout;
        if let Err(e) = write_all_by(&mut self.sock, &bytes, deadline) {
            self.broken = true;
            tracing::debug!(error = %e, "control panel response not delivered");
        }
    }

    /// Answer 200 with `headers` and a body of unknown length, sent chunked
    /// until `body` ends, the recorder stops, or the client stops taking it.
    ///
    /// The caller must have checked the client speaks HTTP/1.1: chunked is the
    /// only framing an endless body has. Gives the connection's in-flight slot
    /// back first — a stream is capped by whoever produces it, not here.
    pub(crate) fn stream(&mut self, headers: &[Header], body: &mut dyn Read) -> io::Result<()> {
        if self.answered {
            return Ok(());
        }
        self.answered = true;
        self.slot = None;
        // Whatever ends the stream, the connection is not drained afterwards:
        // a listener that went away is gone, and one that is stalled has had
        // its chance.
        self.broken = true;

        let mut head = status_line(StatusCode(200));
        for h in headers.iter().filter(|h| !is_framing(h)) {
            push_header(&mut head, h);
        }
        head.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
        common_headers(&mut head);
        head.extend_from_slice(b"\r\n");
        write_all_by(
            &mut self.sock,
            &head,
            Instant::now() + self.limits.stream_write_timeout,
        )?;
        if self.head_only {
            return Ok(());
        }

        let mut buf = [0u8; 4096];
        let mut chunk = Vec::with_capacity(buf.len() + 16);
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            // One read is one chunk, flushed as it is written: a listener
            // hears each frame as it is encoded, not in bursts, and a stalled
            // one is noticed within one chunk's deadline.
            chunk.clear();
            chunk.extend_from_slice(format!("{n:X}\r\n").as_bytes());
            chunk.extend_from_slice(&buf[..n]);
            chunk.extend_from_slice(b"\r\n");
            write_all_by(
                &mut self.sock,
                &chunk,
                Instant::now() + self.limits.stream_write_timeout,
            )?;
        }
        write_all_by(
            &mut self.sock,
            b"0\r\n\r\n",
            Instant::now() + self.limits.stream_write_timeout,
        )
    }

    /// Close the connection politely: stop writing, drain what the client
    /// still sends for a moment, then drop.
    fn finish(mut self) {
        if !self.broken {
            linger(&mut self.sock);
        }
    }
}

/// An in-flight connection. Dropping it gives the slot back.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(count: &Arc<AtomicUsize>) -> Slot {
        count.fetch_add(1, Ordering::AcqRel);
        Slot(Arc::clone(count))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Accept on `listener` until `stop` is set, answering each connection on a
/// thread of its own with `handler`.
///
/// Returns once `stop` is seen. Connections already accepted finish on their
/// own threads, each bounded by its deadlines; a stream ends at its next
/// chunk.
pub(crate) fn serve<H>(
    listener: TcpListener,
    limits: Limits,
    stop: Arc<AtomicBool>,
    handler: Arc<H>,
) where
    H: Fn(Request, &mut Conn) + Send + Sync + 'static,
{
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::error!(error = %e, "control panel listener cannot poll; not serving");
        return;
    }
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut last_warned: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let (sock, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Out of descriptors, or a connection reset before accept
                // returned it. Keep serving; say so at most every ten seconds.
                if last_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                    tracing::warn!(error = %e, "control panel accept failed; continuing");
                    last_warned = Some(Instant::now());
                }
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        if in_flight.load(Ordering::Acquire) >= limits.max_in_flight {
            refuse_busy(sock);
            continue;
        }
        // Only this thread takes slots, so the check above cannot race.
        let slot = Slot::take(&in_flight);
        let stop = Arc::clone(&stop);
        let handler = Arc::clone(&handler);
        let spawned = std::thread::Builder::new()
            .name("trunkd-http-conn".into())
            .spawn(move || connection(sock, peer, slot, limits, stop, &*handler));
        if let Err(e) = spawned {
            // The closure, the socket and the slot went with the failure.
            tracing::warn!(error = %e, "control panel could not start a connection thread");
        }
    }
}

/// The over-capacity answer, written on the accept thread into the empty send
/// buffer of a fresh connection.
fn refuse_busy(mut sock: TcpStream) {
    let _ = sock.set_nonblocking(false);
    let _ = sock.set_write_timeout(Some(BUSY_WRITE));
    let mut head = status_line(StatusCode(503));
    head.extend_from_slice(b"Retry-After: 1\r\n");
    let body = br#"{"error":"the control panel is busy; retry shortly"}"#;
    head.extend_from_slice(b"Content-Type: application/json; charset=utf-8\r\n");
    head.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    common_headers(&mut head);
    head.extend_from_slice(b"\r\n");
    head.extend_from_slice(body);
    if sock.write_all(&head).is_err() {
        return;
    }
    let _ = sock.shutdown(Shutdown::Write);
    // Take whatever of the request has already arrived, without waiting for
    // more: closing on unread input sends a reset, which can cost the client
    // the 503 it was just sent.
    if sock.set_nonblocking(true).is_ok() {
        let mut sink = [0u8; 4096];
        for _ in 0..4 {
            match sock.read(&mut sink) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
        }
    }
}

/// One connection, start to finish.
fn connection<H>(
    mut sock: TcpStream,
    peer: SocketAddr,
    slot: Slot,
    limits: Limits,
    stop: Arc<AtomicBool>,
    handler: &H,
) where
    H: Fn(Request, &mut Conn) + Send + Sync + 'static,
{
    // Winsock hands a non-blocking listener's connections out non-blocking.
    if sock.set_nonblocking(false).is_err() {
        return;
    }
    let _ = sock.set_nodelay(true);
    let request = read_request(&mut sock, peer, &limits);
    let head_only = matches!(&request, Ok(rq) if rq.method == Method::Head);
    let mut conn = Conn {
        sock,
        limits,
        stop,
        head_only,
        answered: false,
        broken: false,
        slot: Some(slot),
    };
    match request {
        Ok(rq) => {
            handler(rq, &mut conn);
            if !conn.answered {
                conn.respond(error(500, "the handler did not answer"));
            }
        }
        Err(Refusal::Answer(resp)) => conn.respond(resp),
        Err(Refusal::Silent) => return,
    }
    conn.finish();
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Why no request came out of a connection.
enum Refusal {
    /// Say nothing and close: the client went away, or never said anything.
    Silent,
    /// Answer this and close.
    Answer(Resp),
}

fn refuse(status: u16, msg: &str) -> Refusal {
    Refusal::Answer(error(status, msg))
}

/// Read and parse one request, head and body, within `limits.read_timeout`.
fn read_request(
    sock: &mut TcpStream,
    peer: SocketAddr,
    limits: &Limits,
) -> Result<Request, Refusal> {
    let deadline = Instant::now() + limits.read_timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(end) = head_end(&buf) {
            break end;
        }
        if buf.len() > limits.max_head {
            return Err(refuse(431, "request head too large"));
        }
        match read_by(sock, &mut chunk, deadline) {
            Ok(0) => return Err(Refusal::Silent),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if is_timeout(&e) && !buf.is_empty() => {
                return Err(refuse(408, "the request did not arrive in time"))
            }
            Err(_) => return Err(Refusal::Silent),
        }
    };
    if head_end > limits.max_head {
        return Err(refuse(431, "request head too large"));
    }

    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| refuse(400, "request head is not UTF-8"))?;
    let mut lines = head
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .skip_while(|l| l.is_empty());
    let (method, url, http11) = parse_request_line(lines.next().unwrap_or_default())?;

    let mut headers = Vec::new();
    for line in lines.take_while(|l| !l.is_empty()) {
        if line.starts_with([' ', '\t']) {
            return Err(refuse(400, "folded header lines are not accepted"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| refuse(400, "malformed header line"))?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(refuse(400, "malformed header name"));
        }
        if headers.len() >= MAX_HEADERS {
            return Err(refuse(431, "too many headers"));
        }
        let header =
            Header::from_bytes(name.as_bytes(), value.trim_matches([' ', '\t']).as_bytes())
                .map_err(|_| refuse(400, "header is not ASCII"))?;
        headers.push(header);
    }

    // Host: required once on HTTP/1.1, never twice (RFC 9112 §3.2).
    let hosts = headers.iter().filter(|h| h.field.equiv("Host")).count();
    if hosts > 1 || (http11 && hosts == 0) {
        return Err(refuse(400, "a request needs exactly one Host header"));
    }

    // The body. Only `Content-Length` framing is taken: no route needs a
    // chunked upload, and reading one is more parser than this panel wants.
    if headers.iter().any(|h| h.field.equiv("Transfer-Encoding")) {
        return Err(refuse(
            411,
            "send the body with a Content-Length, not chunked",
        ));
    }
    let mut length: Option<usize> = None;
    for h in headers.iter().filter(|h| h.field.equiv("Content-Length")) {
        let n = h
            .value
            .as_str()
            .trim()
            .parse::<usize>()
            .map_err(|_| refuse(400, "Content-Length is not a number"))?;
        if length.is_some_and(|l| l != n) {
            return Err(refuse(400, "conflicting Content-Length headers"));
        }
        length = Some(n);
    }
    let length = length.unwrap_or(0);
    if length > limits.max_body {
        return Err(refuse(413, "request body too large"));
    }

    let mut body = buf.split_off(head_end);
    body.truncate(length);
    if body.len() < length {
        match headers.iter().find(|h| h.field.equiv("Expect")) {
            Some(h) if h.value.as_str().eq_ignore_ascii_case("100-continue") => {
                if http11 {
                    let _ = write_all_by(sock, b"HTTP/1.1 100 Continue\r\n\r\n", deadline);
                }
            }
            Some(_) => return Err(refuse(417, "unsupported Expect")),
            None => {}
        }
    }
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        match read_by(sock, &mut chunk[..want], deadline) {
            Ok(0) => return Err(Refusal::Silent),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if is_timeout(&e) => {
                return Err(refuse(408, "the request body did not arrive in time"))
            }
            Err(_) => return Err(Refusal::Silent),
        }
    }

    Ok(Request {
        method,
        url,
        http11,
        headers,
        remote_addr: Some(peer),
        body: Cursor::new(body),
    })
}

/// Where the head ends (the index just past its blank line), if it has.
/// Bare `\n` line ends are accepted as well as `\r\n`.
fn head_end(buf: &[u8]) -> Option<usize> {
    // Skip blank lines before the request line (RFC 9112 §2.2).
    let start = buf.iter().position(|b| *b != b'\r' && *b != b'\n')?;
    let buf_from = &buf[start..];
    buf_from.iter().enumerate().find_map(|(i, b)| {
        if *b != b'\n' {
            return None;
        }
        match buf_from.get(i + 1..) {
            Some([b'\n', ..]) => Some(start + i + 2),
            Some([b'\r', b'\n', ..]) => Some(start + i + 3),
            _ => None,
        }
    })
}

/// `METHOD SP target SP HTTP/1.x`.
fn parse_request_line(line: &str) -> Result<(Method, String, bool), Refusal> {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(refuse(400, "malformed request line"));
    };
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        v if v.starts_with("HTTP/") => {
            return Err(refuse(505, "this server speaks HTTP/1.0 and HTTP/1.1"))
        }
        _ => return Err(refuse(400, "malformed request line")),
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(refuse(400, "malformed method"));
    }
    if !target.starts_with('/') {
        return Err(refuse(400, "the request target must be a path"));
    }
    let method = method
        .parse::<Method>()
        .map_err(|_| refuse(400, "malformed method"))?;
    Ok((method, target.to_owned(), http11))
}

/// One read under what is left of `deadline`.
fn read_by(sock: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> io::Result<usize> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        sock.set_read_timeout(Some(left))?;
        match sock.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Write all of `buf` before `deadline`, or fail. A timed-out write is never
/// retried (see the module docs).
fn write_all_by(sock: &mut TcpStream, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        sock.set_write_timeout(Some(left))?;
        match sock.write(buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A read or write that ran out of time. Unix says `WouldBlock`, Windows
/// `TimedOut`.
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Half-close, and drain briefly so the client reads the answer before the
/// connection goes away (closing with unread input would send a reset).
fn linger(sock: &mut TcpStream) {
    if sock.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = Instant::now() + LINGER;
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    while drained < LINGER_BYTES {
        match read_by(sock, &mut sink, deadline) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// A JSON `{"error": msg}` response.
pub(crate) fn error(status: u16, msg: &str) -> Resp {
    Response::from_string(serde_json::json!({ "error": msg }).to_string())
        .with_status_code(status)
        .with_header(
            Header::from_bytes(
                &b"Content-Type"[..],
                &b"application/json; charset=utf-8"[..],
            )
            .expect("a static header"),
        )
}

fn status_line(status: StatusCode) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\n",
        status.0,
        status.default_reason_phrase()
    )
    .into_bytes()
}

fn push_header(out: &mut Vec<u8>, h: &Header) {
    out.extend_from_slice(format!("{}: {}\r\n", h.field, h.value.as_str()).as_bytes());
}

/// Headers this module sets itself and a handler may not.
fn is_framing(h: &Header) -> bool {
    h.field.equiv("Content-Length")
        || h.field.equiv("Transfer-Encoding")
        || h.field.equiv("Connection")
}

/// What every response carries: the connection closes after it, and nothing
/// may frame it.
fn common_headers(out: &mut Vec<u8>) {
    out.extend_from_slice(
        b"Connection: close\r\n\
          X-Frame-Options: DENY\r\n\
          Content-Security-Policy: frame-ancestors 'none'\r\n\
          X-Content-Type-Options: nosniff\r\n",
    );
}

/// The bytes of a buffered response.
fn render(response: Resp, head_only: bool) -> Vec<u8> {
    let mut out = status_line(response.status_code());
    for h in response.headers().iter().filter(|h| !is_framing(h)) {
        push_header(&mut out, h);
    }
    let body = response.into_reader().into_inner();
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    common_headers(&mut out);
    out.extend_from_slice(b"\r\n");
    if !head_only {
        out.extend_from_slice(&body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader};

    /// A transport with short deadlines, one route that answers a large body
    /// (`/big`), and one that answers a small one (anything else).
    struct Rig {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Rig {
        fn start(limits: Limits) -> Rig {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");
            let stop = Arc::new(AtomicBool::new(false));
            let handler = Arc::new(|mut rq: Request, conn: &mut Conn| {
                if rq.url() == "/big" {
                    conn.respond(Response::from_data(vec![b'x'; 32 * 1024 * 1024]));
                } else if rq.url() == "/echo" {
                    let mut body = String::new();
                    let _ = rq.as_reader().read_to_string(&mut body);
                    conn.respond(Response::from_string(body));
                } else {
                    conn.respond(Response::from_string("ok"));
                }
            });
            let thread = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || serve(listener, limits, stop, handler))
            };
            Rig {
                addr,
                stop,
                thread: Some(thread),
            }
        }

        /// Send raw bytes, read the whole answer (the server always closes).
        fn exchange(&self, raw: &[u8]) -> String {
            let mut s = TcpStream::connect(self.addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            s.write_all(raw).expect("send");
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn quick() -> Limits {
        Limits {
            max_in_flight: 4,
            read_timeout: Duration::from_millis(500),
            write_timeout: Duration::from_millis(500),
            stream_write_timeout: Duration::from_millis(500),
            ..Limits::PANEL
        }
    }

    #[test]
    fn one_request_is_answered_and_the_connection_closes() {
        let rig = Rig::start(quick());
        let answer = rig
            .exchange(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET /again HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("Connection: close\r\n"));
        assert!(answer.contains("Content-Length: 2\r\n"));
        assert!(
            answer.ends_with("\r\n\r\nok"),
            "a pipelined request is never answered: {answer}"
        );
    }

    #[test]
    fn a_body_is_read_before_the_handler_runs() {
        let rig = Rig::start(quick());
        let answer = rig.exchange(
            b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\nhello",
        );
        assert!(answer.ends_with("hello"), "{answer}");
        let answer = rig.exchange(b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 411 "), "{answer}");
        let answer =
            rig.exchange(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 413 "), "{answer}");
    }

    #[test]
    fn malformed_heads_are_refused_rather_than_guessed_at() {
        let rig = Rig::start(quick());
        for (raw, code) in [
            (&b"GET / HTTP/1.1\r\n\r\n"[..], "400"),
            (b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", "400"),
            (b"GET / HTTP/2.0\r\nHost: a\r\n\r\n", "505"),
            (b"GET http://x/ HTTP/1.1\r\nHost: a\r\n\r\n", "400"),
            (b"GARBAGE\r\n\r\n", "400"),
            (b"GET / HTTP/1.1\r\nHost: a\r\n folded\r\n\r\n", "400"),
        ] {
            let answer = rig.exchange(raw);
            assert!(
                answer.starts_with(&format!("HTTP/1.1 {code} ")),
                "{:?} -> {answer}",
                String::from_utf8_lossy(raw)
            );
        }
        // HTTP/1.0 needs no Host.
        assert!(rig
            .exchange(b"GET / HTTP/1.0\r\n\r\n")
            .starts_with("HTTP/1.1 200 "));
    }

    #[test]
    fn a_trickled_head_is_cut_off_at_the_deadline() {
        let rig = Rig::start(quick());
        let mut s = TcpStream::connect(rig.addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let started = Instant::now();
        s.write_all(b"GET / HTTP/1.1\r\n").expect("send");
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 408 "), "{answer}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// The accept loop answers nothing itself: a client that stops reading a
    /// 32 MB answer does not delay anyone else, and its writer gives up at
    /// the deadline, freeing its slot.
    #[test]
    fn a_slow_reader_neither_blocks_others_nor_holds_its_slot() {
        let rig = Rig::start(Limits {
            max_in_flight: 2,
            ..quick()
        });
        let mut stalled = TcpStream::connect(rig.addr).expect("connect");
        stalled
            .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("send");
        // Read the status line only, so the writer is certainly under way,
        // then stop reading.
        let mut first = String::new();
        BufReader::new(&stalled)
            .read_line(&mut first)
            .expect("a status line");
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");

        let started = Instant::now();
        let answer = rig.exchange(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(answer.ends_with("ok"), "{answer}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "another request waited on the stalled one"
        );

        // The stalled writer gives up at its deadline and its slot comes back:
        // with a cap of two, one more connection holding a slot would leave
        // no room for a request if it had not.
        std::thread::sleep(Duration::from_millis(1000));
        let mut holder = TcpStream::connect(rig.addr).expect("connect");
        holder.write_all(b"GET / HTTP/1.1\r\n").expect("send");
        std::thread::sleep(Duration::from_millis(100));
        let answer = rig.exchange(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            answer.ends_with("ok"),
            "the stalled slot never came back: {answer}"
        );
        drop(holder);
        drop(stalled);
    }

    #[test]
    fn past_the_cap_a_connection_is_told_so() {
        let rig = Rig::start(Limits {
            max_in_flight: 1,
            read_timeout: Duration::from_secs(3),
            ..quick()
        });
        // Holds the one slot by never finishing its head.
        let mut holder = TcpStream::connect(rig.addr).expect("connect");
        holder.write_all(b"GET / HTTP/1.1\r\n").expect("send");
        std::thread::sleep(Duration::from_millis(100));
        let mut refused = TcpStream::connect(rig.addr).expect("connect");
        refused
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut answer = String::new();
        let _ = refused.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 503 "), "{answer}");
        assert!(answer.contains("X-Frame-Options: DENY"));
        drop(holder);
    }

    #[test]
    fn head_ends_with_either_line_ending() {
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(head_end(b"GET / HTTP/1.1\n\nbody"), Some(16));
        assert_eq!(head_end(b"\r\nGET / HTTP/1.1\r\n\r\n"), Some(20));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(head_end(b""), None);
    }
}
