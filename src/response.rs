//! Answers: [`RawResponse`], [`BackendError`], and the messages [`HttpResponse`] and
//! `JsonResponse` (feature `json`).

use std::fmt;
use std::time::Duration;

use bevy_ecs::message::Message;
use http::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use http::StatusCode;

use crate::RequestId;

/// What a server sent back: status, headers and the raw body.
///
/// Carried by a successful [`HttpResponse`] and by [`BackendError::Status`] (any status
/// outside 200–299). Its `Debug` output shows the body's length, not its content (a login answer
/// holds a token); read it with [`body`](Self::body), [`text`](Self::text) or `json`.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawResponse {
    /// The HTTP status.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, as received (decompressed with feature `gzip`). Empty for a download to a file.
    pub body: Vec<u8>,
    /// For a download to a file ([`HttpClient::download`](crate::HttpClient::download)): the file
    /// the transport wrote (see [`HttpDownload::receive`](crate::HttpDownload)). `None` otherwise.
    pub file: Option<crate::DownloadedFile>,
}

impl fmt::Debug for RawResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("RawResponse")
            .field("status", &self.status)
            .field("header_names", &headers)
            .field("body_bytes", &self.body.len())
            .field("file", &self.file)
            .finish()
    }
}

impl RawResponse {
    /// A response with this status and body and no headers (for custom transports and tests).
    pub fn new(status: StatusCode, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: HeaderMap::new(), body: body.into(), file: None }
    }

    /// Add a header (builder style).
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.append(name, value);
        self
    }

    /// Whether the status is 200–299.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// The body bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The body as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Decode the body as JSON, for example a server's error body
    /// (`{"message": "...", "errors": {...}}`). An empty body decodes as JSON `null`, so `()`
    /// and `Option<T>` accept a `204 No Content`.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        if self.body.iter().all(u8::is_ascii_whitespace) {
            serde_json::from_slice(b"null")
        } else {
            serde_json::from_slice(&self.body)
        }
    }
}

/// Why a request did not succeed. Every request gets exactly one answer; this is the error half.
///
/// The kinds are transport-neutral and `#[non_exhaustive]`: keep a catch-all arm. The
/// texts are what the dependency (ureq, rustls, serde_json) reported, never a guess.
///
/// `Debug` and `Display` never show a body, a header value or `Decode`'s message (which can
/// quote the body); `Debug` shows the message's length instead.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendError {
    /// The request could not be built (bad path, header, base URL, unregistered response type…);
    /// it was never sent.
    InvalidRequest(String),
    /// Plain text (`http://` or `ws://`) to a host that is not loopback, without
    /// [`allow_insecure_http`](crate::HttpConfig::allow_insecure_http); never sent.
    #[non_exhaustive]
    InsecureHttp {
        /// The host that was refused.
        host: String,
    },
    /// The body could not be serialized to JSON; never sent.
    Encode(String),
    /// A network failure: DNS, connect, reset, protocol, … (the dependency's words). Not sent for
    /// a DNS, connect or worker-start failure; otherwise it may have reached the server.
    Network(String),
    /// A TLS failure: handshake, certificate, … (the dependency's words). A handshake or
    /// certificate failure happens before any request byte is written.
    Tls(String),
    /// The request took longer than its timeout. The timeout counts from the moment the request
    /// is handed to the transport. With the HTTP transport, a text starting with `not sent:`
    /// means it was still waiting for a free worker and never went out; otherwise it may or may
    /// not have reached the server.
    Timeout(String),
    /// The ANSWER was bigger than its limit: an HTTP response body
    /// ([`HttpConfig::with_max_body_bytes`](crate::HttpConfig::with_max_body_bytes)), an SSH
    /// command's output (stdout + stderr) or an SFTP download (feature `ssh`: `SshTarget` /
    /// `SshCommand` limits). The request went out. For SSH the command was stopped (its channel
    /// closed) at the limit. A request that is itself too big is [`RequestTooLarge`](Self::RequestTooLarge).
    #[non_exhaustive]
    BodyTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// The server answered with a status outside 200–299 (redirects are not followed, so 3xx
    /// lands here too). The full answer is kept: decode a server's error body with
    /// `RawResponse::json` (feature `json`).
    Status(Box<RawResponse>),
    /// A 2xx answer whose body is not the expected JSON. `Display` and `Debug` do not show
    /// `message`.
    #[non_exhaustive]
    Decode {
        /// What serde_json reported. It can quote part of the body (a token, say): do not log
        /// it in release builds.
        message: String,
        /// The answer as received.
        response: Box<RawResponse>,
    },
    /// Cancelled by the game ([`HttpClient::cancel`](crate::HttpClient::cancel)).
    Cancelled,
    /// The app is exiting (`AppExit`) before an answer arrived. Never sent, unless it was already
    /// on the wire before the exit frame (then it may still reach the server). Requests made in
    /// the exit frame are never sent.
    Shutdown,
    /// No transport is installed, or the transport was removed or replaced before it answered.
    NoTransport,
    /// A WebSocket connection went away (or never came up, or was closed by the game) before the
    /// request was answered.
    #[non_exhaustive]
    Disconnected {
        /// Why.
        reason: String,
        /// For a request: whether it had gone out on a connection before (`Some(true)`: it may
        /// have reached the server) or never went out (`Some(false)`). `None` when the error is
        /// about the connection itself (in `WsStateChanged` / `WsConnectionInfo`). `Some(true)` errs
        /// on the safe side: a request handed to a link that had just received the server's close
        /// frame counts as sent although the link dropped it.
        sent: Option<bool>,
    },
    /// The server closed the WebSocket connection with a close frame: its code and reason, as
    /// structured data (4001 "logged in elsewhere" and 4003 "banned" need different reactions).
    #[non_exhaustive]
    Closed {
        /// The close code.
        code: u16,
        /// The close reason (may be empty).
        reason: String,
    },
    /// The REQUEST was bigger than its limit and was refused before anything was sent: a
    /// multipart upload over `Multipart::with_max_bytes` (feature `http`), a WebSocket request over
    /// the message limit, an SSH command line over 64 KiB, an SFTP upload over the transfer limit.
    #[non_exhaustive]
    RequestTooLarge {
        /// The limit in bytes.
        limit: u64,
        /// The request's size in bytes.
        size: u64,
    },
    /// The server answered a WebSocket request with an error. The payload is kept as bytes with
    /// `text()` / `json()` helpers; `Debug` and `Display` never show it.
    Rejected(Box<Rejection>),
    /// The SSH server's host key could not be verified (feature `ssh`): the host is not in
    /// known_hosts (and no pinned fingerprint matches), its key changed, or the key is revoked.
    /// The connection was closed before authentication: nothing was sent to the server.
    #[non_exhaustive]
    HostKey {
        /// The host as it was looked up in known_hosts (`host`, or `[host]:port` for a port other
        /// than 22).
        host: String,
        /// The server key's fingerprint as OpenSSH shows it (`SHA256:…`): public information, safe
        /// to show so an admin can compare it with the server's real key.
        fingerprint: String,
        /// What is wrong.
        problem: HostKeyProblem,
    },
    /// The SSH server accepted none of the configured authentication methods (feature `ssh`), or a
    /// key could not be loaded. The text names the methods tried (key file names, not paths), never
    /// a passphrase or key.
    AuthFailed(String),
    /// An SSH protocol error (feature `ssh`): no common algorithm, a refused channel or subsystem,
    /// a server that does not support strict key exchange with a cipher that needs it, an SFTP
    /// error status, … (the dependency's or the server's words).
    Ssh(String),
    /// A sign-in at an OAuth 2.0 / OpenID Connect provider did not succeed (feature `oauth`): the
    /// provider sent the player back with an error (`access_denied` when the player declined), the
    /// token endpoint refused the code, or its answer had no ID token. The text holds the
    /// provider's error code, never a code or a token.
    OAuth(String),
}

/// Why an SSH host key was refused ([`BackendError::HostKey`]). `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HostKeyProblem {
    /// The host is in no known_hosts file that was read, and no pinned fingerprint matches. Add
    /// the key to known_hosts (after checking the fingerprint on the server) or pin it.
    Unknown,
    /// known_hosts lists this host with a different key: possibly a man-in-the-middle attack, or
    /// the server was reinstalled. Never accepted automatically.
    Changed,
    /// The key is marked `@revoked` in known_hosts (or a revoked line for the host could not be
    /// read, which is treated the same way).
    Revoked,
}

impl fmt::Display for HostKeyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HostKeyProblem::Unknown => "unknown host key",
            HostKeyProblem::Changed => "the host key CHANGED",
            HostKeyProblem::Revoked => "the host key is REVOKED",
        })
    }
}

/// The error payload of a [`BackendError::Rejected`] answer (with the default JSON envelope: the
/// JSON of `error` in `{"id":…,"ok":false,"error":…}`).
#[derive(Clone, PartialEq, Eq)]
pub struct Rejection {
    payload: Vec<u8>,
}

impl fmt::Debug for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rejection").field("payload_bytes", &self.payload.len()).finish()
    }
}

impl Rejection {
    /// A rejection with this payload (for custom protocols and tests).
    pub fn new(payload: impl Into<Vec<u8>>) -> Self {
        Self { payload: payload.into() }
    }

    /// The payload bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.payload
    }

    /// The payload as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }

    /// Decode the payload as JSON (e.g. `{"code":"banned","message":…}`).
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }
}

impl BackendError {
    /// The HTTP status, for [`Status`](Self::Status) and [`Decode`](Self::Decode).
    pub fn status(&self) -> Option<StatusCode> {
        self.response().map(|r| r.status)
    }

    /// The server's answer, for [`Status`](Self::Status) and [`Decode`](Self::Decode).
    pub fn response(&self) -> Option<&RawResponse> {
        match self {
            BackendError::Status(response) | BackendError::Decode { response, .. } => Some(response.as_ref()),
            _ => None,
        }
    }

    /// How long the server asked to wait before trying again: the `Retry-After` header of a
    /// [`Status`](Self::Status) answer (a 429 or a 503, say) in its delta-seconds form (`"120"`).
    /// `None` for every other error, without the header, or for a value that is not a whole number
    /// of seconds (an HTTP-date). Anything above [`MAX_TIMEOUT`](crate::MAX_TIMEOUT) is lowered to it.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            BackendError::Status(response) => response
                .headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds).min(crate::config::MAX_TIMEOUT)),
            _ => None,
        }
    }

    /// Whether the request never left the machine because it was invalid (invalid request,
    /// insecure http, encode error, request too large).
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, BackendError::InvalidRequest(_) | BackendError::InsecureHttp { .. } | BackendError::Encode(_) | BackendError::RequestTooLarge { .. })
    }

    /// What this answer says about whether the request reached the network: `Some(false)` never
    /// sent (invalid, `RequestTooLarge`, a `Timeout` whose text starts with `not sent:`, a `Disconnected` with
    /// `sent: Some(false)`, an SSH `HostKey` / `AuthFailed`), `Some(true)` the server answered (`Status`, `Decode`,
    /// `BodyTooLarge`, `Rejected`) or it went out before a loss (`Disconnected` with
    /// `sent: Some(true)`), `None` unknown ("maybe"). `RequestTooLarge` is `Some(false)`,
    /// `BodyTooLarge` (the answer was too big) `Some(true)`.
    pub fn was_sent(&self) -> Option<bool> {
        match self {
            BackendError::InvalidRequest(_) | BackendError::InsecureHttp { .. } | BackendError::Encode(_) | BackendError::RequestTooLarge { .. } => Some(false),
            BackendError::HostKey { .. } | BackendError::AuthFailed(_) => Some(false),
            BackendError::Timeout(why) if why.starts_with("not sent:") => Some(false),
            BackendError::Disconnected { sent, .. } => *sent,
            BackendError::Status(_) | BackendError::Decode { .. } | BackendError::BodyTooLarge { .. } | BackendError::Rejected(_) => Some(true),
            _ => None,
        }
    }

    /// The close code, for [`Closed`](Self::Closed).
    pub fn close_code(&self) -> Option<u16> {
        match self {
            BackendError::Closed { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// A [`HostKey`](Self::HostKey) error (for custom SSH transports and tests).
    pub fn host_key(host: impl Into<String>, fingerprint: impl Into<String>, problem: HostKeyProblem) -> Self {
        BackendError::HostKey { host: host.into(), fingerprint: fingerprint.into(), problem }
    }

    /// A [`RequestTooLarge`](Self::RequestTooLarge) error (for custom transports and tests).
    pub fn request_too_large(limit: u64, size: u64) -> Self {
        BackendError::RequestTooLarge { limit, size }
    }

    #[cfg(any(feature = "ws", feature = "ssh"))]
    pub(crate) fn disconnected(reason: impl Into<String>, sent: Option<bool>) -> Self {
        BackendError::Disconnected { reason: reason.into(), sent }
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::InvalidRequest(why) => f.debug_tuple("InvalidRequest").field(why).finish(),
            BackendError::InsecureHttp { host } => f.debug_struct("InsecureHttp").field("host", host).finish(),
            BackendError::Encode(why) => f.debug_tuple("Encode").field(why).finish(),
            BackendError::Network(why) => f.debug_tuple("Network").field(why).finish(),
            BackendError::Tls(why) => f.debug_tuple("Tls").field(why).finish(),
            BackendError::Timeout(why) => f.debug_tuple("Timeout").field(why).finish(),
            BackendError::BodyTooLarge { limit } => f.debug_struct("BodyTooLarge").field("limit", limit).finish(),
            BackendError::RequestTooLarge { limit, size } => f.debug_struct("RequestTooLarge").field("limit", limit).field("size", size).finish(),
            BackendError::Status(response) => f.debug_tuple("Status").field(response).finish(),
            BackendError::Decode { message, response } => f.debug_struct("Decode").field("message_len", &message.len()).field("response", response).finish(),
            BackendError::Cancelled => f.write_str("Cancelled"),
            BackendError::Shutdown => f.write_str("Shutdown"),
            BackendError::NoTransport => f.write_str("NoTransport"),
            BackendError::Disconnected { reason, sent } => f.debug_struct("Disconnected").field("reason", reason).field("sent", sent).finish(),
            BackendError::Closed { code, reason } => f.debug_struct("Closed").field("code", code).field("reason", reason).finish(),
            BackendError::Rejected(rejection) => f.debug_tuple("Rejected").field(rejection).finish(),
            BackendError::HostKey { host, fingerprint, problem } => {
                f.debug_struct("HostKey").field("host", host).field("fingerprint", fingerprint).field("problem", problem).finish()
            }
            BackendError::AuthFailed(why) => f.debug_tuple("AuthFailed").field(why).finish(),
            BackendError::Ssh(why) => f.debug_tuple("Ssh").field(why).finish(),
            BackendError::OAuth(why) => f.debug_tuple("OAuth").field(why).finish(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::InvalidRequest(why) => write!(f, "invalid request: {why}"),
            BackendError::InsecureHttp { host } => {
                write!(f, "plain http:// to `{host}` refused (use https://, a loopback host, or allow_insecure_http)")
            }
            BackendError::Encode(why) => write!(f, "could not encode the JSON body: {why}"),
            BackendError::Network(why) => write!(f, "network error: {why}"),
            BackendError::Tls(why) => write!(f, "TLS error: {why}"),
            BackendError::Timeout(why) => write!(f, "timed out ({why})"),
            BackendError::BodyTooLarge { limit } => write!(f, "the answer is larger than the limit of {limit} bytes"),
            BackendError::RequestTooLarge { limit, size } => write!(f, "the request ({size} bytes) is larger than the limit of {limit} bytes; not sent"),
            BackendError::Status(response) => write!(f, "HTTP status {}", response.status),
            // serde_json's message can quote the body: it stays in the field, out of Display.
            BackendError::Decode { response, .. } => write!(f, "the answer (HTTP {}) is not the expected JSON", response.status),
            BackendError::Cancelled => f.write_str("cancelled"),
            BackendError::Shutdown => f.write_str("the app is shutting down"),
            BackendError::NoTransport => f.write_str("no transport is installed (or it was replaced before it answered)"),
            BackendError::Disconnected { reason, sent: Some(true) } => write!(f, "disconnected after the request was sent: {reason}"),
            BackendError::Disconnected { reason, sent: Some(false) } => write!(f, "disconnected, the request was never sent: {reason}"),
            BackendError::Disconnected { reason, sent: None } => write!(f, "disconnected: {reason}"),
            BackendError::Closed { code, reason } if reason.is_empty() => write!(f, "closed by the server (code {code})"),
            BackendError::Closed { code, reason } => write!(f, "closed by the server (code {code}: {reason})"),
            BackendError::Rejected(_) => f.write_str("the server rejected the request"),
            BackendError::HostKey { host, fingerprint, problem } => write!(f, "SSH host key check failed for `{host}`: {problem} ({fingerprint})"),
            BackendError::AuthFailed(why) => write!(f, "SSH authentication failed: {why}"),
            BackendError::Ssh(why) => write!(f, "SSH error: {why}"),
            BackendError::OAuth(why) => write!(f, "sign-in failed: {why}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// The answer to a raw request ([`HttpClient::send`](crate::HttpClient::send),
/// [`request`](crate::HttpClient::request), [`get`](crate::HttpClient::get)): the
/// response (status 200–299) or why not.
///
/// Written in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)) of the frame
/// the answer arrived, so `PreUpdate` and `Update` read it that frame.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct HttpResponse {
    /// The request this answers.
    pub id: RequestId,
    /// The response, or the error.
    pub result: Result<RawResponse, BackendError>,
}

/// Upload progress of an HTTP request's body: the bytes read for sending so far (handed to the
/// connection; the server may not have received all of them yet). Only for requests with
/// [`OutgoingRequest::with_upload_progress`](crate::OutgoingRequest::with_upload_progress) (on
/// for multipart forms). The built-in transport writes at most about 10 per second per request,
/// plus one when the whole body is out; the answer ([`HttpResponse`] / `JsonResponse<T>`) follows
/// as usual. Written in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)),
/// before the answers of that frame, and only while the request waits for its answer.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpProgress {
    /// The request.
    pub id: RequestId,
    /// Bytes of the body read for sending so far.
    pub sent: u64,
    /// The body's size, when known.
    pub total: Option<u64>,
}

/// The answer to a typed JSON request (`get_json::<T>`, `post_json::<T>`, `send_json::<T>`):
/// the decoded `T` or why not. Register each `T` once with
/// [`BackendAppExt::add_json_response`](crate::BackendAppExt::add_json_response).
///
/// Written in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)) of the frame
/// the answer arrived, so `PreUpdate` and `Update` read it that frame.
#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct JsonResponse<T: Send + Sync + 'static> {
    /// The request this answers.
    pub id: RequestId,
    /// The decoded body, or the error.
    pub result: Result<T, BackendError>,
}
