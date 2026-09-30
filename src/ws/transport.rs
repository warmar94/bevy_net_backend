//! The WebSocket seam: [`WsTransport`] (one "link" = one connection attempt), its resource, and
//! the in-memory [`FakeWsTransport`].

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bevy_ecs::resource::Resource;
use http::header::{HeaderMap, HeaderName};
use http::Uri;

use super::WsFrame;
use crate::response::BackendError;

/// Identifies one connection attempt (a "link"): each (re)connect of a named connection is a new
/// link. Opaque; `Display` shows it for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WsLinkId(u64);

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

impl WsLinkId {
    pub(crate) fn next() -> Self {
        Self(NEXT_LINK.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for WsLinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "link#{}", self.0)
    }
}

/// Everything a transport needs to open one link: the URL, the handshake headers (credentials
/// applied) and the connection's limits. Built by the plugin; `Debug` never shows header values
/// or the query.
#[derive(Clone)]
#[non_exhaustive]
pub struct WsHandshake {
    /// `ws://` or `wss://` URL (query included).
    pub uri: Uri,
    /// Extra handshake headers (the WebSocket headers themselves are added by the transport).
    pub headers: HeaderMap,
    /// Limit for TCP connect + TLS + handshake.
    pub connect_timeout: Duration,
    /// How long one read waits.
    pub read_timeout: Duration,
    /// Heartbeat ping interval.
    pub ping_interval: Duration,
    /// No frame for this long = dead.
    pub dead_after: Duration,
    /// Largest message / frame accepted.
    pub max_message_bytes: usize,
}

impl fmt::Debug for WsHandshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("WsHandshake")
            .field("url", &crate::request::redacted_url(&self.uri))
            .field("header_names", &headers)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("ping_interval", &self.ping_interval)
            .field("dead_after", &self.dead_after)
            .field("max_message_bytes", &self.max_message_bytes)
            .finish()
    }
}

impl WsHandshake {
    /// Whether the URL is `wss://`.
    pub fn is_secure(&self) -> bool {
        self.uri.scheme_str().is_some_and(|s| s.eq_ignore_ascii_case("wss"))
    }
}

/// What happened on a link. After `Closed` or `Failed` a link reports nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsLinkEvent {
    /// The handshake succeeded; frames flow.
    Opened,
    /// A data frame from the server.
    Frame(WsFrame),
    /// The connection closed (a close handshake, or the socket ended cleanly).
    Closed {
        /// The close code, if the peer sent one.
        code: Option<u16>,
        /// The close reason, if any.
        reason: String,
    },
    /// The attempt or the connection failed (connect, TLS, handshake status, I/O, dead peer,
    /// message too large). The error is the dependency's.
    Failed(BackendError),
}

/// Opens links and moves frames. The plugin owns reconnects, requests and every answer; a
/// transport only delivers. `send`, `close` and `poll` run on the main thread and must never
/// block. Methods added later always come with a default implementation.
pub trait WsTransport: Send + Sync + 'static {
    /// Start a connection attempt. Report `Opened`, then frames, then exactly one `Closed` or
    /// `Failed` (or just `Failed`).
    fn open(&mut self, link: WsLinkId, handshake: WsHandshake);

    /// Send a frame on an open link (ignored for a link that is gone).
    fn send(&mut self, link: WsLinkId, frame: WsFrame);

    /// Close a link with `code`. The plugin stops listening to it right away.
    fn close(&mut self, link: WsLinkId, code: u16);

    /// Everything that happened since the last call.
    fn poll(&mut self) -> Vec<(WsLinkId, WsLinkEvent)>;

    /// The app is exiting: close everything, never wait. Default: nothing.
    fn shutdown(&mut self) {}
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The installed WebSocket transport. The plugin inserts a [`TungsteniteTransport`](crate::TungsteniteTransport)
/// when there is none; insert a [`FakeWsTransport`] for tests. Links of a transport that is
/// removed or replaced count as lost (and reconnect on the new one).
#[derive(Resource)]
pub struct WsTransportRes {
    inner: Box<dyn WsTransport>,
    generation: u64,
}

impl fmt::Debug for WsTransportRes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsTransportRes").field("generation", &self.generation).finish_non_exhaustive()
    }
}

impl WsTransportRes {
    /// Wrap a transport.
    pub fn new(transport: impl WsTransport) -> Self {
        Self { inner: Box::new(transport), generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed) }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn get_mut(&mut self) -> &mut dyn WsTransport {
        self.inner.as_mut()
    }
}

#[derive(Default)]
struct FakeState {
    opened: Vec<(WsLinkId, WsHandshake)>,
    live: HashSet<WsLinkId>,
    sent: Vec<(WsLinkId, WsFrame)>,
    closed: Vec<(WsLinkId, u16)>,
    events: VecDeque<(WsLinkId, WsLinkEvent)>,
    manual_accept: bool,
    reject_next: VecDeque<BackendError>,
    echo_envelope: bool,
    shutdowns: usize,
}

/// An in-memory [`WsTransport`] for tests: records every link, frame and close, and answers from
/// a script. Clones share one state. Events are delivered on the next poll (the next frame's
/// `First`).
///
/// By default every link opens at once. [`manual_accept`](Self::manual_accept) holds them until
/// [`accept`](Self::accept); [`reject_next`](Self::reject_next) fails the next attempts;
/// [`echo_envelope`](Self::echo_envelope) answers every JSON-envelope request with its own `data`.
#[derive(Clone, Default)]
pub struct FakeWsTransport {
    state: Arc<Mutex<FakeState>>,
}

impl fmt::Debug for FakeWsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeWsTransport").field("opened", &state.opened.len()).field("live", &state.live.len()).finish_non_exhaustive()
    }
}

impl FakeWsTransport {
    /// A fake that accepts every link.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn event(&self, link: WsLinkId, event: WsLinkEvent) {
        let mut state = self.lock();
        if matches!(event, WsLinkEvent::Closed { .. } | WsLinkEvent::Failed(_)) {
            state.live.remove(&link);
        }
        state.events.push_back((link, event));
    }

    /// Hold new links until [`accept`](Self::accept) (or `true` → `false` to go back).
    pub fn manual_accept(&self, manual: bool) -> &Self {
        self.lock().manual_accept = manual;
        self
    }

    /// Fail the next attempt with `error` (queue several for several attempts).
    pub fn reject_next(&self, error: BackendError) -> &Self {
        self.lock().reject_next.push_back(error);
        self
    }

    /// Answer every `{"id":…,"type":…,"data":…}` text frame with `{"id":…,"ok":true,"data":…}`.
    pub fn echo_envelope(&self, echo: bool) -> &Self {
        self.lock().echo_envelope = echo;
        self
    }

    /// Open a held link.
    pub fn accept(&self, link: WsLinkId) {
        self.event(link, WsLinkEvent::Opened);
    }

    /// The server sends `frame` on `link`.
    pub fn push(&self, link: WsLinkId, frame: WsFrame) {
        self.event(link, WsLinkEvent::Frame(frame));
    }

    /// The server closes `link` with `code`.
    pub fn drop_link(&self, link: WsLinkId, code: u16) {
        self.event(link, WsLinkEvent::Closed { code: Some(code), reason: String::new() });
    }

    /// `link` fails with `error`.
    pub fn fail_link(&self, link: WsLinkId, error: BackendError) {
        self.event(link, WsLinkEvent::Failed(error));
    }

    /// Every link opened so far with its handshake, in order.
    pub fn opened(&self) -> Vec<(WsLinkId, WsHandshake)> {
        self.lock().opened.clone()
    }

    /// The last link opened.
    pub fn last_link(&self) -> Option<WsLinkId> {
        self.lock().opened.last().map(|(link, _)| *link)
    }

    /// Links neither closed by the plugin nor ended by the script.
    pub fn live_links(&self) -> Vec<WsLinkId> {
        let mut links: Vec<WsLinkId> = self.lock().live.iter().copied().collect();
        links.sort_unstable();
        links
    }

    /// Frames the plugin sent on `link`.
    pub fn sent(&self, link: WsLinkId) -> Vec<WsFrame> {
        self.lock().sent.iter().filter(|(l, _)| *l == link).map(|(_, f)| f.clone()).collect()
    }

    /// Every frame the plugin sent, with its link.
    pub fn all_sent(&self) -> Vec<(WsLinkId, WsFrame)> {
        self.lock().sent.clone()
    }

    /// Links the plugin closed, with the code.
    pub fn closed(&self) -> Vec<(WsLinkId, u16)> {
        self.lock().closed.clone()
    }

    /// How many times the plugin shut the transport down.
    pub fn shutdown_count(&self) -> usize {
        self.lock().shutdowns
    }
}

impl WsTransport for FakeWsTransport {
    fn open(&mut self, link: WsLinkId, handshake: WsHandshake) {
        let mut state = self.lock();
        state.opened.push((link, handshake));
        if let Some(error) = state.reject_next.pop_front() {
            state.events.push_back((link, WsLinkEvent::Failed(error)));
            return;
        }
        state.live.insert(link);
        if !state.manual_accept {
            state.events.push_back((link, WsLinkEvent::Opened));
        }
    }

    fn send(&mut self, link: WsLinkId, frame: WsFrame) {
        let mut state = self.lock();
        if !state.live.contains(&link) {
            return;
        }
        #[cfg(feature = "json")]
        if state.echo_envelope {
            if let Some(answer) = frame.as_text().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()).and_then(|v| {
                let id = v.get("id")?.as_u64()?;
                Some(serde_json::json!({ "id": id, "ok": true, "data": v.get("data").cloned().unwrap_or(serde_json::Value::Null) }).to_string())
            }) {
                state.events.push_back((link, WsLinkEvent::Frame(WsFrame::Text(answer))));
            }
        }
        state.sent.push((link, frame));
    }

    fn close(&mut self, link: WsLinkId, code: u16) {
        let mut state = self.lock();
        state.live.remove(&link);
        state.closed.push((link, code));
    }

    fn poll(&mut self) -> Vec<(WsLinkId, WsLinkEvent)> {
        self.lock().events.drain(..).collect()
    }

    fn shutdown(&mut self) {
        let mut state = self.lock();
        state.shutdowns += 1;
        state.live.clear();
    }
}
