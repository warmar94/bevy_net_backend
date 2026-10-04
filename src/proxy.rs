//! The proxy for HTTP (feature `http`), WebSocket connections (feature `ws`) and the code exchange
//! of a sign-in (feature `oauth`): [`ProxySettings`] (what the game chooses) and the resolved
//! route every transport follows.
//!
//! - From the environment (the default): the first of `ALL_PROXY`, `all_proxy`, `HTTPS_PROXY`,
//!   `https_proxy`, `HTTP_PROXY`, `http_proxy` that holds a valid proxy URL
//!   (`[scheme://][user[:password]@]host[:port]`, scheme `http` when missing), with the bypass list
//!   of `NO_PROXY` (or `no_proxy`), comma separated: `*` = every host, `.example.com` /
//!   `*.example.com` = the subdomains, `example.` / `example*` = a prefix, anything else = exactly
//!   that host (ASCII case ignored, spaces around an entry ignored). The same variables, order
//!   and rules as ureq 3.4.2's `Proxy::try_from_env`. Read once, when a transport is created.
//! - Loopback hosts never use a proxy.
//! - HTTP goes through `http://` and `https://` proxies (`CONNECT`); WebSocket connections through
//!   `http://` proxies only (`CONNECT`). A request or connection that would have to use a proxy
//!   its transport cannot use (SOCKS, or `https://` for WebSocket) is answered `InvalidRequest`
//!   and never made around the proxy.

use std::fmt;

use http::Uri;
use zeroize::Zeroizing;

use crate::config::ConfigError;
use crate::response::BackendError;

/// The variables tried for the proxy, in this order (ureq's order).
const PROXY_VARS: [&str; 6] = ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
/// The variables tried for the bypass list, in this order.
const NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Which proxy `http://` / `https://` requests (feature `http`), WebSocket connections (feature
/// `ws`) and the code exchange of a sign-in (feature `oauth`) go through. Hand it to
/// [`BackendPlugin::with_proxy`](crate::BackendPlugin::with_proxy) (every transport the plugin
/// creates), or to `UreqTransport::with_settings` / `TungsteniteTransport::with_settings` for a
/// transport you create yourself.
///
/// - [`from_env`](Self::from_env) (the default): the `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`
///   environment variables with `NO_PROXY`, read when a transport is created.
/// - [`url`](Self::url): this proxy, whatever the environment says (`NO_PROXY` does not apply).
/// - [`direct`](Self::direct): no proxy, whatever the environment says.
///
/// Loopback hosts (`localhost`, `127.x.x.x`, `[::1]`) never use a proxy. HTTP requests go through
/// `http://` and `https://` proxies, WebSocket connections through `http://` proxies, both with
/// `CONNECT` and with `Proxy-Authorization: Basic` when the URL has a user and password. A request
/// or connection that would have to use a proxy its transport cannot use (a SOCKS proxy, or an
/// `https://` proxy for a WebSocket connection), or a [`url`](Self::url) that is not a proxy URL,
/// is answered [`BackendError::InvalidRequest`] and never sent. [`validate`](Self::validate)
/// checks a URL earlier. `Debug` never shows the user or password.
///
/// ```
/// use bevy_net_backend::{BackendPlugin, HttpConfig, ProxySettings};
///
/// let plugin = BackendPlugin::new(HttpConfig::new("https://api.example.com"))
///     .with_proxy(ProxySettings::url("http://proxy.example.com:3128"));
/// assert!(ProxySettings::url("http://proxy.example.com:3128").validate().is_ok());
/// assert!(ProxySettings::url("not a proxy").validate().is_err());
/// ```
#[derive(Clone, Default)]
pub struct ProxySettings {
    mode: Mode,
}

#[derive(Clone, Default)]
enum Mode {
    #[default]
    Env,
    /// The URL text (it may hold a password): wiped on drop.
    Url(Zeroizing<String>),
    Direct,
}

impl fmt::Debug for ProxySettings {
    /// Scheme, host and port only: never the user or password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.mode {
            Mode::Env => f.write_str("ProxySettings::from_env"),
            Mode::Direct => f.write_str("ProxySettings::direct"),
            Mode::Url(url) => match parse_proxy(url) {
                Some(target) => write!(f, "ProxySettings::url({})", target.shown()),
                None => f.write_str("ProxySettings::url(<invalid>)"),
            },
        }
    }
}

impl ProxySettings {
    /// The proxy from the environment variables (the default): see the type's documentation.
    pub fn from_env() -> Self {
        Self { mode: Mode::Env }
    }

    /// Always this proxy (`http://` or `https://`, `[scheme://][user[:password]@]host[:port]`,
    /// scheme `http` and port 80 / 443 when missing). The environment is not read; loopback
    /// hosts still go direct.
    pub fn url(url: impl Into<String>) -> Self {
        Self { mode: Mode::Url(Zeroizing::new(url.into())) }
    }

    /// No proxy at all, whatever the environment says.
    pub fn direct() -> Self {
        Self { mode: Mode::Direct }
    }

    /// Whether these are the environment settings ([`from_env`](Self::from_env)).
    pub fn is_from_env(&self) -> bool {
        matches!(self.mode, Mode::Env)
    }

    /// Whether these are the [`direct`](Self::direct) settings.
    pub fn is_direct(&self) -> bool {
        matches!(self.mode, Mode::Direct)
    }

    /// Check a [`url`](Self::url) now: a proxy URL with a host and a scheme this crate can use
    /// (`http` or `https`). `from_env` and `direct` always pass (an environment proxy is checked
    /// per request).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let Mode::Url(url) = &self.mode else { return Ok(()) };
        match parse_proxy(url) {
            None => Err(ConfigError::Proxy("not a proxy URL ([scheme://][user[:password]@]host[:port])".into())),
            Some(ProxyTarget { scheme: Scheme::Unsupported(scheme), .. }) => {
                Err(ConfigError::Proxy(format!("a {scheme}:// proxy is not supported (http:// and https:// proxies are)")))
            }
            Some(_) => Ok(()),
        }
    }

    /// The route every connection of one transport follows (reads the environment for
    /// `from_env`).
    pub(crate) fn resolve(&self) -> ProxyRoute {
        match &self.mode {
            Mode::Env => ProxyRoute::from_vars(|name| std::env::var(name).ok()),
            Mode::Direct => ProxyRoute { proxy: None, no_proxy: Vec::new() },
            Mode::Url(url) => ProxyRoute {
                proxy: Some(parse_proxy(url).ok_or_else(|| "the proxy URL of the ProxySettings is not a proxy URL".to_string())),
                no_proxy: Vec::new(),
            },
        }
    }
}

/// How a proxy is spoken to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
    /// A SOCKS proxy (its scheme): no transport of this crate uses one.
    Unsupported(String),
}

/// A proxy: where it is and, when its URL had a user, the `Proxy-Authorization` value.
pub(crate) struct ProxyTarget {
    pub(crate) scheme: Scheme,
    host: String,
    port: u16,
    /// `Basic <base64(user:password)>`, wiped on drop (written by the WebSocket `CONNECT`; ureq
    /// builds its own from `url`).
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) authorization: Option<Zeroizing<String>>,
    /// The URL as given (for ureq), wiped on drop.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    url: Zeroizing<String>,
}

impl ProxyTarget {
    /// The host to connect to (IPv6 without brackets) and the port.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    pub(crate) fn address(&self) -> (&str, u16) {
        let host = self.host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&self.host);
        (host, self.port)
    }

    fn scheme_name(&self) -> &str {
        match &self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::Unsupported(scheme) => scheme,
        }
    }

    /// `scheme://host:port`: no user, no password.
    fn shown(&self) -> String {
        format!("{}://{}:{}", self.scheme_name(), self.host, self.port)
    }

    /// The proxy for ureq (`http://` and `https://` only).
    #[cfg(feature = "http")]
    pub(crate) fn ureq(&self) -> Option<ureq::Proxy> {
        match self.scheme {
            Scheme::Http | Scheme::Https => ureq::Proxy::new(&self.url).ok(),
            Scheme::Unsupported(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NoProxyEntry {
    Exact(String),
    Prefix(String),
    Suffix(String),
    All,
}

impl NoProxyEntry {
    fn parse(entry: &str) -> Self {
        let entry = entry.trim();
        match entry {
            "*" => Self::All,
            e if e.starts_with('*') => Self::Suffix(e.chars().skip(1).collect::<String>().to_ascii_lowercase()),
            e if e.starts_with('.') => Self::Suffix(e.to_ascii_lowercase()),
            e if e.ends_with('*') => Self::Prefix(e.chars().take(e.chars().count().saturating_sub(1)).collect::<String>().to_ascii_lowercase()),
            e if e.ends_with('.') => Self::Prefix(e.to_ascii_lowercase()),
            e => Self::Exact(e.to_ascii_lowercase()),
        }
    }

    fn matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self {
            Self::All => true,
            Self::Exact(pattern) => *pattern == host,
            Self::Prefix(prefix) => host.starts_with(prefix.as_str()),
            Self::Suffix(suffix) => host.ends_with(suffix.as_str()),
        }
    }
}

/// The proxy one transport uses, resolved from its [`ProxySettings`].
#[derive(Default)]
pub(crate) struct ProxyRoute {
    /// `None`: direct. `Err`: a `ProxySettings::url` that is not a proxy URL (why).
    proxy: Option<Result<ProxyTarget, String>>,
    no_proxy: Vec<NoProxyEntry>,
}

impl fmt::Debug for ProxyRoute {
    /// Scheme, host and port only: never the user or password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proxy = match &self.proxy {
            None => "none".to_string(),
            Some(Ok(target)) => target.shown(),
            Some(Err(_)) => "<invalid>".to_string(),
        };
        f.debug_struct("ProxyRoute").field("proxy", &proxy).field("no_proxy_entries", &self.no_proxy.len()).finish()
    }
}

/// Which transport asks (what it can use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    /// HTTP requests: `http://` and `https://` proxies.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    Http,
    /// WebSocket connections: `http://` proxies.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    WebSocket,
}

impl ProxyRoute {
    /// From any source of variables (the environment; tests).
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let proxy = PROXY_VARS.iter().find_map(|name| var(name).and_then(|value| parse_proxy(&value))).map(Ok);
        let no_proxy = NO_PROXY_VARS
            .iter()
            .find_map(|name| var(name))
            .map(|list| list.split(',').filter(|e| !e.trim().is_empty()).map(NoProxyEntry::parse).collect())
            .unwrap_or_default();
        Self { proxy, no_proxy }
    }

    /// The proxy every connection that is not bypassed goes through (`None` = none; `Err` = an
    /// unusable `ProxySettings::url`).
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn proxy(&self) -> Option<Result<&ProxyTarget, &str>> {
        self.proxy.as_ref().map(|p| p.as_ref().map_err(String::as_str))
    }

    /// The proxy a connection to `uri` goes through: `None` = direct (no proxy, a loopback host,
    /// or a host on the bypass list); an error when the proxy is one `via` cannot use (never
    /// made around it).
    pub(crate) fn route(&self, uri: &Uri, via: Via) -> Result<Option<&ProxyTarget>, BackendError> {
        let Some(proxy) = &self.proxy else { return Ok(None) };
        let host = uri.host().unwrap_or_default();
        if crate::request::is_loopback_host(host) || self.no_proxy.iter().any(|entry| entry.matches(host)) {
            return Ok(None);
        }
        let target = proxy.as_ref().map_err(|why| BackendError::InvalidRequest(why.clone()))?;
        match (&target.scheme, via) {
            (Scheme::Http, _) | (Scheme::Https, Via::Http) => Ok(Some(target)),
            (Scheme::Https, Via::WebSocket) => Err(BackendError::InvalidRequest(
                "the proxy is an https:// proxy; WebSocket connections go through http:// proxies (CONNECT) only, and never around the proxy".into(),
            )),
            (Scheme::Unsupported(scheme), Via::Http) => Err(BackendError::InvalidRequest(format!(
                "the proxy is a {scheme}:// proxy, which is not supported; HTTP requests go through http:// or https:// proxies only, and never around the proxy"
            ))),
            (Scheme::Unsupported(scheme), Via::WebSocket) => Err(BackendError::InvalidRequest(format!(
                "the proxy is a {scheme}:// proxy; WebSocket connections go through http:// proxies (CONNECT) only, and never around the proxy"
            ))),
        }
    }
}

/// A proxy URL as ureq 3.4.2 reads it; `None` when it is not a valid proxy URL.
fn parse_proxy(value: &str) -> Option<ProxyTarget> {
    let value = value.trim();
    let uri = value.parse::<Uri>().ok()?;
    let authority = uri.authority()?;
    if authority.host().is_empty() {
        return None;
    }
    let scheme_text = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
    let (scheme, default_port) = match scheme_text.as_str() {
        "http" => (Scheme::Http, 80),
        "https" => (Scheme::Https, 443),
        "socks4" | "socks4a" | "socks" | "socks5" | "socks5h" => (Scheme::Unsupported(scheme_text.clone()), 1080),
        _ => return None,
    };
    let text = authority.as_str();
    let userinfo = text.rfind('@').and_then(|at| text.get(..at));
    let authorization = userinfo.map(|info| {
        let (user, password) = match info.rfind(':') {
            Some(colon) => (info.get(..colon).unwrap_or_default(), info.get(colon + 1..).unwrap_or_default()),
            None => (info, ""),
        };
        let mut plain = Zeroizing::new(String::with_capacity(user.len() + password.len() + 1));
        plain.push_str(user);
        plain.push(':');
        plain.push_str(password);
        let mut header = Zeroizing::new(String::with_capacity(6 + base64_len(plain.len())));
        header.push_str("Basic ");
        base64_into(plain.as_bytes(), &mut header);
        header
    });
    let url = Zeroizing::new(if uri.scheme_str().is_some() { value.to_string() } else { format!("http://{value}") });
    Some(ProxyTarget { scheme, host: authority.host().to_string(), port: authority.port_u16().unwrap_or(default_port), authorization, url })
}

fn base64_len(len: usize) -> usize {
    len.div_ceil(3) * 4
}

/// Standard base64 with padding, appended to `out` (which has room: no reallocation copy).
fn base64_into(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |index: u32| char::from(ALPHABET.get(usize::try_from(index & 63).unwrap_or(0)).copied().unwrap_or(b'A'));
    for chunk in bytes.chunks(3) {
        let b = [chunk.first().copied().unwrap_or(0), chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(symbol(n >> 18));
        out.push(symbol(n >> 12));
        out.push(if chunk.len() > 1 { symbol(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(n) } else { '=' });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> ProxyRoute {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        ProxyRoute::from_vars(|name| vars.get(name).cloned())
    }

    fn routed(route: &ProxyRoute, url: &str, via: Via) -> Result<Option<(String, u16)>, BackendError> {
        let uri: Uri = url.parse().unwrap_or_else(|e| panic!("{e}"));
        route.route(&uri, via).map(|p| p.map(|p| (p.address().0.to_string(), p.address().1)))
    }

    #[test]
    fn the_first_valid_variable_wins_in_ureqs_order() {
        let proxy = Some(("proxy.test".to_string(), 3128));
        for via in [Via::Http, Via::WebSocket] {
            assert_eq!(routed(&env(&[("HTTPS_PROXY", "http://proxy.test:3128")]), "wss://game.test/ws", via).ok(), Some(proxy.clone()));
            assert_eq!(routed(&env(&[("http_proxy", "proxy.test:3128")]), "ws://game.test/ws", via).ok(), Some(proxy.clone()));
            assert_eq!(
                routed(&env(&[("ALL_PROXY", "http://proxy.test:3128"), ("HTTPS_PROXY", "http://other.test:1")]), "wss://game.test/", via).ok(),
                Some(proxy.clone())
            );
            // An invalid value is skipped, the next one is used.
            assert_eq!(
                routed(&env(&[("ALL_PROXY", "fakeproto://x.test"), ("HTTP_PROXY", "http://proxy.test:3128")]), "wss://game.test/", via).ok(),
                Some(proxy.clone())
            );
            assert_eq!(routed(&env(&[("HTTP_PROXY", "http://proxy.test")]), "wss://game.test/", via).ok(), Some(Some(("proxy.test".to_string(), 80))));
            assert_eq!(routed(&env(&[("HTTP_PROXY", "http://[::1]:8080")]), "wss://game.test/", via).ok(), Some(Some(("::1".to_string(), 8080))));
            assert_eq!(routed(&env(&[]), "wss://game.test/", via).ok(), Some(None));
        }
    }

    #[test]
    fn loopback_and_no_proxy_hosts_go_direct() {
        let base = [("HTTPS_PROXY", "http://proxy.test:3128")];
        for via in [Via::Http, Via::WebSocket] {
            for url in ["ws://127.0.0.1:9000/", "http://localhost/", "ws://[::1]:9000/"] {
                assert_eq!(routed(&env(&base), url, via).ok(), Some(None), "{url}");
            }
            let with = |list: &str| env(&[base[0], ("NO_PROXY", list)]);
            assert_eq!(routed(&with("game.test"), "wss://game.test/", via).ok(), Some(None));
            assert_eq!(routed(&with("GAME.test"), "wss://Game.Test/", via).ok(), Some(None));
            assert!(routed(&with("game.test"), "wss://eu.game.test/", via).is_ok_and(|p| p.is_some()));
            assert_eq!(routed(&with(".game.test"), "wss://eu.game.test/", via).ok(), Some(None));
            assert_eq!(routed(&with("*.game.test"), "wss://eu.game.test/", via).ok(), Some(None));
            assert!(routed(&with(".game.test"), "wss://game.test/", via).is_ok_and(|p| p.is_some()));
            assert_eq!(routed(&with("10.0.*"), "ws://10.0.0.7/", via).ok(), Some(None));
            assert_eq!(routed(&with("*"), "wss://anything.test/", via).ok(), Some(None));
            assert_eq!(routed(&env(&[base[0], ("no_proxy", "game.test")]), "wss://game.test/", via).ok(), Some(None));
            assert!(routed(&with("other.test, game.test"), "wss://game.test/", via).is_ok_and(|p| p.is_none()));
        }
    }

    #[test]
    fn socks_proxies_are_refused_never_bypassed_and_https_only_carries_http() {
        for proxy in ["socks5://proxy.test:1080", "socks5h://proxy.test", "socks4://proxy.test", "socks://proxy.test"] {
            for via in [Via::Http, Via::WebSocket] {
                let error = routed(&env(&[("ALL_PROXY", proxy)]), "https://game.test/", via).err();
                assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("never around")), "{proxy}: {error:?}");
                assert_eq!(error.and_then(|e| e.was_sent()), Some(false));
                // Loopback and bypassed hosts still go direct.
                assert_eq!(routed(&env(&[("ALL_PROXY", proxy)]), "http://127.0.0.1:1/", via).ok(), Some(None));
                assert_eq!(routed(&env(&[("ALL_PROXY", proxy), ("NO_PROXY", "game.test")]), "https://game.test/", via).ok(), Some(None));
            }
        }
        let https = env(&[("HTTPS_PROXY", "https://proxy.test")]);
        assert_eq!(routed(&https, "https://game.test/", Via::Http).ok(), Some(Some(("proxy.test".to_string(), 443))));
        let error = routed(&https, "wss://game.test/", Via::WebSocket).err();
        assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("http:// proxies")), "{error:?}");
    }

    #[test]
    fn settings_in_code_override_the_environment() {
        let uri: Uri = "https://game.test/".parse().unwrap_or_else(|e| panic!("{e}"));
        let direct = ProxySettings::direct().resolve();
        assert!(direct.route(&uri, Via::Http).is_ok_and(|p| p.is_none()) && direct.proxy().is_none());
        let url = ProxySettings::url("http://user:pw@proxy.test:3128").resolve();
        assert_eq!(url.route(&uri, Via::WebSocket).ok().flatten().map(ProxyTarget::address), Some(("proxy.test", 3128)));
        // Loopback still goes direct.
        let local: Uri = "http://127.0.0.1:8080/".parse().unwrap_or_else(|e| panic!("{e}"));
        assert!(url.route(&local, Via::Http).is_ok_and(|p| p.is_none()));
        // A URL that is not a proxy URL: every request that would use it is refused.
        let bad = ProxySettings::url("::not a url::").resolve();
        assert!(matches!(bad.route(&uri, Via::Http), Err(BackendError::InvalidRequest(_))));
        assert!(bad.route(&local, Via::Http).is_ok_and(|p| p.is_none()));
        // validate
        assert!(ProxySettings::from_env().validate().is_ok() && ProxySettings::direct().validate().is_ok());
        assert!(ProxySettings::url("proxy.test:3128").validate().is_ok());
        assert!(ProxySettings::url("https://proxy.test").validate().is_ok());
        for bad in ["::not a url::", "ftp://proxy.test", "socks5://proxy.test", ""] {
            assert!(matches!(ProxySettings::url(bad).validate(), Err(ConfigError::Proxy(_))), "{bad}");
        }
    }

    #[test]
    fn credentials_become_basic_authorization_and_never_show_in_debug() {
        let route = env(&[("HTTPS_PROXY", "http://user:p@ss:word@proxy.test:3128")]);
        let Some(Ok(proxy)) = route.proxy() else { panic!("no proxy") };
        // ureq's split: the last `@` ends the user info, the last `:` in it starts the password.
        let mut expected = String::from("Basic ");
        base64_into(b"user:p@ss:word", &mut expected);
        assert_eq!(proxy.authorization.as_deref().map(String::as_str), Some(expected.as_str()));
        assert_eq!(proxy.address(), ("proxy.test", 3128));
        let settings = ProxySettings::url("http://user:p@ss:word@proxy.test:3128");
        for shown in [format!("{route:?}"), format!("{settings:?}")] {
            assert!(!shown.contains("user") && !shown.contains("word") && !shown.contains("Basic"), "{shown}");
            assert!(shown.contains("proxy.test:3128"), "{shown}");
        }
        let mut out = String::new();
        base64_into(b"Aladdin:open sesame", &mut out);
        assert_eq!(out, "QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        for (input, encoded) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foob", "Zm9vYg==")] {
            let mut out = String::new();
            base64_into(input, &mut out);
            assert_eq!(out, encoded);
            assert_eq!(out.len(), base64_len(input.len()));
        }
    }

    #[test]
    #[cfg(feature = "http")]
    fn ureq_gets_http_and_https_proxies_only() {
        let target = |url: &str| parse_proxy(url).unwrap_or_else(|| panic!("{url}"));
        assert!(target("http://proxy.test:3128").ureq().is_some_and(|p| p.protocol() == ureq::ProxyProtocol::Http && p.port() == 3128));
        assert!(target("proxy.test").ureq().is_some_and(|p| p.protocol() == ureq::ProxyProtocol::Http && p.port() == 80));
        assert!(target("https://proxy.test").ureq().is_some_and(|p| p.protocol() == ureq::ProxyProtocol::Https));
        assert!(target("socks5://proxy.test").ureq().is_none());
    }
}
