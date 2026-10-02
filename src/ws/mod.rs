//! Named WebSocket connections (feature `ws`).
//!
//! A game opens a connection by name with [`WsClient::connect`] (`"main"` for most games; a game
//! that needs more opens more), then sends frames and requests on it by name. Everything that
//! comes back is a Bevy message carrying the connection's [`WsName`]: state changes
//! ([`WsStateChanged`]), every data frame ([`WsMessage`]), answers to requests
//! ([`WsRawResponse`], and with feature `json` `WsResponse<T>`) and typed server pushes
//! (`WsPush<P>`, feature `json`). [`WsConnections`] holds each connection's current state.
//!
//! Each live connection runs on its own std thread (tungstenite, sync, no async runtime),
//! reading with a short timeout (default 20 ms), sending heartbeat pings and detecting a dead
//! peer. Reconnects use exponential backoff with jitter; credentials from
//! [`BackendCredentials`](crate::BackendCredentials) are applied to every handshake, so a token
//! changed while connected is used on the next reconnect.

use std::collections::HashMap;
#[cfg(feature = "json")]
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bevy_app::{App, First, Last, PostUpdate};
use bevy_ecs::message::Message;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::common_conditions::on_message;
use bevy_ecs::schedule::IntoScheduleConfigs;
use http::header::{HeaderMap, HeaderName, HeaderValue};

use crate::request::RequestId;
use crate::response::BackendError;
use crate::BackendSystems;

mod link;
mod protocol;
mod proxy;
mod systems;
mod transport;

pub use link::TungsteniteTransport;
#[cfg(feature = "json")]
pub use protocol::JsonEnvelope;
pub use protocol::{WsIncoming, WsProtocol};
/// The WebSocket systems, for ordering the SSH systems after them.
#[cfg(feature = "ssh")]
pub(crate) use systems::{ws_exit, ws_receive, ws_send};
pub use transport::{FakeWsTransport, WsHandshake, WsLinkEvent, WsLinkId, WsTransport, WsTransportRes};

/// The name of a WebSocket connection (`"main"`, `"chat"`, …). Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WsName(Arc<str>);

impl WsName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WsName({:?})", &*self.0)
    }
}

impl fmt::Display for WsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for WsName {
    fn from(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl From<String> for WsName {
    fn from(name: String) -> Self {
        Self(Arc::from(name))
    }
}

impl From<&String> for WsName {
    fn from(name: &String) -> Self {
        Self(Arc::from(name.as_str()))
    }
}

impl From<&WsName> for WsName {
    fn from(name: &WsName) -> Self {
        name.clone()
    }
}

impl PartialEq<str> for WsName {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for WsName {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

/// One WebSocket data frame. `Debug` shows the kind and length only (a frame may hold a token).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsFrame {
    /// A text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
}

impl WsFrame {
    /// The payload length in bytes.
    pub fn len(&self) -> usize {
        match self {
            WsFrame::Text(text) => text.len(),
            WsFrame::Binary(bytes) => bytes.len(),
        }
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The text, for a text frame.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            WsFrame::Text(text) => Some(text),
            WsFrame::Binary(_) => None,
        }
    }

    /// The bytes of either kind.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            WsFrame::Text(text) => text.as_bytes(),
            WsFrame::Binary(bytes) => bytes,
        }
    }
}

impl fmt::Debug for WsFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WsFrame::Text(text) => write!(f, "Text({} bytes)", text.len()),
            WsFrame::Binary(bytes) => write!(f, "Binary({} bytes)", bytes.len()),
        }
    }
}

/// The state of one named connection. A resource ([`WsConnections`]) plus messages
/// ([`WsStateChanged`]), not Bevy `States`. `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsState {
    /// A connection attempt (TCP, TLS, handshake) is running.
    Connecting,
    /// Open: frames and requests flow.
    Connected,
    /// The connection was lost (or an attempt failed); the next attempt starts after `retry_in`.
    Reconnecting {
        /// The next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts (backoff with jitter; at least the `Retry-After` of a 429 / 503
        /// handshake answer).
        retry_in: Duration,
    },
    /// Not connected and not trying: closed by the game, refused for good (401/403, a policy
    /// close code, invalid settings), out of attempts, or the app is exiting.
    Disconnected,
    /// The server refused the credentials and the connection waits for the game to refresh them
    /// (only with [`WsSettings::with_credentials_refresh`]): a [`WsCredentialsRefused`] message
    /// asked for it. New credentials in [`BackendCredentials`](crate::BackendCredentials) start
    /// one new connection; cleared credentials or the refresh timeout end it (`Disconnected`).
    WaitingForCredentials,
}

/// Credentials refresh for one connection (off unless given to
/// [`WsSettings::with_credentials_refresh`]).
///
/// When the server refuses the credentials (a `401` answer to the handshake, a refused
/// first-message authentication, or a close with one of the [close codes](Self::with_close_code)
/// added here), the connection does not end at once: it goes
/// [`WsState::WaitingForCredentials`] and the plugin writes ONE [`WsCredentialsRefused`]
/// message, however many connections were refused with the same credentials. The game refreshes
/// with its own call (for example its refresh route through `HttpClient`) and sets the new
/// credentials with [`BackendCredentials::set`](crate::BackendCredentials::set); every waiting
/// connection then makes ONE new connection with them. The crate never calls a refresh route
/// itself.
///
/// It ends in `Disconnected` (with the server's refusal as the error) when:
/// - the new connection is refused again (one refresh per refusal, never a loop; a further
///   refresh is allowed only after a connection stayed up for the reconnect policy's
///   `stable_after`),
/// - the game clears the credentials ([`BackendCredentials::clear`](crate::BackendCredentials::clear),
///   e.g. because its refresh was refused),
/// - no new credentials arrive within the [timeout](Self::with_timeout) (default 30 s).
///
/// If the credentials already changed since the refused handshake (the game refreshed on its own
/// meanwhile), no message is written and the connection connects again with the new ones at once.
/// Any [`BackendCredentials::set`](crate::BackendCredentials::set) counts as a change, also one
/// with the same token.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::{WsCredentialsRefresh, WsSettings};
///
/// let settings = WsSettings::new("wss://game.example.com/ws")
///     .with_credentials_refresh(WsCredentialsRefresh::new().with_timeout(Duration::from_secs(20)).with_close_code(4001));
/// # let _ = settings;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsCredentialsRefresh {
    pub(crate) timeout: Duration,
    pub(crate) close_codes: Vec<u16>,
}

impl Default for WsCredentialsRefresh {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(30), close_codes: Vec::new() }
    }
}

impl WsCredentialsRefresh {
    /// A refresh after a `401` handshake answer or a refused first-message authentication,
    /// waiting up to 30 s for new credentials.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long a refused connection waits for new credentials (default 30 s, clamped to
    /// 1 ms..=1 h).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// Also refresh when the server closes an open connection with `code` (for example a server
    /// that closes with 4001 when it revokes a session). Codes the protocol would retry anyway
    /// are refreshed too. Call it once per code.
    pub fn with_close_code(mut self, code: u16) -> Self {
        if !self.close_codes.contains(&code) {
            self.close_codes.push(code);
        }
        self
    }

    /// The timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The close codes that also start a refresh.
    pub fn close_codes(&self) -> &[u16] {
        &self.close_codes
    }
}

/// The server refused the credentials of a WebSocket connection that has
/// [`WsSettings::with_credentials_refresh`]: refresh them and set the new ones with
/// [`BackendCredentials::set`](crate::BackendCredentials::set) (or clear them to give up). ONE
/// message per refresh, however many connections wait for it; every waiting connection is in
/// [`WsState::WaitingForCredentials`]. If your game already refreshes on its own and a refresh is
/// in flight, finish that one instead of starting a second. Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct WsCredentialsRefused {
    /// The connection that was refused first.
    pub name: WsName,
    /// The server's refusal (`Status` 401, `Disconnected` for a refused authentication message,
    /// or `Closed`).
    pub error: BackendError,
}

/// Reconnect policy: exponential backoff with full jitter, a cap and optional maximum attempts.
///
/// The delay before attempt `n` (1, 2, …) is a random value in `0..=min(cap, base · 2^(n-1))`
/// (without jitter: exactly that bound). The counter resets once a connection stays up for
/// `stable_after`. Defaults: base 500 ms, cap 30 s, unlimited attempts, stable after 10 s, jitter on.
/// A handshake refused with 429 or 503 and a `Retry-After` header (delta-seconds) waits at least
/// that long ([`BackendError::retry_after`](crate::BackendError::retry_after)), even above the cap
/// (at most [`MAX_TIMEOUT`](crate::MAX_TIMEOUT)).
#[derive(Clone, Debug)]
pub struct WsReconnect {
    base: Duration,
    cap: Duration,
    max_attempts: Option<u32>,
    stable_after: Duration,
    jitter: bool,
    pub(crate) retry_tls: bool,
}

impl Default for WsReconnect {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
            max_attempts: None,
            stable_after: Duration::from_secs(10),
            jitter: true,
            retry_tls: false,
        }
    }
}

impl WsReconnect {
    /// Never reconnect automatically.
    pub fn never() -> Self {
        Self { max_attempts: Some(0), ..Self::default() }
    }

    /// The first delay bound (default 500 ms, at least 1 ms).
    pub fn with_base(mut self, base: Duration) -> Self {
        self.base = base.max(Duration::from_millis(1));
        self
    }

    /// The largest delay (default 30 s, at least the base).
    pub fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap;
        self
    }

    /// Give up after this many reconnect attempts in a row (`None` = never give up; `Some(0)` =
    /// never reconnect; `Some(3)` = the first attempt plus up to 3 retries).
    pub fn with_max_attempts(mut self, max: Option<u32>) -> Self {
        self.max_attempts = max;
        self
    }

    /// How long a connection must stay up before the attempt counter resets (default 10 s).
    pub fn with_stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after;
        self
    }

    /// Also retry after a TLS error (default `false`: a certificate or TLS failure is treated as
    /// permanent, like a 401, and the connection goes `Disconnected` with the `Tls` error).
    pub fn with_tls_retry(mut self, retry: bool) -> Self {
        self.retry_tls = retry;
        self
    }

    /// Random jitter on (default) or off (exact, deterministic delays).
    pub fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }

    /// The upper bound of the delay before attempt `attempt` (1-based).
    pub fn delay_bound(&self, attempt: u32) -> Duration {
        let factor = 2u32.checked_pow(attempt.saturating_sub(1).min(30)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap.max(self.base))
    }

    pub(crate) fn delay(&self, attempt: u32, random: u64) -> Duration {
        let bound = self.delay_bound(attempt);
        if !self.jitter {
            return bound;
        }
        let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(random % nanos.saturating_add(1))
    }

    pub(crate) fn may_retry(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

/// The settings of one connection, given to [`WsClient::connect`]. Private fields + builder.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::{WsReconnect, WsSettings};
///
/// let settings = WsSettings::new("wss://game.example.com/ws")
///     .with_read_timeout(Duration::from_millis(20))
///     .with_request_timeout(Duration::from_secs(10))
///     .with_reconnect(WsReconnect::default().with_max_attempts(Some(10)));
/// assert!(settings.validate().is_ok());
/// ```
#[derive(Clone)]
pub struct WsSettings {
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    header_error: Option<String>,
    pub(crate) read_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) ping_interval: Duration,
    pub(crate) dead_after: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) max_message_bytes: usize,
    pub(crate) reconnect: WsReconnect,
    pub(crate) allow_insecure: bool,
    pub(crate) credentials: bool,
    pub(crate) protocol: Option<Arc<dyn WsProtocol>>,
    pub(crate) outbox_limit: usize,
    pub(crate) resend_limit: usize,
    pub(crate) waiting_limit: usize,
    pub(crate) auth_ack: Option<Duration>,
    pub(crate) refresh: Option<WsCredentialsRefresh>,
}

impl fmt::Debug for WsSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = self.url.parse::<http::Uri>().map(|u| crate::request::redacted_url(&u)).unwrap_or_else(|_| "<invalid>".into());
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("WsSettings")
            .field("url", &url)
            .field("header_names", &headers)
            .field("read_timeout", &self.read_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("ping_interval", &self.ping_interval)
            .field("dead_after", &self.dead_after)
            .field("request_timeout", &self.request_timeout)
            .field("max_message_bytes", &self.max_message_bytes)
            .field("reconnect", &self.reconnect)
            .field("allow_insecure", &self.allow_insecure)
            .field("credentials", &self.credentials)
            .field("protocol", &self.protocol.is_some())
            .field("credentials_refresh", &self.refresh)
            .finish_non_exhaustive()
    }
}

/// Default read timeout of a connection thread.
pub const DEFAULT_WS_READ_TIMEOUT: Duration = Duration::from_millis(20);
/// Default largest message (and frame) accepted: 1 MiB.
pub const DEFAULT_WS_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

impl WsSettings {
    /// A connection to `url` (`wss://…`, or `ws://` to a loopback host). Defaults: read timeout
    /// 20 ms, connect timeout 10 s, ping every 15 s, dead after 45 s without any frame, request
    /// timeout 10 s, 1 MiB messages, [`WsReconnect::default`], credentials applied, and (feature
    /// `json`) the `JsonEnvelope` protocol.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: HeaderMap::new(),
            header_error: None,
            read_timeout: DEFAULT_WS_READ_TIMEOUT,
            connect_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            dead_after: Duration::from_secs(45),
            request_timeout: Duration::from_secs(10),
            max_message_bytes: DEFAULT_WS_MAX_MESSAGE_BYTES,
            reconnect: WsReconnect::default(),
            allow_insecure: false,
            credentials: true,
            #[cfg(feature = "json")]
            protocol: Some(Arc::new(JsonEnvelope)),
            #[cfg(not(feature = "json"))]
            protocol: None,
            outbox_limit: 64,
            resend_limit: 32,
            waiting_limit: 64,
            auth_ack: None,
            refresh: None,
        }
    }

    /// How long one read waits for data (default 20 ms, clamped to 5..=250 ms). It is also about
    /// the latency added to every frame you send (the thread sends between reads) and sets the idle
    /// wake-up rate. Measured median request round trips through a TLS proxy: 30 ms at 5 ms,
    /// 43 ms at 20 ms, 118 ms at 100 ms (HTTP: about 20 ms).
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout.clamp(Duration::from_millis(5), Duration::from_millis(250));
        self
    }

    /// One deadline for TCP connect + TLS + handshake together (default 10 s, clamped to
    /// 100 ms..=1 h). A peer that trickles bytes cannot stretch it.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.clamp(Duration::from_millis(100), crate::config::MAX_TIMEOUT);
        self
    }

    /// Heartbeat: a ping every `interval` (default 15 s, 10 ms..=1 h), and the connection counts
    /// as dead when not a single byte (data, ping or pong) arrived for `dead_after` (default 45 s,
    /// at least the interval, at most 1 h). Both run in the connection thread, so they work while
    /// the game is not ticking. A frame that arrives slowly still counts as alive.
    pub fn with_heartbeat(mut self, interval: Duration, dead_after: Duration) -> Self {
        self.ping_interval = interval.clamp(Duration::from_millis(10), crate::config::MAX_TIMEOUT);
        self.dead_after = dead_after.clamp(self.ping_interval, crate::config::MAX_TIMEOUT);
        self
    }

    /// The default timeout of a request on this connection (default 10 s; clamped to 1 ms..=1 h).
    /// It counts from the call, time waiting for the connection included.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// The largest message and frame accepted from the server (default 1 MiB, at least 1 KiB).
    /// A bigger one closes the connection with code 1009. Frames you send above it are refused.
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes.max(1024);
        self
    }

    /// The reconnect policy (default [`WsReconnect::default`]).
    pub fn with_reconnect(mut self, reconnect: WsReconnect) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// Allow plain `ws://` to hosts other than loopback (default `false`). Development only.
    pub fn allow_insecure_ws(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }

    /// A header sent with every handshake (e.g. `Sec-WebSocket-Protocol`). Calling it again with
    /// the same name adds another value. Credentials are applied after it. An invalid one makes
    /// every attempt fail with `InvalidRequest`.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            (Ok(name), Ok(value)) => {
                self.headers.append(name, value);
            }
            _ => self.header_error = Some(format!("the handshake header `{name}` is not valid")),
        }
        self
    }

    /// Do not apply [`BackendCredentials`](crate::BackendCredentials) to this connection.
    pub fn without_credentials(mut self) -> Self {
        self.credentials = false;
        self
    }

    /// The message protocol for requests and pushes (default with feature `json`:
    /// `JsonEnvelope`; without it: none, so only raw frames work).
    pub fn with_protocol(mut self, protocol: impl WsProtocol) -> Self {
        self.protocol = Some(Arc::new(protocol));
        self
    }

    /// No request protocol: raw frames only ([`WsMessage`] in, `send_*` out).
    pub fn without_protocol(mut self) -> Self {
        self.protocol = None;
        self
    }

    /// How many frames sent while not connected are kept until the connection opens (default
    /// 64); more are dropped with a warning.
    pub fn with_outbox_limit(mut self, frames: usize) -> Self {
        self.outbox_limit = frames;
        self
    }

    /// How many requests marked "resend on reconnect" are kept across one connection loss
    /// (default 32); more are answered `Disconnected` (with `sent: Some(true)`).
    pub fn with_resend_limit(mut self, requests: usize) -> Self {
        self.resend_limit = requests;
        self
    }

    /// How many requests may wait for the connection to open (default 64); more are answered
    /// `Disconnected` with `sent: Some(false)` at once.
    pub fn with_waiting_limit(mut self, requests: usize) -> Self {
        self.waiting_limit = requests;
        self
    }

    /// First-message auth with acknowledgement (off by default): after the
    /// `Credentials::ws_auth_message` frame, nothing else (requests, frames) goes out until the
    /// protocol reports [`WsIncoming::AuthOk`] (with `JsonEnvelope`: `{"type":"auth.ok"}`). If the
    /// server does not acknowledge within `timeout`, the waiting requests are answered `Timeout`
    /// (`"not sent: …"`, or "sent before the connection was lost…" for a resend request that went
    /// out on an earlier link), the link is closed with 1008, and the connection goes
    /// `Disconnected` with that error (not retried). Without an auth message it has no effect.
    pub fn with_auth_ack(mut self, timeout: Duration) -> Self {
        self.auth_ack = Some(timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT));
        self
    }

    /// Refresh refused credentials once instead of ending the connection (off by default: a
    /// refused handshake or authentication ends it at once). See [`WsCredentialsRefresh`]. Has no
    /// effect on a connection [`without_credentials`](Self::without_credentials) or while no
    /// credentials are set.
    pub fn with_credentials_refresh(mut self, refresh: WsCredentialsRefresh) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// The credentials refresh, if enabled.
    pub fn credentials_refresh(&self) -> Option<&WsCredentialsRefresh> {
        self.refresh.as_ref()
    }

    /// The connect timeout.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The heartbeat: ping interval and dead-after.
    pub fn heartbeat(&self) -> (Duration, Duration) {
        (self.ping_interval, self.dead_after)
    }

    /// The URL as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The read timeout.
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// The reconnect policy.
    pub fn reconnect(&self) -> &WsReconnect {
        &self.reconnect
    }

    /// Check the URL (`ws://` / `wss://`, host, no user name / password or fragment, no path
    /// tricks) and the handshake headers. The plain-text rule is checked when connecting.
    pub fn validate(&self) -> Result<(), BackendError> {
        if let Some(why) = &self.header_error {
            return Err(BackendError::InvalidRequest(why.clone()));
        }
        systems::parse_ws_url(&self.url).map(|_| ())
    }
}

/// A request with a raw payload, for [`WsClient::request_raw`]; the protocol wraps it (with
/// `JsonEnvelope`, `kind` is `"type"` and the payload must be JSON).
#[derive(Clone)]
pub struct WsOutgoing {
    pub(crate) kind: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) resend: bool,
    pub(crate) timeout: Option<Duration>,
}

impl fmt::Debug for WsOutgoing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsOutgoing")
            .field("kind", &self.kind)
            .field("payload_bytes", &self.payload.len())
            .field("resend", &self.resend)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl WsOutgoing {
    /// A request of `kind` with this payload.
    pub fn new(kind: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self { kind: kind.into(), payload: payload.into(), resend: false, timeout: None }
    }

    /// Keep it across a connection loss and send it again after the reconnect (default off:
    /// a request already sent when the connection drops is answered `Disconnected`). Only for
    /// requests the server can safely receive twice.
    pub fn resend_on_reconnect(mut self, resend: bool) -> Self {
        self.resend = resend;
        self
    }

    /// This request's own timeout (instead of the connection's request timeout).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT));
        self
    }
}

/// A connection's state changed (or an attempt failed). Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct WsStateChanged {
    /// The connection.
    pub name: WsName,
    /// The new state.
    pub state: WsState,
    /// Why, when a connection was lost or refused (`None` when it opened or was closed by the game).
    pub error: Option<BackendError>,
}

/// A data frame the server sent (every one, whether or not the protocol also turned it into an
/// answer or a push). Written in `First` of the frame it arrived.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct WsMessage {
    /// The connection.
    pub name: WsName,
    /// The frame.
    pub frame: WsFrame,
}

/// The answer to [`WsClient::request_raw`]: the response payload or why not. Exactly one per
/// request. `Debug` shows the payload length only.
#[derive(Message, Clone)]
#[non_exhaustive]
pub struct WsRawResponse {
    /// The request.
    pub id: RequestId,
    /// The connection.
    pub name: WsName,
    /// The response payload (with `JsonEnvelope`: the JSON of `data`), or the error.
    pub result: Result<Vec<u8>, BackendError>,
}

impl fmt::Debug for WsRawResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let result = self.result.as_ref().map(Vec::len);
        f.debug_struct("WsRawResponse").field("id", &self.id).field("name", &self.name).field("result_bytes", &result).finish()
    }
}

/// A typed WebSocket request (features `ws` + `json`), sent with [`WsClient::request`] and
/// answered with [`WsResponse<Self::Response>`](WsResponse). Register with
/// [`add_ws_request`](crate::BackendAppExt::add_ws_request).
///
/// Methods are only ever added to this trait with a default implementation.
#[cfg(feature = "json")]
pub trait WsRequest: serde::Serialize + Send + Sync + 'static {
    /// The decoded answer.
    type Response: serde::de::DeserializeOwned + Send + Sync + 'static;
    /// The request kind on the wire (with [`JsonEnvelope`]: `"type"`).
    const KIND: &'static str;
    /// Resend after a reconnect if it was in flight (default `false`; see
    /// [`WsOutgoing::resend_on_reconnect`]).
    fn resend_on_reconnect(&self) -> bool {
        false
    }
}

/// A typed server push (features `ws` + `json`): pushes of kind `KIND` arrive as
/// [`WsPush<Self>`](WsPush). Register with [`add_ws_push`](crate::BackendAppExt::add_ws_push).
#[cfg(feature = "json")]
pub trait WsPushMessage: serde::de::DeserializeOwned + Send + Sync + 'static {
    /// The push kind on the wire (with [`JsonEnvelope`]: `"type"`).
    const KIND: &'static str;
}

/// The answer to a typed request: the decoded response or why not. Exactly one per request.
#[cfg(feature = "json")]
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct WsResponse<T: Send + Sync + 'static> {
    /// The request.
    pub id: RequestId,
    /// The connection.
    pub name: WsName,
    /// The decoded response, or the error.
    pub result: Result<T, BackendError>,
}

/// A typed server push (features `ws` + `json`).
#[cfg(feature = "json")]
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct WsPush<P: Send + Sync + 'static> {
    /// The connection.
    pub name: WsName,
    /// The decoded push.
    pub data: P,
}

/// What one connection is doing, in [`WsConnections`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WsConnectionInfo {
    /// The state.
    pub state: WsState,
    /// Failed attempts in a row (0 once a connection stayed up long enough).
    pub attempt: u32,
    /// The last error, if the connection was lost or refused.
    pub last_error: Option<BackendError>,
    /// Requests waiting for their answer.
    pub pending_requests: usize,
    /// Frames waiting for the connection to open.
    pub queued_frames: usize,
}

/// The state of every named connection (read-only for the game).
#[derive(Resource, Default, Debug)]
pub struct WsConnections {
    pub(crate) map: HashMap<WsName, WsConnectionInfo>,
}

impl WsConnections {
    /// The connection named `name`, once `connect` was applied for it (it stays after a
    /// disconnect, with state `Disconnected`).
    pub fn get(&self, name: &str) -> Option<&WsConnectionInfo> {
        self.map.get(&WsName::from(name))
    }

    /// Its state.
    pub fn state(&self, name: &str) -> Option<WsState> {
        self.get(name).map(|c| c.state)
    }

    /// Whether it is connected.
    pub fn is_connected(&self, name: &str) -> bool {
        self.state(name) == Some(WsState::Connected)
    }

    /// Every connection.
    pub fn iter(&self) -> impl Iterator<Item = (&WsName, &WsConnectionInfo)> {
        self.map.iter()
    }
}

/// Where an answer goes.
pub(crate) enum WsRoute {
    Raw,
    #[cfg(feature = "json")]
    Typed(Box<dyn TypedRoute>),
}

#[cfg(feature = "json")]
pub(crate) trait TypedRoute: Send + Sync + 'static {
    fn deliver(&self, id: RequestId, name: WsName, result: Result<Vec<u8>, BackendError>, commands: &mut bevy_ecs::system::Commands);
}

#[cfg(feature = "json")]
struct TypedRouteFor<T>(std::marker::PhantomData<fn() -> T>);

#[cfg(feature = "json")]
impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> TypedRoute for TypedRouteFor<T> {
    fn deliver(&self, id: RequestId, name: WsName, result: Result<Vec<u8>, BackendError>, commands: &mut bevy_ecs::system::Commands) {
        let result = result.and_then(|bytes| {
            let bytes = if bytes.iter().all(u8::is_ascii_whitespace) { b"null".to_vec() } else { bytes };
            serde_json::from_slice::<T>(&bytes)
                .map_err(|e| BackendError::Decode { message: e.to_string(), response: Box::new(crate::RawResponse::new(http::StatusCode::OK, bytes.clone())) })
        });
        commands.queue(move |world: &mut bevy_ecs::world::World| {
            if world.write_message(WsResponse::<T> { id, name, result }).is_none() {
                tracing::error!(">>> NET-BACKEND: {id}: `WsResponse<{}>` is not registered; the answer is lost", std::any::type_name::<T>());
            }
        });
    }
}

#[cfg(feature = "json")]
pub(crate) trait PushRoute: Send + Sync + 'static {
    fn deliver(&self, name: WsName, data: &[u8], commands: &mut bevy_ecs::system::Commands);
}

#[cfg(feature = "json")]
struct PushRouteFor<P>(std::marker::PhantomData<fn() -> P>);

#[cfg(feature = "json")]
impl<P: WsPushMessage> PushRoute for PushRouteFor<P> {
    fn deliver(&self, name: WsName, data: &[u8], commands: &mut bevy_ecs::system::Commands) {
        match serde_json::from_slice::<P>(data) {
            Ok(data) => commands.queue(move |world: &mut bevy_ecs::world::World| {
                world.write_message(WsPush::<P> { name, data });
            }),
            // serde_json's message can quote the payload: not logged.
            Err(_) => tracing::debug!(">>> NET-BACKEND: ws `{name}`: a `{}` push did not decode as `{}`", P::KIND, std::any::type_name::<P>()),
        }
    }
}

/// Something game systems asked for.
pub(crate) enum WsQueued {
    Connect(WsName, Box<WsSettings>),
    Disconnect(WsName),
    Send(WsName, WsFrame),
    Request {
        name: WsName,
        id: RequestId,
        out: WsOutgoing,
        route: WsRoute,
    },
    #[cfg_attr(not(feature = "json"), allow(dead_code))]
    Fail {
        name: WsName,
        id: RequestId,
        route: WsRoute,
        error: BackendError,
    },
}

/// Opens, closes and uses named WebSocket connections. Like [`HttpClient`](crate::HttpClient) a
/// resource with shared access only (`Res<WsClient>`), so any number of systems can use it
/// without ordering. Everything is applied in `PostUpdate`
/// ([`BackendSystems::Send`]).
///
/// ```no_run
/// use bevy::prelude::*;
/// use bevy_net_backend::prelude::*;
///
/// fn open(ws: Res<WsClient>) {
///     ws.connect("main", WsSettings::new("wss://game.example.com/ws"));
///     ws.send_text("main", r#"{"type":"hello"}"#);
/// }
///
/// fn show(mut changes: MessageReader<WsStateChanged>, mut frames: MessageReader<WsMessage>) {
///     for change in changes.read() {
///         info!("{} is now {:?}", change.name, change.state);
///     }
///     for message in frames.read().filter(|m| m.name == "main") {
///         info!("got {} bytes", message.frame.len());
///     }
/// }
/// # let _ = (open, show);
/// ```
#[derive(Resource, Default)]
pub struct WsClient {
    queue: Mutex<Vec<WsQueued>>,
    cancels: crate::inflight::CancelList,
    #[cfg(feature = "json")]
    response_types: HashSet<std::any::TypeId>,
    #[cfg(feature = "json")]
    pub(crate) push_routes: HashMap<&'static str, Vec<Box<dyn PushRoute>>>,
}

impl fmt::Debug for WsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsClient").field("queued", &self.lock().len()).finish_non_exhaustive()
    }
}

impl WsClient {
    fn lock(&self) -> MutexGuard<'_, Vec<WsQueued>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn drain(&self) -> Vec<WsQueued> {
        std::mem::take(&mut *self.lock())
    }

    /// Open (or re-open with new settings) the connection `name`. A connection of that name that
    /// is open is closed first; its waiting requests are answered `Disconnected`.
    pub fn connect(&self, name: impl Into<WsName>, settings: WsSettings) {
        self.lock().push(WsQueued::Connect(name.into(), Box::new(settings)));
    }

    /// Close the connection `name` (code 1000) and stop reconnecting. Waiting requests are
    /// answered `Disconnected`; queued frames are dropped.
    pub fn disconnect(&self, name: impl Into<WsName>) {
        self.lock().push(WsQueued::Disconnect(name.into()));
    }

    /// Send a frame. While the connection is still opening (or reconnecting) it waits (up to the
    /// outbox limit); on a closed or unknown connection it is dropped with a warning. Fire and
    /// forget: there is no answer.
    pub fn send(&self, name: impl Into<WsName>, frame: WsFrame) {
        self.lock().push(WsQueued::Send(name.into(), frame));
    }

    /// Send a text frame (see [`send`](Self::send)).
    pub fn send_text(&self, name: impl Into<WsName>, text: impl Into<String>) {
        self.send(name, WsFrame::Text(text.into()));
    }

    /// Send a binary frame (see [`send`](Self::send)).
    pub fn send_binary(&self, name: impl Into<WsName>, bytes: impl Into<Vec<u8>>) {
        self.send(name, WsFrame::Binary(bytes.into()));
    }

    /// A request through the connection's protocol; the answer is a [`WsRawResponse`]. Exactly
    /// one answer: the response, `Rejected`, `Timeout`, `Disconnected`, `Cancelled`, `Shutdown`,
    /// or `InvalidRequest` (unknown connection, no protocol).
    pub fn request_raw(&self, name: impl Into<WsName>, request: WsOutgoing) -> RequestId {
        let id = RequestId::next();
        self.lock().push(WsQueued::Request { name: name.into(), id, out: request, route: WsRoute::Raw });
        id
    }

    /// A typed request (features `ws` + `json`); the answer is a
    /// [`WsResponse<R::Response>`](WsResponse). Register `R` first with
    /// [`add_ws_request::<R>()`](crate::BackendAppExt::add_ws_request); an unregistered type is not
    /// sent and is answered on [`WsRawResponse`] with `InvalidRequest` (an error is logged).
    #[cfg(feature = "json")]
    pub fn request<R: WsRequest>(&self, name: impl Into<WsName>, request: &R) -> RequestId {
        let id = RequestId::next();
        let name = name.into();
        if !self.response_types.contains(&std::any::TypeId::of::<R::Response>()) {
            let type_name = std::any::type_name::<R>();
            tracing::error!(">>> NET-BACKEND: `{type_name}` is not registered: call `app.add_ws_request::<{type_name}>()`; answered on WsRawResponse");
            let error = BackendError::InvalidRequest(format!("request type `{type_name}` is not registered; call `app.add_ws_request::<{type_name}>()`"));
            self.lock().push(WsQueued::Fail { name, id, route: WsRoute::Raw, error });
            return id;
        }
        let route = WsRoute::Typed(Box::new(TypedRouteFor::<R::Response>(std::marker::PhantomData)));
        match serde_json::to_vec(request) {
            Ok(payload) => {
                let out = WsOutgoing::new(R::KIND, payload).resend_on_reconnect(request.resend_on_reconnect());
                self.lock().push(WsQueued::Request { name, id, out, route });
            }
            Err(e) => self.lock().push(WsQueued::Fail { name, id, route, error: BackendError::Encode(e.to_string()) }),
        }
        id
    }

    /// Cancel a request: the same shared path as [`HttpClient::cancel`](crate::HttpClient::cancel),
    /// for HTTP and WebSocket ids alike. A WebSocket request still waiting is answered `Cancelled`
    /// in the next frame's `First` (one already sent may still reach the server; its answer is
    /// discarded).
    pub fn cancel(&self, id: RequestId) {
        self.cancels.push(id);
    }

    pub(crate) fn share_cancels(&mut self, cancels: crate::inflight::CancelList) {
        self.cancels = cancels;
    }
}

#[cfg(feature = "json")]
pub(crate) fn register_request<R: WsRequest>(app: &mut App) {
    app.add_message::<WsResponse<R::Response>>();
    app.init_resource::<WsClient>();
    if let Some(mut client) = app.world_mut().get_resource_mut::<WsClient>() {
        client.response_types.insert(std::any::TypeId::of::<R::Response>());
    }
}

#[cfg(feature = "json")]
pub(crate) fn register_push<P: WsPushMessage>(app: &mut App) {
    app.add_message::<WsPush<P>>();
    app.init_resource::<WsClient>();
    if let Some(mut client) = app.world_mut().get_resource_mut::<WsClient>() {
        client.push_routes.entry(P::KIND).or_default().push(Box::new(PushRouteFor::<P>(std::marker::PhantomData)));
    }
}

/// Plugin part of `ws`: resources, messages, the three systems, and the real transport unless one
/// is installed.
pub(crate) fn build(app: &mut App) {
    app.init_resource::<WsClient>();
    let cancels = app.world().resource::<crate::InFlight>().cancel_list();
    if let Some(mut client) = app.world_mut().get_resource_mut::<WsClient>() {
        client.share_cancels(cancels);
    }
    // After the HTTP systems of the same set: they share `InFlight` and hand over the cancels
    // that are not HTTP's.
    app.init_resource::<WsConnections>()
        .init_resource::<systems::WsRuntime>()
        .add_message::<WsStateChanged>()
        .add_message::<WsCredentialsRefused>()
        .add_message::<WsMessage>()
        .add_message::<WsRawResponse>()
        .add_systems(First, systems::ws_receive.in_set(BackendSystems::Receive).after(crate::inflight::receive_answers))
        .add_systems(PostUpdate, systems::ws_send.in_set(BackendSystems::Send).after(crate::inflight::send_requests))
        .add_systems(Last, systems::ws_exit.in_set(BackendSystems::Exit).after(crate::inflight::shutdown_on_exit).run_if(on_message::<bevy_app::AppExit>));
    if !app.world().contains_resource::<WsTransportRes>() {
        app.insert_resource(WsTransportRes::new(TungsteniteTransport::new()));
    }
}
