//! `multipart/form-data` uploads: the real `UreqTransport` against the mock's `POST /upload` (a
//! real multipart parser) on 127.0.0.1 in this process, and the plugin's bookkeeping with the
//! `FakeHttpTransport`: round trips, typed answers, limits (client and server side), cancel,
//! `AppExit`, credentials, and honest `was_sent`.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::header::{AUTHORIZATION, CONTENT_TYPE};
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::*;
use serde::Deserialize;

#[allow(dead_code)]
#[path = "../examples/mock_server.rs"]
mod mock_server;

use mock_server::MockServer;

/// What the mock (and the live echo servers) answer.
#[derive(Deserialize, Clone, Debug, PartialEq)]
struct Echo {
    fields: Vec<Field>,
    files: Vec<FileInfo>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct Field {
    name: String,
    value: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct FileInfo {
    name: String,
    filename: String,
    content_type: Option<String>,
    size: u64,
    crc32: String,
}

fn crc32(data: &[u8]) -> String {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// Binary content that contains everything a naive encoder trips over.
fn tricky_bytes(n: usize) -> Vec<u8> {
    let mut data: Vec<u8> = (0..n).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    for (i, chunk) in [&b"\r\n--"[..], b"--bnb-", b"\r\n\r\n", b"Content-Disposition: form-data; name=\"x\""].iter().enumerate() {
        let at = (i + 1) * n / 6;
        if at + chunk.len() < data.len() {
            data[at..at + chunk.len()].copy_from_slice(chunk);
        }
    }
    data
}

fn app(config: HttpConfig) -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::new(config)).add_json_response::<Echo>();
    app.watch::<HttpResponse>().watch::<JsonResponse<Echo>>();
    app
}

fn mock() -> MockServer {
    MockServer::start().unwrap_or_else(|e| panic!("mock server: {e}"))
}

fn client(app: &TestApp) -> &HttpClient {
    app.world().resource::<HttpClient>()
}

fn wait(app: &mut TestApp, id: RequestId) -> Result<RawResponse, BackendError> {
    for _ in 0..3000 {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

#[test]
fn a_form_round_trips_through_a_real_multipart_parser() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let avatar = tricky_bytes(200_000);
    let form = Multipart::new()
        .text("title", "Héllo, \"multipart\" ✓\r\nsecond line")
        .text("tags[]", "ranger")
        .text("tags[]", "elf")
        .text("empty", "")
        .file("avatar", "my \"avatar\".png", "image/png", avatar.clone())
        .file("notes", "notes.txt", "", Vec::new());
    let id = client(&app).post_multipart("/upload", &form);
    let response = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    let echo: Echo = response.json().unwrap_or_else(|e| panic!("{e}: {}", response.text()));
    let fields: Vec<(&str, &str)> = echo.fields.iter().map(|f| (f.name.as_str(), f.value.as_str())).collect();
    assert_eq!(fields, vec![("title", "Héllo, \"multipart\" ✓\r\nsecond line"), ("tags[]", "ranger"), ("tags[]", "elf"), ("empty", "")]);
    assert_eq!(echo.files.len(), 2);
    let file = &echo.files[0];
    // Quotes in names are percent-encoded as browsers do; the server sees `%22`.
    assert_eq!((file.name.as_str(), file.filename.as_str(), file.content_type.as_deref()), ("avatar", "my %22avatar%22.png", Some("image/png")));
    assert_eq!((file.size, file.crc32.as_str()), (200_000, crc32(&avatar).as_str()));
    let empty = &echo.files[1];
    assert_eq!((empty.size, empty.content_type.as_deref()), (0, Some("application/octet-stream")));
    assert_eq!(server.hits("/upload"), 1);
}

#[test]
fn a_typed_answer_and_other_methods() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let id = client(&app).post_multipart_json::<Echo>("/upload", &Multipart::new().text("a", "1"));
    let mut echo = None;
    for _ in 0..3000 {
        app.step();
        if let Some(answer) = app.messages::<JsonResponse<Echo>>().into_iter().find(|a| a.id == id) {
            echo = Some(answer.result);
            break;
        }
    }
    let echo = echo.unwrap_or_else(|| panic!("no answer")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(echo.fields, vec![Field { name: "a".into(), value: "1".into() }]);
    // PUT reaches the route too (the mock only serves POST: 404 proves the method went out).
    let id = client(&app).send_multipart(Method::PUT, "/upload", &Multipart::new().text("a", "1"));
    assert_eq!(wait(&mut app, id).err().and_then(|e| e.status()), Some(StatusCode::NOT_FOUND));
}

#[test]
fn a_form_over_the_client_limit_is_never_sent() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let form = Multipart::new().with_max_bytes(64 * 1024).file("big", "big.bin", "", vec![7u8; 100_000]);
    let id = client(&app).post_multipart("/upload", &form);
    let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("sent"));
    assert!(matches!(error, BackendError::RequestTooLarge { limit: 65_536, size, .. } if size > 100_000), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    assert!(error.to_string().contains("not sent"));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.hits("/upload"), 0);
}

#[test]
fn a_form_over_the_server_limit_is_a_413_answer() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_timeout(Duration::from_secs(10)));
    let id = client(&app).post_multipart("/upload", &Multipart::new().file("big", "big.bin", "", vec![1u8; 9 * 1024 * 1024]));
    let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("accepted"));
    assert_eq!(error.status(), Some(StatusCode::PAYLOAD_TOO_LARGE), "{error:?}");
    assert_eq!(error.was_sent(), Some(true));
}

#[test]
fn the_mock_refuses_malformed_multipart_with_400() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let cases: [(&str, &[u8]); 4] = [
        ("multipart/form-data", b"--x\r\n\r\n"),
        ("multipart/form-data; boundary=x", b"--x\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nno end"),
        ("multipart/form-data; boundary=x", b"--x\r\nContent-Disposition: form-data\r\n\r\nv\r\n--x--\r\n"),
        ("text/plain", b"hello"),
    ];
    for (content_type, body) in cases {
        let id = client(&app).send(OutgoingRequest::post("/upload").with_header("Content-Type", content_type).with_body(body.to_vec()));
        assert_eq!(wait(&mut app, id).err().and_then(|e| e.status()), Some(StatusCode::BAD_REQUEST), "{content_type}");
    }
    // `filename*` (RFC 5987) wins over `filename`.
    let body = b"--x\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"; filename*=UTF-8''%C3%A9t%C3%A9.txt\r\n\r\nv\r\n--x--\r\n";
    let id = client(&app).send(OutgoingRequest::post("/upload").with_header("Content-Type", "multipart/form-data; boundary=x").with_body(body.to_vec()));
    let echo: Echo = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}")).json().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(echo.files[0].filename, "été.txt");
}

#[test]
fn cancel_and_app_exit_never_send_a_queued_upload() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()).with_workers(1).with_timeout(Duration::from_secs(5)));
    client(&app).send(OutgoingRequest::get("/slow").with_query("ms", "500"));
    app.step();
    let cancelled = client(&app).post_multipart("/upload", &Multipart::new().text("a", "1"));
    app.step();
    client(&app).cancel(cancelled);
    assert_eq!(wait(&mut app, cancelled).err(), Some(BackendError::Cancelled));
    let at_exit = client(&app).post_multipart("/upload", &Multipart::new().text("b", "2"));
    app.world_mut().write_message(AppExit::Success);
    app.step();
    let answer = app.all_messages::<HttpResponse>().into_iter().find(|a| a.id == at_exit).map(|a| a.result);
    assert_eq!(answer.and_then(Result::err), Some(BackendError::Shutdown));
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(server.hits("/upload"), 0, "a cancelled or exit-frame upload reached the server");
}

#[test]
fn credentials_apply_and_json_body_field_refuses_a_form() {
    let fake = FakeHttpTransport::new();
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpResponse>();
    app.world_mut().resource_mut::<BackendCredentials>().set(BearerToken::new("fake-token-1"));
    client(&app).post_multipart("/upload", &Multipart::new().text("a", "1"));
    app.step();
    let (_, prepared) = fake.last_request().unwrap_or_else(|| panic!("not submitted"));
    assert!(prepared.headers.get(AUTHORIZATION).is_some());
    let content_type = prepared.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    assert!(content_type.starts_with("multipart/form-data; boundary=bnb-"), "{content_type}");
    // A JSON body field cannot go into a form: a clear refusal, never sent.
    app.world_mut().resource_mut::<BackendCredentials>().set(JsonBodyField::new("token", "fake-token-2"));
    let id = client(&app).post_multipart("/upload", &Multipart::new().text("a", "1"));
    app.step_n(2);
    let error = app.all_messages::<HttpResponse>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err()).unwrap_or_else(|| panic!("no error"));
    assert!(matches!(&error, BackendError::InvalidRequest(why) if why.contains("JsonBodyField")), "{error:?}");
    assert_eq!(fake.requests().len(), 1);
    // A form later replaced by a JSON body (or raw bytes) is a plain request again: JsonBodyField applies.
    let replaced = OutgoingRequest::post("/save").with_multipart(&Multipart::new().text("a", "1")).with_json(&serde_json::json!({"slot": 1}));
    assert!(!replaced.is_multipart());
    let id = client(&app).send(replaced);
    app.step_n(2);
    assert!(app.all_messages::<HttpResponse>().into_iter().all(|a| a.id != id || a.result.is_ok()), "the JSON request was refused");
    let (_, prepared) = fake.last_request().unwrap_or_else(|| panic!("not submitted"));
    let body: serde_json::Value = serde_json::from_slice(prepared.body.as_deref().unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(body, serde_json::json!({"slot": 1, "token": "fake-token-2"}));
    assert_eq!(prepared.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()), Some("application/json"));
    assert!(!OutgoingRequest::post("/x").with_multipart(&Multipart::new()).with_body(b"raw".to_vec()).is_multipart());
    let mut request = OutgoingRequest::post("/x").with_multipart(&Multipart::new());
    assert!(request.is_multipart());
    request.set_body(None);
    assert!(!request.is_multipart());
}

#[test]
fn backslashes_in_names_are_sent_as_given_and_a_trailing_one_is_refused() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    // A trailing backslash (name or file name) would be misread by real parsers: refused, never sent.
    for form in
        [Multipart::new().file("up", "x\\", "", vec![1u8]), Multipart::new().file("f\\", "a.png", "image/png", vec![1u8]), Multipart::new().text("n\\", "v")]
    {
        let id = client(&app).post_multipart("/upload", &form);
        let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("sent"));
        assert!(matches!(&error, BackendError::InvalidRequest(why) if why.contains("backslash")), "{error:?}");
        assert_eq!(error.was_sent(), Some(false));
    }
    assert_eq!(server.hits("/upload"), 0);
    // Inside a name the backslash goes out unchanged (the mock echoes the wire; frameworks differ).
    let form = Multipart::new().text("a\\b", "v").file("up", "C:\\Users\\me\\a.png", "image/png", vec![1u8, 2, 3]).file("dir", "a\\b.png", "", vec![4u8]);
    let id = client(&app).post_multipart("/upload", &form);
    let echo: Echo = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}")).json().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(echo.fields, vec![Field { name: "a\\b".into(), value: "v".into() }]);
    let names: Vec<&str> = echo.files.iter().map(|f| f.filename.as_str()).collect();
    assert_eq!(names, vec!["C:\\Users\\me\\a.png", "a\\b.png"]);
    assert_eq!(server.hits("/upload"), 1);
}
