//! The `ws` feature on its own (no `json`, so no request protocol): raw frames, states, exit.
//! Strict `TestApp`, `FakeWsTransport`, no sockets.

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

fn app(fake: &FakeWsTransport) -> TestApp {
    let mut app = TestApp::new();
    app.insert_resource(WsTransportRes::new(fake.clone())).add_plugins(BackendPlugin::default());
    app.watch::<WsStateChanged>().watch::<WsMessage>().watch::<WsRawResponse>();
    app
}

fn ws(app: &TestApp) -> &WsClient {
    app.world().resource::<WsClient>()
}

#[test]
fn raw_frames_both_ways() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("main", WsSettings::new("wss://game.example.com/ws").without_protocol());
    ws(&app).send_binary("main", vec![1, 2, 3]);
    app.step_n(2);
    let link = fake.last_link().unwrap_or_else(|| panic!("no link"));
    assert_eq!(fake.sent(link), vec![WsFrame::Binary(vec![1, 2, 3])]);
    fake.push(link, WsFrame::Text("hello".into()));
    app.step();
    let frames = app.messages::<WsMessage>();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame, WsFrame::Text("hello".into()));
    assert!(app.world().resource::<WsConnections>().is_connected("main"));
}

#[test]
fn requests_need_a_protocol() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("main", WsSettings::new("wss://game.example.com/ws").without_protocol());
    let id = ws(&app).request_raw("main", WsOutgoing::new("x", b"1".to_vec()));
    app.step_n(2);
    let answers: Vec<WsRawResponse> = app.all_messages::<WsRawResponse>().into_iter().filter(|a| a.id == id).collect();
    assert_eq!(answers.len(), 1);
    assert!(matches!(answers[0].result, Err(BackendError::InvalidRequest(_))));
}

#[test]
fn exit_closes_every_link() {
    let fake = FakeWsTransport::new();
    let mut app = app(&fake);
    ws(&app).connect("a", WsSettings::new("wss://a.example.com/"));
    ws(&app).connect("b", WsSettings::new("wss://b.example.com/"));
    app.step_n(2);
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(fake.closed().len(), 2);
    assert_eq!(fake.shutdown_count(), 1);
    let connections = app.world().resource::<WsConnections>();
    assert_eq!(connections.state("a"), Some(WsState::Disconnected));
    assert_eq!(connections.state("b"), Some(WsState::Disconnected));
}
