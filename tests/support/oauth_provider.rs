//! A mock OpenID Connect provider on 127.0.0.1 for the `oauth` and `redaction` tests: an
//! authorization endpoint that sends the "browser" back to the loopback redirect (or declines),
//! and a token endpoint that checks the PKCE verifier, the redirect address, the client and that
//! each code is used once. Plus the test's "browser" (`browser_get`, `browse`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use bevy_net_backend::OAuthFlow;

pub const CLIENT_ID: &str = "game-client";
pub const CLIENT_SECRET: &str = "fake-client-secret";

pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn params(query: &str) -> HashMap<String, String> {
    query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode(k), decode(v))).collect()
}

pub fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        let take = c.len() + 1;
        for k in 0..take {
            out.push(char::from(A[((n >> (18 - 6 * k)) & 63) as usize]));
        }
    }
    out
}

/// One authorization the mock handed out.
#[derive(Clone)]
pub struct Grant {
    challenge: String,
    redirect_uri: String,
    nonce: String,
}

#[derive(Default)]
pub struct ProviderState {
    pub grants: HashMap<String, Grant>,
    pub used: Vec<String>,
    pub verifiers: Vec<String>,
    pub deny: bool,
    pub no_id_token: bool,
    pub next: usize,
}

/// The mock provider: `GET /authorize?…` (302 to the redirect with a code, or with
/// `error=access_denied` when `deny` is on) and `POST /token`.
pub struct Provider {
    pub port: u16,
    stop: Arc<AtomicBool>,
    pub state: Arc<Mutex<ProviderState>>,
    pub token_hits: Arc<AtomicUsize>,
}

impl Provider {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        listener.set_nonblocking(true).unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ProviderState::default()));
        let token_hits = Arc::new(AtomicUsize::new(0));
        let (stop2, state2, hits2) = (Arc::clone(&stop), Arc::clone(&state), Arc::clone(&token_hits));
        thread::Builder::new()
            .name("mock-oauth".into())
            .spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => provider_request(stream, &state2, &hits2),
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
            .unwrap_or_else(|e| panic!("{e}"));
        Self { port, stop, state, token_hits }
    }

    pub fn flow(&self) -> OAuthFlow {
        OAuthFlow::new(format!("http://127.0.0.1:{}/authorize", self.port), format!("http://127.0.0.1:{}/token", self.port), CLIENT_ID)
            .with_client_secret(CLIENT_SECRET)
            .with_scopes(["openid", "email"])
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut ProviderState) -> R) -> R {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
    while data.len() < end + length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
    let mut first = head.lines().next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("").to_string();
    Some((method, target, data[end..].to_vec()))
}

pub fn respond(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len());
    let _ = stream.flush();
}

pub fn provider_request(mut stream: TcpStream, state: &Mutex<ProviderState>, token_hits: &AtomicUsize) {
    let Some((method, target, body)) = read_request(&mut stream) else { return };
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    match (method.as_str(), path) {
        ("GET", "/authorize") => {
            let p = params(query);
            let ok = p.get("response_type").map(String::as_str) == Some("code")
                && p.get("client_id").map(String::as_str) == Some(CLIENT_ID)
                && p.get("code_challenge_method").map(String::as_str) == Some("S256")
                && p.get("scope").is_some_and(|s| s.split(' ').any(|x| x == "openid"))
                && p.get("redirect_uri").is_some_and(|r| r.starts_with("http://127.0.0.1:") && r.ends_with("/callback"));
            if !ok {
                return respond(&mut stream, "400 Bad Request", "", "bad authorization request");
            }
            let redirect = p.get("redirect_uri").cloned().unwrap_or_default();
            let st = p.get("state").cloned().unwrap_or_default();
            let location = if state.deny {
                format!("{redirect}?error=access_denied&state={st}")
            } else {
                state.next += 1;
                let code = format!("fake-code-{}", state.next);
                state.grants.insert(
                    code.clone(),
                    Grant {
                        challenge: p.get("code_challenge").cloned().unwrap_or_default(),
                        redirect_uri: redirect.clone(),
                        nonce: p.get("nonce").cloned().unwrap_or_default(),
                    },
                );
                format!("{redirect}?code={code}&state={st}")
            };
            respond(&mut stream, "302 Found", &format!("Location: {location}\r\n"), "");
        }
        ("POST", "/token") => {
            token_hits.fetch_add(1, Ordering::SeqCst);
            let p = params(&String::from_utf8_lossy(&body));
            let code = p.get("code").cloned().unwrap_or_default();
            let verifier = p.get("code_verifier").cloned().unwrap_or_default();
            state.verifiers.push(verifier.clone());
            let refuse = |stream: &mut TcpStream, error: &str| {
                respond(stream, "400 Bad Request", "Content-Type: application/json\r\n", &format!(r#"{{"error":"{error}"}}"#));
            };
            if state.used.contains(&code) {
                return refuse(&mut stream, "invalid_grant");
            }
            let Some(grant) = state.grants.get(&code).cloned() else { return refuse(&mut stream, "invalid_grant") };
            state.used.push(code.clone());
            let challenge = base64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
            if p.get("grant_type").map(String::as_str) != Some("authorization_code")
                || p.get("client_id").map(String::as_str) != Some(CLIENT_ID)
                || p.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
                || p.get("redirect_uri") != Some(&grant.redirect_uri)
                || challenge != grant.challenge
            {
                return refuse(&mut stream, "invalid_request");
            }
            let id_token = if state.no_id_token { String::new() } else { format!(r#""id_token":"fake-id-token.{}.sig","#, grant.nonce) };
            respond(
                &mut stream,
                "200 OK",
                "Content-Type: application/json\r\n",
                &format!(
                    r#"{{{id_token}"access_token":"fake-access-{code}","refresh_token":"fake-refresh-{code}","token_type":"Bearer","expires_in":3599,"scope":"openid email"}}"#
                ),
            );
        }
        _ => respond(&mut stream, "404 Not Found", "", ""),
    }
}

/// The browser: `GET url`, returns (status, Location header). `None` when nothing listens.
pub fn browser_get(url: &str) -> Option<(u16, Option<String>)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let mut stream = TcpStream::connect(authority).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n").ok()?;
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    let status = answer.split(' ').nth(1).and_then(|s| s.parse().ok())?;
    let location = answer.lines().find_map(|l| l.strip_prefix("Location: ").map(str::to_string));
    Some((status, location))
}

pub fn query_of(url: &str) -> HashMap<String, String> {
    params(url.split_once('?').map(|(_, q)| q).unwrap_or(""))
}

/// Run the browser part of a normal sign-in: open the URL, follow the provider's redirect.
pub fn browse(url: &str) -> String {
    let (status, location) = browser_get(url).unwrap_or_else(|| panic!("the provider is down"));
    assert_eq!(status, 302);
    let location = location.unwrap_or_else(|| panic!("no redirect"));
    let (status, _) = browser_get(&location).unwrap_or_else(|| panic!("the loopback listener is down"));
    assert_eq!(status, 200, "the listener's page for the right redirect");
    location
}
