//! Live WSS checks against a real server: the WebSocket mock (`examples/mock_ws_server.rs`) run
//! behind a TLS-terminating reverse proxy on a test machine. Every test is `#[ignore]`d and does
//! nothing unless `BNB_TEST_WSS_URL` is set to that server's base URL (e.g.
//! `wss://test.example.com/ws`, no trailing slash); no address is ever written into this
//! repository. The proxy must pass WebSocket upgrades through and must not compress.
//!
//! ```text
//! BNB_TEST_WSS_URL=wss://<test host>/ws cargo test --features ws --test live_ws -- --ignored
//! ```
//!
//! Bounded: every wait has a frame limit (about 20 s). Only the mock's fake token is sent.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::*;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[path = "../examples/mock_ws_server.rs"]
mod mock_ws_server;

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

#[derive(Deserialize, Clone, Debug)]
struct Tick {
    n: u64,
}

impl WsPushMessage for Tick {
    const KIND: &'static str = "server.tick";
}

/// The live app and base URL, or `None` (test skipped) without `BNB_TEST_WSS_URL`.
fn live() -> Option<(TestApp, String)> {
    let Ok(url) = std::env::var("BNB_TEST_WSS_URL") else {
        eprintln!("BNB_TEST_WSS_URL is not set: skipped");
        return None;
    };
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(5)).build();
    app.add_plugins(BackendPlugin::default()).add_ws_request::<Echo>().add_ws_push::<Tick>();
    app.watch::<WsResponse<EchoBack>>().watch::<WsPush<Tick>>().watch::<WsStateChanged>();
    Some((app, url.trim_end_matches('/').to_string()))
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

#[test]
#[ignore = "live: needs BNB_TEST_WSS_URL"]
fn live_wss_echo_and_pushes() {
    let Some((mut app, url)) = live() else { return };
    app.world().resource::<WsClient>().connect("main", WsSettings::new(format!("{url}?tick_ms=200")));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let id = app.world().resource::<WsClient>().request("main", &Echo { text: "over TLS".into() });
    run_until(&mut app, |app| {
        app.all_messages::<WsResponse<EchoBack>>().iter().any(|a| a.id == id) && app.all_messages::<WsPush<Tick>>().iter().any(|p| p.data.n >= 2)
    });
    let answer = app.all_messages::<WsResponse<EchoBack>>().into_iter().find(|a| a.id == id).map(|a| a.result);
    assert_eq!(answer, Some(Ok(EchoBack { text: "over TLS".into() })));
}

#[test]
#[ignore = "live: needs BNB_TEST_WSS_URL"]
fn live_wss_large_message() {
    let Some((mut app, url)) = live() else { return };
    app.world().resource::<WsClient>().connect("main", WsSettings::new(format!("{url}?tick_ms=0")));
    run_until(&mut app, |app| state(app, "main") == Some(WsState::Connected));
    let text = "0123456789".repeat(60 * 1024);
    let id = app.world().resource::<WsClient>().request("main", &Echo { text: text.clone() });
    run_until(&mut app, |app| app.all_messages::<WsResponse<EchoBack>>().iter().any(|a| a.id == id));
    let answer = app.all_messages::<WsResponse<EchoBack>>().into_iter().find(|a| a.id == id).map(|a| a.result);
    assert_eq!(answer, Some(Ok(EchoBack { text })));
}

#[test]
#[ignore = "live: needs BNB_TEST_WSS_URL"]
fn live_wss_handshake_credentials() {
    let Some((mut app, url)) = live() else { return };
    let secure = format!("{}/secure?tick_ms=0", url.split('?').next().unwrap_or(&url));
    app.world().resource::<WsClient>().connect("anon", WsSettings::new(secure.clone()));
    run_until(&mut app, |app| state(app, "anon") == Some(WsState::Disconnected));
    let error = app.world().resource::<WsConnections>().get("anon").and_then(|c| c.last_error.clone());
    assert_eq!(error.as_ref().and_then(BackendError::status), Some(StatusCode::UNAUTHORIZED));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(mock_ws_server::TOKEN));
    app.world().resource::<WsClient>().connect("authed", WsSettings::new(secure));
    run_until(&mut app, |app| state(app, "authed") == Some(WsState::Connected));
}
