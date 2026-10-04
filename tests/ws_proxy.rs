//! WebSocket connections through an HTTP `CONNECT` proxy set in the environment or in code (the same
//! variables as for HTTP), with the real `TungsteniteTransport`, the mock from
//! `examples/mock_ws_server.rs` and a small proxy, all on 127.0.0.1 in this process. The client asks
//! for `game.test`, a name only the proxy "resolves" (it sends every tunnel to the mock), so a
//! connection that skipped the proxy could not succeed. Own test binary with ONE test: it sets
//! process environment variables.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[path = "../examples/mock_ws_server.rs"]
mod mock_ws_server;

use mock_ws_server::MockWsServer;

#[derive(Serialize)]
struct Echo {
    text: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct EchoBack {
    text: String,
}

impl WsRequest for Echo {
    type Response = EchoBack;
    const KIND: &'static str = "echo";
}

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

fn ws(app: &TestApp) -> &WsClient {
    app.world().resource::<WsClient>()
}

fn wait_for(app: &mut TestApp, name: &str, wanted: impl Fn(Option<WsState>) -> bool) {
    for _ in 0..4000 {
        if wanted(app.world().resource::<WsConnections>().state(name)) {
            return;
        }
        app.step();
    }
    panic!("`{name}` stayed {:?}", app.world().resource::<WsConnections>().get(name));
}

fn set_proxy_env(https_proxy: &str) {
    for name in ["ALL_PROXY", "all_proxy", "https_proxy", "HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
    std::env::set_var("HTTPS_PROXY", https_proxy);
}

#[test]
fn websocket_connections_use_the_environment_proxy_except_for_loopback() {
    let server = MockWsServer::start(0).unwrap_or_else(|e| panic!("mock: {e}"));
    let mock_port = server.url().rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or_else(|| panic!("no port in {}", server.url()));
    let (proxy_port, seen) = proxy(mock_port);
    set_proxy_env(&format!("http://user:secret@127.0.0.1:{proxy_port}"));

    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default()).add_ws_request::<Echo>();
    app.watch::<WsResponse<EchoBack>>();
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(mock_ws_server::TOKEN));

    // Through the proxy: the handshake header (the token) arrives through the tunnel.
    let through = WsSettings::new(format!("ws://game.test:{mock_port}/secure")).allow_insecure_ws(true).with_reconnect(WsReconnect::never());
    ws(&app).connect("proxied", through);
    wait_for(&mut app, "proxied", |s| s == Some(WsState::Connected));
    let id = ws(&app).request("proxied", &Echo { text: "through the tunnel".into() });
    for _ in 0..4000 {
        if app.all_messages::<WsResponse<EchoBack>>().iter().any(|a| a.id == id) {
            break;
        }
        app.step();
    }
    let answer = app.all_messages::<WsResponse<EchoBack>>().into_iter().find(|a| a.id == id).map(|a| a.result);
    assert_eq!(answer, Some(Ok(EchoBack { text: "through the tunnel".into() })));
    {
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with(&format!("CONNECT game.test:{mock_port} HTTP/1.1\r\n")), "{}", seen[0]);
        assert!(!seen[0].contains(mock_ws_server::TOKEN), "the tunnel request carries no credentials of the connection");
    }

    // Loopback goes direct even with a proxy set.
    ws(&app).connect("direct", WsSettings::new(format!("{}/secure", server.url())).with_reconnect(WsReconnect::never()));
    wait_for(&mut app, "direct", |s| s == Some(WsState::Connected));
    assert_eq!(seen.lock().unwrap_or_else(PoisonError::into_inner).len(), 1, "loopback was proxied");

    // A proxy URL without the proxy's credentials: refused by the proxy, never connected.
    set_proxy_env(&format!("http://127.0.0.1:{proxy_port}"));
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::new()));
    let refused = WsSettings::new(format!("ws://game.test:{mock_port}/secure")).allow_insecure_ws(true).with_reconnect(WsReconnect::never());
    ws(&app).connect("refused", refused);
    wait_for(&mut app, "refused", |s| s == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("refused").and_then(|info| info.last_error.clone());
    assert!(matches!(&error, Some(BackendError::Network(why)) if why.contains("407")), "{error:?}");
    assert_eq!(seen.lock().unwrap_or_else(PoisonError::into_inner).len(), 2);

    // An https:// or SOCKS proxy cannot carry the connection: refused, never bypassed.
    set_proxy_env("socks5://127.0.0.1:1");
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::new()));
    ws(&app).connect("socks", WsSettings::new(format!("ws://game.test:{mock_port}/")).allow_insecure_ws(true).with_reconnect(WsReconnect::never()));
    wait_for(&mut app, "socks", |s| s == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("socks").and_then(|info| info.last_error.clone());
    assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("socks5://")), "{error:?}");

    // A proxy set in code (`ProxySettings::url`), with no proxy in the environment: through the
    // tunnel, with the proxy's credentials.
    std::env::remove_var("HTTPS_PROXY");
    let in_code = ProxySettings::url(format!("http://user:secret@127.0.0.1:{proxy_port}"));
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::with_settings(&TlsSettings::default(), &in_code)));
    ws(&app).connect("in-code", WsSettings::new(format!("ws://game.test:{mock_port}/secure")).allow_insecure_ws(true).with_reconnect(WsReconnect::never()));
    wait_for(&mut app, "in-code", |s| s == Some(WsState::Connected));
    assert_eq!(seen.lock().unwrap_or_else(PoisonError::into_inner).len(), 3);
    // `direct()` with a proxy in the environment: the proxy is never asked.
    set_proxy_env(&format!("http://user:secret@127.0.0.1:{proxy_port}"));
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::with_settings(&TlsSettings::default(), &ProxySettings::direct())));
    ws(&app).connect("no-proxy", WsSettings::new(format!("ws://game.test:{mock_port}/")).allow_insecure_ws(true).with_reconnect(WsReconnect::never()));
    wait_for(&mut app, "no-proxy", |s| s == Some(WsState::Disconnected));
    assert_eq!(seen.lock().unwrap_or_else(PoisonError::into_inner).len(), 3, "direct() used the environment proxy");
    // An https:// proxy set in code cannot carry a WebSocket connection: refused, never bypassed.
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::with_settings(&TlsSettings::default(), &ProxySettings::url("https://127.0.0.1:1"))));
    ws(&app).connect("https-proxy", WsSettings::new(format!("ws://game.test:{mock_port}/")).allow_insecure_ws(true).with_reconnect(WsReconnect::never()));
    wait_for(&mut app, "https-proxy", |s| s == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("https-proxy").and_then(|info| info.last_error.clone());
    assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("https://")), "{error:?}");
    std::env::remove_var("HTTPS_PROXY");
}
