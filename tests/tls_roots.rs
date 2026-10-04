//! `TlsSettings` against loopback TLS servers started in this process (127.0.0.1 only, never another
//! host): an HTTPS server and a WSS echo server whose certificate for `localhost` / `127.0.0.1` is signed by a
//! throwaway CA generated in memory (rcgen, ring; no key material in the repository).
//!
//! - default trust (Mozilla's roots): both refuse the server (`Tls`), nothing reaches the app;
//! - the CA as extra root (PEM text, PEM file): HTTPS answers and the WebSocket echoes;
//! - another CA as extra root: still refused;
//! - settings that cannot be used: `InvalidRequest`, never sent, `http://` unaffected;
//! - feature `os-certificates`: the operating system's verifier with the CA on top works, without
//!   it the server is refused.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

#[allow(dead_code)]
#[path = "../examples/mock_server.rs"]
mod mock_server;

/// A throwaway CA (its PEM) and a `localhost` / `127.0.0.1` certificate it signed.
struct Pki {
    ca_pem: String,
    leaf: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

fn ca() -> (rcgen::CertifiedIssuer<'static, rcgen::KeyPair>, String) {
    let key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|e| panic!("{e}"));
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name.push(rcgen::DnType::CommonName, "bevy_net_backend test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(params, key).unwrap_or_else(|e| panic!("{e}"));
    let pem = ca.pem();
    (ca, pem)
}

fn pki() -> Pki {
    let (ca, ca_pem) = ca();
    let leaf_key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .unwrap_or_else(|e| panic!("{e}"))
        .signed_by(&leaf_key, &ca)
        .unwrap_or_else(|e| panic!("{e}"));
    Pki { ca_pem, leaf: leaf.der().clone(), key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())) }
}

#[derive(Clone, Copy)]
enum Kind {
    /// Answers every request with `200 {"hello":"tls"}`.
    Https,
    /// A WebSocket echo server.
    Wss,
}

/// A TLS server on 127.0.0.1, one thread per connection, stopped on drop. Counts the connections
/// whose TLS handshake succeeded.
struct TlsServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handshakes: Arc<AtomicUsize>,
}

impl TlsServer {
    fn start(pki: &Pki, kind: Kind) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap_or_else(|e| panic!("{e}"))
                .with_no_client_auth()
                .with_single_cert(vec![pki.leaf.clone()], pki.key.clone_key())
                .unwrap_or_else(|e| panic!("{e}")),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let handshakes = Arc::new(AtomicUsize::new(0));
        let (stop2, handshakes2) = (Arc::clone(&stop), Arc::clone(&handshakes));
        thread::spawn(move || {
            for tcp in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(tcp) = tcp else { continue };
                let (config, handshakes) = (Arc::clone(&config), Arc::clone(&handshakes2));
                thread::spawn(move || serve(tcp, config, kind, &handshakes));
            }
        });
        Self { port, stop, handshakes }
    }

    fn handshakes(&self) -> usize {
        self.handshakes.load(Ordering::SeqCst)
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(tcp: TcpStream, config: Arc<rustls::ServerConfig>, kind: Kind, handshakes: &AtomicUsize) {
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(connection) = rustls::ServerConnection::new(config) else { return };
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    // A client that does not trust the certificate ends the handshake here.
    while tls.conn.is_handshaking() {
        if tls.conn.complete_io(&mut tls.sock).is_err() {
            return;
        }
    }
    handshakes.fetch_add(1, Ordering::SeqCst);
    match kind {
        Kind::Https => {
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let body = r#"{"hello":"tls"}"#;
            let answer = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = tls.write_all(answer.as_bytes());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
        Kind::Wss => {
            let Ok(mut ws) = tungstenite::accept(tls) else { return };
            loop {
                match ws.read() {
                    Ok(tungstenite::Message::Text(text)) => {
                        if ws.send(tungstenite::Message::Text(text)).is_err() {
                            return;
                        }
                    }
                    Ok(tungstenite::Message::Close(_)) | Err(_) => {
                        let _ = ws.flush();
                        return;
                    }
                    Ok(_) => {}
                }
            }
        }
    }
}

/// A strict app on the real transports, with these TLS settings.
fn app(base_url: String, tls: TlsSettings) -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::new(HttpConfig::new(base_url).with_timeout(Duration::from_secs(5))).with_tls(tls));
    app.watch::<HttpResponse>().watch::<WsStateChanged>().watch::<WsMessage>();
    app
}

/// GET `/hello` and wait for the answer (at most ~3000 frames).
fn get(app: &mut TestApp) -> Result<RawResponse, BackendError> {
    let id = app.world().resource::<HttpClient>().get("/hello");
    for _ in 0..3000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered");
}

fn ws_state(app: &TestApp) -> Option<WsState> {
    app.world().resource::<WsConnections>().state("tls")
}

fn ws_error(app: &TestApp) -> Option<BackendError> {
    app.world().resource::<WsConnections>().get("tls").and_then(|c| c.last_error.clone())
}

fn run_until(app: &mut TestApp, mut done: impl FnMut(&TestApp) -> bool) {
    for _ in 0..4000 {
        if done(app) {
            return;
        }
        app.step();
    }
    assert!(done(app), "condition not reached");
}

/// Connect `wss://127.0.0.1:<port>/` and, when it opens, check one echo. Returns the final error
/// when it does not open.
fn ws_round_trip(app: &mut TestApp, port: u16) -> Result<(), Option<BackendError>> {
    app.world().resource::<WsClient>().connect("tls", WsSettings::new(format!("wss://127.0.0.1:{port}/")));
    run_until(app, |app| matches!(ws_state(app), Some(WsState::Connected | WsState::Disconnected)));
    if ws_state(app) != Some(WsState::Connected) {
        return Err(ws_error(app));
    }
    app.world().resource::<WsClient>().send_text("tls", "over tls");
    run_until(app, |app| app.all_messages::<WsMessage>().iter().any(|m| m.frame == WsFrame::Text("over tls".into())));
    app.world().resource::<WsClient>().disconnect("tls");
    Ok(())
}

#[test]
fn the_default_trust_refuses_a_server_of_an_unknown_ca() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let mut app = app(format!("https://127.0.0.1:{}", https.port), TlsSettings::new());
    let result = get(&mut app);
    assert!(matches!(&result, Err(BackendError::Tls(why)) if why.to_lowercase().contains("issuer")), "{result:?}");
    let refused = ws_round_trip(&mut app, wss.port);
    assert!(matches!(&refused, Err(Some(BackendError::Tls(_)))), "{refused:?}");
    assert_eq!((https.handshakes(), wss.handshakes()), (0, 0));
}

#[test]
fn an_extra_root_as_pem_text_is_trusted_by_https_and_wss() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let mut app = app(format!("https://127.0.0.1:{}", https.port), TlsSettings::new().with_root_certificates_pem(&pki.ca_pem));
    let answer = get(&mut app).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((answer.status, answer.text().as_str()), (StatusCode::OK, r#"{"hello":"tls"}"#));
    ws_round_trip(&mut app, wss.port).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(https.handshakes() >= 1 && wss.handshakes() == 1);
}

#[test]
fn an_extra_root_from_a_pem_file_next_to_other_blocks_is_trusted() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tls-roots-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    let file = dir.join("dev-ca.pem");
    // Another CA first, then ours: every CERTIFICATE block counts.
    let (_, other_pem) = ca();
    std::fs::write(&file, format!("{other_pem}\n{}", pki.ca_pem)).unwrap_or_else(|e| panic!("{e}"));
    let tls = TlsSettings::new().with_root_certificates_file(&file);
    assert!(tls.validate().is_ok());
    let mut app = app(format!("https://127.0.0.1:{}", https.port), tls);
    assert_eq!(get(&mut app).map(|a| a.status), Ok(StatusCode::OK));
    ws_round_trip(&mut app, wss.port).unwrap_or_else(|e| panic!("{e:?}"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_root_of_another_ca_does_not_help() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let (_, other_pem) = ca();
    let mut app = app(format!("https://127.0.0.1:{}", https.port), TlsSettings::new().with_root_certificates_pem(other_pem));
    assert!(matches!(get(&mut app), Err(BackendError::Tls(_))));
    assert!(matches!(ws_round_trip(&mut app, wss.port), Err(Some(BackendError::Tls(_)))));
}

#[test]
fn unusable_settings_refuse_https_and_wss_but_not_plain_loopback_http() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let tls = TlsSettings::new().with_root_certificates_file("no-such-dev-ca.pem");
    assert!(matches!(tls.validate(), Err(ConfigError::Tls(ref why)) if why.contains("no-such-dev-ca.pem")));
    let mut app = app(format!("https://127.0.0.1:{}", https.port), tls.clone());
    let result = get(&mut app);
    assert!(matches!(&result, Err(BackendError::InvalidRequest(why)) if why.contains("no-such-dev-ca.pem")), "{result:?}");
    assert_eq!(result.err().and_then(|e| e.was_sent()), Some(false));
    let refused = ws_round_trip(&mut app, wss.port);
    assert!(matches!(&refused, Err(Some(BackendError::InvalidRequest(_)))), "{refused:?}");
    assert_eq!((https.handshakes(), wss.handshakes()), (0, 0), "nothing reached the servers");
    // Plain http:// to loopback does not use TLS: unaffected.
    let plain = mock_server::MockServer::start().unwrap_or_else(|e| panic!("{e}"));
    let mut app = self::app(plain.url(), tls);
    let id = app.world().resource::<HttpClient>().get("/empty");
    let mut status = None;
    for _ in 0..3000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            status = Some(answer.result.map(|r| r.status));
            break;
        }
    }
    assert_eq!(status, Some(Ok(StatusCode::NO_CONTENT)));
}

#[test]
fn custom_transports_take_the_settings_too() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let config = HttpConfig::new(format!("https://127.0.0.1:{}", https.port));
    let tls = TlsSettings::new().with_root_certificates_pem(&pki.ca_pem);
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    // Inserted before the plugin: the plugin keeps it (its own default trust is not used).
    app.insert_resource(HttpTransportRes::new(UreqTransport::with_tls(&config, &tls)));
    app.insert_resource(WsTransportRes::new(TungsteniteTransport::with_tls(&tls)));
    app.add_plugins(BackendPlugin::new(config));
    app.watch::<HttpResponse>().watch::<WsStateChanged>().watch::<WsMessage>();
    assert_eq!(get(&mut app).map(|a| a.status), Ok(StatusCode::OK));
    let wss = TlsServer::start(&pki, Kind::Wss);
    ws_round_trip(&mut app, wss.port).unwrap_or_else(|e| panic!("{e:?}"));
}

#[cfg(feature = "os-certificates")]
#[test]
fn the_os_store_with_the_ca_on_top_is_trusted_and_without_it_refused() {
    let pki = pki();
    let https = TlsServer::start(&pki, Kind::Https);
    let wss = TlsServer::start(&pki, Kind::Wss);
    let os_only = TlsSettings::new().with_os_certificates(true);
    let mut app = app(format!("https://127.0.0.1:{}", https.port), os_only);
    assert!(matches!(get(&mut app), Err(BackendError::Tls(_))));
    assert!(matches!(ws_round_trip(&mut app, wss.port), Err(Some(BackendError::Tls(_)))));

    let with_ca = TlsSettings::new().with_os_certificates(true).with_root_certificates_pem(&pki.ca_pem);
    let mut app = self::app(format!("https://127.0.0.1:{}", https.port), with_ca);
    let answer = get(&mut app);
    assert_eq!(answer.as_ref().map(|a| a.status), Ok(StatusCode::OK), "{answer:?}");
    ws_round_trip(&mut app, wss.port).unwrap_or_else(|e| panic!("{e:?}"));
}
