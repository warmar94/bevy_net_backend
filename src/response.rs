//! Answers: [`RawResponse`], [`BackendError`], and the messages [`HttpResponse`] and
//! `JsonResponse` (feature `json`).

use std::fmt;

use bevy_ecs::message::Message;
use http::header::{HeaderMap, HeaderName, HeaderValue};
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
    /// The body, as received (decompressed with feature `gzip`).
    pub body: Vec<u8>,
}

impl fmt::Debug for RawResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("RawResponse").field("status", &self.status).field("header_names", &headers).field("body_bytes", &self.body.len()).finish()
    }
}

impl RawResponse {
    /// A response with this status and body and no headers (for custom transports and tests).
    pub fn new(status: StatusCode, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: HeaderMap::new(), body: body.into() }
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
/// The kinds are transport-neutral and `#[non_exhaustive]`: later versions may add kinds. The
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
    /// Plain text (`http://`, later also `ws://`) to a host that is not loopback, without
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
    /// The response body was bigger than the limit
    /// ([`HttpConfig::with_max_body_bytes`](crate::HttpConfig::with_max_body_bytes)).
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

    /// Whether the request never left the machine because it was invalid (invalid request,
    /// insecure http, encode error).
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, BackendError::InvalidRequest(_) | BackendError::InsecureHttp { .. } | BackendError::Encode(_))
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
            BackendError::Status(response) => f.debug_tuple("Status").field(response).finish(),
            BackendError::Decode { message, response } => f.debug_struct("Decode").field("message_len", &message.len()).field("response", response).finish(),
            BackendError::Cancelled => f.write_str("Cancelled"),
            BackendError::Shutdown => f.write_str("Shutdown"),
            BackendError::NoTransport => f.write_str("NoTransport"),
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
            BackendError::BodyTooLarge { limit } => write!(f, "the response body is larger than the limit of {limit} bytes"),
            BackendError::Status(response) => write!(f, "HTTP status {}", response.status),
            // serde_json's message can quote the body: it stays in the field, out of Display.
            BackendError::Decode { response, .. } => write!(f, "the answer (HTTP {}) is not the expected JSON", response.status),
            BackendError::Cancelled => f.write_str("cancelled"),
            BackendError::Shutdown => f.write_str("the app is shutting down"),
            BackendError::NoTransport => f.write_str("no transport is installed (or it was replaced before it answered)"),
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
