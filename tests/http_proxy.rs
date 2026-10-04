//! HTTP requests and proxies, with the real `UreqTransport`: the proxy environment variables, a
//! proxy set in code (`ProxySettings`), and a SOCKS proxy (which the HTTP client cannot use: such a
//! request is answered `InvalidRequest` and never sent around the proxy). The mock from
//! `examples/mock_server.rs` and a small `CONNECT` proxy run on 127.0.0.1 in this process; the
//! client asks for `game.test`, a name only the proxy "resolves" (it sends every tunnel to the
//! mock). A listener on a loopback address, asked for by a name that is not a loopback host by
//! the crate's rule (`localhost.` or `127.1`, so the proxy applies), counts every direct connection, so "never sent around the proxy" is
//! checked on the wire. Every listener binds a loopback address only. Own test binary with ONE
//! test: it sets process environment variables.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

#[allow(dead_code)]
#[path = "../examples/mock_server.rs"]
mod mock_server;

use mock_server::MockServer;

/// The proxy's credentials (obviously fake): `Basic` + base64 of `user:secret`.
const PROXY_AUTHORIZATION: &str = "Proxy-Authorization: Basic dXNlcjpzZWNyZXQ=";

/// A `CONNECT` proxy on 127.0.0.1 that sends every tunnel to `target` (whatever host was asked
/// for) and refuses tunnels without [`PROXY_AUTHORIZATION`] with `407`. Records each request
/// header it received.
fn proxy(target: u16) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { continue };
            let record = Arc::clone(&record);
            thread::spawn(move || {
                let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    if client.read(&mut byte).map_or(true, |n| n == 0) {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let authorized = head.lines().any(|line| line == PROXY_AUTHORIZATION);
                record.lock().unwrap_or_else(PoisonError::into_inner).push(head);
                if !authorized {
                    let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let Ok(upstream) = TcpStream::connect(("127.0.0.1", target)) else { return };
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                let _ = client.set_read_timeout(None);
                let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone()) else { return };
                let (mut upstream_read, mut client_write) = (upstream, client);
                thread::spawn(move || {
                    let _ = io::copy(&mut client_read, &mut upstream_write);
                    let _ = upstream_write.shutdown(std::net::Shutdown::Write);
                });
                let _ = io::copy(&mut upstream_read, &mut client_write);
                let _ = client_write.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    (port, seen)
}

/// Host names that are NOT loopback hosts by the crate's rule (not `localhost`, not an IP address)
/// but that the system resolver turns into a loopback address, with the loopback listener address
/// for each: `localhost.` (Windows: `::1`) and the short IPv4 form `127.1` (Linux, macOS).
const NEAR_LOOPBACK: [(&str, &str); 2] = [("localhost.", "[::1]:0"), ("127.1", "127.0.0.1:0")];

/// A listener on a loopback address that counts every connection and answers each with an empty
/// `200`.
fn counting_listener(addr: &str) -> Option<(u16, Arc<AtomicUsize>)> {
    let listener = TcpListener::bind(addr).ok()?;
    let port = listener.local_addr().ok()?.port();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { continue };
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let _ = client.read(&mut buf);
            let _ = client.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    Some((port, count))
}

fn clear_proxy_env() {
    for name in ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
}

/// A strict app on the real transport (created now, so it reads the environment as it is now).
fn app(base: &str, proxy: Option<ProxySettings>) -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    let config = HttpConfig::new(base).allow_insecure_http(true).with_timeout(Duration::from_secs(5));
    let plugin = BackendPlugin::new(config);
    app.add_plugins(match proxy {
        Some(proxy) => plugin.with_proxy(proxy),
        None => plugin,
    });
    app.watch::<HttpResponse>();
    app
}

fn get(app: &mut TestApp, path: &str) -> Result<RawResponse, BackendError> {
    let id = app.world().resource::<HttpClient>().send(OutgoingRequest::get(path));
    for _ in 0..4000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

fn seen_count(seen: &Mutex<Vec<String>>) -> usize {
    seen.lock().unwrap_or_else(PoisonError::into_inner).len()
}

fn refused_around_the_proxy(result: &Result<RawResponse, BackendError>) -> bool {
    matches!(result, Err(BackendError::InvalidRequest(why)) if why.contains("never around the proxy"))
        && result.as_ref().err().and_then(BackendError::was_sent) == Some(false)
}

#[test]
fn http_requests_follow_the_proxy_settings_and_never_go_around_a_socks_proxy() {
    let server = MockServer::start().unwrap_or_else(|e| panic!("mock server: {e}"));
    let mock_port = server.url().rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or_else(|| panic!("no port in {}", server.url()));
    let (proxy_port, seen) = proxy(mock_port);
    let game = format!("http://game.test:{mock_port}");
    // The checks on the wire run where the resolver turns one of the names into the listener's
    // loopback address; the probe connection counts as one.
    let lan = NEAR_LOOPBACK.iter().find_map(|(host, bind)| {
        let (port, count) = counting_listener(bind)?;
        TcpStream::connect((*host, port)).ok()?;
        for _ in 0..500 {
            if count.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        Some((*host, port, count))
    });
    let lan_base = lan.as_ref().map(|(host, port, _)| format!("http://{host}:{port}"));
    let direct_count = || lan.as_ref().map_or(0, |(_, _, count)| count.load(Ordering::SeqCst));
    if lan.is_none() {
        eprintln!("no near-loopback name resolves here: the checks on the wire are skipped");
    }

    // 1. The environment proxy: through the tunnel, with the proxy's credentials.
    clear_proxy_env();
    std::env::set_var("HTTPS_PROXY", format!("http://user:secret@127.0.0.1:{proxy_port}"));
    let mut through = app(&game, None);
    let echo = get(&mut through, "/echo").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(echo.status, http::StatusCode::OK);
    {
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with(&format!("CONNECT game.test:{mock_port} HTTP/1.1\r\n")), "{}", seen[0]);
    }
    // Loopback goes direct even with a proxy set.
    let mut local = app(&server.url(), None);
    assert!(get(&mut local, "/echo").is_ok());
    assert_eq!(seen_count(&seen), 1, "loopback was proxied");
    // A NO_PROXY host goes direct.
    if let Some(base) = &lan_base {
        let host = lan.as_ref().map(|(host, _, _)| (*host).to_string()).unwrap_or_default();
        std::env::set_var("NO_PROXY", host);
        let before = direct_count();
        let mut bypassed = app(base, None);
        assert!(get(&mut bypassed, "/").is_ok());
        assert_eq!((direct_count(), seen_count(&seen)), (before + 1, 1), "NO_PROXY host not direct");
        std::env::remove_var("NO_PROXY");
    }

    // 2. A SOCKS proxy in the environment (which the HTTP client cannot use): refused, never sent,
    //    never sent around it; loopback still direct.
    for (name, socks) in [("ALL_PROXY", "socks5://127.0.0.1:1"), ("HTTPS_PROXY", "socks5h://127.0.0.1:1"), ("http_proxy", "socks4://127.0.0.1:1")] {
        clear_proxy_env();
        std::env::set_var(name, socks);
        let mut refused = app(&game, None);
        let result = get(&mut refused, "/echo");
        assert!(refused_around_the_proxy(&result), "{name}={socks}: {result:?}");
        if let Some(base) = &lan_base {
            let before = direct_count();
            let mut refused = app(base, None);
            let result = get(&mut refused, "/");
            assert!(refused_around_the_proxy(&result), "{name}={socks}: {result:?}");
            assert_eq!(direct_count(), before, "{name}={socks}: a request went around the SOCKS proxy");
        }
        let mut local = app(&server.url(), None);
        assert!(get(&mut local, "/echo").is_ok(), "{name}={socks}: loopback");
    }
    assert_eq!(seen_count(&seen), 1);

    // 3. A proxy set in code, with an empty environment: through the tunnel, credentials sent.
    clear_proxy_env();
    let mut in_code = app(&game, Some(ProxySettings::url(format!("http://user:secret@127.0.0.1:{proxy_port}"))));
    assert!(get(&mut in_code, "/echo").is_ok());
    assert_eq!(seen_count(&seen), 2);
    // `direct()` with the environment proxy set: nothing reaches the proxy.
    std::env::set_var("HTTPS_PROXY", format!("http://user:secret@127.0.0.1:{proxy_port}"));
    if let Some(base) = &lan_base {
        let before = direct_count();
        let mut direct = app(base, Some(ProxySettings::direct()));
        assert!(get(&mut direct, "/").is_ok());
        assert_eq!(direct_count(), before + 1);
    }
    let mut direct = app(&game, Some(ProxySettings::direct()));
    let _ = get(&mut direct, "/echo");
    assert_eq!(seen_count(&seen), 2, "direct() used the environment proxy");
    // A SOCKS proxy or a URL that is not a proxy URL, set in code: refused, never sent.
    for bad in ["socks5://127.0.0.1:1", "::not a proxy::"] {
        assert!(matches!(ProxySettings::url(bad).validate(), Err(ConfigError::Proxy(_))), "{bad}");
        let mut refused = app(&game, Some(ProxySettings::url(bad)));
        let result = get(&mut refused, "/echo");
        assert!(
            matches!(&result, Err(BackendError::InvalidRequest(_))) && result.as_ref().err().and_then(BackendError::was_sent) == Some(false),
            "{bad}: {result:?}"
        );
        let mut local = app(&server.url(), Some(ProxySettings::url(bad)));
        assert!(get(&mut local, "/echo").is_ok(), "{bad}: loopback");
    }
    assert_eq!(seen_count(&seen), 2);
    clear_proxy_env();
}
