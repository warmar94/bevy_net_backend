//! Named WebSocket connections driven by the `FakeWsTransport`: states, named connections,
//! frames, exactly-one answers (response, timeout, cancel, disconnect, link loss, exit), backoff,
//! credentials on every handshake, the plain-text rule. Strict `TestApp`, no sockets.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::*;

const URL: &str = "wss://game.example.com/ws";

fn app(fake: &FakeWsTransport) -> TestApp {
    let mut app = TestApp::new();
    app.insert_resource(WsTransportRes::new(fake.clone())).add_plugins(BackendPlugin::default());
    app.watch::<WsStateChanged>().watch::<WsMessage>().watch::<WsRawResponse>();
    app
}

fn ws(app: &TestApp) -> &WsClient {
    app.world().resource::<WsClient>()
}

fn settings() -> WsSettings {
    WsSettings::new(URL).with_reconnect(WsReconnect::default().with_jitter(false))
}

fn state(app: &TestApp, name: &str) -> Option<WsState> {
    app.world().resource::<WsConnections>().state(name)
}

fn raw_answers(app: &TestApp, id: RequestId) -> Vec<WsRawResponse> {
    app.all_messages::<WsRawResponse>().into_iter().filter(|a| a.id == id).collect()
}

fn one_error(app: &TestApp, id: RequestId) -> BackendError {
    let answers = raw_answers(app, id);
    assert_eq!(answers.len(), 1, "{answers:?}");
    answers.into_iter().next().and_then(|a| a.result.err()).unwrap_or_else(|| panic!("{id} succeeded"))
}

fn connected(fake: &FakeWsTransport, name: &str) -> (TestApp, WsLinkId) {
    let mut app = app(fake);
    ws(&app).connect(name, settings());
    app.step_n(2);
    assert_eq!(state(&app, name), Some(WsState::Connected));
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    (app, link)
}

/// Steps until `done` or `max` frames.
fn run_until(app: &mut TestApp, max: u32, mut done: impl FnMut(&TestApp) -> bool) {
    for _ in 0..max {
        if done(app) {
            return;
        }
        app.step();
    }
    assert!(done(app), "condition not reached in {max} frames");
}

#[test]
fn connect_reports_connecting_then_connected() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("main", settings());
    app.step();
    assert_eq!(fake.opened().len(), 1);
    app.step();
    let states: Vec<WsState> = app.all_messages::<WsStateChanged>().into_iter().map(|c| c.state).collect();
    assert_eq!(states, vec![WsState::Connecting, WsState::Connected]);
    let (_, handshake) = fake.opened().remove(0);
    assert_eq!(handshake.uri.to_string(), URL);
    assert_eq!(handshake.read_timeout, Duration::from_millis(20));
    assert!(handshake.is_secure());
}

#[test]
fn named_connections_are_independent() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("main", settings());
    ws(&app).connect("chat", WsSettings::new("wss://chat.example.com/"));
    app.step_n(2);
    let opened = fake.opened();
    assert_eq!(opened.len(), 2);
    let link_of = |host: &str| opened.iter().find(|(_, h)| h.uri.host() == Some(host)).map(|(l, _)| *l).unwrap_or_else(|| panic!("{host}"));
    let (main, chat) = (link_of("game.example.com"), link_of("chat.example.com"));
    ws(&app).send_text("main", "to main");
    ws(&app).send_text("chat", "to chat");
    app.step();
    assert_eq!(fake.sent(main), vec![WsFrame::Text("to main".into())]);
    assert_eq!(fake.sent(chat), vec![WsFrame::Text("to chat".into())]);
    fake.push(chat, WsFrame::Text("hi chat".into()));
    fake.drop_link(main, 1006);
    app.step();
    let frames = app.messages::<WsMessage>();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].name, "chat");
    assert!(matches!(state(&app, "main"), Some(WsState::Reconnecting { attempt: 1, .. })));
    assert_eq!(state(&app, "chat"), Some(WsState::Connected));
}

#[test]
fn frames_sent_while_connecting_wait_and_go_out_in_order() {
    let fake = FakeWsTransport::new();
    fake.manual_accept(true);
    let mut app = app(&fake);
    ws(&app).connect("main", settings());
    ws(&app).send_text("main", "one");
    ws(&app).send_binary("main", vec![2]);
    app.step_n(3);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    assert!(fake.sent(link).is_empty());
    assert_eq!(app.world().resource::<WsConnections>().get("main").map(|c| c.queued_frames), Some(2));
    fake.accept(link);
    app.step();
    assert_eq!(fake.sent(link), vec![WsFrame::Text("one".into()), WsFrame::Binary(vec![2])]);
}

#[test]
fn frames_to_an_unknown_or_closed_connection_are_dropped() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    ws(&app).send_text("nope", "x");
    ws(&app).disconnect("main");
    ws(&app).send_text("main", "late");
    app.step_n(2);
    assert!(fake.sent(link).is_empty());
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
}

#[test]
fn a_raw_request_gets_exactly_one_answer() {
    let fake = FakeWsTransport::new();
    fake.echo_envelope(true);
    let (mut app, link) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", br#"{"n":1}"#.to_vec()));
    app.step_n(2);
    let answers = raw_answers(&app, id);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].result.as_deref().ok(), Some(br#"{"n":1}"#.as_slice()));
    // A duplicate answer from the server is ignored.
    let sent = fake.sent(link);
    let text = sent.last().and_then(WsFrame::as_text).unwrap_or_default().to_string();
    let wire: u64 = text.split("\"id\":").nth(1).and_then(|r| r.split([',', '}']).next()).and_then(|n| n.parse().ok()).unwrap_or(0);
    fake.push(link, WsFrame::Text(format!(r#"{{"id":{wire},"ok":true,"data":2}}"#)));
    app.step_n(2);
    assert_eq!(raw_answers(&app, id).len(), 1);
}

#[test]
fn server_errors_are_rejected_answers() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("fail", b"null".to_vec()));
    app.step();
    let text = fake.sent(link).last().and_then(WsFrame::as_text).unwrap_or_default().to_string();
    let wire: u64 = text.split("\"id\":").nth(1).and_then(|r| r.split([',', '}']).next()).and_then(|n| n.parse().ok()).unwrap_or(0);
    fake.push(link, WsFrame::Text(format!(r#"{{"id":{wire},"ok":false,"error":{{"code":"no"}}}}"#)));
    app.step();
    let error = one_error(&app, id);
    assert_eq!(error, BackendError::Rejected(Box::new(Rejection::new(br#"{"code":"no"}"#.to_vec()))));
    assert_eq!(error.was_sent(), Some(true));
    let BackendError::Rejected(rejection) = error else { unreachable!() };
    assert_eq!(rejection.text(), r#"{"code":"no"}"#);
    assert_eq!(rejection.json::<serde_json::Value>().ok(), Some(serde_json::json!({"code": "no"})));
}

#[test]
fn timeouts_say_whether_the_request_was_sent() {
    let fake = FakeWsTransport::new();
    fake.manual_accept(true);
    let mut app = app(&fake);
    ws(&app).connect("main", settings().with_request_timeout(Duration::from_millis(100)));
    let unsent = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    run_until(&mut app, 30, |app| !raw_answers(app, unsent).is_empty());
    assert!(matches!(one_error(&app, unsent), BackendError::Timeout(why) if why.starts_with("not sent:")));
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    fake.accept(link);
    app.step();
    let sent = ws(&app).request_raw("main", WsOutgoing::new("echo", b"2".to_vec()));
    run_until(&mut app, 30, |app| !raw_answers(app, sent).is_empty());
    assert!(matches!(one_error(&app, sent), BackendError::Timeout(why) if why.starts_with("no answer")));
    assert_eq!(fake.sent(link).len(), 1);
}

#[test]
fn cancel_answers_once() {
    let fake = FakeWsTransport::new();
    let (mut app, _) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    ws(&app).cancel(id);
    app.step_n(3);
    assert_eq!(one_error(&app, id), BackendError::Cancelled);
}

#[test]
fn disconnect_answers_pending_requests_and_does_not_reconnect() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    ws(&app).disconnect("main");
    app.step_n(100);
    assert!(matches!(one_error(&app, id), BackendError::Disconnected { sent: Some(true), .. }));
    assert_eq!(fake.closed(), vec![(link, 1000)]);
    assert_eq!(fake.opened().len(), 1, "no reconnect after a disconnect");
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
}

#[test]
fn a_lost_link_reconnects_with_backoff_and_resends_only_what_is_marked() {
    let fake = FakeWsTransport::new();
    fake.echo_envelope(false);
    let (mut app, link) = connected(&fake, "main");
    let plain = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    let resend = ws(&app).request_raw("main", WsOutgoing::new("echo", b"2".to_vec()).resend_on_reconnect(true));
    app.step();
    assert_eq!(fake.sent(link).len(), 2);
    fake.drop_link(link, 1006);
    app.step();
    assert!(matches!(one_error(&app, plain), BackendError::Disconnected { sent: Some(true), .. }));
    assert!(raw_answers(&app, resend).is_empty());
    let change = app.messages::<WsStateChanged>().pop().unwrap_or_else(|| panic!("no change"));
    assert_eq!(change.state, WsState::Reconnecting { attempt: 1, retry_in: Duration::from_millis(500) });
    assert!(change.error.is_some());
    fake.echo_envelope(true);
    run_until(&mut app, 60, |app| state(app, "main") == Some(WsState::Connected));
    let second = fake.last_link().unwrap_or_else(|| panic!("no second link"));
    assert_ne!(second, link);
    app.step_n(2);
    assert_eq!(fake.sent(second).len(), 1, "only the resend request goes out again");
    assert_eq!(raw_answers(&app, resend).len(), 1);
    assert!(raw_answers(&app, resend)[0].result.is_ok());
}

#[test]
fn backoff_doubles_and_stops_after_max_attempts() {
    let fake = FakeWsTransport::new();
    for _ in 0..5 {
        fake.reject_next(BackendError::Network("connection refused".into()));
    }
    let mut app = app(&fake);
    let reconnect =
        WsReconnect::default().with_jitter(false).with_base(Duration::from_millis(100)).with_cap(Duration::from_millis(300)).with_max_attempts(Some(3));
    ws(&app).connect("main", WsSettings::new(URL).with_reconnect(reconnect));
    run_until(&mut app, 200, |app| state(app, "main") == Some(WsState::Disconnected));
    let delays: Vec<Duration> = app
        .all_messages::<WsStateChanged>()
        .into_iter()
        .filter_map(|c| if let WsState::Reconnecting { retry_in, .. } = c.state { Some(retry_in) } else { None })
        .collect();
    assert_eq!(delays, vec![Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(300)]);
    assert_eq!(fake.opened().len(), 4, "the first attempt + 3 retries");
    let info = app.world().resource::<WsConnections>().get("main").cloned().unwrap_or_else(|| panic!("no info"));
    assert!(matches!(info.last_error, Some(BackendError::Network(_))));
}

#[test]
fn jitter_stays_within_the_bound() {
    let reconnect = WsReconnect::default().with_base(Duration::from_millis(500)).with_cap(Duration::from_secs(30));
    assert_eq!(reconnect.delay_bound(1), Duration::from_millis(500));
    assert_eq!(reconnect.delay_bound(4), Duration::from_secs(4));
    assert_eq!(reconnect.delay_bound(40), Duration::from_secs(30));
}

#[test]
fn a_refused_handshake_or_policy_close_is_not_retried() {
    let fake = FakeWsTransport::new();
    fake.reject_next(BackendError::Status(Box::new(RawResponse::new(StatusCode::UNAUTHORIZED, ""))));
    let mut app = app(&fake);
    ws(&app).connect("main", settings());
    app.step_n(100);
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
    assert_eq!(fake.opened().len(), 1);

    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    fake.drop_link(link, 4001);
    app.step_n(100);
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
    assert_eq!(fake.opened().len(), 1);
}

#[test]
fn credentials_are_applied_to_every_handshake_and_a_new_token_waits_for_the_reconnect() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new("fake-token-a"));
    ws(&app).connect("main", settings());
    app.step_n(2);
    let auth = |fake: &FakeWsTransport, i: usize| fake.opened()[i].1.headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
    assert_eq!(auth(&fake, 0).as_deref(), Some("Bearer fake-token-a"));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new("fake-token-b"));
    app.step_n(10);
    assert_eq!(fake.opened().len(), 1, "no forced reconnect on a token change");
    fake.drop_link(fake.last_link().unwrap_or_else(|| panic!("no link")), 1006);
    run_until(&mut app, 60, |_| fake.opened().len() == 2);
    assert_eq!(auth(&fake, 1).as_deref(), Some("Bearer fake-token-b"));
}

#[test]
fn query_credentials_and_handshake_headers() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(ApiKeyQuery::new("key", "fake key"));
    ws(&app).connect("main", WsSettings::new("wss://game.example.com/ws?room=7").with_header("Sec-WebSocket-Protocol", "game.v1"));
    app.step();
    let (_, handshake) = fake.opened().remove(0);
    assert_eq!(handshake.uri.query(), Some("room=7&key=fake%20key"));
    assert_eq!(handshake.headers.get("sec-websocket-protocol").and_then(|v| v.to_str().ok()), Some("game.v1"));
    assert!(!format!("{handshake:?}").contains("fake"), "{handshake:?}");
}

#[test]
fn a_resend_request_that_times_out_while_reconnecting_is_not_called_unsent() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    let slow = WsReconnect::default().with_jitter(false).with_base(Duration::from_secs(10));
    ws(&app).connect("main", WsSettings::new(URL).with_reconnect(slow).with_request_timeout(Duration::from_millis(200)));
    app.step_n(2);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    let id = ws(&app).request_raw("main", WsOutgoing::new("buy", b"1".to_vec()).resend_on_reconnect(true));
    app.step();
    fake.drop_link(link, 1006);
    run_until(&mut app, 40, |app| !raw_answers(app, id).is_empty());
    let error = one_error(&app, id);
    assert!(matches!(&error, BackendError::Timeout(why) if why.starts_with("sent before")), "{error:?}");
    assert_ne!(error.was_sent(), Some(false), "it went out on the first link");
}

#[test]
fn a_server_close_code_is_structured() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    fake.drop_link(link, 4003);
    app.step();
    let change = app.messages::<WsStateChanged>().pop().unwrap_or_else(|| panic!("no change"));
    assert_eq!(change.state, WsState::Disconnected, "4000-4099 is not retried");
    assert_eq!(change.error.as_ref().and_then(BackendError::close_code), Some(4003));
    assert!(matches!(change.error, Some(BackendError::Closed { code: 4003, .. })));
}

#[test]
fn websocket_requests_are_in_flight_and_share_the_http_cancel() {
    let fake = FakeWsTransport::new();
    let (mut app, _) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    let info = app.world().resource::<InFlight>().describe(id).cloned().unwrap_or_else(|| panic!("not in flight"));
    assert_eq!((info.kind, info.target.as_str()), (RequestKind::WebSocket, "main"));
    app.world().resource::<HttpClient>().cancel(id);
    app.step_n(2);
    assert_eq!(one_error(&app, id), BackendError::Cancelled);
    assert!(!app.world().resource::<InFlight>().contains(id));
}

#[test]
fn requests_can_wait_for_the_auth_acknowledgement() {
    struct FirstMessage;
    impl Credentials for FirstMessage {
        fn apply(&self, _request: &mut OutgoingRequest) {}
        fn ws_auth_message(&self) -> Option<String> {
            Some(r#"{"type":"auth","token":"fake-token"}"#.into())
        }
    }
    let fake = FakeWsTransport::new();
    fake.echo_envelope(true);
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
    ws(&app).connect("main", settings().with_auth_ack(Duration::from_millis(300)));
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    ws(&app).send_text("main", "held too");
    app.step_n(3);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    assert_eq!(fake.sent(link).len(), 1, "only the auth message before the ack");
    fake.push(link, WsFrame::Text(r#"{"type":"auth.ok"}"#.into()));
    app.step_n(3);
    assert_eq!(fake.sent(link).len(), 3);
    assert!(raw_answers(&app, id).first().is_some_and(|a| a.result.is_ok()));

    // No acknowledgement: the held request is answered "not sent" and the link is closed.
    let fake = FakeWsTransport::new();
    let mut app = self::app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
    ws(&app).connect("main", settings().with_auth_ack(Duration::from_millis(100)));
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    run_until(&mut app, 30, |app| !raw_answers(app, id).is_empty());
    let error = one_error(&app, id);
    assert!(matches!(&error, BackendError::Timeout(why) if why.starts_with("not sent:")), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    assert!(fake.closed().iter().any(|(_, code)| *code == 1008));
    // Not a silent Connected: the connection is Disconnected with the reason.
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert!(matches!(error, Some(BackendError::Disconnected { reason, .. }) if reason.contains("acknowledgement")));
}

/// Review r2b probe F2: a resend request goes out on an acked link, the link drops, and the
/// reconnect is never acknowledged. The answer must not claim "not sent".
#[test]
fn an_unacknowledged_reconnect_does_not_call_an_earlier_send_unsent() {
    struct FirstMessage;
    impl Credentials for FirstMessage {
        fn apply(&self, _request: &mut OutgoingRequest) {}
        fn ws_auth_message(&self) -> Option<String> {
            Some(r#"{"type":"auth"}"#.into())
        }
    }
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
    let quick = WsReconnect::default().with_jitter(false).with_base(Duration::from_millis(20));
    ws(&app).connect("main", settings().with_reconnect(quick).with_auth_ack(Duration::from_millis(200)).with_request_timeout(Duration::from_secs(10)));
    app.step_n(2);
    let first = fake.last_link().unwrap_or_else(|| panic!("no link"));
    fake.push(first, WsFrame::Text(r#"{"type":"auth.ok"}"#.into()));
    app.step();
    let buy = ws(&app).request_raw("main", WsOutgoing::new("buy", b"1".to_vec()).resend_on_reconnect(true));
    app.step();
    assert_eq!(fake.sent(first).len(), 2, "auth + buy went out on the first link");
    fake.drop_link(first, 1006);
    run_until(&mut app, 60, |app| !raw_answers(app, buy).is_empty());
    let error = one_error(&app, buy);
    assert!(matches!(&error, BackendError::Timeout(why) if why.starts_with("sent before")), "{error:?}");
    assert_ne!(error.was_sent(), Some(false));
}

#[test]
fn a_request_cancelled_in_its_own_frame_is_never_sent() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("buy", b"1".to_vec()));
    ws(&app).cancel(id);
    app.step_n(2);
    assert_eq!(one_error(&app, id), BackendError::Cancelled);
    assert!(fake.sent(link).is_empty(), "a request cancelled in its own frame must not go out");
}

#[test]
fn tls_errors_are_permanent_unless_the_game_opts_in() {
    let fake = FakeWsTransport::new();
    fake.reject_next(BackendError::Tls("invalid peer certificate: UnknownIssuer".into()));
    let mut app = app(&fake);
    ws(&app).connect("main", settings());
    app.step_n(60);
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
    assert_eq!(fake.opened().len(), 1);
    assert!(matches!(app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone()), Some(BackendError::Tls(_))));

    let fake = FakeWsTransport::new();
    fake.reject_next(BackendError::Tls("invalid peer certificate: UnknownIssuer".into()));
    let mut app = self::app(&fake);
    ws(&app).connect("main", WsSettings::new(URL).with_reconnect(WsReconnect::default().with_jitter(false).with_tls_retry(true)));
    run_until(&mut app, 60, |app| state(app, "main") == Some(WsState::Connected));
    assert_eq!(fake.opened().len(), 2);
}

#[test]
fn too_many_waiting_requests_are_refused_unsent() {
    let fake = FakeWsTransport::new();
    fake.manual_accept(true);
    let mut app = app(&fake);
    ws(&app).connect("main", settings().with_waiting_limit(2));
    let ids: Vec<RequestId> = (0..3).map(|i| ws(&app).request_raw("main", WsOutgoing::new("echo", format!("{i}").into_bytes()))).collect();
    app.step_n(2);
    assert!(matches!(one_error(&app, ids[2]), BackendError::Disconnected { sent: Some(false), .. }));
    assert!(raw_answers(&app, ids[0]).is_empty());
}

#[test]
fn a_second_connect_answers_the_old_requests_once() {
    let fake = FakeWsTransport::new();
    let (mut app, old) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    ws(&app).connect("main", settings());
    app.step_n(3);
    assert!(matches!(one_error(&app, id), BackendError::Disconnected { sent: Some(true), .. }));
    // Anything the old link still reports is ignored.
    fake.push(old, WsFrame::Text("stale".into()));
    app.step_n(2);
    assert!(app.all_messages::<WsMessage>().iter().all(|m| m.frame != WsFrame::Text("stale".into())));
    assert_eq!(state(&app, "main"), Some(WsState::Connected));
}

#[test]
fn a_cancel_in_the_exit_frame_is_cancelled_not_shutdown() {
    let fake = FakeWsTransport::new();
    let (mut app, _) = connected(&fake, "main");
    let id = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    ws(&app).cancel(id);
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(one_error(&app, id), BackendError::Cancelled);
}

#[test]
fn huge_settings_are_clamped() {
    let settings = WsSettings::new(URL).with_connect_timeout(Duration::MAX).with_heartbeat(Duration::MAX, Duration::MAX).with_request_timeout(Duration::MAX);
    assert_eq!(settings.connect_timeout(), MAX_TIMEOUT);
    assert_eq!(settings.heartbeat(), (MAX_TIMEOUT, MAX_TIMEOUT));
}

#[cfg(feature = "json")]
#[test]
fn a_json_body_credential_is_a_clear_error() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(JsonBodyField::new("token", "fake"));
    ws(&app).connect("main", settings());
    app.step_n(2);
    assert!(fake.opened().is_empty());
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("main").and_then(|c| c.last_error.clone());
    assert!(matches!(error, Some(BackendError::InvalidRequest(why)) if why.contains("first-message auth")));
}

#[test]
fn first_message_auth_is_the_first_frame_on_every_connection() {
    struct FirstMessage;
    impl Credentials for FirstMessage {
        fn apply(&self, _request: &mut OutgoingRequest) {}
        fn ws_auth_message(&self) -> Option<String> {
            Some(r#"{"type":"auth","token":"fake-token"}"#.into())
        }
    }
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    app.world_mut().resource_mut::<BackendCredentials>().set(FirstMessage);
    ws(&app).connect("main", settings());
    ws(&app).send_text("main", "after auth");
    app.step_n(2);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    let sent = fake.sent(link);
    assert_eq!(sent.first().and_then(WsFrame::as_text), Some(r#"{"type":"auth","token":"fake-token"}"#));
    assert_eq!(sent.get(1).and_then(WsFrame::as_text), Some("after auth"));
    fake.push(link, WsFrame::Text(r#"{"type":"auth.failed","error":"expired"}"#.into()));
    app.step_n(50);
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected), "a refused auth message is not retried");
}

#[test]
fn plain_ws_only_to_loopback_unless_allowed() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("lan", WsSettings::new("ws://192.168.1.20:9001/"));
    ws(&app).connect("local", WsSettings::new("ws://127.0.0.1:9001/"));
    ws(&app).connect("dev", WsSettings::new("ws://192.168.1.20:9001/").allow_insecure_ws(true));
    ws(&app).connect("bad", WsSettings::new("https://example.com/"));
    app.step_n(2);
    let connections = app.world().resource::<WsConnections>();
    assert!(matches!(connections.get("lan").and_then(|c| c.last_error.clone()), Some(BackendError::InsecureHttp { .. })));
    assert!(matches!(connections.get("bad").and_then(|c| c.last_error.clone()), Some(BackendError::InvalidRequest(_))));
    assert!(connections.is_connected("local") && connections.is_connected("dev"));
    assert_eq!(fake.opened().len(), 2);
}

#[test]
fn requests_on_unknown_or_closed_connections_are_answered() {
    let fake = FakeWsTransport::new();
    let (mut app, _) = connected(&fake, "main");
    let unknown = ws(&app).request_raw("nope", WsOutgoing::new("echo", b"1".to_vec()));
    ws(&app).connect("raw", settings().without_protocol());
    let no_protocol = ws(&app).request_raw("raw", WsOutgoing::new("echo", b"1".to_vec()));
    app.step_n(2);
    assert!(matches!(one_error(&app, unknown), BackendError::InvalidRequest(_)));
    assert!(matches!(one_error(&app, no_protocol), BackendError::InvalidRequest(_)));
    ws(&app).disconnect("main");
    app.step();
    let closed = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step_n(2);
    assert!(matches!(one_error(&app, closed), BackendError::Disconnected { sent: Some(false), .. }));
}

#[test]
fn oversize_frames_and_requests_are_refused() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("main", settings().with_max_message_bytes(1024));
    app.step_n(2);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    ws(&app).send_text("main", "x".repeat(2000));
    let big = ws(&app).request_raw("main", WsOutgoing::new("echo", format!("\"{}\"", "x".repeat(2000)).into_bytes()));
    app.step_n(2);
    assert!(fake.sent(link).is_empty());
    assert!(matches!(one_error(&app, big), BackendError::InvalidRequest(_)));
}

#[test]
fn app_exit_answers_shutdown_closes_links_and_sends_nothing_of_the_exit_frame() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    let earlier = ws(&app).request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    app.step();
    let same_frame = ws(&app).request_raw("main", WsOutgoing::new("echo", b"2".to_vec()));
    ws(&app).send_text("main", "bye");
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(one_error(&app, earlier), BackendError::Shutdown);
    assert_eq!(one_error(&app, same_frame), BackendError::Shutdown);
    assert_eq!(fake.sent(link).len(), 1, "nothing of the exit frame is sent");
    assert_eq!(fake.closed(), vec![(link, 1001)]);
    assert_eq!(fake.shutdown_count(), 1);
    assert_eq!(state(&app, "main"), Some(WsState::Disconnected));
}

#[test]
fn a_replaced_transport_counts_as_a_lost_link() {
    let old = FakeWsTransport::new();
    let (mut app, _) = connected(&old, "main");
    let new = FakeWsTransport::new();
    app.insert_resource(WsTransportRes::new(new.clone()));
    app.step();
    assert!(matches!(state(&app, "main"), Some(WsState::Reconnecting { .. })));
    run_until(&mut app, 60, |app| state(app, "main") == Some(WsState::Connected));
    assert_eq!(new.opened().len(), 1);
}

#[test]
fn many_systems_use_the_client_without_ordering() {
    let fake = FakeWsTransport::new();
    let (mut app, link) = connected(&fake, "main");
    let fire = |ws: Res<WsClient>| ws.send_text("main", "x");
    app.add_systems(PreUpdate, fire).add_systems(Update, (fire, fire)).add_systems(Last, fire);
    app.step_n(3);
    assert!(fake.sent(link).len() >= 8);
}

#[test]
fn debug_output_hides_secrets() {
    let settings = WsSettings::new("wss://game.example.com/ws?key=fake-secret").with_header("X-Key", "fake-secret");
    assert!(!format!("{settings:?}").contains("fake-secret"));
    assert!(!format!("{:?}", WsFrame::Text("fake-secret".into())).contains("fake-secret"));
    assert!(!format!("{:?}", WsOutgoing::new("login", b"fake-secret".to_vec())).contains("fake-secret"));
    let rejected = BackendError::Rejected(Box::new(Rejection::new(b"fake-secret".to_vec())));
    assert!(!format!("{rejected:?}").contains("fake-secret"));
    assert!(!rejected.to_string().contains("fake-secret"));
}

#[cfg(feature = "json")]
mod json {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    struct Echo {
        n: u32,
    }

    #[derive(Deserialize, Clone, Debug, PartialEq)]
    struct EchoBack {
        n: u32,
    }

    impl WsRequest for Echo {
        type Response = EchoBack;
        const KIND: &'static str = "echo";
    }

    #[derive(Deserialize, Clone, Debug, PartialEq)]
    struct Tick {
        n: u64,
    }

    impl WsPushMessage for Tick {
        const KIND: &'static str = "server.tick";
    }

    #[derive(Serialize)]
    struct NotRegistered;

    impl WsRequest for NotRegistered {
        type Response = ();
        const KIND: &'static str = "x";
    }

    fn json_app(fake: &FakeWsTransport) -> TestApp {
        let mut app = app(fake);
        app.add_ws_request::<Echo>().add_ws_push::<Tick>();
        app.watch::<WsResponse<EchoBack>>().watch::<WsPush<Tick>>();
        app
    }

    #[test]
    fn typed_requests_and_pushes() {
        let fake = FakeWsTransport::new();
        fake.echo_envelope(true);
        let mut app = json_app(&fake);
        ws(&app).connect("main", settings());
        app.step_n(2);
        let id = ws(&app).request("main", &Echo { n: 5 });
        let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
        fake.push(link, WsFrame::Text(r#"{"type":"server.tick","data":{"n":1}}"#.into()));
        app.step_n(2);
        let answer = app.all_messages::<WsResponse<EchoBack>>().pop().unwrap_or_else(|| panic!("no answer"));
        assert_eq!((answer.id, answer.result), (id, Ok(EchoBack { n: 5 })));
        let push = app.all_messages::<WsPush<Tick>>().pop().unwrap_or_else(|| panic!("no push"));
        assert_eq!((push.name.as_str(), push.data), ("main", Tick { n: 1 }));
        // Every frame is also raw.
        assert!(app.all_messages::<WsMessage>().len() >= 2);
    }

    #[test]
    fn an_unregistered_request_type_is_answered_raw() {
        let fake = FakeWsTransport::new();
        let mut app = json_app(&fake);
        ws(&app).connect("main", settings());
        app.step_n(2);
        let id = ws(&app).request("main", &NotRegistered);
        app.step_n(2);
        assert!(matches!(one_error(&app, id), BackendError::InvalidRequest(why) if why.contains("add_ws_request")));
    }
}
