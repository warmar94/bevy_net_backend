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

// --- Files from disk, upload progress, typed parts ----------------------------------------------

/// A fresh directory under `target/tmp` for one test's files.
fn scratch(test: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("multipart-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    dir
}

fn progress_of(app: &TestApp, id: RequestId) -> Vec<HttpProgress> {
    app.all_messages::<HttpProgress>().into_iter().filter(|p| p.id == id).collect()
}

/// Progress only grows, keeps one total, and ends at the whole body.
fn check_progress(progress: &[HttpProgress]) -> u64 {
    assert!(!progress.is_empty(), "no progress");
    let total = progress[0].total.unwrap_or_else(|| panic!("no total"));
    assert!(progress.windows(2).all(|w| w[0].sent <= w[1].sent), "{progress:?}");
    assert!(progress.iter().all(|p| p.total == Some(total) && p.sent <= total), "{progress:?}");
    assert_eq!(progress.last().map(|p| p.sent), Some(total), "the last progress is the whole body");
    total
}

#[test]
fn a_form_with_a_file_from_disk_and_a_json_part_arrives_exactly() {
    #[derive(serde::Serialize)]
    struct Meta {
        slot: u32,
        name: &'static str,
    }
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    app.watch::<HttpProgress>();
    let dir = scratch("disk");
    let on_disk = tricky_bytes(3 * 1024 * 1024 + 17);
    let path = dir.join("save.bin");
    std::fs::write(&path, &on_disk).unwrap_or_else(|e| panic!("{e}"));
    let in_memory = tricky_bytes(1000);
    let form = Multipart::new()
        .text("title", "streamed")
        .json("meta", &Meta { slot: 3, name: "Ayla" })
        .part("raw", "text/csv", b"a,b\n1,2\n".to_vec())
        .file_from_path("save", "save.bin", "application/octet-stream", &path)
        .file("thumb", "thumb.png", "image/png", in_memory.clone());
    let request = OutgoingRequest::post("/upload").with_multipart(&form);
    assert!(request.is_multipart() && request.body().is_none() && request.streaming_body().is_some() && request.upload_progress());
    let id = client(&app).send(request);
    let response = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    let echo: serde_json::Value = response.json().unwrap_or_else(|e| panic!("{e}: {}", response.text()));
    let fields = echo["fields"].as_array().cloned().unwrap_or_default();
    assert_eq!(fields.len(), 3, "{echo}");
    assert_eq!((fields[0]["name"].as_str(), fields[0]["value"].as_str(), fields[0].get("content_type")), (Some("title"), Some("streamed"), None));
    assert_eq!(fields[1]["name"].as_str(), Some("meta"));
    assert_eq!(fields[1]["content_type"].as_str(), Some("application/json"));
    let meta: serde_json::Value = serde_json::from_str(fields[1]["value"].as_str().unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(meta, serde_json::json!({"slot": 3, "name": "Ayla"}));
    assert_eq!((fields[2]["content_type"].as_str(), fields[2]["value"].as_str()), (Some("text/csv"), Some("a,b\n1,2\n")));
    let files = echo["files"].as_array().cloned().unwrap_or_default();
    assert_eq!(files[0]["filename"].as_str(), Some("save.bin"));
    assert_eq!(files[0]["size"].as_u64(), Some(on_disk.len() as u64));
    assert_eq!(files[0]["crc32"].as_str(), Some(crc32(&on_disk).as_str()));
    assert_eq!(files[1]["crc32"].as_str(), Some(crc32(&in_memory).as_str()));
    // The progress ends at the whole body, which is more than the file.
    let total = check_progress(&progress_of(&app, id));
    assert!(total > on_disk.len() as u64);
    assert_eq!(server.hits("/upload"), 1);
}

#[test]
fn upload_progress_is_opt_in_except_for_forms() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    app.watch::<HttpProgress>();
    // An in-memory form reports progress; a JSON post does not unless asked.
    let form = client(&app).post_multipart("/upload", &Multipart::new().file("f", "f.bin", "", tricky_bytes(2 * 1024 * 1024)));
    let quiet = client(&app).send(OutgoingRequest::post("/purchase").with_body(vec![b'x'; 300_000]));
    let asked = client(&app).send(OutgoingRequest::post("/purchase").with_body(vec![b'x'; 300_000]).with_upload_progress(true));
    let off = client(&app).send(OutgoingRequest::post("/upload").with_multipart(&Multipart::new().text("a", "1")).with_upload_progress(false));
    for _ in 0..3000 {
        let answered = app.all_messages::<HttpResponse>();
        if [form, quiet, asked, off].iter().all(|id| answered.iter().any(|a| a.id == *id)) {
            break;
        }
        app.step();
    }
    let answered = app.all_messages::<HttpResponse>();
    assert!([form, quiet, asked, off].iter().all(|id| answered.iter().any(|a| a.id == *id && a.result.is_ok())), "{answered:?}");
    check_progress(&progress_of(&app, form));
    assert_eq!(check_progress(&progress_of(&app, asked)), 300_000);
    assert!(progress_of(&app, quiet).is_empty());
    assert!(progress_of(&app, off).is_empty());
}

#[test]
fn a_missing_file_or_a_streamed_form_over_its_limit_is_never_sent() {
    let server = mock();
    let mut app = app(HttpConfig::new(server.url()));
    let dir = scratch("refused");
    let missing = dir.join("missing.bin");
    let _ = std::fs::remove_file(&missing);
    let id = client(&app).post_multipart("/upload", &Multipart::new().text("a", "1").file_from_path("save", "save.bin", "", &missing));
    let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("sent"));
    assert!(matches!(&error, BackendError::InvalidRequest(why) if why.contains("missing.bin") && why.contains("`save`")), "{error:?}");
    assert_eq!(error.was_sent(), Some(false));
    let big = dir.join("big.bin");
    std::fs::write(&big, vec![1u8; 2 * 1024 * 1024]).unwrap_or_else(|e| panic!("{e}"));
    let form = Multipart::new().with_max_bytes(1024 * 1024).file_from_path("big", "big.bin", "", &big);
    let id = client(&app).post_multipart("/upload", &form);
    let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("sent"));
    assert!(matches!(error, BackendError::RequestTooLarge { limit: 1_048_576, size, .. } if size > 2 * 1024 * 1024), "{error:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.hits("/upload"), 0);
}

#[test]
fn bad_typed_parts_are_refused_and_debug_hides_paths() {
    struct NotJson;
    impl serde::Serialize for NotJson {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("not serializable"))
        }
    }
    let request = OutgoingRequest::post("/upload").with_multipart(&Multipart::new().json("meta", &NotJson));
    assert!(matches!(request.error(), Some(BackendError::Encode(_))), "{request:?}");
    let request = OutgoingRequest::post("/upload").with_multipart(&Multipart::new().part("meta", "application/json\r\nX-Evil: 1", b"{}".to_vec()));
    assert!(matches!(request.error(), Some(BackendError::InvalidRequest(_))));
    let debug = format!("{:?}", Multipart::new().file_from_path("save", "secret-name.bin", "", "private-folder/secret.bin"));
    assert!(!debug.contains("secret") && !debug.contains("private"), "{debug}");
    let request = OutgoingRequest::post("/upload").with_multipart(&Multipart::new().file_from_path("save", "a.bin", "", "private-folder/secret.bin"));
    let debug = format!("{request:?}");
    assert!(!debug.contains("secret") && !debug.contains("private"), "{debug}");
}

#[test]
fn the_fake_records_a_streamed_form_and_a_transport_that_cannot_stream_refuses_it() {
    let dir = scratch("fake");
    let path = dir.join("f.bin");
    std::fs::write(&path, b"file bytes").unwrap_or_else(|e| panic!("{e}"));
    let form = Multipart::new().text("a", "1").file_from_path("f", "f.bin", "", &path);

    let fake = FakeHttpTransport::new();
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpResponse>().watch::<HttpProgress>();
    let id = client(&app).post_multipart("/upload", &form);
    app.step();
    let (_, prepared) = fake.last_request().unwrap_or_else(|| panic!("not submitted"));
    assert!(prepared.body.is_none() && prepared.upload_progress);
    let body = prepared.streaming_body.as_ref().map(StreamingBody::read_all).unwrap_or_else(|| panic!("no streaming body")).unwrap_or_else(|e| panic!("{e}"));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("name=\"f\"; filename=\"f.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nfile bytes\r\n--bnb-"), "{text}");
    // Scripted progress reaches the game while the request waits, never after its answer.
    fake.progress(id, 5, Some(10));
    app.step();
    fake.reply(id, Ok(RawResponse::new(StatusCode::OK, "{}")));
    fake.progress(id, 10, Some(10));
    app.step_n(2);
    let progress = progress_of(&app, id);
    assert_eq!(progress.iter().map(|p| p.sent).collect::<Vec<_>>(), vec![5, 10]);
    fake.progress(id, 10, Some(10));
    app.step_n(2);
    assert_eq!(progress_of(&app, id).len(), 2, "no progress after the answer");

    struct BytesOnly;
    impl HttpTransport for BytesOnly {
        fn submit(&mut self, _id: RequestId, _request: PreparedRequest) {
            panic!("a streamed form reached a transport that cannot stream it");
        }
        fn poll(&mut self) -> Vec<(RequestId, HttpTransportResult)> {
            Vec::new()
        }
    }
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(BytesOnly)).add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpResponse>();
    let id = client(&app).post_multipart("/upload", &form);
    app.step_n(2);
    let error = app.all_messages::<HttpResponse>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err());
    assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("streamed")), "{error:?}");
}

/// FNV-1a 64 (a quick content check for big streams).
fn fnv(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}
const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// A one-shot HTTP server on 127.0.0.1 that streams a request body through FNV without keeping
/// it: it checks the single file part's head and tail and answers `{"size":N,"fnv":"hex"}`.
fn streaming_sink() -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let url = format!("http://{}", listener.local_addr().unwrap_or_else(|e| panic!("{e}")));
    let thread = std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else { return };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let mut reader = BufReader::with_capacity(64 * 1024, stream.try_clone().unwrap_or_else(|e| panic!("{e}")));
        let (mut length, mut boundary) = (0u64, String::new());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
            if let Some(at) = line.find("boundary=") {
                boundary = line[at + 9..].trim().to_string();
            }
        }
        let head =
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"blob\"; filename=\"blob.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n");
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut got_head = vec![0u8; head.len()];
        reader.read_exact(&mut got_head).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(got_head, head.as_bytes());
        let mut left = length - head.len() as u64 - tail.len() as u64;
        let (size, mut hash) = (left, FNV_START);
        let mut buf = vec![0u8; 64 * 1024];
        while left > 0 {
            let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(0);
            let n = reader.read(&mut buf[..want]).unwrap_or_else(|e| panic!("{e}"));
            assert!(n > 0, "the body ended early");
            hash = fnv(hash, &buf[..n]);
            left -= n as u64;
        }
        let mut got_tail = vec![0u8; tail.len()];
        reader.read_exact(&mut got_tail).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(got_tail, tail.as_bytes());
        let body = format!(r#"{{"size":{size},"fnv":"{hash:016x}"}}"#);
        let mut stream = stream;
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    });
    (url, thread)
}

/// The heavy one: a 256 MiB file on disk goes out as a streamed form (never loaded into memory:
/// the reader hands out 64 KiB at a time) and arrives exactly; the progress ends at the body size.
#[test]
fn a_256_mib_file_from_disk_streams_and_arrives_exactly() {
    use std::io::Write;
    const SIZE: usize = 256 * 1024 * 1024;
    let dir = scratch("heavy");
    let path = dir.join("blob.bin");
    // Written in 1 MiB pieces, hashed on the way: the test never holds the file either.
    let mut hash = FNV_START;
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap_or_else(|e| panic!("{e}")));
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut piece = vec![0u8; 1024 * 1024];
        for _ in 0..SIZE / piece.len() {
            for byte in piece.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state >> 24) as u8;
            }
            hash = fnv(hash, &piece);
            file.write_all(&piece).unwrap_or_else(|e| panic!("{e}"));
        }
        file.flush().unwrap_or_else(|e| panic!("{e}"));
    }
    let (url, server) = streaming_sink();
    let mut app = app(HttpConfig::new(url).with_timeout(Duration::from_secs(300)));
    app.watch::<HttpProgress>();
    let form = Multipart::new().with_max_bytes(512 * 1024 * 1024).file_from_path("blob", "blob.bin", "", &path);
    let start = std::time::Instant::now();
    let id = client(&app).post_multipart("/upload", &form);
    let response = wait_long(&mut app, id, Duration::from_secs(300)).unwrap_or_else(|e| panic!("{e}"));
    let seconds = start.elapsed().as_secs_f64();
    let echo: serde_json::Value = response.json().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(echo["size"].as_u64(), Some(SIZE as u64));
    assert_eq!(echo["fnv"].as_str(), Some(format!("{hash:016x}").as_str()));
    let total = check_progress(&progress_of(&app, id));
    assert!(total > SIZE as u64);
    assert!(!progress_of(&app, id).is_empty(), "progress while it ran");
    let _ = server.join();
    let _ = std::fs::remove_file(&path);
    eprintln!("256 MiB streamed upload over loopback (debug build): {seconds:.1} s = {:.0} MB/s", SIZE as f64 / 1e6 / seconds);
}

fn wait_long(app: &mut TestApp, id: RequestId, limit: Duration) -> Result<RawResponse, BackendError> {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        app.step();
        if let Some(answer) = app.messages::<HttpResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}
