//! Authentication: [`Secret`], the [`Credentials`] hook, the [`BackendCredentials`] resource and
//! the ready-made [`BearerToken`], [`ApiKeyHeader`], [`ApiKeyQuery`] (and `JsonBodyField` with
//! feature `json`).

use std::fmt;
use std::sync::Arc;

use bevy_ecs::resource::Resource;
use http::header::{HeaderName, HeaderValue, AUTHORIZATION};

use crate::request::OutgoingRequest;

/// A secret string (token, key, password) that never shows up in `Debug`, `Display` or this
/// crate's logs: both print `<redacted>`. Read it with [`expose`](Self::expose) where it is really
/// needed.
///
/// It is a plain `String` inside: no comparison (compare `expose()` yourself if you must) and no
/// zeroing of memory on drop. At `trace` level the HTTP client's own logging (ureq / ureq_proto)
/// writes raw request bytes, secrets included: keep those targets below `trace`.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself. Do not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Adds the game's authentication to a request, right before it is sent (after the config's
/// default headers and the request's own headers, so it wins over both).
///
/// The game logs in with its own call, then puts an implementation into
/// [`BackendCredentials`]. Ready-made: [`BearerToken`], [`ApiKeyHeader`], [`ApiKeyQuery`],
/// `JsonBodyField` (feature `json`). Anything else (a signature, two headers, …) is a few
/// lines:
///
/// ```
/// use bevy_net_backend::{Credentials, OutgoingRequest, Secret};
/// use bevy_net_backend::http::HeaderValue;
///
/// struct SessionCookie(Secret);
///
/// impl Credentials for SessionCookie {
///     fn apply(&self, request: &mut OutgoingRequest) {
///         match HeaderValue::try_from(format!("session={}", self.0.expose())) {
///             Ok(mut value) => {
///                 value.set_sensitive(true);
///                 request.headers_mut().insert("cookie", value);
///             }
///             // Never put the secret into the reason.
///             Err(_) => request.reject("the session cookie is not a valid header value"),
///         }
///     }
/// }
/// ```
///
/// `apply` must not block (it runs on the main thread) and must never log the secret. (The HTTP
/// client's own `trace` logging writes raw request bytes, headers included: keep the `ureq` and
/// `ureq_proto` log targets below `trace`, see the README.) A request
/// made with [`OutgoingRequest::without_credentials`] is not passed to it.
///
/// **Compatibility promise:** methods added to this trait in later versions always come with a
/// default implementation, so an implementation written today keeps compiling.
pub trait Credentials: Send + Sync + 'static {
    /// Edit the request: add a header, a query parameter or a body field. Look at
    /// [`OutgoingRequest::purpose`] if a kind of request needs different handling.
    fn apply(&self, request: &mut OutgoingRequest);

    /// First-message authentication for WebSocket connections (feature `ws`): a text frame sent
    /// as the very first frame on every (re)connection, for backends that read the token from the
    /// first message instead of the handshake. Default: none. Never log what it returns.
    fn ws_auth_message(&self) -> Option<String> {
        None
    }
}

/// The game's current credentials, applied to every request (except those made
/// [`without_credentials`](OutgoingRequest::without_credentials)). Empty by default; the game
/// sets it after its own login call and clears it on logout.
///
/// Its `Debug` output only says whether credentials are set.
#[derive(Resource, Default, Clone)]
pub struct BackendCredentials {
    inner: Option<Arc<dyn Credentials>>,
}

impl fmt::Debug for BackendCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendCredentials").field("set", &self.inner.is_some()).finish()
    }
}

impl BackendCredentials {
    /// Credentials set to `credentials`.
    pub fn new(credentials: impl Credentials) -> Self {
        Self { inner: Some(Arc::new(credentials)) }
    }

    /// Use these credentials from now on (requests already sent keep what they had).
    pub fn set(&mut self, credentials: impl Credentials) {
        self.inner = Some(Arc::new(credentials));
    }

    /// Stop sending credentials (logout).
    pub fn clear(&mut self) {
        self.inner = None;
    }

    /// Whether credentials are set.
    pub fn is_set(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn apply(&self, request: &mut OutgoingRequest) {
        if let Some(credentials) = &self.inner {
            credentials.apply(request);
        }
    }

    #[cfg(feature = "ws")]
    pub(crate) fn ws_auth_message(&self) -> Option<String> {
        self.inner.as_ref().and_then(|c| c.ws_auth_message())
    }
}

/// `Authorization: Bearer <token>`: Laravel Sanctum / Passport, most Node and Go APIs, JWTs.
#[derive(Clone, Debug)]
pub struct BearerToken(Secret);

impl BearerToken {
    /// A bearer token. A token with characters not allowed in a header (such as a line break)
    /// makes each request fail with `InvalidRequest` instead of being sent.
    pub fn new(token: impl Into<Secret>) -> Self {
        Self(token.into())
    }

    /// The token.
    pub fn token(&self) -> &Secret {
        &self.0
    }
}

impl Credentials for BearerToken {
    fn apply(&self, request: &mut OutgoingRequest) {
        match sensitive_value(&format!("Bearer {}", self.0.expose())) {
            Some(value) => {
                request.headers_mut().insert(AUTHORIZATION, value);
            }
            None => request.reject("the bearer token contains characters that are not allowed in a header"),
        }
    }
}

/// A key in a header of your choice, e.g. `X-Api-Key: <key>`.
#[derive(Clone, Debug)]
pub struct ApiKeyHeader {
    name: String,
    key: Secret,
}

impl ApiKeyHeader {
    /// The header `name` set to `key`. An invalid name or key makes each request fail with
    /// `InvalidRequest` instead of being sent.
    pub fn new(name: impl Into<String>, key: impl Into<Secret>) -> Self {
        Self { name: name.into(), key: key.into() }
    }

    /// The header name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Credentials for ApiKeyHeader {
    fn apply(&self, request: &mut OutgoingRequest) {
        let Ok(name) = HeaderName::try_from(self.name.as_str()) else {
            request.reject(format!("`{}` is not a valid header name", self.name));
            return;
        };
        match sensitive_value(self.key.expose()) {
            Some(value) => {
                request.headers_mut().insert(name, value);
            }
            None => request.reject(format!("the key for header `{}` contains characters that are not allowed in a header", self.name)),
        }
    }
}

/// A key in a query parameter, e.g. `?api_key=<key>`. Prefer a header where the API allows it:
/// URLs end up in server access logs, and ureq logs the full path and query at `trace` level.
#[derive(Clone, Debug)]
pub struct ApiKeyQuery {
    name: String,
    key: Secret,
}

impl ApiKeyQuery {
    /// The query parameter `name` set to `key` (percent-encoded when the URL is built).
    pub fn new(name: impl Into<String>, key: impl Into<Secret>) -> Self {
        Self { name: name.into(), key: key.into() }
    }

    /// The parameter name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Credentials for ApiKeyQuery {
    fn apply(&self, request: &mut OutgoingRequest) {
        let query = request.query_mut();
        query.retain(|(name, _)| name != &self.name);
        query.push((self.name.clone(), self.key.expose().to_string()));
    }
}

/// A field in the JSON body, e.g. `{"token": "<token>", ...}`, for APIs that want it there.
///
/// Applied only to requests whose body is a JSON object: the field is added (or replaced).
/// Requests without a body, or with a body that is not a JSON object (and every
/// [`RequestPurpose`](crate::RequestPurpose) other than `Http`), are left unchanged.
#[cfg(feature = "json")]
#[cfg_attr(docsrs, doc(cfg(feature = "json")))]
#[derive(Clone, Debug)]
pub struct JsonBodyField {
    name: String,
    value: Secret,
}

#[cfg(feature = "json")]
impl JsonBodyField {
    /// The body field `name` set to the string `value`.
    pub fn new(name: impl Into<String>, value: impl Into<Secret>) -> Self {
        Self { name: name.into(), value: value.into() }
    }

    /// The field name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(feature = "json")]
impl Credentials for JsonBodyField {
    fn apply(&self, request: &mut OutgoingRequest) {
        if request.purpose() == crate::RequestPurpose::WebSocketHandshake {
            request.reject("JsonBodyField cannot authenticate a WebSocket handshake (it has no body); use first-message auth (Credentials::ws_auth_message)");
            return;
        }
        if request.purpose() != crate::RequestPurpose::Http {
            return;
        }
        if request.is_multipart() {
            request.reject("JsonBodyField cannot authenticate a multipart upload (its body is a form, not JSON); use a header credential (BearerToken, ApiKeyHeader) or add the field to the form yourself");
            return;
        }
        let Some(body) = request.body() else { return };
        let Ok(serde_json::Value::Object(mut object)) = serde_json::from_slice::<serde_json::Value>(body) else {
            return;
        };
        object.insert(self.name.clone(), serde_json::Value::String(self.value.expose().to_string()));
        match serde_json::to_vec(&object) {
            Ok(body) => request.set_body(Some(body)),
            Err(_) => request.reject("could not re-encode the JSON body with the credential field"),
        }
    }
}

/// A header value marked sensitive (the `http` crate then prints it as `Sensitive`), or `None`
/// when the text is not a valid header value.
fn sensitive_value(text: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::try_from(text).ok()?;
    value.set_sensitive(true);
    Some(value)
}
