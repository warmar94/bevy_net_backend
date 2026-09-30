//! Requests: [`RequestId`], [`OutgoingRequest`] (what the game builds), [`PreparedRequest`]
//! (what a transport receives), and the URL rules (base URL + path, the plain-`http://` policy).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Method, Uri};

use crate::config::ConfigError;
use crate::BackendError;

/// Identifies one request made through `bevy_net_backend`. Every answer carries the id of the
/// request it answers, so a game can match them.
///
/// Opaque on purpose: ids come from one process-wide counter (never reused while the process
/// runs), there is no public constructor, and the number inside is not part of the API. `Display`
/// shows it for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl RequestId {
    /// A new, unique id.
    pub(crate) fn next() -> Self {
        RequestId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// The number for a wire protocol (unique per process). Not public: the protocol sees it as a
    /// plain `u64`.
    #[cfg(feature = "ws")]
    pub(crate) fn wire(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What a request is for, so [`Credentials`](crate::Credentials) can treat kinds differently.
/// 0.1.0 only makes [`Http`](Self::Http) requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RequestPurpose {
    /// A normal HTTP request / response.
    Http,
    /// The opening handshake of a WebSocket connection (feature `ws`): no body; credentials may
    /// add headers or query parameters.
    WebSocketHandshake,
}

/// A request the game builds: method, path (appended to the base URL), query parameters,
/// headers, an optional body and an optional timeout.
///
/// Every field is private; use the constructors, the `with_*` builders and the accessors.
/// [`Credentials`](crate::Credentials) edit it through the `*_mut` accessors.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::http::Method;
/// use bevy_net_backend::OutgoingRequest;
///
/// let request = OutgoingRequest::new(Method::PUT, "/players/me/settings")
///     .with_query("lang", "en")
///     .with_header("X-Request-Source", "options-menu")
///     .with_body(br#"{"volume":0.8}"#.to_vec())
///     .with_header("Content-Type", "application/json")
///     .with_timeout(Duration::from_secs(5));
/// assert_eq!(request.path(), "/players/me/settings");
/// ```
///
/// Its `Debug` output never shows header values, query values or the body.
#[derive(Clone)]
pub struct OutgoingRequest {
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    purpose: RequestPurpose,
    credentials: bool,
    error: Option<BackendError>,
}

impl fmt::Debug for OutgoingRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let query: Vec<&str> = self.query.iter().map(|(name, _)| name.as_str()).collect();
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("OutgoingRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("query_names", &query)
            .field("header_names", &headers)
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("purpose", &self.purpose)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl OutgoingRequest {
    /// A request with this method for `path`, which must start with `/` and is appended to the
    /// base URL. The path is sent as given: percent-encode anything outside `A-Z a-z 0-9 - . _ ~ /`
    /// yourself (query parameters are encoded for you, see [`with_query`](Self::with_query)).
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            body: None,
            timeout: None,
            purpose: RequestPurpose::Http,
            credentials: true,
            error: None,
        }
    }

    /// A `GET` request.
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    /// A `POST` request.
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path)
    }

    /// A `PUT` request.
    pub fn put(path: impl Into<String>) -> Self {
        Self::new(Method::PUT, path)
    }

    /// A `PATCH` request.
    pub fn patch(path: impl Into<String>) -> Self {
        Self::new(Method::PATCH, path)
    }

    /// A `DELETE` request.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::DELETE, path)
    }

    /// Add a query parameter (`?name=value`); name and value are percent-encoded when the URL is
    /// built. Query values are never logged or printed by `Debug`.
    pub fn with_query(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Set a header (replacing one of the same name). An invalid name or value does not panic:
    /// the request is answered with [`BackendError::InvalidRequest`] and never sent.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            (Ok(name), Ok(value)) => {
                self.headers.insert(name, value);
            }
            (Err(_), _) => self.reject(format!("`{name}` is not a valid header name")),
            (Ok(_), Err(_)) => self.reject(format!("the value of header `{name}` is not a valid header value")),
        }
        self
    }

    /// Set the body (bytes, sent as they are; set a `Content-Type` header to match).
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Serialize `value` as the JSON body and set `Content-Type: application/json`. A value that
    /// cannot be serialized does not panic: the request is answered with
    /// [`BackendError::Encode`] and never sent.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn with_json<B: serde::Serialize + ?Sized>(mut self, value: &B) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => {
                self.body = Some(body);
                self.headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            Err(e) => {
                if self.error.is_none() {
                    self.error = Some(BackendError::Encode(e.to_string()));
                }
            }
        }
        self
    }

    /// This request's own timeout, instead of the config's (clamped like
    /// [`HttpConfig::with_timeout`](crate::HttpConfig::with_timeout)).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT));
        self
    }

    /// Do not apply the game's [`BackendCredentials`](crate::BackendCredentials) to this request
    /// (for example the login call itself).
    pub fn without_credentials(mut self) -> Self {
        self.credentials = false;
        self
    }

    /// Mark the request as invalid: it is answered with [`BackendError::InvalidRequest`] and
    /// `reason`, and never sent. The first reason given wins. For [`Credentials`](crate::Credentials)
    /// that cannot apply themselves (never put the secret itself into `reason`).
    pub fn reject(&mut self, reason: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(BackendError::InvalidRequest(reason.into()));
        }
    }

    /// The method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The path (appended to the base URL).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The query parameters, in order.
    pub fn query(&self) -> &[(String, String)] {
        &self.query
    }

    /// The query parameters, to edit (for example to add a key).
    pub fn query_mut(&mut self) -> &mut Vec<(String, String)> {
        &mut self.query
    }

    /// The headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The headers, to edit. Mark secret values with `HeaderValue::set_sensitive(true)`.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// The body, if any.
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Replace the body.
    pub fn set_body(&mut self, body: Option<Vec<u8>>) {
        self.body = body;
    }

    /// The request's own timeout, if set.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// What the request is for (always [`RequestPurpose::Http`] in this version).
    pub fn purpose(&self) -> RequestPurpose {
        self.purpose
    }

    /// Whether the game's credentials are applied to it.
    pub fn uses_credentials(&self) -> bool {
        self.credentials
    }

    /// The error recorded by a builder or [`reject`](Self::reject), if any.
    pub fn error(&self) -> Option<&BackendError> {
        self.error.as_ref()
    }

    #[cfg(feature = "ws")]
    pub(crate) fn set_purpose(&mut self, purpose: RequestPurpose) {
        self.purpose = purpose;
    }

    pub(crate) fn take_error(&mut self) -> Option<BackendError> {
        self.error.take()
    }

    pub(crate) fn into_parts(self) -> (Method, HeaderMap, Option<Vec<u8>>, Option<Duration>, RequestPurpose) {
        (self.method, self.headers, self.body, self.timeout, self.purpose)
    }
}

/// A request ready for an [`HttpTransport`](crate::HttpTransport): the full URL, every header (defaults
/// and credentials applied), the body, the timeout and the response body limit.
///
/// Built by the plugin; a transport only reads it. Its `Debug` output never shows header values,
/// the query string or the body.
#[derive(Clone)]
#[non_exhaustive]
pub struct PreparedRequest {
    /// The method.
    pub method: Method,
    /// The full URL (`http://` or `https://`).
    pub uri: Uri,
    /// Every header to send.
    pub headers: HeaderMap,
    /// The body, if any.
    pub body: Option<Vec<u8>>,
    /// The timeout of the whole call.
    pub timeout: Duration,
    /// The largest response body to accept, in bytes.
    pub max_body_bytes: u64,
    /// What the request is for.
    pub purpose: RequestPurpose,
}

impl fmt::Debug for PreparedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("PreparedRequest")
            .field("method", &self.method)
            .field("url", &redacted_url(&self.uri))
            .field("header_names", &headers)
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("purpose", &self.purpose)
            .finish()
    }
}

impl PreparedRequest {
    /// The URL's path (no query).
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    /// Whether the host is a loopback host (`localhost`, `127.x.x.x`, `[::1]`).
    pub fn is_loopback(&self) -> bool {
        self.uri.host().is_some_and(is_loopback_host)
    }

    /// Whether the URL is `https://`.
    pub fn is_https(&self) -> bool {
        self.uri.scheme_str().is_some_and(|s| s.eq_ignore_ascii_case("https"))
    }
}

/// `scheme://host[:port]/path` with the query replaced by `?…`, for logs and `Debug`.
pub(crate) fn redacted_url(uri: &Uri) -> String {
    let scheme = uri.scheme_str().unwrap_or("?");
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or("");
    let query = if uri.query().is_some() { "?…" } else { "" };
    format!("{scheme}://{authority}{}{query}", uri.path())
}

/// Whether `host` (as `Uri::host` returns it: IPv6 in brackets) is a loopback host:
/// `localhost`, any `127.x.x.x`, or `[::1]`. The one place this rule lives.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4: Ipv4Addr| v4.is_loopback()),
        Err(_) => false,
    }
}

/// The plain-text policy for a URL: the secure schemes (`https`, `wss`) always pass, the plain
/// ones (`http`, `ws`) only for a loopback host unless `allow_insecure` is set. Anything else is
/// an invalid request.
pub(crate) fn check_scheme(uri: &Uri, allow_insecure: bool) -> Result<(), BackendError> {
    let scheme = uri.scheme_str().unwrap_or("").to_ascii_lowercase();
    let host = uri.host().unwrap_or("");
    match scheme.as_str() {
        "https" | "wss" => Ok(()),
        "http" | "ws" if allow_insecure || is_loopback_host(host) => Ok(()),
        "http" | "ws" => Err(BackendError::InsecureHttp { host: host.to_string() }),
        _ => Err(BackendError::InvalidRequest(format!("unsupported URL scheme `{scheme}`"))),
    }
}

/// A validated base URL: `scheme://authority` + a path prefix without a trailing `/`.
pub(crate) struct Base {
    pub(crate) origin: String,
    pub(crate) prefix: String,
}

pub(crate) fn parse_base(base: &str) -> Result<Base, ConfigError> {
    let base = base.trim();
    if base.is_empty() {
        return Err(ConfigError::NoBaseUrl);
    }
    let uri = Uri::try_from(base).map_err(|e| ConfigError::BadBaseUrl(e.to_string()))?;
    let scheme = match uri.scheme_str() {
        Some(s) if s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https") => s.to_ascii_lowercase(),
        Some(s) => return Err(ConfigError::BadBaseUrl(format!("the scheme must be http or https, not `{s}`"))),
        None => return Err(ConfigError::BadBaseUrl("it needs a scheme, e.g. https://api.example.com".into())),
    };
    let Some(authority) = uri.authority() else {
        return Err(ConfigError::BadBaseUrl("it needs a host".into()));
    };
    if authority.as_str().contains('@') {
        return Err(ConfigError::BadBaseUrl("a user name or password in the URL is not supported; use Credentials".into()));
    }
    if authority.host().is_empty() {
        return Err(ConfigError::BadBaseUrl("it needs a host".into()));
    }
    if uri.query().is_some() || base.contains('#') {
        return Err(ConfigError::BadBaseUrl("it must not have a query or fragment".into()));
    }
    Ok(Base { origin: format!("{scheme}://{}", authority.as_str()), prefix: uri.path().trim_end_matches('/').to_string() })
}

/// Percent-encode a query name or value (everything but `A-Z a-z 0-9 - . _ ~`).
pub(crate) fn encode_component(s: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            // Both nibbles are < 16, so `get` always finds a digit.
            let hex = |nibble: u8| HEX.get(usize::from(nibble)).copied().map_or('0', char::from);
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 0x0f));
        }
    }
}

/// Refuse paths a server could resolve outside the base URL's prefix: every percent-decoding
/// level (until the text stops changing) is checked for control characters (NUL included), a
/// backslash, an encoded separator (`%2f`, `%5c`, any case), and a `.` / `..` segment (split on
/// `/` and `\`, `;` parameters ignored: `..;` counts).
pub(crate) fn check_path(path: &str) -> Result<(), BackendError> {
    let refuse = |why: &str| Err(BackendError::InvalidRequest(format!("path refused: {why}")));
    let mut current = path.to_string();
    for _ in 0..8 {
        if current.chars().any(char::is_control) {
            return refuse("control characters are not accepted");
        }
        let lower = current.to_ascii_lowercase();
        if current.contains('\\') || lower.contains("%2f") || lower.contains("%5c") {
            return refuse("backslashes and encoded `/` or `\\` are not accepted");
        }
        let dot_segment = current.split(['/', '\\']).any(|segment| {
            let segment = segment.split(';').next().unwrap_or("");
            segment == "." || segment == ".."
        });
        if dot_segment {
            return refuse("`.` and `..` segments (also encoded) are not accepted");
        }
        let decoded = percent_decode(&current);
        if decoded == current {
            return Ok(());
        }
        current = decoded;
    }
    refuse("it is percent-encoded too many times")
}

/// One level of percent-decoding (`%XX` → byte; anything else kept), invalid UTF-8 replaced.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: Option<&u8>| b.and_then(|b| char::from(*b).to_digit(16));
        match (bytes.get(i), hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
            (Some(b'%'), Some(hi), Some(lo)) => {
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                i += 3;
            }
            (Some(b), _, _) => {
                out.push(*b);
                i += 1;
            }
            (None, _, _) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Base URL + path + query → a checked URI (scheme policy included).
pub(crate) fn build_uri(base: &str, path: &str, query: &[(String, String)], allow_insecure: bool) -> Result<Uri, BackendError> {
    let base = parse_base(base).map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
    if !path.starts_with('/') {
        return Err(BackendError::InvalidRequest("the path must start with `/` (it is appended to the base URL; absolute URLs are not accepted)".into()));
    }
    if path.contains('?') || path.contains('#') {
        return Err(BackendError::InvalidRequest("put query parameters in `with_query`, not in the path".into()));
    }
    check_path(path)?;
    let mut url = String::with_capacity(base.origin.len() + base.prefix.len() + path.len() + 16);
    url.push_str(&base.origin);
    url.push_str(&base.prefix);
    url.push_str(path);
    for (i, (name, value)) in query.iter().enumerate() {
        url.push(if i == 0 { '?' } else { '&' });
        encode_component(name, &mut url);
        url.push('=');
        encode_component(value, &mut url);
    }
    // The error of `http` names no part of the URL, so no query value can leak through it.
    let uri = Uri::try_from(url.as_str()).map_err(|e| BackendError::InvalidRequest(format!("the path is not a valid URL path ({e})")))?;
    check_scheme(&uri, allow_insecure)?;
    Ok(uri)
}
