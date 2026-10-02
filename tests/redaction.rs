//! Secrets never reach the logs: every `tracing` event (at every level, TRACE included) and every
//! `log` record at TRACE (the crate the WebSocket and SSH libraries log through) is captured while
//! requests with every kind of credential run, succeed, fail and are cancelled; none contains a
//! secret. With feature `ssh` also a real SSH connection to the mock (encrypted key + passphrase, a
//! wrong passphrase, a command line, stdin and output holding secrets); with `ws` + `json` also
//! real WebSocket connections to the mock with a bearer token, an API key header, an API key in
//! the URL and a first-message authentication. Records written on the in-process mock servers'
//! threads (`mock-*`) are left out: they are the server side. (Debug / Display redaction is
//! unit-tested in the crate.) Own test binary: it installs a process-wide subscriber and logger.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::*;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

const SECRETS: [&str; 4] = ["fake-bearer-7f3a", "fake-header-9c2e", "fake-query-41bd", "fake-field-d00d"];
#[cfg(feature = "ssh")]
const SSH_SECRETS: [&str; 4] = ["fake-ssh-pass-5e1f", "fake-ssh-cmd-77aa", "fake-ssh-stdin-9b9b", "fake-ssh-wrong-0a0a"];
#[cfg(feature = "ws")]
const WS_SECRETS: [&str; 2] = ["fake-ws-old-3c3c", "fake-ws-new-4d4d"];
#[cfg(all(feature = "ws", feature = "json"))]
const WS_LINK_SECRETS: [&str; 4] = ["fake-ws-bearer-6e6e", "fake-ws-header-7f7f", "fake-ws-query-8a8a", "fake-ws-first-9b9b"];

#[cfg(feature = "ssh")]
#[allow(dead_code)]
#[path = "../examples/mock_ssh_server.rs"]
mod mock_ssh_server;

#[cfg(all(feature = "ws", feature = "json"))]
#[allow(dead_code)]
#[path = "../examples/mock_ws_server.rs"]
mod mock_ws_server;

/// Puts one kind of credentials into the resource.
type SetCredentials = dyn Fn(&mut BackendCredentials);

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        all.push_str(event.metadata().target());
        all.push_str(": ");
        all.push_str(&line);
        all.push('\n');
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Every `log` record at every level, with its thread (records of the mock servers' threads are
/// kept apart: they are the server side).
#[derive(Clone, Default)]
struct LogCapture {
    client: Arc<Mutex<String>>,
    mock: Arc<Mutex<String>>,
}

impl log::Log for LogCapture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let thread = std::thread::current();
        let line = format!("{}: {}\n", record.target(), record.args());
        let into = if thread.name().is_some_and(|n| n.starts_with("mock-")) { &self.mock } else { &self.client };
        into.lock().unwrap_or_else(PoisonError::into_inner).push_str(&line);
    }
    fn flush(&self) {}
}

/// `secret` in `logs`: as text, or as the hex dump tungstenite prints frame payloads with.
fn leaked(logs: &str, secret: &str) -> bool {
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    logs.contains(secret) || logs.to_ascii_lowercase().contains(&hex)
}

#[test]
fn no_secret_is_ever_logged() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap_or_else(|e| panic!("{e}"));
    let records = LogCapture::default();
    log::set_boxed_logger(Box::new(records.clone())).unwrap_or_else(|e| panic!("{e}"));
    log::set_max_level(log::LevelFilter::Trace);

    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/ok", Ok(RawResponse::new(StatusCode::OK, format!(r#"{{"token":"{}"}}"#, SECRETS[0]))));
    fake.route(Method::POST, "/ok", Ok(RawResponse::new(StatusCode::UNAUTHORIZED, SECRETS[1])));
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(
        HttpConfig::new("https://api.example.com").with_header("X-Api-Key", SECRETS[1]).with_timeout(Duration::from_millis(1)),
    ));

    let credentials: Vec<Box<SetCredentials>> = vec![
        Box::new(|c| c.set(BearerToken::new(SECRETS[0]))),
        Box::new(|c| c.set(ApiKeyHeader::new("X-Key", SECRETS[1]))),
        Box::new(|c| c.set(ApiKeyQuery::new("key", SECRETS[2]))),
        // Invalid as a header value: the request is refused, and the reason must not quote it.
        Box::new(|c| c.set(BearerToken::new(format!("{}\n", SECRETS[0])))),
        #[cfg(feature = "json")]
        Box::new(|c| c.set(JsonBodyField::new("token", SECRETS[3]))),
    ];
    for set in credentials {
        set(&mut app.world_mut().resource_mut::<BackendCredentials>());
        let backend = app.world().resource::<HttpClient>();
        backend.get("/ok");
        backend.send(OutgoingRequest::post("/ok").with_body(br#"{"a":1}"#.to_vec()).with_query("q", SECRETS[2]));
        let never = backend.get("/never");
        backend.get("/timeout");
        app.step();
        app.world().resource::<HttpClient>().cancel(never);
        app.step_n(400);
    }
    app.world_mut().write_message(AppExit::Success);
    app.step();
    #[cfg(feature = "ssh")]
    ssh_part();
    #[cfg(feature = "ws")]
    ws_part();
    #[cfg(all(feature = "ws", feature = "json"))]
    ws_link_part();

    let logs = capture.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let log_records = records.client.lock().unwrap_or_else(PoisonError::into_inner).clone();
    #[cfg_attr(not(any(feature = "ws", feature = "ssh")), allow(unused_mut))]
    let mut every_secret: Vec<&str> = SECRETS.to_vec();
    #[cfg(feature = "ws")]
    every_secret.extend(WS_SECRETS);
    #[cfg(all(feature = "ws", feature = "json"))]
    every_secret.extend(WS_LINK_SECRETS);
    #[cfg(feature = "ssh")]
    every_secret.extend(SSH_SECRETS);
    for secret in &every_secret {
        assert!(!leaked(&log_records, secret), "`{secret}` was in a `log` record:\n{log_records}");
    }
    let lower = log_records.to_ascii_lowercase();
    assert!(
        !lower.contains("bearer ") && !lower.contains("authorization") && !lower.contains("x-key"),
        "a credential header was in a `log` record:\n{log_records}"
    );
    #[cfg(all(feature = "ws", feature = "json"))]
    {
        // The capture works: tungstenite's TRACE records of the client's connections are there.
        assert!(log_records.contains("tungstenite::protocol"), "no tungstenite record captured:\n{log_records}");
        // The server side (tungstenite on the mock's threads) did see the secrets: proof that they
        // went over the wire, and that only the client's records are clean.
        let mock = records.mock.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(leaked(&mock, WS_LINK_SECRETS[3]), "the mock never received the first-message token");
    }
    #[cfg(feature = "ssh")]
    assert!(log_records.contains("russh"), "no russh record captured:\n{log_records}");
    assert!(logs.contains(">>> NET-BACKEND"), "nothing captured:\n{logs}");
    assert!(logs.contains("not sent"), "the refused request is logged:\n{logs}");
    for secret in SECRETS {
        assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
    }
    #[cfg(feature = "ws")]
    {
        assert!(logs.contains("refused the credentials"), "the WebSocket part logged nothing:\n{logs}");
        for secret in WS_SECRETS {
            assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
        }
    }
    #[cfg(feature = "ssh")]
    {
        assert!(logs.contains("ssh `main` connected"), "the SSH part logged nothing:\n{logs}");
        for secret in SSH_SECRETS {
            assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
        }
    }
}

/// A real SSH connection (to the mock) whose passphrase, command line, stdin and output are
/// secrets, plus a failed login with a wrong passphrase.
#[cfg(feature = "ssh")]
fn ssh_part() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("redaction-ssh-{}", std::process::id()));
    let client = mock_ssh_server::random_key();
    let key = dir.join("id_test");
    mock_ssh_server::write_key(&client, &key, Some(SSH_SECRETS[0])).unwrap_or_else(|e| panic!("{e}"));
    let mock = mock_ssh_server::MockSshServer::start("tester", client.public_key().clone()).unwrap_or_else(|e| panic!("{e}"));
    let target = |passphrase: &str| {
        SshTarget::new("127.0.0.1", "tester")
            .with_port(mock.port())
            .with_auth(SshAuth::key_file_with_passphrase(&key, passphrase))
            .trust_host_key_fingerprint(mock.fingerprint())
    };
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default());
    fn ssh(app: &TestApp) -> &SshClient {
        app.world().resource::<SshClient>()
    }
    ssh(&app).connect("main", target(SSH_SECRETS[0]));
    ssh(&app).connect("wrong", target(SSH_SECRETS[3]));
    ssh(&app).run("main", format!("echo {}", SSH_SECRETS[1]));
    ssh(&app).run("main", SshCommand::new("cat").with_stdin(SSH_SECRETS[2].as_bytes().to_vec()));
    ssh(&app).run("main", format!("stderr {}", SSH_SECRETS[1]));
    app.step();
    app.run_until(|world| world.resource::<InFlight>().is_empty(), 3000);
    app.step_n(5);
    app.world_mut().write_message(AppExit::Success);
    app.step();
}

/// A WebSocket connection whose token is refused, refreshed by the game and accepted, plus a
/// first-message token that is refused twice (final).
#[cfg(feature = "ws")]
fn ws_part() {
    struct FirstMessage;
    impl Credentials for FirstMessage {
        fn apply(&self, _request: &mut OutgoingRequest) {}
        fn ws_auth_message(&self) -> Option<String> {
            Some(format!(r#"{{"type":"auth","token":"{}"}}"#, WS_SECRETS[0]))
        }
    }
    let fake = FakeWsTransport::new();
    fake.reject_next(BackendError::Status(Box::new(RawResponse::new(StatusCode::UNAUTHORIZED, WS_SECRETS[0]))));
    let mut app = TestApp::new();
    app.insert_resource(WsTransportRes::new(fake.clone())).add_plugins(BackendPlugin::default());
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(WS_SECRETS[0]));
    let settings = WsSettings::new("wss://game.example.com/ws").with_credentials_refresh(WsCredentialsRefresh::new());
    app.world().resource::<WsClient>().connect("main", settings.clone());
    app.step_n(3);
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(WS_SECRETS[1]));
    app.step_n(3);
    app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
    app.world().resource::<WsClient>().connect("first", settings);
    app.step_n(2);
    for _ in 0..2 {
        if let Some(link) = fake.last_link() {
            fake.push(link, WsFrame::Text(format!(r#"{{"type":"auth.failed","error":"{}"}}"#, WS_SECRETS[0])));
        }
        app.step_n(2);
        app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
        app.step_n(2);
    }
    app.world_mut().write_message(AppExit::Success);
    app.step();
}

/// Real WebSocket connections (to the mock, on 127.0.0.1) with each kind of credentials: a bearer
/// token and an API key in handshake headers, an API key in the URL, and a first-message
/// authentication; each also sends a frame and receives one.
#[cfg(all(feature = "ws", feature = "json"))]
fn ws_link_part() {
    struct FirstMessage;
    impl Credentials for FirstMessage {
        fn apply(&self, _request: &mut OutgoingRequest) {}
        fn ws_auth_message(&self) -> Option<String> {
            // An envelope request: the mock answers it ("unknown") instead of echoing it back.
            Some(format!(r#"{{"id":0,"type":"auth","data":{{"token":"{}"}}}}"#, WS_LINK_SECRETS[3]))
        }
    }
    let server = mock_ws_server::MockWsServer::start(0).unwrap_or_else(|e| panic!("mock: {e}"));
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default());
    app.watch::<WsMessage>();
    let credentials: Vec<Box<SetCredentials>> = vec![
        Box::new(|c| c.set(BearerToken::new(WS_LINK_SECRETS[0]))),
        Box::new(|c| c.set(ApiKeyHeader::new("X-Key", WS_LINK_SECRETS[1]))),
        Box::new(|c| c.set(ApiKeyQuery::new("key", WS_LINK_SECRETS[2]))),
        Box::new(|c| c.set(FirstMessage)),
    ];
    for (n, set) in credentials.into_iter().enumerate() {
        set(&mut app.world_mut().resource_mut::<BackendCredentials>());
        let name = format!("link{n}");
        app.world().resource::<WsClient>().connect(name.as_str(), WsSettings::new(format!("{}/plain", server.url())));
        app.step();
        app.run_until(|world| world.resource::<WsConnections>().state(&name) == Some(WsState::Connected), 4000);
        app.world().resource::<WsClient>().send_text(name.as_str(), "plain text");
        let before = app.all_messages::<WsMessage>().len();
        for _ in 0..2000 {
            if app.all_messages::<WsMessage>().len() > before {
                break;
            }
            app.step();
        }
        app.world().resource::<WsClient>().disconnect(name.as_str());
        app.step_n(5);
    }
    assert_eq!(server.accepted(), 4, "every connection was made");
    app.world_mut().write_message(AppExit::Success);
    app.step();
}
