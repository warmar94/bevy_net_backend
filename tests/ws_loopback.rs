//! The real `TungsteniteTransport` against the mock from `examples/mock_ws_server.rs`, started on
//! 127.0.0.1 in this process (never another host). Bounded: every wait has a frame limit.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::StatusCode;
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

#[derive(Serialize)]
struct ChatSend {
    text: String,
}

#[derive(Deserialize, Clone, Debug)]
struct ChatAck {
    accepted: bool,
}

impl WsRequest for ChatSend {
    type Response = ChatAck;
    const KIND: &'static str = "chat.send";
}

#[derive(Deserialize, Clone, Debug)]
struct ChatMessage {
    text: String,
}

impl WsPushMessage for ChatMessage {
    const KIND: &'static str = "chat.message";
}

#[derive(Deserialize, Clone, Debug)]
struct Tick {
    n: u64,
}

impl WsPushMessage for Tick {
    const KIND: &'static str = "server.tick";
}

fn app() -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default()).add_ws_request::<Echo>().add_ws_request::<ChatSend>().add_ws_push::<ChatMessage>().add_ws_push::<Tick>();
    app.watch::<WsStateChanged>()
        .watch::<WsMessage>()
        .watch::<WsRawResponse>()
        .watch::<WsResponse<EchoBack>>()
        .watch::<WsResponse<ChatAck>>()
        .watch::<WsPush<ChatMessage>>()
        .watch::<WsPush<Tick>>();
    app
}

fn mock(tick_ms: u64) -> MockWsServer {
    MockWsServer::start(tick_ms).unwrap_or_else(|e| panic!("mock: {e}"))
}

fn ws(app: &TestApp) -> &WsClient {
    app.world().resource::<WsClient>()
}

fn state(app: &TestApp, name: &str) -> Option<WsState> {
    app.world().resource::<WsConnections>().state(name)
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

fn echo_answer(app: &TestApp, id: RequestId) -> Option<Result<EchoBack, BackendError>> {
    app.all_messages::<WsResponse<EchoBack>>().into_iter().find(|a| a.id == id).map(|a| a.result)
}

#[test]
fn typed_requests_pushes_and_raw_frames() {
    let server = mock(50);
    let mut app = app();
    ws(&app).connect("main", WsSettings::new(server.url()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let echo = ws(&app).request("main", &Echo { text: "hi".into() });
    let chat = ws(&app).request("main", &ChatSend { text: "hello".into() });
    ws(&app).send_text("main", "plain text");
    ws(&app).send_binary("main", vec![1, 2, 3]);
    run_until(&mut app, |app| {
        echo_answer(app, echo).is_some()
            && !app.all_messages::<WsResponse<ChatAck>>().is_empty()
            && !app.all_messages::<WsPush<ChatMessage>>().is_empty()
            && app.all_messages::<WsPush<Tick>>().len() >= 2
            && app.all_messages::<WsMessage>().iter().any(|m| m.frame == WsFrame::Binary(vec![1, 2, 3]))
    });
    assert_eq!(echo_answer(&app, echo), Some(Ok(EchoBack { text: "hi".into() })));
    let ack = app.all_messages::<WsResponse<ChatAck>>().pop().unwrap_or_else(|| panic!("no ack"));
    assert_eq!(ack.id, chat);
    assert!(ack.result.is_ok_and(|a| a.accepted));
    assert_eq!(app.all_messages::<WsPush<ChatMessage>>()[0].data.text, "hello");
    assert!(app.all_messages::<WsMessage>().iter().any(|m| m.frame == WsFrame::Text("plain text".into())));
    let ticks: Vec<u64> = app.all_messages::<WsPush<Tick>>().into_iter().map(|p| p.data.n).collect();
    assert_eq!(&ticks[..2], &[1, 2]);
}

#[test]
fn two_named_connections_at_once() {
    let server = mock(0);
    let mut app = app();
    ws(&app).connect("a", WsSettings::new(server.url()));
    ws(&app).connect("b", WsSettings::new(server.url()));
    run_until(&mut app, |app| state(app, "a") == Some(WsState::Connected) && state(app, "b") == Some(WsState::Connected));
    ws(&app).send_text("a", "for a");
    ws(&app).send_text("b", "for b");
    run_until(&mut app, |app| app.all_messages::<WsMessage>().len() >= 2);
    for message in app.all_messages::<WsMessage>() {
        assert_eq!(message.frame, WsFrame::Text(format!("for {}", message.name)));
    }
    assert_eq!(server.accepted(), 2);
}

#[test]
fn large_messages_across_many_short_read_timeouts() {
    let server = mock(0);
    let mut app = app();
    ws(&app).connect("main", WsSettings::new(server.url()).with_read_timeout(Duration::from_millis(5)).with_max_message_bytes(1024 * 1024));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    for i in 0..6 {
        let text = format!("{i}-{}", "0123456789".repeat(40 * 1024));
        let id = ws(&app).request("main", &Echo { text: text.clone() });
        run_until(&mut app, |app| echo_answer(app, id).is_some());
        assert_eq!(echo_answer(&app, id), Some(Ok(EchoBack { text })), "echo {i}");
        let binary: Vec<u8> = (0..700 * 1024u32).map(|n| u8::try_from((n + i) % 253).unwrap_or(0)).collect();
        ws(&app).send_binary("main", binary.clone());
        run_until(&mut app, |app| app.all_messages::<WsMessage>().iter().any(|m| m.frame == WsFrame::Binary(binary.clone())));
    }
}

#[test]
fn a_policy_close_is_final_and_a_normal_drop_reconnects() {
    let server = mock(0);
    let mut app = app();
    let quick = WsReconnect::default().with_jitter(false).with_base(Duration::from_millis(10));
    ws(&app).connect("main", WsSettings::new(server.url()).with_reconnect(quick.clone()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let pending = ws(&app).request_raw("main", WsOutgoing::new("stall", br#"{"ms":300}"#.to_vec()));
    ws(&app).request_raw("main", WsOutgoing::new("drop", b"null".to_vec()));
    run_until(&mut app, |app| matches!(state(app, "main"), Some(WsState::Reconnecting { .. })));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    assert_eq!(server.accepted(), 2);
    let answer = app.all_messages::<WsRawResponse>().into_iter().find(|a| a.id == pending).map(|a| a.result);
    assert!(matches!(answer, Some(Err(BackendError::Disconnected { sent: Some(true), .. }))), "{answer:?}");

    ws(&app).request_raw("main", WsOutgoing::new("close", br#"{"code":4001}"#.to_vec()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert_eq!(error.as_ref().and_then(BackendError::close_code), Some(4001));
    app.step_n(200);
    assert_eq!(server.accepted(), 2, "a 4001 close is not retried");
}

#[test]
fn a_silent_peer_is_detected_by_the_heartbeat() {
    let server = mock(0);
    let mut app = app();
    let settings = WsSettings::new(server.url())
        .with_heartbeat(Duration::from_millis(50), Duration::from_millis(300))
        .with_reconnect(WsReconnect::default().with_jitter(false).with_base(Duration::from_millis(10)));
    ws(&app).connect("main", settings);
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    ws(&app).request_raw("main", WsOutgoing::new("stall", br#"{"ms":2000}"#.to_vec()));
    run_until(&mut app, |app| matches!(state(app, "main"), Some(WsState::Reconnecting { .. })));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert!(matches!(&error, Some(BackendError::Timeout(why)) if why.contains("dead")), "{error:?}");
}

#[test]
fn the_handshake_carries_credentials_and_a_401_is_final() {
    let server = mock(0);
    let mut app = app();
    ws(&app).connect("anon", WsSettings::new(format!("{}/secure", server.url())));
    run_until(&mut app, |app| state(app, "anon") == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("anon").and_then(|c| c.last_error.clone());
    assert_eq!(error.as_ref().and_then(BackendError::status), Some(StatusCode::UNAUTHORIZED));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(mock_ws_server::TOKEN));
    ws(&app).connect("authed", WsSettings::new(format!("{}/secure", server.url())));
    run_until(&mut app, |app| state(app, "authed") == Some(WsState::Connected));
}

#[test]
fn a_message_over_the_limit_closes_with_1009() {
    let server = mock(0);
    let mut app = app();
    let settings = WsSettings::new(server.url()).with_max_message_bytes(64 * 1024).with_reconnect(WsReconnect::never());
    ws(&app).connect("main", settings);
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let id = ws(&app).request_raw("main", WsOutgoing::new("big", br#"{"bytes":200000}"#.to_vec()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert!(matches!(&error, Some(BackendError::Disconnected { reason, .. }) if reason.contains("1009")), "{error:?}");
    let answer = app.all_messages::<WsRawResponse>().into_iter().find(|a| a.id == id).map(|a| a.result);
    assert!(matches!(answer, Some(Err(BackendError::Disconnected { sent: Some(true), .. }))));
}

#[test]
fn unlimited_looking_settings_still_connect() {
    let server = mock(0);
    let mut app = app();
    let settings = WsSettings::new(server.url()).with_connect_timeout(Duration::MAX).with_heartbeat(Duration::MAX, Duration::MAX);
    ws(&app).connect("main", settings);
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
}

#[test]
fn wss_to_a_plain_server_is_a_tls_error() {
    let server = mock(0);
    let mut app = app();
    let url = server.url().replace("ws://", "wss://");
    ws(&app).connect("main", WsSettings::new(url).with_reconnect(WsReconnect::never()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert!(matches!(&error, Some(BackendError::Tls(_) | BackendError::Network(_))), "{error:?}");
}

#[test]
fn app_exit_closes_and_answers_shutdown() {
    let server = mock(0);
    let mut app = app();
    ws(&app).connect("main", WsSettings::new(server.url()));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let pending = ws(&app).request_raw("main", WsOutgoing::new("stall", br#"{"ms":500}"#.to_vec()));
    app.step_n(3);
    app.world_mut().write_message(AppExit::Success);
    app.step();
    let answer = app.all_messages::<WsRawResponse>().into_iter().find(|a| a.id == pending).map(|a| a.result);
    assert_eq!(answer.and_then(Result::err), Some(BackendError::Shutdown));
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
}
