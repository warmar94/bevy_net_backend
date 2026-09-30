//! The real `UreqTransport` against the mock server from `examples/mock_server.rs`, started on
//! 127.0.0.1 inside this process (never another host). Bounded: every wait has a frame limit and
//! every request a timeout.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::header::LOCATION;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::*;

#[allow(dead_code)]
#[path = "../examples/mock_server.rs"]
mod mock_server;

use mock_server::MockServer;

/// A strict app on the real transport: 1 ms of simulated time per frame and a 2 ms real pause,
/// so the plugin's own deadline (simulated time) never beats ureq's timeout (real time).
fn app(config: HttpConfig) -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::new(config));
    app.watch::<HttpResponse>();
    app
}

fn mock() -> MockServer {
    MockServer::start().unwrap_or_else(|e| panic!("mock server: {e}"))
}

fn client(app: &TestApp) -> &HttpClient {
    app.world().resource::<HttpClient>()
}

/// Step until `id` is answered (at most ~3000 frames ≈ 6+ s), then return its answer.
fn wait(app: &mut TestApp, id: RequestId) -> Result<RawResponse, BackendError> {
    let mut found = None;
    for _ in 0..3000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            found = Some(answer.result);
            break;
        }
    }
    found.unwrap_or_else(|| panic!("{id} was not answered"))
}

#[test]
fn raw_get_and_what_the_server_saw() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_header("X-Game-Version", "1.2.3"));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(mock_server::TOKEN));
    let id = client(&app).send(OutgoingRequest::get("/echo").with_query("page", "2 & 3"));
    let echo = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}")).text();
    assert!(echo.contains(r#""query":"page=2%20%26%203""#), "{echo}");
    assert!(echo.contains(r#""authorization":"Bearer mock-token-123""#), "{echo}");
    assert!(echo.contains(r#""x-game-version":"1.2.3""#), "{echo}");
    assert!(echo.contains(r#""user-agent":"bevy_net_backend/"#), "{echo}");
}

#[test]
fn statuses_redirects_and_empty_bodies() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let missing = client(&app).get("/characters/9");
    let err = wait(&mut app, missing).err();
    assert_eq!(err.as_ref().and_then(BackendError::status), Some(StatusCode::NOT_FOUND));
    assert_eq!(err.as_ref().and_then(BackendError::response).map(RawResponse::text).as_deref(), Some(r#"{"message":"character not found"}"#));

    let redirect = client(&app).get("/redirect");
    let err = wait(&mut app, redirect).err();
    assert_eq!(err.as_ref().and_then(BackendError::status), Some(StatusCode::FOUND), "redirects are not followed");
    assert!(err.as_ref().and_then(BackendError::response).is_some_and(|r| r.headers.contains_key(LOCATION)));

    let empty = client(&app).get("/empty");
    assert_eq!(wait(&mut app, empty).map(|r| (r.status, r.body.len())), Ok((StatusCode::NO_CONTENT, 0)));
}

#[test]
fn ureq_timeout_is_a_timeout() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_timeout(Duration::from_secs(5)));
    let started = Instant::now();
    let id = client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "3000").with_timeout(Duration::from_millis(300)));
    let result = wait(&mut app, id);
    assert!(matches!(&result, Err(BackendError::Timeout(why)) if why == "global limit"), "{result:?}");
    assert_eq!(result.err().map(|e| e.to_string()).as_deref(), Some("timed out (global limit)"));
    assert!(started.elapsed() < Duration::from_millis(2500), "took {:?}", started.elapsed());
}

#[test]
fn the_body_limit_stops_a_big_body() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_max_body_bytes(1000));
    let big = client(&app).send(OutgoingRequest::get("/big").with_query("bytes", "200000"));
    assert!(matches!(wait(&mut app, big), Err(BackendError::BodyTooLarge { limit: 1000, .. })));
    let fits = client(&app).send(OutgoingRequest::get("/big").with_query("bytes", "1000"));
    assert_eq!(wait(&mut app, fits).map(|r| r.body.len()), Ok(1000));
}

#[test]
fn connection_refused_is_a_network_error() {
    // A port that was free a moment ago: nothing listens there.
    let port = std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(9);
    let mut app = app(HttpConfig::new(format!("http://127.0.0.1:{port}")).with_timeout(Duration::from_secs(3)));
    let id = client(&app).get("/x");
    let result = wait(&mut app, id);
    assert!(matches!(&result, Err(BackendError::Network(_)) | Err(BackendError::Timeout(_))), "{result:?}");
}

#[test]
fn insecure_http_is_refused_before_any_network_use() {
    // `.invalid` never resolves; the request is refused before DNS anyway.
    let mut app = app(HttpConfig::new("http://game-api.invalid"));
    let id = client(&app).get("/x");
    assert!(matches!(wait(&mut app, id), Err(BackendError::InsecureHttp { .. })));
}

#[test]
fn app_exit_does_not_wait_for_a_busy_worker() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_timeout(Duration::from_secs(5)));
    let id = client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "3000"));
    app.step_n(20);
    let started = Instant::now();
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert!(started.elapsed() < Duration::from_millis(500), "exit waited {:?}", started.elapsed());
    let answer = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id).map(|a| a.result);
    assert_eq!(answer.and_then(Result::err), Some(BackendError::Shutdown));
}

#[test]
fn one_worker_runs_requests_one_after_another() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(1));
    let started = Instant::now();
    let a = client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "300"));
    let b = client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "300"));
    assert!(wait(&mut app, a).is_ok());
    assert!(wait(&mut app, b).is_ok());
    assert!(started.elapsed() >= Duration::from_millis(590), "took {:?}", started.elapsed());
}

#[test]
fn gzip_bodies_are_decoded_with_the_feature() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let id = client(&app).get("/gzip");
    let response = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    // With feature `gzip` ureq asks for gzip and decodes the mock's gzip body; without it ureq
    // does not ask, and the mock sends plain JSON. Either way the game sees the same JSON.
    assert_eq!(response.text(), r#"{"compressed":true}"#);
}

/// A gzip body that decodes to far more than the limit: stopped at the limit after decoding
/// (feature `gzip`), so it can never fill memory. Without `gzip` the compressed bytes arrive as
/// they are (a few KiB, under the limit).
#[test]
fn a_gzip_bomb_is_stopped_by_the_body_limit() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_max_body_bytes(64 * 1024));
    // 1 MiB of zeros in about 7 KB of gzip.
    let bomb = client(&app).get("/gzip-bomb");
    let result = wait(&mut app, bomb);
    if cfg!(feature = "gzip") {
        assert!(matches!(result, Err(BackendError::BodyTooLarge { limit: 65_536, .. })), "{result:?}");
        let small = client(&app).send(OutgoingRequest::get("/gzip-bomb").with_query("bytes", "32768"));
        let body = wait(&mut app, small).map(|r| r.body).unwrap_or_else(|e| panic!("{e}"));
        assert!(body.len() == 32_768 && body.iter().all(|b| *b == 0));
    } else {
        let response = result.unwrap_or_else(|e| panic!("{e}"));
        assert!(response.body.len() < 64 * 1024 && response.body.starts_with(&[0x1f, 0x8b]));
    }
}

/// Real time for the mock to see whatever is still going to arrive.
fn settle(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// One worker busy with `/slow`, three `POST /purchase` queued behind it.
fn busy_with_three_purchases(app: &mut TestApp) -> Vec<RequestId> {
    client(app).send(OutgoingRequest::get("/slow").with_query("ms", "600"));
    let ids = (0..3).map(|_| client(app).send(OutgoingRequest::post("/purchase").with_body(b"{}".to_vec()))).collect();
    app.step_n(5);
    ids
}

#[test]
fn nothing_queued_is_sent_after_app_exit() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(1).with_timeout(Duration::from_secs(5)));
    let purchases = busy_with_three_purchases(&mut app);
    // The worker must be busy with `/slow` (not still starting) before the app exits.
    for _ in 0..1000 {
        if server.hits("/slow") == 1 {
            break;
        }
        settle(2);
    }
    app.world_mut().write_message(AppExit::Success);
    app.step();
    for id in purchases {
        assert_eq!(app.all_messages::<HttpResponse>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err()), Some(BackendError::Shutdown));
    }
    settle(1200);
    assert_eq!(server.hits("/slow"), 1);
    assert_eq!(server.hits("/purchase"), 0, "a request answered Shutdown was sent anyway");
}

#[test]
fn nothing_queued_is_sent_after_the_transport_is_replaced_and_no_post_is_duplicated() {
    let server = mock();
    let config = HttpConfig::new(server.url()).with_workers(1).with_timeout(Duration::from_secs(5));
    let mut app = app(config.clone());
    let purchases = busy_with_three_purchases(&mut app);
    app.insert_resource(HttpTransportRes::new(UreqTransport::new(&config)));
    app.step();
    for id in &purchases {
        assert_eq!(app.all_messages::<HttpResponse>().into_iter().find(|a| a.id == *id).and_then(|a| a.result.err()), Some(BackendError::NoTransport));
    }
    // The game retries once on the new transport: the server must see exactly one purchase.
    let retry = client(&app).send(OutgoingRequest::post("/purchase").with_body(b"{}".to_vec()));
    assert!(wait(&mut app, retry).is_ok());
    settle(1200);
    assert_eq!(server.hits("/purchase"), 1, "the old pool sent its queued purchases too");
}

/// "Save on quit" done wrong: the save and `AppExit` in the same frame. It is answered
/// `Shutdown`, and it must then never reach the server (warm pool, idle worker waiting).
#[test]
fn a_request_made_in_the_app_exit_frame_never_reaches_the_server() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(2).with_timeout(Duration::from_secs(5)));
    let warm = client(&app).get("/characters/1");
    assert!(wait(&mut app, warm).is_ok());
    let save = client(&app).send(OutgoingRequest::post("/purchase").with_body(b"{}".to_vec()));
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(app.messages::<HttpResponse>().into_iter().find(|a| a.id == save).and_then(|a| a.result.err()), Some(BackendError::Shutdown));
    settle(500);
    assert_eq!(server.hits("/purchase"), 0);
}

/// The gzip bomb straight at the transport (no plugin re-check behind it): the transport itself
/// must stop at the limit. Before the fix it returned all 16 MiB.
#[cfg(feature = "gzip")]
#[test]
fn the_transport_itself_stops_a_gzip_bomb() {
    let server = mock();
    let config = HttpConfig::new(server.url()).with_max_body_bytes(64 * 1024).with_timeout(Duration::from_secs(10));
    // Capture a real PreparedRequest through the fake.
    let fake = FakeHttpTransport::new();
    let mut capture = TestApp::new();
    capture.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(config.clone()));
    capture.world().resource::<HttpClient>().send(OutgoingRequest::get("/gzip-bomb").with_query("bytes", "16777216"));
    capture.step();
    let (id, prepared) = fake.last_request().unwrap_or_else(|| panic!("nothing captured"));
    let mut transport = UreqTransport::new(&config);
    transport.submit(id, prepared);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let result = loop {
        if let Some((_, result)) = transport.poll().into_iter().find(|(got, _)| *got == id) {
            break result;
        }
        assert!(std::time::Instant::now() < deadline, "no answer from the transport");
        settle(5);
    };
    assert!(matches!(result, Err(BackendError::BodyTooLarge { limit: 65_536, .. })), "{result:?}");
}

#[test]
fn a_request_that_times_out_while_queued_is_never_sent() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(1).with_timeout(Duration::from_secs(5)));
    client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "800"));
    let id = client(&app).send(OutgoingRequest::post("/purchase").with_body(b"{}".to_vec()).with_timeout(Duration::from_millis(300)));
    let result = wait(&mut app, id);
    assert!(matches!(&result, Err(BackendError::Timeout(why)) if why.starts_with("not sent:")), "{result:?}");
    settle(600);
    assert_eq!(server.hits("/purchase"), 0);
}

#[test]
fn a_request_cancelled_while_queued_is_never_sent() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(1).with_timeout(Duration::from_secs(5)));
    client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "500"));
    let id = client(&app).send(OutgoingRequest::post("/purchase").with_body(b"{}".to_vec()));
    app.step_n(3);
    client(&app).cancel(id);
    assert_eq!(wait(&mut app, id).err(), Some(BackendError::Cancelled));
    settle(1000);
    assert_eq!(server.hits("/slow"), 1);
    assert_eq!(server.hits("/purchase"), 0);
}

#[test]
fn the_mock_refuses_oversize_bodies_and_reads_chunked_ones() {
    use std::io::{Read, Write};
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let big = client(&app).send(OutgoingRequest::post("/purchase").with_body(vec![b'x'; 2 * 1024 * 1024]));
    assert_eq!(wait(&mut app, big).err().as_ref().and_then(BackendError::status), Some(StatusCode::PAYLOAD_TOO_LARGE));
    // A chunked body, sent by hand (the client itself always sends Content-Length).
    let addr = server.url().replace("http://", "");
    let mut stream = std::net::TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    let request = format!(
        "POST /saves HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{{\"a\":\r\n3;x=y\r\n12}}\r\n0\r\n\r\n",
        mock_server::TOKEN
    );
    stream.write_all(request.as_bytes()).unwrap_or_else(|e| panic!("{e}"));
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap_or_else(|e| panic!("{e}"));
    assert!(answer.starts_with("HTTP/1.1 201"), "{answer}");
    assert!(answer.ends_with(r#"{"id":42,"stored_bytes":8}"#), "{answer}");
}

#[test]
fn ipv6_loopback_works_when_available() {
    let Ok(server) = MockServer::start_on("[::1]:0") else {
        eprintln!("no IPv6 loopback here: skipped");
        return;
    };
    let mut app = app(HttpConfig::new(server.url()));
    let id = client(&app).get("/characters/1");
    assert_eq!(wait(&mut app, id).map(|r| r.status), Ok(StatusCode::OK));
}

/// TLS to the plain-HTTP mock fails in the handshake: the ring provider is used, nothing panics,
/// and the failure is a TLS error. (127.0.0.1 only; no certificate is ever trusted here.)
#[test]
fn a_tls_handshake_failure_is_a_tls_error() {
    let server = mock();
    let https = server.url().replace("http://", "https://");
    let mut app = app(HttpConfig::new(https).with_timeout(Duration::from_secs(3)));
    let id = client(&app).get("/characters/1");
    let result = wait(&mut app, id);
    assert!(matches!(&result, Err(BackendError::Tls(_))), "{result:?}");
}

#[cfg(feature = "json")]
mod json {
    use super::*;
    use bevy_net_backend::http::Method;
    use serde::{Deserialize, Serialize};

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

    fn json_app(server: &MockServer) -> TestApp {
        let mut app = app(HttpConfig::new(server.url()));
        app.add_json_response::<Character>().add_json_response::<Token>().add_json_response::<SaveAck>();
        app.watch::<JsonResponse<Character>>().watch::<JsonResponse<Token>>().watch::<JsonResponse<SaveAck>>();
        app
    }

    fn wait_json<T: Clone + Send + Sync + 'static>(app: &mut TestApp, id: RequestId) -> Result<T, BackendError> {
        for _ in 0..3000 {
            app.step();
            if let Some(answer) = app.messages::<JsonResponse<T>>().into_iter().find(|a| a.id == id) {
                return answer.result;
            }
        }
        panic!("{id} was not answered")
    }

    #[test]
    fn get_json_from_the_mock() {
        let server = mock();
        let mut app = json_app(&server);
        let id = client(&app).get_json::<Character>("/characters/1");
        assert_eq!(wait_json::<Character>(&mut app, id), Ok(Character { id: 1, name: "Ayla".into(), level: 7 }));
    }

    #[test]
    fn login_then_authenticated_post() {
        let server = mock();
        let mut app = json_app(&server);
        let early = client(&app).post_json::<SaveAck>("/saves", &serde_json::json!({"slot": 1}));
        assert_eq!(wait_json::<SaveAck>(&mut app, early).err().and_then(|e| e.status()), Some(StatusCode::UNAUTHORIZED));

        let request =
            OutgoingRequest::new(Method::POST, "/login").with_json(&Login { username: "demo", password: mock_server::PASSWORD }).without_credentials();
        let login = client(&app).send_json::<Token>(request);
        let token = wait_json::<Token>(&mut app, login).unwrap_or_else(|e| panic!("{e}"));
        app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new(token.token));

        let bad = client(&app).post_json::<SaveAck>("/saves", &[1, 2]);
        let err = wait_json::<SaveAck>(&mut app, bad).err();
        assert_eq!(err.as_ref().and_then(BackendError::status), Some(StatusCode::UNPROCESSABLE_ENTITY));
        let details = err.as_ref().and_then(BackendError::response).and_then(|r| r.json::<serde_json::Value>().ok());
        assert_eq!(details.and_then(|d| d["errors"]["body"][0].as_str().map(str::to_string)).as_deref(), Some("must be a JSON object"));

        let good = client(&app).post_json::<SaveAck>("/saves", &serde_json::json!({"slot": 1, "gold": 10}));
        assert_eq!(wait_json::<SaveAck>(&mut app, good).map(|a| a.id), Ok(42));
    }

    #[test]
    fn wrong_login_is_a_401_with_the_server_message() {
        let server = mock();
        let mut app = json_app(&server);
        let request = OutgoingRequest::post("/login").with_json(&Login { username: "demo", password: "wrong" });
        let id = client(&app).send_json::<Token>(request);
        let err = wait_json::<Token>(&mut app, id).err();
        assert_eq!(err.as_ref().and_then(BackendError::status), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(err.as_ref().and_then(BackendError::response).map(RawResponse::text).as_deref(), Some(r#"{"message":"invalid credentials"}"#));
    }
}
