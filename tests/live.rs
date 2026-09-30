//! Live HTTPS checks against a real server: the mock API (`examples/mock_server.rs`) run behind a
//! TLS-terminating reverse proxy on a test machine. Every test is `#[ignore]`d and does nothing
//! unless `BNB_TEST_HTTPS_URL` is set to that server's base URL (e.g. `https://test.example.com`);
//! no address is ever written into this repository. The proxy must not compress or re-encode
//! responses (turn its response compression off).
//!
//! ```text
//! BNB_TEST_HTTPS_URL=https://<test host> cargo test --test live -- --ignored
//! ```
//!
//! Bounded: every request has a 10 s timeout and every wait a frame limit. Only fake credentials
//! from the mock's contract are sent.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::*;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[path = "../examples/mock_server.rs"]
mod mock_server;

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct Character {
    id: u32,
    name: String,
    level: u32,
}

#[derive(Serialize)]
struct Login<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize, Clone, Debug)]
struct Token {
    token: String,
}

#[derive(Deserialize, Clone, Debug)]
struct SaveAck {
    id: u64,
}

/// The live app, or `None` (test skipped) without `BNB_TEST_HTTPS_URL`.
fn live_app() -> Option<TestApp> {
    let Ok(url) = std::env::var("BNB_TEST_HTTPS_URL") else {
        eprintln!("BNB_TEST_HTTPS_URL is not set: skipped");
        return None;
    };
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::new(HttpConfig::new(url).with_timeout(Duration::from_secs(10))))
        .add_json_response::<Character>()
        .add_json_response::<Token>()
        .add_json_response::<SaveAck>();
    app.watch::<HttpResponse>().watch::<JsonResponse<Character>>().watch::<JsonResponse<Token>>().watch::<JsonResponse<SaveAck>>();
    Some(app)
}

fn client(app: &TestApp) -> &HttpClient {
    app.world().resource::<HttpClient>()
}

fn wait_json<T: Clone + Send + Sync + 'static>(app: &mut TestApp, id: RequestId) -> Result<T, BackendError> {
    for _ in 0..8000 {
        app.step();
        if let Some(answer) = app.messages::<JsonResponse<T>>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

fn wait_raw(app: &mut TestApp, id: RequestId) -> Result<RawResponse, BackendError> {
    for _ in 0..8000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

#[test]
#[ignore = "live: needs BNB_TEST_HTTPS_URL"]
fn live_get_json_over_https() {
    let Some(mut app) = live_app() else { return };
    let id = client(&app).get_json::<Character>("/characters/1");
    assert_eq!(wait_json::<Character>(&mut app, id), Ok(Character { id: 1, name: "Ayla".into(), level: 7 }));
}

#[test]
#[ignore = "live: needs BNB_TEST_HTTPS_URL"]
fn live_status_errors_keep_the_body() {
    let Some(mut app) = live_app() else { return };
    let id = client(&app).get("/characters/9");
    let err = wait_raw(&mut app, id).err();
    assert_eq!(err.as_ref().and_then(BackendError::status), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.as_ref().and_then(BackendError::response).map(RawResponse::text).as_deref(), Some(r#"{"message":"character not found"}"#));
}

#[test]
#[ignore = "live: needs BNB_TEST_HTTPS_URL"]
fn live_login_then_authenticated_post() {
    let Some(mut app) = live_app() else { return };
    let request = OutgoingRequest::post("/login").with_json(&Login { username: "live-test", password: mock_server::PASSWORD }).without_credentials();
    let login = client(&app).send_json::<Token>(request);
    let token = wait_json::<Token>(&mut app, login).unwrap_or_else(|e| panic!("{e}"));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(token.token));
    let save = client(&app).post_json::<SaveAck>("/saves", &serde_json::json!({"slot": 1}));
    assert_eq!(wait_json::<SaveAck>(&mut app, save).map(|a| a.id), Ok(42));
}
