//! [`HttpConfig`]: where the API lives and how requests are made.

use std::fmt;
use std::time::Duration;

use bevy_ecs::resource::Resource;
use http::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};

/// The default request timeout: 15 s for the whole call (connect, send, receive).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// The default number of worker threads of the HTTP transport.
pub const DEFAULT_WORKERS: usize = 2;
/// The most worker threads the HTTP transport starts.
pub const MAX_WORKERS: usize = 8;
/// The default limit of a response body: 10 MiB. A bigger body is answered with
/// [`BackendError::BodyTooLarge`](crate::BackendError::BodyTooLarge).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;
/// The longest timeout accepted: one hour.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// What is wrong with an [`HttpConfig`] (from [`HttpConfig::validate`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// No base URL is set: every request is answered with an error until one is.
    NoBaseUrl,
    /// The base URL cannot be used; the text says why.
    BadBaseUrl(String),
    /// A default header name or value is not valid HTTP; the text names the header.
    BadHeader(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoBaseUrl => f.write_str("no base URL is configured"),
            ConfigError::BadBaseUrl(why) => write!(f, "bad base URL: {why}"),
            ConfigError::BadHeader(why) => write!(f, "bad default header: {why}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The backend's settings: a resource the plugin inserts, which the game may change at any time
/// (for example [`set_base_url`](Self::set_base_url) after reading its own settings file).
///
/// Build it with [`new`](Self::new) and the `with_*` methods; every field is private so that
/// a new setting is not a breaking change.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::HttpConfig;
///
/// let config = HttpConfig::new("https://api.example.com/v1")
///     .with_timeout(Duration::from_secs(10))
///     .with_header("X-Game-Version", "1.4.2")
///     .with_workers(2);
/// assert!(config.validate().is_ok());
/// ```
///
/// What is read when:
///
/// - base URL, timeout, default headers, `allow_insecure_http` and the body limit are read for
///   every request, when it is sent;
/// - the worker count is read once, when the HTTP transport is created (at plugin build).
#[derive(Resource, Clone)]
pub struct HttpConfig {
    base_url: String,
    timeout: Duration,
    headers: HeaderMap,
    /// Invalid default headers by (lowercased) name; a later valid value or removal clears one.
    header_errors: std::collections::BTreeMap<String, String>,
    workers: usize,
    allow_insecure_http: bool,
    max_body_bytes: u64,
}

impl Default for HttpConfig {
    /// No base URL; everything else at its default.
    fn default() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(concat!("bevy_net_backend/", env!("CARGO_PKG_VERSION"))));
        Self {
            base_url: String::new(),
            timeout: DEFAULT_TIMEOUT,
            headers,
            header_errors: std::collections::BTreeMap::new(),
            workers: DEFAULT_WORKERS,
            allow_insecure_http: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl fmt::Debug for HttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header VALUES are never printed: a game may put a key in a default header.
        let header_names: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        f.debug_struct("HttpConfig")
            // Never the raw text: a mistaken `user:pw@` or `?key=` must not reach a log.
            .field("base_url", &crate::request::parse_base(&self.base_url).map(|b| format!("{}{}", b.origin, b.prefix)).unwrap_or_else(|_| "<invalid>".into()))
            .field("timeout", &self.timeout)
            .field("headers", &header_names)
            .field("workers", &self.workers)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("max_body_bytes", &self.max_body_bytes)
            .finish()
    }
}

impl HttpConfig {
    /// A config for the API at `base_url` (for example `https://api.example.com/v1`). Request
    /// paths are appended to it: `get("/me")` asks for `https://api.example.com/v1/me`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), ..Self::default() }
    }

    /// Change the base URL (see [`new`](Self::new)).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Change the base URL in place, e.g. on the resource at runtime.
    pub fn set_base_url(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into();
    }

    /// The timeout of one whole request (default [`DEFAULT_TIMEOUT`]). Zero is raised to 1 ms and
    /// anything above [`MAX_TIMEOUT`] is lowered to it. A request can override it
    /// ([`OutgoingRequest::with_timeout`](crate::OutgoingRequest::with_timeout)).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = clamp_timeout(timeout);
        self
    }

    /// Change the timeout in place (see [`with_timeout`](Self::with_timeout)).
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = clamp_timeout(timeout);
    }

    /// A header sent with every request (a request's own header of the same name wins, and
    /// credentials are applied after both). An invalid name or value is reported by
    /// [`validate`](Self::validate) and makes every request fail with
    /// [`BackendError::InvalidRequest`](crate::BackendError::InvalidRequest) until fixed.
    ///
    /// The default set holds `User-Agent: bevy_net_backend/<version>`; set your own to replace it.
    /// Header values are never logged or printed by `Debug`.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        let key = name.to_ascii_lowercase();
        match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            (Ok(name), Ok(value)) => {
                self.headers.insert(name, value);
                self.header_errors.remove(&key);
            }
            (Err(_), _) => {
                self.header_errors.insert(key, format!("`{name}` is not a valid header name"));
            }
            (Ok(_), Err(_)) => {
                self.header_errors.insert(key, format!("the value of `{name}` is not a valid header value"));
            }
        }
        self
    }

    /// Remove a default header (for example `User-Agent`), including an invalid one.
    pub fn without_header(mut self, name: &str) -> Self {
        self.headers.remove(name);
        self.header_errors.remove(&name.to_ascii_lowercase());
        self
    }

    /// How many worker threads the HTTP transport runs (default [`DEFAULT_WORKERS`], clamped to
    /// `1..=`[`MAX_WORKERS`]). At most this many requests are on the wire at once; the rest wait
    /// in a queue; the time a request waits there counts against its timeout (see
    /// [`BackendError::Timeout`](crate::BackendError::Timeout)). Read once, when the transport is
    /// created.
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers.clamp(1, MAX_WORKERS);
        self
    }

    /// Allow plain `http://` to hosts other than `localhost`, `127.x.x.x` and `[::1]` (default
    /// `false`: those requests are answered with
    /// [`BackendError::InsecureHttp`](crate::BackendError::InsecureHttp) and never sent).
    /// Only for development servers on a trusted LAN: plain HTTP shows tokens to everyone on the
    /// path.
    pub fn allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// The largest response body accepted, in bytes (default [`DEFAULT_MAX_BODY_BYTES`]; at
    /// least 1). A bigger body is answered with
    /// [`BackendError::BodyTooLarge`](crate::BackendError::BodyTooLarge), so a broken or hostile
    /// server cannot exhaust memory. With feature `gzip` the limit applies to the decoded bytes
    /// too, so a small compressed "gzip bomb" stops at the limit.
    pub fn with_max_body_bytes(mut self, bytes: u64) -> Self {
        self.max_body_bytes = bytes.max(1);
        self
    }

    /// The base URL as set.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The default headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The worker count.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Whether plain `http://` to non-loopback hosts is allowed.
    pub fn insecure_http_allowed(&self) -> bool {
        self.allow_insecure_http
    }

    /// The response body limit in bytes.
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }

    /// Check the settings: a base URL is set, is `http://` or `https://` with a host, has no
    /// user name / password, query or fragment, and every default header is valid HTTP.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(why) = self.header_errors.values().next() {
            return Err(ConfigError::BadHeader(why.clone()));
        }
        crate::request::parse_base(&self.base_url).map(|_| ())
    }
}

fn clamp_timeout(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(1), MAX_TIMEOUT)
}
