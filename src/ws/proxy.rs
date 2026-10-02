//! The proxy for WebSocket connections: the same environment variables and bypass rules the HTTP
//! transport uses (ureq 3.4.2's `Proxy::try_from_env`, `src/proxy.rs`), applied to `ws://` and
//! `wss://` URLs, and the HTTP `CONNECT` tunnel through an `http://` proxy.
//!
//! - The proxy: the first of `ALL_PROXY`, `all_proxy`, `HTTPS_PROXY`, `https_proxy`, `HTTP_PROXY`,
//!   `http_proxy` that holds a valid proxy URL (`[scheme://][user[:password]@]host[:port]`, scheme
//!   `http` when missing, port 80 for `http`). Read once, when the transport is created.
//! - Bypass: `NO_PROXY` (or `no_proxy`), comma separated: `*` = every host, `.example.com` /
//!   `*.example.com` = the subdomains, `example.` / `example*` = a prefix, anything else = exactly
//!   that host (ASCII case ignored). Loopback hosts never use a proxy (as for HTTP).
//! - Only `http://` proxies carry WebSocket connections (`CONNECT host:port`, `Proxy-Authorization:
//!   Basic` from the URL's user and password). A connection that should go through an `https://`
//!   or SOCKS proxy fails with `InvalidRequest` instead of going around the proxy.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::Instant;

use http::Uri;
use tungstenite::handshake::machine::TryParse;
use zeroize::Zeroizing;

use crate::response::BackendError;

/// The variables tried for the proxy, in this order (ureq's order).
const PROXY_VARS: [&str; 6] = ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
/// The variables tried for the bypass list, in this order.
const NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];
/// The longest `CONNECT` answer header accepted.
const MAX_CONNECT_ANSWER: usize = 16 * 1024;

/// An `http://` proxy: where it is and, when its URL had a user, the `Proxy-Authorization` value.
pub(crate) struct HttpProxy {
    host: String,
    port: u16,
    /// `Basic <base64(user:password)>`, wiped on drop.
    authorization: Option<Zeroizing<String>>,
}

impl HttpProxy {
    /// The host to connect to (IPv6 without brackets) and the port.
    pub(crate) fn address(&self) -> (&str, u16) {
        let host = self.host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&self.host);
        (host, self.port)
    }
}

enum Setting {
    Http(HttpProxy),
    /// A proxy that cannot carry WebSocket connections (its scheme).
    Unsupported(String),
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

/// The proxy settings WebSocket connections use (read from the environment by the transport).
#[derive(Default)]
pub(crate) struct ProxyEnv {
    setting: Option<Setting>,
    no_proxy: Vec<NoProxyEntry>,
}

impl fmt::Debug for ProxyEnv {
    /// Scheme, host and port only: never the user or password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proxy = match &self.setting {
            None => "none".to_string(),
            Some(Setting::Http(proxy)) => format!("http://{}:{}", proxy.host, proxy.port),
            Some(Setting::Unsupported(scheme)) => format!("{scheme}://… (not used for WebSocket)"),
        };
        f.debug_struct("ProxyEnv").field("proxy", &proxy).field("no_proxy_entries", &self.no_proxy.len()).finish()
    }
}

impl ProxyEnv {
    /// From the process environment.
    pub(crate) fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// From any source of variables (tests).
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let setting = PROXY_VARS.iter().find_map(|name| var(name).and_then(|value| parse_proxy(&value)));
        let no_proxy = NO_PROXY_VARS.iter().find_map(|name| var(name)).map(|list| list.split(',').map(NoProxyEntry::parse).collect()).unwrap_or_default();
        Self { setting, no_proxy }
    }

    /// The proxy a connection to `uri` goes through: `None` = direct (no proxy, a loopback host,
    /// or a host on the bypass list).
    pub(crate) fn route(&self, uri: &Uri) -> Result<Option<&HttpProxy>, BackendError> {
        let Some(setting) = &self.setting else { return Ok(None) };
        let host = uri.host().unwrap_or_default();
        if crate::request::is_loopback_host(host) || self.no_proxy.iter().any(|entry| entry.matches(host)) {
            return Ok(None);
        }
        match setting {
            Setting::Http(proxy) => Ok(Some(proxy)),
            Setting::Unsupported(scheme) => Err(BackendError::InvalidRequest(format!(
                "the proxy set in the environment is a {scheme}:// proxy; WebSocket connections go through http:// proxies (CONNECT) only"
            ))),
        }
    }
}

/// A proxy URL as ureq 3.4.2 reads it; `None` when it is not a valid proxy URL.
fn parse_proxy(value: &str) -> Option<Setting> {
    let uri = value.parse::<Uri>().ok()?;
    let authority = uri.authority()?;
    let scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
    match scheme.as_str() {
        "http" => {}
        "https" | "socks4" | "socks4a" | "socks" | "socks5" | "socks5h" => return Some(Setting::Unsupported(scheme)),
        _ => return None,
    }
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
        let mut value = Zeroizing::new(String::with_capacity(6 + base64_len(plain.len())));
        value.push_str("Basic ");
        base64_into(plain.as_bytes(), &mut value);
        value
    });
    Some(Setting::Http(HttpProxy { host: authority.host().to_string(), port: authority.port_u16().unwrap_or(80), authorization }))
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

/// Ask the proxy on `stream` for a tunnel to `target` (`host:port`, IPv6 in brackets), within
/// `deadline`. Reads exactly the proxy's answer header, nothing of the tunnel.
pub(crate) fn connect_tunnel<S: Read + Write>(stream: &mut S, proxy: &HttpProxy, target: &str, deadline: Instant) -> Result<(), BackendError> {
    let mut request = Zeroizing::new(Vec::with_capacity(64 + 2 * target.len() + proxy.authorization.as_ref().map_or(0, |a| a.len() + 24)));
    request.extend_from_slice(b"CONNECT ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b"\r\n");
    if let Some(authorization) = &proxy.authorization {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(authorization.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    let failed = |e: io::Error| {
        if Instant::now() >= deadline {
            BackendError::Timeout("connect limit".into())
        } else {
            BackendError::Network(format!("could not connect through the proxy: {e}"))
        }
    };
    stream.write_all(&request).map_err(failed)?;
    stream.flush().map_err(failed)?;
    // One byte at a time up to the blank line: the bytes after it belong to the tunnel.
    let mut answer = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\r\n\r\n") {
        if answer.len() >= MAX_CONNECT_ANSWER {
            return Err(BackendError::Network(format!("could not connect through the proxy: its answer is longer than {MAX_CONNECT_ANSWER} bytes")));
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err(BackendError::Network("could not connect through the proxy: it closed the connection".into())),
            Ok(_) => answer.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(BackendError::Timeout("connect limit".into()));
                }
            }
            Err(e) => return Err(failed(e)),
        }
    }
    let parsed = tungstenite::handshake::client::Response::try_parse(&answer)
        .map_err(|e| BackendError::Network(format!("could not connect through the proxy: its answer is not HTTP ({e})")))?;
    match parsed {
        Some((_, response)) if response.status().is_success() => Ok(()),
        Some((_, response)) => Err(BackendError::Network(format!("could not connect through the proxy: it answered {}", response.status()))),
        None => Err(BackendError::Network("could not connect through the proxy: its answer is incomplete".into())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> ProxyEnv {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        ProxyEnv::from_vars(|name| vars.get(name).cloned())
    }

    fn routed(env: &ProxyEnv, url: &str) -> Result<Option<(String, u16)>, BackendError> {
        let uri: Uri = url.parse().unwrap_or_else(|e| panic!("{e}"));
        env.route(&uri).map(|p| p.map(|p| (p.address().0.to_string(), p.address().1)))
    }

    #[test]
    fn the_first_valid_variable_wins_in_ureqs_order() {
        let proxy = Some(("proxy.test".to_string(), 3128));
        assert_eq!(routed(&env(&[("HTTPS_PROXY", "http://proxy.test:3128")]), "wss://game.test/ws").ok(), Some(proxy.clone()));
        assert_eq!(routed(&env(&[("http_proxy", "proxy.test:3128")]), "ws://game.test/ws").ok(), Some(proxy.clone()));
        assert_eq!(
            routed(&env(&[("ALL_PROXY", "http://proxy.test:3128"), ("HTTPS_PROXY", "http://other.test:1")]), "wss://game.test/").ok(),
            Some(proxy.clone())
        );
        // An invalid value is skipped, the next one is used.
        assert_eq!(routed(&env(&[("ALL_PROXY", "fakeproto://x.test"), ("HTTP_PROXY", "http://proxy.test:3128")]), "wss://game.test/").ok(), Some(proxy));
        assert_eq!(routed(&env(&[("HTTP_PROXY", "http://proxy.test")]), "wss://game.test/").ok(), Some(Some(("proxy.test".to_string(), 80))));
        assert_eq!(routed(&env(&[("HTTP_PROXY", "http://[::1]:8080")]), "wss://game.test/").ok(), Some(Some(("::1".to_string(), 8080))));
        assert_eq!(routed(&env(&[]), "wss://game.test/").ok(), Some(None));
    }

    #[test]
    fn loopback_and_no_proxy_hosts_go_direct() {
        let base = [("HTTPS_PROXY", "http://proxy.test:3128")];
        for url in ["ws://127.0.0.1:9000/", "ws://localhost/", "ws://[::1]:9000/"] {
            assert_eq!(routed(&env(&base), url).ok(), Some(None), "{url}");
        }
        let with = |list: &str| env(&[base[0], ("NO_PROXY", list)]);
        assert_eq!(routed(&with("game.test"), "wss://game.test/").ok(), Some(None));
        assert_eq!(routed(&with("GAME.test"), "wss://Game.Test/").ok(), Some(None));
        assert!(routed(&with("game.test"), "wss://eu.game.test/").is_ok_and(|p| p.is_some()));
        assert_eq!(routed(&with(".game.test"), "wss://eu.game.test/").ok(), Some(None));
        assert_eq!(routed(&with("*.game.test"), "wss://eu.game.test/").ok(), Some(None));
        assert!(routed(&with(".game.test"), "wss://game.test/").is_ok_and(|p| p.is_some()));
        assert_eq!(routed(&with("10.0.*"), "ws://10.0.0.7/").ok(), Some(None));
        assert_eq!(routed(&with("*"), "wss://anything.test/").ok(), Some(None));
        assert_eq!(routed(&env(&[base[0], ("no_proxy", "game.test")]), "wss://game.test/").ok(), Some(None));
        assert!(routed(&with("other.test,game.test"), "wss://game.test/").is_ok_and(|p| p.is_none()));
    }

    #[test]
    fn https_and_socks_proxies_are_refused_never_bypassed() {
        for proxy in ["https://proxy.test:443", "socks5://proxy.test:1080", "socks5h://proxy.test", "socks4://proxy.test"] {
            let error = routed(&env(&[("ALL_PROXY", proxy)]), "wss://game.test/").err();
            assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("http:// proxies")), "{proxy}: {error:?}");
            // Loopback and bypassed hosts still go direct.
            assert_eq!(routed(&env(&[("ALL_PROXY", proxy)]), "ws://127.0.0.1:1/").ok(), Some(None));
            assert_eq!(routed(&env(&[("ALL_PROXY", proxy), ("NO_PROXY", "game.test")]), "wss://game.test/").ok(), Some(None));
        }
    }

    #[test]
    fn credentials_become_basic_authorization_and_never_show_in_debug() {
        let env = env(&[("HTTPS_PROXY", "http://user:p@ss:word@proxy.test:3128")]);
        let Some(Setting::Http(proxy)) = &env.setting else { panic!("no http proxy") };
        // ureq's split: the last `@` ends the user info, the last `:` in it starts the password.
        let mut expected = String::from("Basic ");
        base64_into(b"user:p@ss:word", &mut expected);
        assert_eq!(proxy.authorization.as_deref().map(String::as_str), Some(expected.as_str()));
        assert_eq!(proxy.address(), ("proxy.test", 3128));
        let shown = format!("{env:?}");
        assert!(!shown.contains("user") && !shown.contains("word") && !shown.contains("Basic"), "{shown}");
        assert!(shown.contains("proxy.test:3128"), "{shown}");
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
}
