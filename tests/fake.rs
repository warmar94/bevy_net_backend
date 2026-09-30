//! The plugin driven by the `FakeHttpTransport`: every request gets exactly one answer (ok, status,
//! decode, timeout, cancel, exit, no transport, body limit, invalid), answers arrive in the frame
//! they are polled, credentials are applied last. Every app is a strict `TestApp` (ambiguity
//! detection on every main schedule). No network.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, LOCATION};
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::*;

const BASE: &str = "https://api.example.com/v1";

fn ok(body: &str) -> HttpTransportResult {
    Ok(RawResponse::new(StatusCode::OK, body))
}

/// A strict test app with the plugin on a fake transport (inserted first, so the plugin does not
/// install its HTTP transport).
fn app_with(fake: &FakeHttpTransport, config: HttpConfig) -> TestApp {
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(config));
    app.watch::<HttpResponse>();
    app
}

fn app(fake: &FakeHttpTransport) -> TestApp {
    app_with(fake, HttpConfig::new(BASE))
}

fn client(app: &TestApp) -> &HttpClient {
    app.world().resource::<HttpClient>()
}

/// Every raw answer seen since the app was built.
fn answers(app: &TestApp) -> Vec<HttpResponse> {
    app.all_messages::<HttpResponse>()
}

fn answers_for(app: &TestApp, id: RequestId) -> Vec<HttpResponse> {
    answers(app).into_iter().filter(|a| a.id == id).collect()
}

#[test]
fn raw_ok_is_answered_once_in_the_frame_it_arrives() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/ping", ok("pong"));
    let mut app = app(&fake);

    /// Records the frame (1-based) in which `Update` read each answer.
    #[derive(Resource, Default)]
    struct ReadIn(Vec<u32>);
    app.init_resource::<ReadIn>().add_systems(Update, |mut frame: Local<u32>, mut reader: MessageReader<HttpResponse>, mut seen: ResMut<ReadIn>| {
        *frame += 1;
        for _ in reader.read() {
            seen.0.push(*frame);
        }
    });

    let id = client(&app).get("/ping");
    app.step(); // frame 1: sent in PostUpdate
    assert_eq!(fake.requests().len(), 1);
    assert!(app.world().resource::<InFlight>().contains(id));
    app.assert_none::<HttpResponse>();
    app.step(); // frame 2: polled in First, read in Update
    let answer = app.assert_exactly_one::<HttpResponse>();
    assert_eq!(answer.id, id);
    assert_eq!(answer.result.ok().map(|r| r.body), Some(b"pong".to_vec()));
    assert_eq!(app.world().resource::<ReadIn>().0, vec![2]);
    assert!(app.world().resource::<InFlight>().is_empty());
    app.step_n(3);
    assert_eq!(answers(&app).len(), 1);
}

#[test]
fn the_url_is_base_plus_path_plus_query() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    client(&app).send(OutgoingRequest::get("/items").with_query("page", "2"));
    app.step();
    let (_, request) = fake.last_request().unwrap_or_else(|| panic!("nothing submitted"));
    assert_eq!(request.uri.to_string(), "https://api.example.com/v1/items?page=2");
    assert_eq!(request.path(), "/v1/items");
    assert!(request.is_https());
}

#[test]
fn non_2xx_is_a_status_error_with_status_headers_and_body() {
    let fake = FakeHttpTransport::new();
    let body = r#"{"message":"The given data was invalid.","errors":{"name":["required"]}}"#;
    let response = RawResponse::new(StatusCode::UNPROCESSABLE_ENTITY, body).with_header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    fake.route(Method::POST, "/v1/players", Ok(response));
    fake.route(Method::GET, "/v1/old", Ok(RawResponse::new(StatusCode::FOUND, "").with_header(LOCATION, HeaderValue::from_static("/v1/new"))));
    let mut app = app(&fake);
    let a = client(&app).request(Method::POST, "/players", Some(b"{}".to_vec()));
    let b = client(&app).get("/old");
    app.step_n(2);
    let answer = answers_for(&app, a).pop().map(|a| a.result);
    let Some(Err(BackendError::Status(response))) = answer else { panic!("expected a status error, got {answer:?}") };
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.headers.get(CONTENT_TYPE), Some(&HeaderValue::from_static("application/json")));
    assert_eq!(response.text(), body);
    #[cfg(feature = "json")]
    {
        #[derive(serde::Deserialize)]
        struct Invalid {
            message: String,
            errors: std::collections::BTreeMap<String, Vec<String>>,
        }
        let invalid = response.json::<Invalid>().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(invalid.message, "The given data was invalid.");
        assert_eq!(invalid.errors.get("name"), Some(&vec!["required".to_string()]));
    }
    let redirect = answers_for(&app, b).pop().map(|a| a.result);
    assert!(matches!(&redirect, Some(Err(e)) if e.status() == Some(StatusCode::FOUND) && e.response().is_some_and(|r| r.headers.contains_key(LOCATION))));
}

#[test]
fn transport_errors_pass_through_unchanged() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/net", Err(BackendError::Network("io: connection refused".into())));
    fake.route(Method::GET, "/v1/tls", Err(BackendError::Tls("rustls: bad certificate".into())));
    let mut app = app(&fake);
    let a = client(&app).get("/net");
    let b = client(&app).get("/tls");
    app.step_n(2);
    assert_eq!(answers_for(&app, a).pop().map(|a| a.result.err()), Some(Some(BackendError::Network("io: connection refused".into()))));
    assert_eq!(answers_for(&app, b).pop().map(|a| a.result.err()), Some(Some(BackendError::Tls("rustls: bad certificate".into()))));
}

#[test]
fn timeout_backstop_answers_once_and_a_late_result_is_discarded() {
    let fake = FakeHttpTransport::new();
    let mut app = app_with(&fake, HttpConfig::new(BASE).with_timeout(Duration::from_millis(100)));
    let id = client(&app).get("/never");
    app.step();
    assert_eq!(fake.waiting(), vec![id]);
    // timeout 100 ms + DEADLINE_GRACE 5 s, at 1/64 s per frame.
    let frames = app.run_until(|world| world.resource::<InFlight>().is_empty(), 1000);
    assert!(frames > 300, "answered after {frames} frames");
    let got = answers_for(&app, id);
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0].result, Err(BackendError::Timeout(_))), "{:?}", got[0].result);
    assert_eq!(fake.cancelled(), vec![id]);
    fake.reply(id, ok("too late"));
    app.step_n(3);
    assert_eq!(answers_for(&app, id).len(), 1);
}

#[test]
fn cancel_answers_cancelled_once_and_a_late_result_is_discarded() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    let id = client(&app).get("/slow");
    app.step();
    client(&app).cancel(id);
    app.step(); // cancel applied in PostUpdate
    assert_eq!(fake.cancelled(), vec![id]);
    assert!(!app.world().resource::<InFlight>().contains(id));
    app.step(); // answered in First
    fake.reply(id, ok("late"));
    app.step_n(3);
    let got = answers_for(&app, id);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].result.as_ref().err(), Some(&BackendError::Cancelled));
}

#[test]
fn cancel_in_the_same_frame_and_after_the_answer() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/a", ok("a"));
    let mut app = app(&fake);
    let a = client(&app).get("/a");
    client(&app).cancel(a);
    app.step_n(3);
    assert_eq!(answers_for(&app, a).iter().map(|x| x.result.as_ref().err().cloned()).collect::<Vec<_>>(), vec![Some(BackendError::Cancelled)]);

    let b = client(&app).get("/a");
    app.step_n(2);
    client(&app).cancel(b); // already answered: nothing happens
    app.step_n(3);
    let got = answers_for(&app, b);
    assert_eq!(got.len(), 1);
    assert!(got[0].result.is_ok());
}

#[test]
fn app_exit_answers_everything_with_shutdown_and_stops_the_transport() {
    /// Set to fire one request in `Last` before `BackendSystems::Exit`: after `Send`, so it is
    /// still queued when the app exits.
    #[derive(Resource, Default)]
    struct FireInLast(Option<RequestId>, bool);
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    app.init_resource::<FireInLast>().add_systems(
        Last,
        (|backend: Res<HttpClient>, mut fire: ResMut<FireInLast>| {
            if fire.1 {
                fire.1 = false;
                fire.0 = Some(backend.get("/queued"));
            }
        })
        .before(BackendSystems::Exit),
    );
    let sent = client(&app).get("/never");
    app.step();
    let same_frame = client(&app).get("/same-frame");
    app.world_mut().resource_mut::<FireInLast>().1 = true;
    app.world_mut().write_message(AppExit::Success);
    app.step();
    let queued = app.world().resource::<FireInLast>().0.unwrap_or_else(|| panic!("not fired"));
    let got = app.messages::<HttpResponse>();
    assert_eq!(got.len(), 3, "{got:?}");
    for id in [sent, same_frame, queued] {
        assert_eq!(answers_for(&app, id).pop().and_then(|a| a.result.err()), Some(BackendError::Shutdown), "{id}");
    }
    assert_eq!(fake.shutdown_count(), 1);
    assert_eq!(fake.requests().len(), 1, "requests made in the exit frame are never sent");
    fake.reply(sent, ok("late"));
    app.step_n(2);
    assert_eq!(answers(&app).len(), 3);
}

#[test]
fn a_request_made_in_the_app_exit_frame_is_answered_shutdown_and_never_sent() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::POST, "/v1/save", ok("{}"));
    let mut app = app(&fake);
    let cancel_me = client(&app).get("/x");
    client(&app).cancel(cancel_me);
    let save = client(&app).send(OutgoingRequest::post("/save").with_body(b"{}".to_vec()));
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(answers_for(&app, save).pop().and_then(|a| a.result.err()), Some(BackendError::Shutdown));
    assert!(answers_for(&app, cancel_me).pop().is_some_and(|a| a.result.is_err()));
    assert!(fake.requests().is_empty(), "a request answered Shutdown was handed to the transport");
}

#[test]
fn a_cancel_in_the_same_frame_means_never_submitted() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    let id = client(&app).get("/a");
    client(&app).cancel(id);
    app.step_n(2);
    assert_eq!(answers_for(&app, id).pop().and_then(|a| a.result.err()), Some(BackendError::Cancelled));
    assert!(fake.requests().is_empty());
}

#[test]
fn no_transport_is_an_answer_too() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    app.world_mut().remove_resource::<HttpTransportRes>();
    let id = client(&app).get("/a");
    app.step_n(2);
    assert_eq!(answers_for(&app, id).pop().and_then(|a| a.result.err()), Some(BackendError::NoTransport));
}

#[test]
fn replacing_the_transport_answers_its_pending_requests() {
    let old = FakeHttpTransport::new();
    let mut app = app(&old);
    let id = client(&app).get("/a");
    app.step();
    let new = FakeHttpTransport::new();
    app.insert_resource(HttpTransportRes::new(new.clone()));
    app.step();
    assert_eq!(answers_for(&app, id).pop().and_then(|a| a.result.err()), Some(BackendError::NoTransport));
    old.reply(id, ok("late"));
    app.step_n(2);
    assert_eq!(answers_for(&app, id).len(), 1);
}

#[test]
fn the_body_limit_holds_for_any_transport() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/big", ok(&"x".repeat(2000)));
    fake.route(Method::GET, "/v1/fits", ok(&"x".repeat(1000)));
    let mut app = app_with(&fake, HttpConfig::new(BASE).with_max_body_bytes(1000));
    let big = client(&app).get("/big");
    let fits = client(&app).get("/fits");
    app.step_n(2);
    assert!(matches!(answers_for(&app, big).pop().and_then(|a| a.result.err()), Some(BackendError::BodyTooLarge { limit: 1000, .. })));
    assert!(answers_for(&app, fits).pop().is_some_and(|a| a.result.is_ok()));
}

#[test]
fn plain_http_is_refused_unless_loopback_or_allowed() {
    let fake = FakeHttpTransport::new();
    let mut app = app_with(&fake, HttpConfig::new("http://192.168.1.20:8000"));
    let refused = client(&app).get("/a");
    app.step_n(2);
    assert!(matches!(answers_for(&app, refused).pop().map(|a| a.result), Some(Err(BackendError::InsecureHttp { .. }))));
    assert!(fake.requests().is_empty(), "never handed to the transport");

    for (config, path) in [
        (HttpConfig::new("http://127.0.0.1:8000"), "/v1"),
        (HttpConfig::new("http://localhost:3000/api"), "/v2"),
        (HttpConfig::new("http://[::1]:9000"), "/v3"),
        (HttpConfig::new("http://192.168.1.20:8000").allow_insecure_http(true), "/v4"),
    ] {
        let fake = FakeHttpTransport::new();
        let mut app = app_with(&fake, config);
        client(&app).get(path);
        app.step();
        assert_eq!(fake.requests().len(), 1, "{path}");
    }
}

#[test]
fn credentials_are_applied_last_and_can_be_skipped_or_cleared() {
    let fake = FakeHttpTransport::new();
    let mut app = app_with(&fake, HttpConfig::new(BASE).with_header("Authorization", "config-value"));
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new("fake-token-1"));
    client(&app).send(OutgoingRequest::get("/a").with_header("Authorization", "request-value"));
    client(&app).send(OutgoingRequest::post("/login").without_credentials());
    app.step();
    app.world_mut().resource_mut::<BackendCredentials>().set(ApiKeyQuery::new("key", "fake-key-2"));
    client(&app).get("/b");
    app.step();
    app.world_mut().resource_mut::<BackendCredentials>().clear();
    client(&app).get("/c");
    app.step();
    let requests = fake.requests();
    let auth = |i: usize| requests[i].1.headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string);
    assert_eq!(auth(0).as_deref(), Some("Bearer fake-token-1"));
    assert_eq!(auth(1).as_deref(), Some("config-value"));
    assert_eq!(requests[2].1.uri.query(), Some("key=fake-key-2"));
    assert_eq!(auth(3).as_deref(), Some("config-value"));
    assert_eq!(requests[3].1.uri.query(), None);
}

#[test]
fn invalid_requests_are_answered_and_never_sent() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    let bad_header = client(&app).send(OutgoingRequest::get("/a").with_header("X-A", "line\nbreak"));
    let bad_path = client(&app).get("no-slash");
    let absolute = client(&app).get("https://elsewhere.example.com/");
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new("bad\ntoken"));
    let bad_token = client(&app).get("/a");
    app.step_n(2);
    for id in [bad_header, bad_path, absolute, bad_token] {
        let got = answers_for(&app, id);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0].result, Err(BackendError::InvalidRequest(_))), "{:?}", got[0].result);
    }
    assert!(fake.requests().is_empty());
}

#[test]
fn a_missing_base_url_fails_each_request_until_set() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/a", ok("a"));
    let mut app = app_with(&fake, HttpConfig::default());
    let before = client(&app).get("/a");
    app.step_n(2);
    assert!(matches!(answers_for(&app, before).pop().map(|a| a.result), Some(Err(BackendError::InvalidRequest(_)))));
    app.world_mut().resource_mut::<HttpConfig>().set_base_url("https://api.example.com");
    let after = client(&app).get("/a");
    app.step_n(2);
    assert!(answers_for(&app, after).pop().is_some_and(|a| a.result.is_ok()));
}

#[test]
fn many_requests_each_get_exactly_one_answer() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/ok", ok("1"));
    fake.route(Method::GET, "/v1/err", Err(BackendError::Network("reset".into())));
    let mut app = app_with(&fake, HttpConfig::new(BASE).with_timeout(Duration::from_millis(10)));
    let mut ids = Vec::new();
    for i in 0..120 {
        let path = ["/ok", "/err", "/never", "/manual"][i % 4];
        let id = client(&app).get(path);
        if i % 5 == 0 {
            client(&app).cancel(id);
        }
        ids.push(id);
        if i % 7 == 0 {
            app.step();
        }
    }
    app.step();
    for id in fake.waiting().into_iter().step_by(2) {
        fake.reply(id, ok("manual"));
        fake.reply(id, ok("duplicate"));
    }
    app.run_until(|world| world.resource::<InFlight>().is_empty(), 1000);
    app.step_n(3);
    for id in &ids {
        assert_eq!(answers_for(&app, *id).len(), 1, "{id}");
    }
    assert_eq!(answers(&app).len(), ids.len());
}

#[test]
fn requests_from_many_systems_need_no_ordering() {
    // Shared access only: these unordered systems (in several schedules) pass the strict check.
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/x", ok("x"));
    let mut app = app(&fake);
    let fire = |backend: Res<HttpClient>| {
        backend.get("/x");
    };
    app.add_systems(PreUpdate, fire).add_systems(Update, (fire, fire)).add_systems(PostUpdate, fire).add_systems(Last, fire);
    app.step_n(4);
    assert!(answers(&app).len() >= 10);
    // (A system in `First` must be ordered against Bevy's exclusive `message_update_system`, like any
    // system there; that is Bevy's rule, not this crate's.)
}

#[test]
fn unsupported_methods_and_a_head_body_are_never_sent() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    let extension = Method::from_bytes(b"PURGE").unwrap_or_else(|e| panic!("{e}"));
    let ids = [
        client(&app).send(OutgoingRequest::new(extension, "/cache")),
        client(&app).send(OutgoingRequest::new(Method::CONNECT, "/tunnel")),
        client(&app).send(OutgoingRequest::new(Method::HEAD, "/x").with_body(b"no".to_vec())),
    ];
    app.step_n(2);
    for id in ids {
        assert!(matches!(answers_for(&app, id).pop().map(|a| a.result), Some(Err(BackendError::InvalidRequest(_)))), "{id}");
    }
    assert!(fake.requests().is_empty());
}

#[test]
fn config_changes_at_runtime_apply_to_the_next_request() {
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/v1/a", ok(&"x".repeat(500)));
    let mut app = app(&fake);
    let before = client(&app).get("/a");
    app.step_n(2);
    app.world_mut().resource_mut::<HttpConfig>().set_timeout(Duration::from_secs(2));
    let config = app.world().resource::<HttpConfig>().clone().with_max_body_bytes(100);
    app.insert_resource(config);
    let after = client(&app).get("/a");
    app.step_n(2);
    assert!(answers_for(&app, before).pop().is_some_and(|a| a.result.is_ok()));
    assert!(matches!(answers_for(&app, after).pop().and_then(|a| a.result.err()), Some(BackendError::BodyTooLarge { limit: 100, .. })));
    assert_eq!(fake.last_request().map(|(_, r)| r.timeout), Some(Duration::from_secs(2)));
}

#[test]
fn in_flight_describes_requests_without_the_query() {
    let fake = FakeHttpTransport::new();
    let mut app = app(&fake);
    let id = client(&app).send(OutgoingRequest::put("/profile").with_query("token", "fake-q"));
    app.step();
    let info = app.world().resource::<InFlight>().describe(id).cloned().unwrap_or_else(|| panic!("not in flight"));
    assert_eq!((info.kind, info.method, info.target.as_str()), (RequestKind::Http, Some(Method::PUT), "/profile"));
}

#[cfg(feature = "json")]
mod json {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
    struct Profile {
        name: String,
        level: u32,
    }

    fn json_app(fake: &FakeHttpTransport) -> TestApp {
        let mut app = app(fake);
        app.add_json_response::<Profile>().add_json_response::<()>();
        app.watch::<JsonResponse<Profile>>().watch::<JsonResponse<()>>();
        app
    }

    #[test]
    fn get_json_decodes_and_asks_for_json() {
        let fake = FakeHttpTransport::new();
        fake.route(Method::GET, "/v1/me", ok(r#"{"name":"Ayla","level":7,"extra":true}"#));
        let mut app = json_app(&fake);
        let id = client(&app).get_json::<Profile>("/me");
        app.step_n(2);
        let answer = app.assert_exactly_one::<JsonResponse<Profile>>();
        assert_eq!(answer.id, id);
        assert_eq!(answer.result, Ok(Profile { name: "Ayla".into(), level: 7 }));
        app.assert_none::<HttpResponse>();
        let (_, request) = fake.last_request().unwrap_or_else(|| panic!("nothing submitted"));
        assert_eq!(request.headers.get("accept"), Some(&HeaderValue::from_static("application/json")));
    }

    #[test]
    fn json_answers_are_read_in_update_of_the_frame_they_arrive() {
        /// The (1-based) frames in which `Update` read a `JsonResponse<Profile>`.
        #[derive(Resource, Default)]
        struct ReadIn(Vec<u32>);
        let fake = FakeHttpTransport::new();
        fake.route(Method::GET, "/v1/me", ok(r#"{"name":"Ayla","level":7}"#));
        let mut app = json_app(&fake);
        app.init_resource::<ReadIn>().add_systems(
            Update,
            |mut frame: Local<u32>, mut reader: MessageReader<JsonResponse<Profile>>, mut seen: ResMut<ReadIn>| {
                *frame += 1;
                for _ in reader.read() {
                    seen.0.push(*frame);
                }
            },
        );
        client(&app).get_json::<Profile>("/me");
        app.step_n(4);
        assert_eq!(app.world().resource::<ReadIn>().0, vec![2]);
    }

    #[test]
    fn post_json_sends_the_body_as_json() {
        let fake = FakeHttpTransport::new();
        fake.route(Method::POST, "/v1/profiles", Ok(RawResponse::new(StatusCode::CREATED, r#"{"name":"Bo","level":1}"#)));
        let mut app = json_app(&fake);
        client(&app).post_json::<Profile>("/profiles", &Profile { name: "Bo".into(), level: 1 });
        app.step_n(2);
        assert!(app.assert_exactly_one::<JsonResponse<Profile>>().result.is_ok());
        let (_, request) = fake.last_request().unwrap_or_else(|| panic!("nothing submitted"));
        assert_eq!(request.headers.get(CONTENT_TYPE), Some(&HeaderValue::from_static("application/json")));
        assert_eq!(request.body.as_deref(), Some(br#"{"name":"Bo","level":1}"#.as_slice()));
    }

    #[test]
    fn a_body_that_is_not_the_type_is_a_decode_error_with_the_response() {
        let fake = FakeHttpTransport::new();
        fake.route(Method::GET, "/v1/me", ok(r#"{"name":"Ayla"}"#));
        let mut app = json_app(&fake);
        client(&app).get_json::<Profile>("/me");
        app.step_n(2);
        let answer = app.assert_exactly_one::<JsonResponse<Profile>>();
        let Err(BackendError::Decode { message, response, .. }) = answer.result else { panic!("expected a decode error") };
        assert!(message.contains("level"), "{message}");
        assert_eq!(response.status, StatusCode::OK);
    }

    #[test]
    fn errors_of_json_requests_arrive_on_their_json_channel() {
        let fake = FakeHttpTransport::new();
        fake.route(Method::GET, "/v1/me", Ok(RawResponse::new(StatusCode::UNAUTHORIZED, r#"{"message":"Unauthenticated."}"#)));
        let mut app = json_app(&fake);
        client(&app).get_json::<Profile>("/me");
        let cancelled = client(&app).get_json::<Profile>("/never");
        client(&app).cancel(cancelled);
        app.step_n(3);
        let got = app.all_messages::<JsonResponse<Profile>>();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|a| a.result.as_ref().err().and_then(BackendError::status) == Some(StatusCode::UNAUTHORIZED)));
        assert!(got.iter().any(|a| a.id == cancelled && a.result == Err(BackendError::Cancelled)));
        assert!(answers(&app).is_empty());
    }

    #[test]
    fn no_content_decodes_as_unit() {
        let fake = FakeHttpTransport::new();
        fake.route(Method::DELETE, "/v1/saves/3", Ok(RawResponse::new(StatusCode::NO_CONTENT, "")));
        let mut app = json_app(&fake);
        client(&app).send_json::<()>(OutgoingRequest::delete("/saves/3"));
        app.step_n(2);
        assert_eq!(app.assert_exactly_one::<JsonResponse<()>>().result, Ok(()));
    }

    #[test]
    fn an_unregistered_type_is_answered_on_http_response() {
        #[derive(Deserialize, Clone, Debug)]
        struct NotRegistered;
        let fake = FakeHttpTransport::new();
        let mut app = json_app(&fake);
        assert!(!client(&app).is_json_registered::<NotRegistered>());
        let id = client(&app).get_json::<NotRegistered>("/x");
        app.step_n(2);
        let answer = app.assert_exactly_one::<HttpResponse>();
        assert_eq!(answer.id, id);
        assert!(matches!(&answer.result, Err(BackendError::InvalidRequest(why)) if why.contains("add_json_response")));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn an_unserializable_body_is_an_encode_error() {
        let fake = FakeHttpTransport::new();
        let mut app = json_app(&fake);
        let mut map = std::collections::HashMap::new();
        map.insert((1u8, 2u8), 3u8); // JSON object keys must be strings
        client(&app).post_json::<Profile>("/x", &map);
        app.step_n(2);
        assert!(matches!(app.assert_exactly_one::<JsonResponse<Profile>>().result, Err(BackendError::Encode(_))));
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn json_body_field_credential() {
        let fake = FakeHttpTransport::new();
        let mut app = json_app(&fake);
        app.world_mut().resource_mut::<BackendCredentials>().set(JsonBodyField::new("token", "fake-token-3"));
        client(&app).post_json::<Profile>("/scores", &serde_json::json!({"score": 99}));
        client(&app).get_json::<Profile>("/me");
        app.step();
        let requests = fake.requests();
        let body: serde_json::Value = serde_json::from_slice(requests[0].1.body.as_deref().unwrap_or_default()).unwrap_or_default();
        assert_eq!(body, serde_json::json!({"score": 99, "token": "fake-token-3"}));
        assert_eq!(requests[1].1.body, None);
    }
}
