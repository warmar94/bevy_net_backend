//! [`TungsteniteTransport`]: every link on its own std thread (tungstenite, sync).
//!
//! The thread connects (TCP, through an `http://` proxy's `CONNECT` tunnel when the environment
//! sets one, then rustls with the crate's explicit ring config for `wss://`, then the WebSocket
//! handshake), all within ONE absolute deadline (the connect timeout), then loops:
//! apply commands (send / close), heartbeat ping, flush, one read with a budget of one read
//! timeout, dead-peer check. The budget is enforced below rustls and tungstenite
//! ([`TimedTcp`]), so a peer that trickles bytes can neither stretch the handshake past its limit
//! nor keep a read going long enough to starve outgoing frames and pings. Events go back over a
//! channel that `poll` drains without blocking.
//!
//! Credentials never pass through tungstenite: it logs the whole handshake request and every frame
//! it sends at TRACE (`log` crate). So the crate writes the HTTP upgrade request itself (one
//! buffer, wiped after the write), checks the answer as tungstenite does, and only then hands the
//! stream to tungstenite (`WebSocket::from_partially_read`). The first-message authentication text
//! is written by the crate too: one masked text frame from a wiped buffer.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::error::{ProtocolError, SubProtocolError};
use tungstenite::handshake::machine::TryParse;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};
use zeroize::Zeroizing;

use super::proxy::{self, ProxyEnv};
use super::transport::{WsHandshake, WsLinkEvent, WsLinkId, WsTransport};
use super::WsFrame;
use crate::config::MAX_TIMEOUT;
use crate::response::{BackendError, RawResponse};

type Event = (WsLinkId, WsLinkEvent);

enum Command {
    Send(WsFrame),
    /// The first-message authentication text: written by the crate, wiped after the write.
    SendAuth(Zeroizing<String>),
    Close(u16),
}

/// The longest handshake answer header accepted.
const MAX_ANSWER_HEADER: usize = 64 * 1024;

/// How many bytes of received frames a link may have waiting for the game (times the message
/// limit) before it is closed with 1008: a game that stops reading must not grow memory forever.
const EVENT_BUDGET_MESSAGES: usize = 32;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `now + duration` that never overflows (clamped to one hour, then to now).
fn deadline_after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration.min(MAX_TIMEOUT)).unwrap_or(now)
}

struct LinkHandle {
    commands: Sender<Command>,
    queued_bytes: Arc<AtomicUsize>,
}

/// The real WebSocket transport (feature `ws`): tungstenite 0.30 (no permessage-deflate) on one
/// std thread per link, named `net-backend-ws-link#N`. TLS is rustls with ring, passed explicitly,
/// and Mozilla's roots, exactly like the HTTP transport. No async runtime.
///
/// The proxy comes from the same environment variables as for HTTP (`HTTPS_PROXY` / `HTTP_PROXY` /
/// `ALL_PROXY`, with `NO_PROXY`), read when the transport is created; loopback hosts never use
/// it. An `http://` proxy carries the connection through a `CONNECT` tunnel (with
/// `Proxy-Authorization: Basic` when the proxy URL has a user); a connection that should go through
/// an `https://` or SOCKS proxy fails with [`BackendError::InvalidRequest`].
///
/// The handshake request and the first-message authentication frame are written by the crate,
/// not by tungstenite, so the credentials in them never reach tungstenite's `trace` log lines.
///
/// A thread is never joined: a closed link's thread finishes its close handshake (at most 1 s)
/// and exits on its own. Frames the game sends are queued without a limit while a large write is
/// in progress; received frames waiting for the game are limited to 32 times the message limit
/// (then the link closes with 1008).
pub struct TungsteniteTransport {
    proxy: Arc<ProxyEnv>,
    links: HashMap<WsLinkId, LinkHandle>,
    events_tx: Sender<Event>,
    events_rx: Mutex<Receiver<Event>>,
    immediate: Vec<Event>,
}

impl fmt::Debug for TungsteniteTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TungsteniteTransport").field("links", &self.links.len()).field("proxy", &self.proxy).finish_non_exhaustive()
    }
}

impl Default for TungsteniteTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl TungsteniteTransport {
    /// A transport with no links; each `open` starts a thread. Reads the proxy environment
    /// variables now.
    pub fn new() -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self { proxy: Arc::new(ProxyEnv::from_env()), links: HashMap::new(), events_tx, events_rx: Mutex::new(events_rx), immediate: Vec::new() }
    }
}

impl WsTransport for TungsteniteTransport {
    fn open(&mut self, link: WsLinkId, handshake: WsHandshake) {
        let (commands_tx, commands_rx) = mpsc::channel();
        let events = self.events_tx.clone();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&queued_bytes);
        let proxy = Arc::clone(&self.proxy);
        let spawned = thread::Builder::new().name(format!("net-backend-ws-{link}")).spawn(move || {
            let sink = EventSink { link, events, queued_bytes: counter };
            let last = catch_unwind(AssertUnwindSafe(|| session(&handshake, &commands_rx, &sink, None, &proxy)))
                .unwrap_or_else(|panic| WsLinkEvent::Failed(BackendError::Network(format!("the WebSocket thread panicked: {}", panic_text(panic.as_ref())))));
            let _ = sink.events.send((link, last));
        });
        match spawned {
            Ok(_) => {
                self.links.insert(link, LinkHandle { commands: commands_tx, queued_bytes });
            }
            Err(e) => self.immediate.push((link, WsLinkEvent::Failed(BackendError::Network(format!("could not start a WebSocket thread: {e}"))))),
        }
    }

    fn send(&mut self, link: WsLinkId, frame: WsFrame) {
        if let Some(handle) = self.links.get(&link) {
            let _ = handle.commands.send(Command::Send(frame));
        }
    }

    fn send_auth(&mut self, link: WsLinkId, text: String) {
        let text = Zeroizing::new(text);
        if let Some(handle) = self.links.get(&link) {
            let _ = handle.commands.send(Command::SendAuth(text));
        }
    }

    fn close(&mut self, link: WsLinkId, code: u16) {
        if let Some(handle) = self.links.remove(&link) {
            let _ = handle.commands.send(Command::Close(code));
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        let mut out = std::mem::take(&mut self.immediate);
        let rx = lock(&self.events_rx);
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        drop(rx);
        for (link, event) in &out {
            match event {
                WsLinkEvent::Frame(frame) => {
                    if let Some(handle) = self.links.get(link) {
                        let _ = handle.queued_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(frame.len())));
                    }
                }
                WsLinkEvent::Closed { .. } | WsLinkEvent::Failed(_) => {
                    self.links.remove(link);
                }
                _ => {}
            }
        }
        out
    }

    fn shutdown(&mut self) {
        for (_, handle) in self.links.drain() {
            let _ = handle.commands.send(Command::Close(1001));
        }
    }
}

impl Drop for TungsteniteTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn panic_text(panic: &(dyn Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

/// Where a link thread reports; counts the bytes of frames the game has not taken yet.
struct EventSink {
    link: WsLinkId,
    events: Sender<Event>,
    queued_bytes: Arc<AtomicUsize>,
}

impl EventSink {
    fn send(&self, event: WsLinkEvent) -> bool {
        self.events.send((self.link, event)).is_ok()
    }
}

/// The TCP socket with an absolute read deadline. Every `read` waits at most until the deadline
/// (and at most `max_wait`); past it, it fails with `TimedOut` without touching the socket. rustls
/// and tungstenite keep partial records / frames across that error, so the caller can continue.
struct TimedTcp {
    tcp: TcpStream,
    deadline: Option<Instant>,
    max_wait: Duration,
    applied: Option<Duration>,
    last_byte: Instant,
}

impl TimedTcp {
    fn new(tcp: TcpStream, deadline: Option<Instant>, max_wait: Duration) -> Self {
        Self { tcp, deadline, max_wait, applied: None, last_byte: Instant::now() }
    }
}

impl Read for TimedTcp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut wait = self.max_wait;
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "read deadline reached"));
            }
            wait = wait.min(left);
        }
        let wait = wait.max(Duration::from_millis(1));
        if self.applied != Some(wait) {
            self.tcp.set_read_timeout(Some(wait))?;
            self.applied = Some(wait);
        }
        // EINTR (a signal) on a socket with a receive timeout: just read again.
        let n = loop {
            match self.tcp.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other?,
            }
        };
        if n > 0 {
            self.last_byte = Instant::now();
        }
        Ok(n)
    }
}

impl Write for TimedTcp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// A plain or TLS stream over [`TimedTcp`].
enum Stream {
    Plain(TimedTcp),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TimedTcp>>),
}

impl Stream {
    fn timed(&self) -> &TimedTcp {
        match self {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(tls) => &tls.sock,
        }
    }

    fn timed_mut(&mut self) -> &mut TimedTcp {
        match self {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(tls) => &mut tls.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.read(buf),
            Stream::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.write(buf),
            Stream::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(tcp) => tcp.flush(),
            Stream::Tls(tls) => tls.flush(),
        }
    }
}

/// A read timeout: Windows says `TimedOut`, Unix says `WouldBlock`. Both mean "no data yet".
fn is_no_data(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// tungstenite's error in the crate's kinds, with its own words.
fn map_error(error: tungstenite::Error) -> BackendError {
    match error {
        tungstenite::Error::Http(response) => {
            let (parts, body) = (*response).into_parts();
            BackendError::Status(Box::new(RawResponse { status: parts.status, headers: parts.headers, body: body.unwrap_or_default() }))
        }
        tungstenite::Error::Io(ref io) if is_no_data(io) => BackendError::Timeout(format!("socket: {io}")),
        tungstenite::Error::Io(ref io) if io.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) => BackendError::Tls(error.to_string()),
        tungstenite::Error::Capacity(_) => BackendError::disconnected("a message was larger than the limit (closed with 1009)", None),
        tungstenite::Error::Url(_) | tungstenite::Error::HttpFormat(_) => BackendError::InvalidRequest(error.to_string()),
        _ => BackendError::Network(error.to_string()),
    }
}

fn host_for_connect(uri: &http::Uri) -> Option<String> {
    let host = uri.host()?;
    Some(host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host).to_string())
}

/// Whether the plugin already let go of this link (a `Close`, or the channel is gone). Only a
/// `Close` can arrive before `Opened`: the plugin sends frames only on open links.
fn close_requested(commands: &Receiver<Command>) -> bool {
    matches!(commands.try_recv(), Ok(Command::Close(_)) | Err(TryRecvError::Disconnected))
}

/// A TCP connection to `host:port` within `deadline` (DNS is not under it: std has no resolve
/// timeout). `proxy`: the peer is the proxy (named in errors).
fn tcp_connect(host: &str, port: u16, deadline: Instant, proxy: bool) -> Result<TcpStream, BackendError> {
    let what = if proxy { "the proxy" } else { "the host" };
    let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| BackendError::Network(format!("could not resolve {what}: {e}")))?.collect();
    let mut last = BackendError::Network(format!("{what} resolved to no address"));
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(BackendError::Timeout("connect limit".into()));
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(stream) => return Ok(stream),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => last = BackendError::Timeout("connect limit".into()),
            Err(e) if proxy => last = BackendError::Network(format!("could not connect to the proxy: {e}")),
            Err(e) => last = BackendError::Network(format!("could not connect: {e}")),
        }
    }
    Err(last)
}

/// TCP (+ the proxy's tunnel) (+ TLS) + WebSocket handshake, all within ONE deadline
/// (`connect_timeout`). `None` when the plugin let go of the link before the upgrade request was
/// written.
fn connect(
    handshake: &WsHandshake,
    commands: &Receiver<Command>,
    tls: Option<Arc<rustls::ClientConfig>>,
    proxy: &ProxyEnv,
) -> Result<Option<WebSocket<Stream>>, BackendError> {
    let deadline = deadline_after(handshake.connect_timeout);
    let host = host_for_connect(&handshake.uri).ok_or_else(|| BackendError::InvalidRequest("the URL has no host".into()))?;
    let secure = handshake.is_secure();
    let port = handshake.uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let route = proxy.route(&handshake.uri)?;
    let tcp = match route {
        None => tcp_connect(&host, port, deadline, false)?,
        Some(proxy) => {
            let (proxy_host, proxy_port) = proxy.address();
            tcp_connect(proxy_host, proxy_port, deadline, true)?
        }
    };
    let io_error = |e: io::Error| BackendError::Network(format!("socket setup: {e}"));
    tcp.set_nodelay(true).map_err(io_error)?;
    tcp.set_write_timeout(Some(write_limit(handshake))).map_err(io_error)?;
    // Every read of the handshake (proxy, TLS and HTTP) stops at the connect deadline.
    let mut timed = TimedTcp::new(tcp, Some(deadline), handshake.connect_timeout.min(MAX_TIMEOUT));
    if let Some(proxy) = route {
        // `Uri::host` keeps an IPv6 address in brackets, as `CONNECT` needs it.
        let target = format!("{}:{port}", handshake.uri.host().unwrap_or(host.as_str()));
        proxy::connect_tunnel(&mut timed, proxy, &target, deadline)?;
    }
    let mut stream = if secure {
        let config = match tls {
            Some(config) => config,
            None => crate::tls::client_config().map_err(BackendError::Tls)?,
        };
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| BackendError::InvalidRequest(format!("bad TLS server name: {e}")))?;
        let connection = rustls::ClientConnection::new(config, name).map_err(|e| BackendError::Tls(e.to_string()))?;
        Stream::Tls(Box::new(rustls::StreamOwned::new(connection, timed)))
    } else {
        Stream::Plain(timed)
    };
    if close_requested(commands) {
        return Ok(None);
    }
    let config = WebSocketConfig::default().max_message_size(Some(handshake.max_message_bytes)).max_frame_size(Some(handshake.max_message_bytes));
    let past_deadline = |error: BackendError| if Instant::now() >= deadline { BackendError::Timeout("connect limit".into()) } else { error };
    let tail = upgrade(&mut stream, handshake, deadline).map_err(past_deadline)?;
    let mut ws = WebSocket::from_partially_read(stream, tail, Role::Client, Some(config));
    let timed = ws.get_mut().timed_mut();
    timed.deadline = None;
    timed.max_wait = handshake.read_timeout;
    Ok(Some(ws))
}

/// The WebSocket headers the crate writes itself; the same names from the game are left out.
const WS_HEADERS: [&str; 5] = ["host", "connection", "upgrade", "sec-websocket-version", "sec-websocket-key"];

/// The HTTP/1.1 upgrade, written and checked by the crate (tungstenite 0.30's rules, without its
/// `trace` line of the whole request). Returns the bytes the server sent after the `101` answer
/// (the start of the WebSocket stream).
fn upgrade(stream: &mut Stream, handshake: &WsHandshake, deadline: Instant) -> Result<Vec<u8>, BackendError> {
    let io_failed = |e: io::Error| map_error(tungstenite::Error::Io(e));
    let uri = &handshake.uri;
    let authority = uri.authority().ok_or_else(|| BackendError::InvalidRequest("the URL has no host".into()))?.as_str();
    let host = authority.rfind('@').and_then(|at| authority.get(at + 1..)).unwrap_or(authority);
    if host.is_empty() {
        return Err(BackendError::InvalidRequest("the URL has an empty host".into()));
    }
    let target = uri.path_and_query().map(http::uri::PathAndQuery::as_str).filter(|p| !p.is_empty()).unwrap_or("/");
    let key = tungstenite::handshake::client::generate_key();
    let fixed = [("Host", host), ("Connection", "Upgrade"), ("Upgrade", "websocket"), ("Sec-WebSocket-Version", "13"), ("Sec-WebSocket-Key", key.as_str())];
    let extra = || handshake.headers.iter().filter(|(name, _)| !WS_HEADERS.contains(&name.as_str()));
    // Sized up front: the buffer that holds the credentials is never reallocated (no stray copy).
    let size = 16
        + target.len()
        + fixed.iter().map(|(n, v)| n.len() + v.len() + 4).sum::<usize>()
        + extra().map(|(n, v)| n.as_str().len() + v.len() + 4).sum::<usize>()
        + 2;
    let mut request = Zeroizing::new(Vec::with_capacity(size));
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in fixed {
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    for (name, value) in extra() {
        // Some servers compare these two names case-sensitively (tungstenite writes them so too).
        let name = match name.as_str() {
            "sec-websocket-protocol" => "Sec-WebSocket-Protocol",
            "origin" => "Origin",
            other => other,
        };
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    write_all_within(stream, &request, deadline).map_err(io_failed)?;
    drop(request);
    // The answer header (bytes after it are kept: the server may already have sent frames).
    let mut received = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let (size, response) = loop {
        if let Some(parsed) = tungstenite::handshake::client::Response::try_parse(&received).map_err(map_error)? {
            break parsed;
        }
        if received.len() > MAX_ANSWER_HEADER {
            return Err(BackendError::Network(format!("the handshake answer's header is longer than {MAX_ANSWER_HEADER} bytes")));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(BackendError::Network("the server closed the connection during the WebSocket handshake".into())),
            Ok(n) => received.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_no_data(&e) => {
                if Instant::now() >= deadline {
                    return Err(BackendError::Timeout("connect limit".into()));
                }
            }
            Err(e) => return Err(io_failed(e)),
        }
    };
    let tail = received.get(size..).map(<[u8]>::to_vec).unwrap_or_default();
    check_answer(response, &key, handshake, tail)
}

/// The checks of RFC 6455 4.1 as tungstenite 0.30 makes them (`VerifyData::verify_response`). A
/// refusal (not `101`) is a `Status` error with the bytes of its body that came with the header.
fn check_answer(response: tungstenite::handshake::client::Response, key: &str, handshake: &WsHandshake, tail: Vec<u8>) -> Result<Vec<u8>, BackendError> {
    let protocol_error = |error: ProtocolError| map_error(tungstenite::Error::Protocol(error));
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let (parts, _) = response.into_parts();
        return Err(BackendError::Status(Box::new(RawResponse { status: parts.status, headers: parts.headers, body: tail })));
    }
    let headers = response.headers();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !header("Upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket")) {
        return Err(protocol_error(ProtocolError::MissingUpgradeWebSocketHeader));
    }
    if !header("Connection").is_some_and(|v| v.eq_ignore_ascii_case("Upgrade")) {
        return Err(protocol_error(ProtocolError::MissingConnectionUpgradeHeader));
    }
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    if headers.get("Sec-WebSocket-Accept").is_none_or(|v| v != accept.as_str()) {
        return Err(protocol_error(ProtocolError::SecWebSocketAcceptKeyMismatch));
    }
    let asked: Option<Vec<String>> =
        handshake.headers.get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    match (headers.get("Sec-WebSocket-Protocol"), &asked) {
        (None, Some(_)) => return Err(protocol_error(ProtocolError::SecWebSocketSubProtocolError(SubProtocolError::NoSubProtocol))),
        (Some(_), None) => return Err(protocol_error(ProtocolError::SecWebSocketSubProtocolError(SubProtocolError::ServerSentSubProtocolNoneRequested))),
        (Some(chosen), Some(asked)) if !chosen.to_str().is_ok_and(|chosen| asked.iter().any(|a| a == chosen)) => {
            return Err(protocol_error(ProtocolError::SecWebSocketSubProtocolError(SubProtocolError::InvalidSubProtocol)));
        }
        _ => {}
    }
    Ok(tail)
}

/// `write_all` + `flush` that keeps going after a read timeout below rustls (a TLS handshake read
/// under the write) until `deadline`.
fn write_all_within(stream: &mut Stream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "connect limit");
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "the connection took no data")),
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_no_data(&e) && Instant::now() < deadline => {}
            Err(e) if is_no_data(&e) => return Err(timed_out()),
            Err(e) => return Err(e),
        }
    }
    loop {
        match stream.flush() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_no_data(&e) && Instant::now() < deadline => {}
            Err(e) if is_no_data(&e) => return Err(timed_out()),
            Err(e) => return Err(e),
        }
    }
}

/// One final, masked text frame (RFC 6455 5.2) holding `text`, built in a wiped buffer. The mask
/// comes from the operating system's secure random source (through ring).
fn masked_text_frame(text: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut mask = [0u8; 4];
    crate::tls::random_bytes(&mut mask).map_err(|()| io::Error::other("no secure random source for the frame mask"))?;
    let len = text.len();
    let mut frame = Zeroizing::new(Vec::with_capacity(len + 14));
    frame.push(0x81); // FIN + text
    if len < 126 {
        frame.push(0x80 | u8::try_from(len).unwrap_or(0));
    } else if let Ok(len) = u16::try_from(len) {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&len.to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(text.iter().zip(mask.iter().cycle()).map(|(byte, m)| byte ^ m));
    Ok(frame)
}

/// Send the first-message authentication text: whatever tungstenite still holds goes out first,
/// then the crate's own frame, written straight to the stream.
fn write_auth(ws: &mut WebSocket<Stream>, text: &str) -> Result<(), tungstenite::Error> {
    ws.flush()?;
    let frame = masked_text_frame(text.as_bytes())?;
    let stream = ws.get_mut();
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

fn to_message(frame: WsFrame) -> Message {
    match frame {
        WsFrame::Text(text) => Message::text(text),
        WsFrame::Binary(bytes) => Message::binary(bytes),
    }
}

fn close_with(ws: &mut WebSocket<Stream>, code: CloseCode) {
    let _ = ws.close(Some(CloseFrame { code, reason: "".into() }));
}

/// One link from connect to its end; returns the last event (`Closed` or `Failed`).
///
/// Each loop turn is bounded: commands and pings, a flush, and ONE `ws.read()` whose socket reads
/// together stop after `read_timeout` (a trickling peer cannot hold the read longer). So a frame
/// the game sends goes out within about one read timeout, plus the time the socket needs to take
/// earlier outgoing data.
fn session(handshake: &WsHandshake, commands: &Receiver<Command>, sink: &EventSink, tls: Option<Arc<rustls::ClientConfig>>, proxy: &ProxyEnv) -> WsLinkEvent {
    let closed_by_game = || WsLinkEvent::Closed { code: None, reason: "closed by the game".into() };
    let mut ws = match connect(handshake, commands, tls, proxy) {
        Ok(Some(ws)) => ws,
        Ok(None) => return closed_by_game(),
        Err(error) => return WsLinkEvent::Failed(error),
    };
    // A quick re-connect may have let go of this link during the handshake: never report it open.
    if close_requested(commands) {
        close_with(&mut ws, CloseCode::Normal);
        let _ = ws.flush();
        return closed_by_game();
    }
    if !sink.send(WsLinkEvent::Opened) {
        return WsLinkEvent::Closed { code: None, reason: "the plugin is gone".into() };
    }
    let budget = EVENT_BUDGET_MESSAGES.saturating_mul(handshake.max_message_bytes);
    let write_limit = write_limit(handshake);
    let mut last_ping = Instant::now();
    // The last sign of life from the server: a received message, or (below the protocol) a byte.
    // Time spent blocked in the link's own writes is not counted against it.
    let mut alive = Instant::now();
    let mut closing: Option<Instant> = None;
    // Set when the CLIENT closes because of an error (1008 / 1009): the link reports it after the close
    // handshake had its chance to reach the server.
    let mut failure: Option<BackendError> = None;
    let mut close_code: Option<u16> = None;
    let mut close_reason = String::new();
    let finish = |failure: Option<BackendError>, code: Option<u16>, reason: String| match failure {
        Some(error) => WsLinkEvent::Failed(error),
        None => WsLinkEvent::Closed { code, reason },
    };
    // A write that timed out: the server took no data for `write_limit` (fatal: on Windows a
    // timed-out send leaves the socket in an undefined state).
    let write_failed = |e: tungstenite::Error| match e {
        tungstenite::Error::Io(ref io) if is_no_data(io) => BackendError::Timeout(format!("the server accepted no data for {write_limit:?}")),
        other => map_error(other),
    };
    loop {
        let before_writes = Instant::now();
        // Commands from the plugin.
        loop {
            match commands.try_recv() {
                Ok(Command::Send(frame)) if closing.is_none() => {
                    if let Err(e) = ws.write(to_message(frame)) {
                        return WsLinkEvent::Failed(write_failed(e));
                    }
                }
                Ok(Command::SendAuth(text)) if closing.is_none() => {
                    if let Err(e) = write_auth(&mut ws, &text) {
                        return WsLinkEvent::Failed(write_failed(e));
                    }
                }
                Ok(Command::Send(_) | Command::SendAuth(_)) => {}
                Ok(Command::Close(code)) => {
                    if closing.is_none() {
                        close_with(&mut ws, CloseCode::from(code));
                        closing = Some(Instant::now());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if closing.is_none() {
                        close_with(&mut ws, CloseCode::Away);
                        closing = Some(Instant::now());
                    }
                    break;
                }
            }
        }
        // Heartbeat (never after a close frame, either way).
        if closing.is_none() && last_ping.elapsed() >= handshake.ping_interval {
            if let Err(e) = ws.write(Message::Ping(Default::default())) {
                return WsLinkEvent::Failed(write_failed(e));
            }
            last_ping = Instant::now();
        }
        match ws.flush() {
            Ok(()) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return finish(failure, close_code, close_reason),
            Err(_) if closing.is_some() => return finish(failure, close_code, close_reason),
            Err(e) => return WsLinkEvent::Failed(write_failed(e)),
        }
        // Time blocked on the link's own writes does not count as the server's silence.
        let blocked = before_writes.elapsed();
        if blocked > handshake.read_timeout {
            alive = alive.checked_add(blocked).map_or_else(Instant::now, |a| a.min(Instant::now()));
        }
        // One read, with a budget of one read timeout for all its socket reads.
        ws.get_mut().timed_mut().deadline = Some(deadline_after(handshake.read_timeout));
        match ws.read() {
            Ok(message) => {
                alive = Instant::now();
                let frame = match message {
                    Message::Text(text) => Some(WsFrame::Text(text.as_str().to_string())),
                    Message::Binary(bytes) => Some(WsFrame::Binary(bytes.to_vec())),
                    Message::Close(frame) => {
                        if let Some(frame) = frame {
                            close_code = Some(u16::from(frame.code));
                            close_reason = frame.reason.as_str().to_string();
                        }
                        // tungstenite answers the close; nothing else goes out from now on.
                        closing.get_or_insert_with(Instant::now);
                        None
                    }
                    _ => None,
                };
                // After the client closed because of an error, frames are discarded.
                if let (Some(frame), None) = (frame, &failure) {
                    let len = frame.len();
                    if sink.queued_bytes.load(Ordering::SeqCst).saturating_add(len) > budget {
                        close_with(&mut ws, CloseCode::Policy);
                        closing.get_or_insert_with(Instant::now);
                        failure = Some(BackendError::disconnected("the game did not take the received frames fast enough (closed with 1008)", None));
                    } else {
                        sink.queued_bytes.fetch_add(len, Ordering::SeqCst);
                        if !sink.send(WsLinkEvent::Frame(frame)) && closing.is_none() {
                            close_with(&mut ws, CloseCode::Away);
                            closing = Some(Instant::now());
                        }
                    }
                }
            }
            Err(tungstenite::Error::Io(ref e)) if is_no_data(e) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return finish(failure, close_code, close_reason),
            // After a close frame (either way) many servers drop TCP without a TLS close_notify.
            Err(_) if closing.is_some() => return finish(failure, close_code, close_reason),
            Err(tungstenite::Error::Capacity(_)) => {
                // tungstenite must not read again after a capacity error (it asserts on its
                // buffer). Send the close, then drain the socket below it for up to 1 s so the
                // close reaches the server instead of a reset.
                close_with(&mut ws, CloseCode::Size);
                let _ = ws.flush();
                drain_below(&mut ws);
                return WsLinkEvent::Failed(BackendError::disconnected("a message was larger than the limit (closed with 1009)", None));
            }
            Err(e) => return WsLinkEvent::Failed(map_error(e)),
        }
        if let Some(started) = closing {
            if started.elapsed() > Duration::from_secs(1) {
                return finish(failure, close_code, close_reason);
            }
        } else {
            let last = alive.max(ws.get_ref().timed().last_byte);
            if last.elapsed() > handshake.dead_after {
                // Silence: not a single byte (data, ping, pong) for `dead_after`.
                return WsLinkEvent::Failed(BackendError::Timeout(format!(
                    "no byte from the server within {:?}; connection considered dead",
                    handshake.dead_after
                )));
            }
        }
    }
}

/// Read and discard what the server still sends, straight from the stream under tungstenite, for
/// up to 1 s or until it closes (so the client's close frame is not overtaken by a TCP reset).
fn drain_below(ws: &mut WebSocket<Stream>) {
    let until = deadline_after(Duration::from_secs(1));
    let stream = ws.get_mut();
    stream.timed_mut().deadline = Some(until);
    let mut scratch = [0u8; 16 * 1024];
    while Instant::now() < until {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if is_no_data(&e) => {}
            Err(_) => break,
        }
    }
}

/// How long one write may block before the link gives up: the dead-peer window, at least 30 s
/// (a server that is slow to read a large message is not dead), at most 1 h.
fn write_limit(handshake: &WsHandshake) -> Duration {
    handshake.dead_after.max(Duration::from_secs(30)).min(MAX_TIMEOUT)
}

/// TLS across read timeouts, and peers that trickle bytes (handshake and mid-frame). The test CA
/// and localhost certificate are generated in memory with rcgen (a dev-dependency, ring backend):
/// no key material exists in the repository.
#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    use super::*;

    struct Pki {
        ca: CertificateDer<'static>,
        leaf: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
    }

    fn pki() -> Pki {
        pki_for("localhost")
    }

    /// A throwaway CA and a leaf certificate for `host`.
    fn pki_for(host: &str) -> Pki {
        let ca_key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|e| panic!("{e}"));
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap_or_else(|e| panic!("{e}"));
        let leaf_key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
        let leaf =
            rcgen::CertificateParams::new(vec![host.to_string()]).unwrap_or_else(|e| panic!("{e}")).signed_by(&leaf_key, &ca).unwrap_or_else(|e| panic!("{e}"));
        Pki { ca: ca.der().clone(), leaf: leaf.der().clone(), key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())) }
    }

    fn client_config(pki: &Pki) -> Arc<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(pki.ca.clone()).unwrap_or_else(|e| panic!("{e}"));
        let config = rustls::ClientConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .unwrap_or_else(|e| panic!("{e}"))
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    }

    fn server_config(pki: &Pki) -> Arc<rustls::ServerConfig> {
        let config = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .unwrap_or_else(|e| panic!("{e}"))
            .with_no_client_auth()
            .with_single_cert(vec![pki.leaf.clone()], pki.key.clone_key())
            .unwrap_or_else(|e| panic!("{e}"));
        Arc::new(config)
    }

    fn handshake(uri: String, connect_timeout: Duration, read_timeout: Duration, ping: Duration, dead: Duration) -> WsHandshake {
        WsHandshake {
            uri: uri.parse().unwrap_or_else(|e| panic!("{e}")),
            headers: http::HeaderMap::new(),
            connect_timeout,
            read_timeout,
            ping_interval: ping,
            dead_after: dead,
            max_message_bytes: 4 << 20,
        }
    }

    /// Run a session on its own thread; returns (commands, events, the thread).
    fn start(handshake: WsHandshake, tls: Option<Arc<rustls::ClientConfig>>) -> (Sender<Command>, Receiver<Event>, thread::JoinHandle<WsLinkEvent>) {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let sink = EventSink { link: WsLinkId::next(), events: events_tx, queued_bytes: Arc::default() };
        let thread = thread::spawn(move || session(&handshake, &commands_rx, &sink, tls, &ProxyEnv::default()));
        (commands_tx, events_rx, thread)
    }

    /// A TLS WebSocket echo server for one client, reading with a 5 ms timeout.
    fn echo_server(pki: &Pki) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let config = server_config(pki);
        let handle = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap_or_else(|e| panic!("{e}"));
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap_or_else(|e| panic!("{e}"));
            let connection = rustls::ServerConnection::new(config).unwrap_or_else(|e| panic!("{e}"));
            let tls = rustls::StreamOwned::new(connection, tcp);
            let limits = WebSocketConfig::default().max_message_size(Some(8 << 20)).max_frame_size(Some(8 << 20));
            let mut ws = tungstenite::accept_with_config(tls, Some(limits)).unwrap_or_else(|e| panic!("server handshake: {e}"));
            ws.get_ref().get_ref().set_read_timeout(Some(Duration::from_millis(5))).unwrap_or_else(|e| panic!("{e}"));
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match ws.read() {
                    Ok(Message::Text(text)) => ws.send(Message::Text(text)).unwrap_or_else(|e| panic!("server send: {e}")),
                    Ok(Message::Binary(bytes)) => ws.send(Message::Binary(bytes)).unwrap_or_else(|e| panic!("server send: {e}")),
                    Ok(Message::Close(_)) => {
                        let _ = ws.flush();
                        return;
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e)) if is_no_data(&e) => {}
                    Err(_) => return,
                }
            }
        });
        (port, handle)
    }

    #[test]
    fn large_tls_messages_survive_read_timeouts_mid_record() {
        let pki = pki();
        let (port, server) = echo_server(&pki);
        let handshake = handshake(
            format!("wss://localhost:{port}/"),
            Duration::from_secs(10),
            Duration::from_millis(5),
            Duration::from_millis(50),
            Duration::from_secs(20),
        );
        let (commands, events, client) = start(handshake, Some(client_config(&pki)));
        assert_eq!(events.recv_timeout(Duration::from_secs(10)).map(|(_, e)| e), Ok(WsLinkEvent::Opened));
        // One message at a time (the echo server is single-threaded and blocking: it cannot read
        // while it writes). Each is many TLS records (16 KiB each) received across many 5 ms
        // read budgets on both sides.
        for i in 0..12u8 {
            let binary = (0..700 * 1024u32).map(|n| u8::try_from(n % 251).unwrap_or(0) ^ i).collect::<Vec<u8>>();
            let text = format!("{i}:{}", "abcdefghij".repeat(30 * 1024));
            for frame in [WsFrame::Binary(binary), WsFrame::Text(text)] {
                commands.send(Command::Send(frame.clone())).unwrap_or_else(|e| panic!("{e}"));
                match events.recv_timeout(Duration::from_secs(30)) {
                    Ok((_, WsLinkEvent::Frame(echo))) => assert!(echo == frame, "echo {i} differs"),
                    other => panic!("unexpected: {other:?}"),
                }
            }
        }
        commands.send(Command::Close(1000)).unwrap_or_else(|e| panic!("{e}"));
        let last = client.join().unwrap_or_else(|_| panic!("client thread"));
        assert!(matches!(last, WsLinkEvent::Closed { .. }), "{last:?}");
        let _ = server.join();
    }

    /// A server that accepts TCP, reads what comes, writes `prefix`, then trickles one byte every
    /// 300 ms for 6 s.
    fn trickling_server(prefix: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        thread::spawn(move || {
            let Ok((mut tcp, _)) = listener.accept() else { return };
            let _ = tcp.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 4096];
            let _ = tcp.read(&mut buf);
            if tcp.write_all(prefix).is_err() {
                return;
            }
            for _ in 0..20 {
                thread::sleep(Duration::from_millis(300));
                if tcp.write_all(b"x").is_err() {
                    return;
                }
            }
        });
        port
    }

    fn assert_connect_limit_holds(uri: String, tls: Option<Arc<rustls::ClientConfig>>) {
        let started = Instant::now();
        let handshake = handshake(uri, Duration::from_secs(1), Duration::from_millis(20), Duration::from_secs(15), Duration::from_secs(45));
        let (_commands, events, client) = start(handshake, tls);
        let last = client.join().unwrap_or_else(|_| panic!("client thread"));
        assert!(matches!(&last, WsLinkEvent::Failed(BackendError::Timeout(why)) if why == "connect limit"), "{last:?}");
        assert!(started.elapsed() < Duration::from_millis(2500), "the attempt took {:?}", started.elapsed());
        assert!(events.try_recv().is_err(), "never reported open");
    }

    #[test]
    fn a_trickled_http_handshake_stops_at_the_connect_limit() {
        let port = trickling_server(b"HTTP/1.1 101 Switching Protocols\r\nX-Slow: ");
        assert_connect_limit_holds(format!("ws://127.0.0.1:{port}/"), None);
    }

    #[test]
    fn a_trickled_tls_handshake_stops_at_the_connect_limit() {
        // A TLS handshake record header announcing 16 KiB, then one byte at a time.
        let port = trickling_server(&[0x16, 0x03, 0x03, 0x40, 0x00]);
        let pki = pki();
        assert_connect_limit_holds(format!("wss://localhost:{port}/"), Some(client_config(&pki)));
    }

    #[test]
    fn a_trickled_incoming_frame_does_not_starve_sends_and_pings() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let received = Arc::new(AtomicUsize::new(0));
        let during_trickle = Arc::new(AtomicUsize::new(0));
        let (count, window) = (Arc::clone(&received), Arc::clone(&during_trickle));
        thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else { return };
            let Ok(ws) = tungstenite::accept(tcp.try_clone().unwrap_or_else(|e| panic!("{e}"))) else { return };
            drop(ws);
            // A reader thread counts what the client sends.
            let mut reader = tcp.try_clone().unwrap_or_else(|e| panic!("{e}"));
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    count.fetch_add(n, Ordering::SeqCst);
                }
            });
            // A binary frame of 60 000 bytes, announced, then trickled: 1 byte every 8 ms for 3 s.
            let mut writer = tcp;
            let _ = writer.write_all(&[0x82, 126, 0xEA, 0x60]);
            let before = received.load(Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(8));
                if writer.write_all(b"z").is_err() {
                    return;
                }
            }
            window.store(received.load(Ordering::SeqCst).saturating_sub(before), Ordering::SeqCst);
            // Then silence: the client must find the peer dead.
            thread::sleep(Duration::from_secs(5));
        });
        let handshake = handshake(
            format!("ws://127.0.0.1:{port}/"),
            Duration::from_secs(5),
            Duration::from_millis(20),
            Duration::from_millis(50),
            Duration::from_millis(500),
        );
        let (commands, events, client) = start(handshake, None);
        assert_eq!(events.recv_timeout(Duration::from_secs(5)).map(|(_, e)| e), Ok(WsLinkEvent::Opened));
        thread::sleep(Duration::from_millis(1000));
        commands.send(Command::Send(WsFrame::Text("sent during the trickle".into()))).unwrap_or_else(|e| panic!("{e}"));
        let last = client.join().unwrap_or_else(|_| panic!("client thread"));
        // About 60 pings (6+ bytes each) plus the text frame during the 3 s trickle.
        assert!(during_trickle.load(Ordering::SeqCst) >= 200, "only {} bytes sent while the frame trickled", during_trickle.load(Ordering::SeqCst));
        assert!(matches!(&last, WsLinkEvent::Failed(BackendError::Timeout(why)) if why.contains("dead")), "{last:?}");
    }

    /// A server that stops reading for 2.5 s while it keeps sending, and a client that sends a
    /// 32 MiB frame meanwhile: the write blocks, but the connection is alive and must stay up.
    #[test]
    fn a_large_send_to_a_slow_reader_does_not_kill_a_live_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let received = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&received);
        thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else { return };
            let Ok(ws) = tungstenite::accept(tcp.try_clone().unwrap_or_else(|e| panic!("{e}"))) else { return };
            drop(ws);
            let mut writer = tcp.try_clone().unwrap_or_else(|e| panic!("{e}"));
            thread::spawn(move || {
                // "alive", unmasked text frames, every 100 ms for 6 s.
                for _ in 0..60 {
                    if writer.write_all(&[0x81, 5, b'a', b'l', b'i', b'v', b'e']).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            });
            let mut reader = tcp;
            thread::sleep(Duration::from_millis(2500));
            let mut buf = vec![0u8; 1 << 16];
            let until = Instant::now() + Duration::from_secs(20);
            while Instant::now() < until {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        count.fetch_add(n, Ordering::SeqCst);
                    }
                }
            }
        });
        let handshake =
            handshake(format!("ws://127.0.0.1:{port}/"), Duration::from_secs(5), Duration::from_millis(20), Duration::from_millis(200), Duration::from_secs(1));
        let (commands, events, client) = start(handshake, None);
        assert_eq!(events.recv_timeout(Duration::from_secs(5)).map(|(_, e)| e), Ok(WsLinkEvent::Opened));
        commands.send(Command::Send(WsFrame::Binary(vec![7u8; 32 << 20]))).unwrap_or_else(|e| panic!("{e}"));
        let until = Instant::now() + Duration::from_secs(5);
        let mut frames = 0;
        while Instant::now() < until {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok((_, WsLinkEvent::Frame(_))) => frames += 1,
                Ok((_, other)) => panic!("the live connection ended: {other:?}"),
                Err(_) => {}
            }
        }
        assert!(frames >= 20, "only {frames} frames");
        assert!(received.load(Ordering::SeqCst) >= 32 << 20, "the server got {} bytes", received.load(Ordering::SeqCst));
        commands.send(Command::Close(1000)).unwrap_or_else(|e| panic!("{e}"));
        let _ = client.join();
    }

    #[test]
    fn huge_durations_never_overflow() {
        let far = deadline_after(Duration::MAX);
        assert!(far > Instant::now());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let tcp = TcpStream::connect(listener.local_addr().unwrap_or_else(|e| panic!("{e}"))).unwrap_or_else(|e| panic!("{e}"));
        let mut timed = TimedTcp::new(tcp, Some(Instant::now()), Duration::MAX);
        let mut buf = [0u8; 4];
        assert_eq!(timed.read(&mut buf).map_err(|e| e.kind()), Err(io::ErrorKind::TimedOut));
    }

    #[test]
    fn the_auth_frame_is_one_masked_final_text_frame_tungstenite_can_parse() {
        use tungstenite::protocol::frame::coding::{Data, OpCode};
        use tungstenite::protocol::frame::FrameHeader;
        let mut masks = std::collections::HashSet::new();
        for len in [0usize, 1, 125, 126, 127, 65_535, 65_536, 70_000] {
            let payload: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            let frame = masked_text_frame(&payload).unwrap_or_else(|e| panic!("{e}"));
            let mut cursor = io::Cursor::new(frame.as_slice());
            let (header, length) = FrameHeader::parse(&mut cursor).unwrap_or_else(|e| panic!("{e}")).unwrap_or_else(|| panic!("incomplete header"));
            assert!(header.is_final && !header.rsv1 && !header.rsv2 && !header.rsv3, "{header:?}");
            assert_eq!(header.opcode, OpCode::Data(Data::Text));
            assert_eq!(length, len as u64);
            let mask = header.mask.unwrap_or_else(|| panic!("a client frame must be masked"));
            masks.insert(mask);
            let start = usize::try_from(cursor.position()).unwrap_or(0);
            let body = frame.get(start..).unwrap_or_default();
            assert_eq!(body.len(), len);
            let unmasked: Vec<u8> = body.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m).collect();
            assert!(unmasked == payload, "payload of {len} bytes");
        }
        assert!(masks.len() > 1, "the masks are random");
    }

    /// A local HTTP proxy for ONE tunnel: it reads the `CONNECT` header, answers 407 when
    /// `password` is set and the header lacks the matching `Proxy-Authorization`, else 200, and
    /// then joins the client to 127.0.0.1:`server` (whatever host the client asked for). Returns
    /// its port and the `CONNECT` header it received.
    fn connect_proxy(server: u16, authorization: Option<&'static str>) -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let handle = thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else { return String::new() };
            let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                match client.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => return String::from_utf8_lossy(&head).into_owned(),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            if let Some(expected) = authorization {
                if !head.lines().any(|line| line == format!("Proxy-Authorization: {expected}")) {
                    let _ = client
                        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n");
                    return head;
                }
            }
            let Ok(upstream) = TcpStream::connect(("127.0.0.1", server)) else { return head };
            let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let _ = client.set_read_timeout(None);
            let (mut client_read, mut upstream_write) =
                (client.try_clone().unwrap_or_else(|e| panic!("{e}")), upstream.try_clone().unwrap_or_else(|e| panic!("{e}")));
            let (mut upstream_read, mut client_write) = (upstream, client);
            let up = thread::spawn(move || {
                let _ = io::copy(&mut client_read, &mut upstream_write);
                let _ = upstream_write.shutdown(std::net::Shutdown::Write);
            });
            let _ = io::copy(&mut upstream_read, &mut client_write);
            let _ = client_write.shutdown(std::net::Shutdown::Write);
            let _ = up.join();
            head
        });
        (port, handle)
    }

    fn proxy_env(proxy_url: &str) -> ProxyEnv {
        let url = proxy_url.to_string();
        ProxyEnv::from_vars(move |name| (name == "HTTPS_PROXY").then(|| url.clone()))
    }

    /// `wss://` to a host only the proxy "knows" (never resolved locally), through an
    /// authenticating `CONNECT` proxy: TLS end to end inside the tunnel, the crate's own handshake
    /// and auth frame (the server receives and echoes the auth text), then a normal frame.
    #[test]
    fn wss_goes_through_an_http_connect_proxy_with_credentials() {
        let pki = pki_for("game.test");
        let (server_port, server) = echo_server(&pki);
        // "user:secret" in base64.
        let (proxy_port, proxy) = connect_proxy(server_port, Some("Basic dXNlcjpzZWNyZXQ="));
        let env = proxy_env(&format!("http://user:secret@127.0.0.1:{proxy_port}"));
        let mut handshake = handshake(
            format!("wss://game.test:{server_port}/ws"),
            Duration::from_secs(10),
            Duration::from_millis(5),
            Duration::from_secs(15),
            Duration::from_secs(20),
        );
        handshake.headers.insert(http::header::AUTHORIZATION, http::HeaderValue::from_static("Bearer fake-token"));
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let sink = EventSink { link: WsLinkId::next(), events: events_tx, queued_bytes: Arc::default() };
        let tls = client_config(&pki);
        let client = thread::spawn(move || session(&handshake, &commands_rx, &sink, Some(tls), &env));
        assert_eq!(events.recv_timeout(Duration::from_secs(10)).map(|(_, e)| e), Ok(WsLinkEvent::Opened));
        commands_tx.send(Command::SendAuth(Zeroizing::new(r#"{"type":"auth","token":"fake-token"}"#.to_string()))).unwrap_or_else(|e| panic!("{e}"));
        commands_tx.send(Command::Send(WsFrame::Text("after the auth".into()))).unwrap_or_else(|e| panic!("{e}"));
        for expected in [r#"{"type":"auth","token":"fake-token"}"#, "after the auth"] {
            match events.recv_timeout(Duration::from_secs(10)) {
                Ok((_, WsLinkEvent::Frame(WsFrame::Text(text)))) => assert_eq!(text, expected),
                other => panic!("unexpected: {other:?}"),
            }
        }
        commands_tx.send(Command::Close(1000)).unwrap_or_else(|e| panic!("{e}"));
        let last = client.join().unwrap_or_else(|_| panic!("client thread"));
        assert!(matches!(last, WsLinkEvent::Closed { .. }), "{last:?}");
        let _ = server.join();
        let head = proxy.join().unwrap_or_else(|_| panic!("proxy thread"));
        assert!(head.starts_with(&format!("CONNECT game.test:{server_port} HTTP/1.1\r\n")), "{head}");
        assert!(head.contains(&format!("Host: game.test:{server_port}\r\n")), "{head}");
        assert!(!head.contains("fake-token"), "the tunnel request carries no credentials of the connection: {head}");
    }

    #[test]
    fn a_refusing_proxy_is_a_network_error_and_the_link_never_opens() {
        let (proxy_port, proxy) = connect_proxy(1, Some("Basic dXNlcjpzZWNyZXQ="));
        // No credentials in the proxy URL: the proxy answers 407.
        let env = proxy_env(&format!("http://127.0.0.1:{proxy_port}"));
        let handshake =
            handshake("wss://game.test/ws".into(), Duration::from_secs(5), Duration::from_millis(20), Duration::from_secs(15), Duration::from_secs(45));
        let (_commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let sink = EventSink { link: WsLinkId::next(), events: events_tx, queued_bytes: Arc::default() };
        let last = session(&handshake, &commands_rx, &sink, None, &env);
        assert!(matches!(&last, WsLinkEvent::Failed(BackendError::Network(why)) if why.contains("407")), "{last:?}");
        assert!(events.try_recv().is_err(), "never reported open");
        let head = proxy.join().unwrap_or_else(|_| panic!("proxy thread"));
        assert!(head.starts_with("CONNECT game.test:443 HTTP/1.1\r\n") && !head.contains("Proxy-Authorization"), "{head}");
        // A proxy that is not there.
        let gone = TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let last = session(&handshake, &commands_rx, &sink, None, &proxy_env(&format!("http://127.0.0.1:{gone}")));
        assert!(matches!(&last, WsLinkEvent::Failed(BackendError::Network(why)) if why.contains("proxy")), "{last:?}");
    }

    /// The crate's own handshake checks the answer like tungstenite: a wrong accept key, a missing
    /// `Upgrade`, and a refusal with its status, headers and the body bytes that came with it.
    #[test]
    fn handshake_answers_are_checked() {
        fn answer_with(answer: &'static [u8]) -> WsLinkEvent {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
            let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
            thread::spawn(move || {
                let Ok((mut tcp, _)) = listener.accept() else { return };
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(5)));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if tcp.read(&mut byte).map_or(true, |n| n == 0) {
                        return;
                    }
                    head.push(byte[0]);
                }
                let _ = tcp.write_all(answer);
                thread::sleep(Duration::from_millis(200));
            });
            let (_commands, events, client) = start(
                handshake(
                    format!("ws://127.0.0.1:{port}/"),
                    Duration::from_secs(5),
                    Duration::from_millis(20),
                    Duration::from_secs(15),
                    Duration::from_secs(45),
                ),
                None,
            );
            let last = client.join().unwrap_or_else(|_| panic!("client thread"));
            assert!(events.try_recv().is_err(), "never reported open");
            last
        }
        let wrong_key = answer_with(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: AAAA\r\n\r\n");
        assert!(matches!(&wrong_key, WsLinkEvent::Failed(BackendError::Network(why)) if why.contains("Sec-WebSocket-Accept")), "{wrong_key:?}");
        let no_upgrade = answer_with(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n");
        assert!(matches!(&no_upgrade, WsLinkEvent::Failed(BackendError::Network(_))), "{no_upgrade:?}");
        let refused = answer_with(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 4\r\n\r\nslow");
        match refused {
            WsLinkEvent::Failed(BackendError::Status(raw)) => {
                assert_eq!(raw.status, http::StatusCode::TOO_MANY_REQUESTS);
                assert_eq!(raw.headers.get("retry-after").and_then(|v| v.to_str().ok()), Some("7"));
                assert_eq!(raw.body, b"slow".to_vec());
            }
            other => panic!("{other:?}"),
        }
    }
}
