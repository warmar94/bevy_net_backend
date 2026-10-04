//! Downloads streamed to a file: the real `UreqTransport` against a small server this test starts
//! on 127.0.0.1 (never another host), and the `FakeHttpTransport`. Bounded: every wait has a frame
//! limit, every request a timeout, and the server stops with the test.
//!
//! What is checked: the file's bytes and SHA-256, progress (with and without `Content-Length`),
//! a file larger than the in-memory answer limit, the expected SHA-256 / size, the size limit,
//! answers outside 200-299, a body cut short, cancel, timeout and app exit during a transfer, a
//! folder that does not exist (never sent), and that no part file is ever left behind.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::*;

/// The byte at `i` of every served file (a pattern that is not periodic in 64 KiB pieces).
fn byte(i: u64) -> u8 {
    u8::try_from((i.wrapping_mul(2_654_435_761) >> 13) & 0xff).unwrap_or(0)
}

fn content(n: u64) -> Vec<u8> {
    (0..n).map(byte).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The download server: `/file?n=N` (with `Content-Length`), `/chunked?n=N` (chunked),
/// `/slow?n=N&ms=M` (64 KiB pieces with a pause, `Content-Length`), `/short` (announces more than it
/// sends, then closes), `/missing` (404 with a JSON body). Counts requests.
struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("bind: {e}"));
        listener.set_nonblocking(true).unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let (stop2, hits2) = (Arc::clone(&stop), Arc::clone(&hits));
        thread::Builder::new()
            .name("mock-download".into())
            .spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            hits2.fetch_add(1, Ordering::SeqCst);
                            let stop3 = Arc::clone(&stop2);
                            let _ = thread::Builder::new().name("mock-download-conn".into()).spawn(move || serve(stream, &stop3));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
            .unwrap_or_else(|e| panic!("{e}"));
        Self { port, stop, hits }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(mut stream: TcpStream, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(buf.get(..n).unwrap_or_default()),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let target = head.split(' ').nth(1).unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let number = |name: &str| query.split('&').find_map(|p| p.strip_prefix(&format!("{name}="))).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let n = number("n");
    match path {
        "/file" => {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&content(n));
        }
        "/chunked" => {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            for piece in content(n).chunks(10_000) {
                let _ = write!(stream, "{:x}\r\n", piece.len());
                let _ = stream.write_all(piece);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        "/slow" => {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n");
            for piece in content(n).chunks(64 * 1024) {
                if stop.load(Ordering::SeqCst) || stream.write_all(piece).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(number("ms")));
            }
        }
        "/short" => {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&content(5000));
        }
        _ => {
            let body = br#"{"message":"no such file"}"#;
            let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(body);
        }
    }
    let _ = stream.flush();
}

/// A fresh folder under the test target dir.
fn folder(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("download-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).map(|it| it.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

/// A strict app on the real transport (1 ms simulated per frame, 2 ms real pause).
fn app(server: &Server) -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::new(HttpConfig::new(server.url()).with_timeout(Duration::from_secs(10))));
    app.watch::<HttpDownloadResponse>().watch::<HttpDownloadProgress>().watch::<HttpResponse>();
    app
}

fn client(app: &TestApp) -> &HttpClient {
    app.world().resource::<HttpClient>()
}

fn wait(app: &mut TestApp, id: RequestId) -> Result<DownloadedFile, BackendError> {
    for _ in 0..5000 {
        app.step();
        if let Some(answer) = app.all_messages::<HttpDownloadResponse>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

fn progress(app: &TestApp, id: RequestId) -> Vec<(u64, Option<u64>)> {
    app.all_messages::<HttpDownloadProgress>().into_iter().filter(|p| p.id == id).map(|p| (p.received, p.total)).collect()
}

/// Wait (real time, bounded) until `dir` holds exactly `expected`.
fn settles(dir: &Path, expected: &[&str]) {
    for _ in 0..500 {
        if names(dir) == expected {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(names(dir), expected);
}

#[test]
fn a_file_larger_than_the_answer_limit_is_streamed_with_progress() {
    let server = Server::start();
    let mut app = app(&server);
    let dir = folder("large");
    // 12 MiB: more than the 10 MiB answer limit that applies to answers kept in memory.
    let n: u64 = 12 * 1024 * 1024;
    let expected = content(n);
    let sha = sha256_hex(&expected);
    let id = client(&app)
        .download(OutgoingRequest::get("/file").with_query("n", n.to_string()), HttpDownload::to(dir.join("big.bin")).with_sha256(&sha).with_size(n));
    let file = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((file.bytes, file.sha256.as_str(), file.status), (n, sha.as_str(), StatusCode::OK));
    assert_eq!(file.path, dir.join("big.bin"));
    assert_eq!(file.headers.get("content-type").and_then(|v| v.to_str().ok()), Some("application/octet-stream"));
    assert!(std::fs::read(dir.join("big.bin")).ok() == Some(expected), "the file holds the served bytes");
    let reports = progress(&app, id);
    assert!(!reports.is_empty() && reports.iter().all(|(_, total)| *total == Some(n)), "{reports:?}");
    assert_eq!(reports.last(), Some(&(n, Some(n))), "the last report is the whole file");
    assert!(reports.windows(2).all(|w| w.first().map(|a| a.0) <= w.get(1).map(|b| b.0)), "progress only grows");
    assert_eq!(names(&dir), ["big.bin"], "no part file is left");
    assert!(app.all_messages::<HttpResponse>().is_empty(), "a download answers on HttpDownloadResponse only");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chunked_answers_have_no_total_and_progress_can_be_turned_off() {
    let server = Server::start();
    let mut app = app(&server);
    let dir = folder("chunked");
    let id = client(&app).download(OutgoingRequest::get("/chunked").with_query("n", "300000"), HttpDownload::to(dir.join("c.bin")));
    let file = wait(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((file.bytes, file.sha256.clone()), (300_000, sha256_hex(&content(300_000))));
    assert_eq!(progress(&app, id).last(), Some(&(300_000, None)));
    let quiet = client(&app).download(OutgoingRequest::get("/file").with_query("n", "300000"), HttpDownload::to(dir.join("q.bin")).with_progress(false));
    assert!(wait(&mut app, quiet).is_ok());
    assert!(progress(&app, quiet).is_empty());
    // The short form: GET into a file; an existing file is replaced.
    let again = client(&app).download_to("/file?", dir.join("c.bin"));
    assert!(matches!(wait(&mut app, again), Err(BackendError::InvalidRequest(_))), "the path rules of any request apply");
    let again = client(&app).download_to("/file", dir.join("c.bin"));
    assert_eq!(wait(&mut app, again).map(|f| f.bytes), Ok(0), "n missing: an empty file");
    assert_eq!(std::fs::metadata(dir.join("c.bin")).map(|m| m.len()).ok(), Some(0));
    assert_eq!(names(&dir), ["c.bin", "q.bin"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checks_refuse_a_file_and_keep_the_old_one() {
    let server = Server::start();
    let mut app = app(&server);
    let dir = folder("checks");
    std::fs::write(dir.join("keep.bin"), b"old").unwrap_or_else(|e| panic!("{e}"));
    let get = |n: u64| OutgoingRequest::get("/file").with_query("n", n.to_string());
    let target = || HttpDownload::to(dir.join("keep.bin"));
    let cases: Vec<(OutgoingRequest, HttpDownload)> = vec![
        (get(100_000), target().with_sha256("0".repeat(64))),
        (get(100_000), target().with_size(99_999)),
        (get(100_000), target().with_max_bytes(50_000)),
        (OutgoingRequest::get("/chunked").with_query("n", "100000"), target().with_max_bytes(50_000)),
        (OutgoingRequest::get("/chunked").with_query("n", "100000"), target().with_size(5)),
        (OutgoingRequest::get("/short"), target()),
        (OutgoingRequest::get("/missing"), target()),
    ];
    let mut errors = Vec::new();
    for (request, download) in cases {
        let id = client(&app).download(request, download);
        errors.push(wait(&mut app, id).err().unwrap_or_else(|| panic!("{id} succeeded")));
        assert_eq!(std::fs::read(dir.join("keep.bin")).ok().as_deref(), Some(&b"old"[..]), "the existing file is untouched");
        settles(&dir, &["keep.bin"]);
    }
    assert!(matches!(&errors[0], BackendError::Network(why) if why.contains("SHA-256")), "{:?}", errors[0]);
    assert!(matches!(&errors[1], BackendError::Network(why) if why.contains("100000") && why.contains("99999")), "{:?}", errors[1]);
    assert!(matches!(errors[2], BackendError::BodyTooLarge { limit: 50_000, .. }), "{:?}", errors[2]);
    assert!(matches!(errors[3], BackendError::BodyTooLarge { limit: 50_000, .. }), "{:?}", errors[3]);
    assert!(matches!(&errors[4], BackendError::Network(why) if why.contains("expected size")), "{:?}", errors[4]);
    assert!(matches!(&errors[5], BackendError::Network(_)), "a body cut short: {:?}", errors[5]);
    assert_eq!(errors[6].status(), Some(StatusCode::NOT_FOUND));
    assert_eq!(errors[6].response().map(RawResponse::text).as_deref(), Some(r#"{"message":"no such file"}"#), "an error answer keeps its body");
    // Settings that cannot work are refused before anything is sent.
    let hits = server.hits();
    for download in [target().with_sha256("not-hex"), HttpDownload::to(dir.join("no-such-folder").join("x.bin")), target().with_size(10).with_max_bytes(5)] {
        let id = client(&app).download(get(10), download);
        let error = wait(&mut app, id).err().unwrap_or_else(|| panic!("{id} succeeded"));
        assert_eq!(error.was_sent(), Some(false), "{error:?}");
    }
    assert_eq!(server.hits(), hits, "nothing reached the server");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_timeout_and_exit_stop_a_transfer_and_remove_the_part_file() {
    let server = Server::start();
    let dir = folder("stop");
    let slow = || OutgoingRequest::get("/slow").with_query("n", (4 * 1024 * 1024).to_string()).with_query("ms", "40");

    // Cancel after the first progress.
    let mut app = app(&server);
    let id = client(&app).download(slow(), HttpDownload::to(dir.join("a.bin")));
    for _ in 0..3000 {
        app.step();
        if !progress(&app, id).is_empty() {
            break;
        }
    }
    assert!(!progress(&app, id).is_empty(), "the transfer started");
    client(&app).cancel(id);
    assert_eq!(wait(&mut app, id).err(), Some(BackendError::Cancelled));
    settles(&dir, &[]);

    // The request's timeout covers the transfer.
    let id = client(&app).download(slow().with_timeout(Duration::from_millis(400)), HttpDownload::to(dir.join("b.bin")));
    assert!(matches!(wait(&mut app, id), Err(BackendError::Timeout(_))));
    settles(&dir, &[]);

    // App exit during a transfer.
    let id = client(&app).download(slow(), HttpDownload::to(dir.join("c.bin")));
    for _ in 0..3000 {
        app.step();
        if !progress(&app, id).is_empty() {
            break;
        }
    }
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(app.all_messages::<HttpDownloadResponse>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err()), Some(BackendError::Shutdown));
    // No waiting here: the exit itself gave the worker its bounded time to stop, so the part file
    // is already gone when the exit frame ends (as when `main` returns right after it).
    assert_eq!(names(&dir), Vec::<String>::new());

    // A part file an earlier run left (another process number) goes with the next download to
    // that target; this run's own and other files stay.
    let mut app = crate::app(&server);
    let stale = format!("d.bin.{}-4-2.part", std::process::id().wrapping_add(1));
    std::fs::write(dir.join(&stale), b"left over").unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("d.bin.notes"), b"keep").unwrap_or_else(|e| panic!("{e}"));
    let id = client(&app).download_to("/file?n=1000", dir.join("d.bin"));
    assert!(matches!(wait(&mut app, id), Err(BackendError::InvalidRequest(_))), "a query in the path is refused before anything");
    assert_eq!(names(&dir), [stale.clone(), "d.bin.notes".to_string()], "a refused request touches nothing");
    let id = client(&app).download(OutgoingRequest::get("/file").with_query("n", "1000"), HttpDownload::to(dir.join("d.bin")));
    assert_eq!(wait(&mut app, id).map(|f| f.bytes), Ok(1000));
    assert_eq!(names(&dir), ["d.bin", "d.bin.notes"]);

    // A target that is a folder is refused before anything is sent.
    std::fs::create_dir(dir.join("folder")).unwrap_or_else(|e| panic!("{e}"));
    let hits = server.hits();
    let id = client(&app).download_to("/file", dir.join("folder"));
    let error = wait(&mut app, id).err();
    assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("folder")), "{error:?}");
    assert_eq!(server.hits(), hits, "never sent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_fake_transport_writes_downloads_and_other_transports_refuse_them() {
    let dir = folder("fake");
    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/save", Ok(RawResponse::new(StatusCode::OK, content(1000))));
    fake.route(Method::GET, "/gone", Ok(RawResponse::new(StatusCode::GONE, "gone")));
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpDownloadResponse>().watch::<HttpDownloadProgress>();
    let ok = client(&app).download_to("/save", dir.join("save.bin"));
    let gone = client(&app).download_to("/gone", dir.join("gone.bin"));
    let later = client(&app).download_to("/later", dir.join("later.bin"));
    app.step_n(3);
    fake.reply(later, Ok(RawResponse::new(StatusCode::OK, "by hand")));
    app.step_n(2);
    let answers = app.all_messages::<HttpDownloadResponse>();
    let of = |id: RequestId| answers.iter().find(|a| a.id == id).map(|a| a.result.clone());
    assert_eq!(of(ok).and_then(Result::ok).map(|f| (f.bytes, f.sha256)), Some((1000, sha256_hex(&content(1000)))));
    assert_eq!(of(gone).and_then(Result::err).and_then(|e| e.status()), Some(StatusCode::GONE));
    assert_eq!(of(later).and_then(Result::ok).map(|f| f.bytes), Some(7));
    assert_eq!(app.all_messages::<HttpDownloadProgress>().into_iter().rfind(|p| p.id == ok).map(|p| (p.received, p.total)), Some((1000, Some(1000))));
    assert_eq!(names(&dir), ["later.bin", "save.bin"]);
    assert!(fake.requests().iter().all(|(_, r)| r.download.is_some()));

    // A transport that does not write files: refused, never handed over.
    struct Plain;
    impl HttpTransport for Plain {
        fn submit(&mut self, _: RequestId, _: PreparedRequest) {
            panic!("a download was handed to a transport that does not write files");
        }
        fn poll(&mut self) -> Vec<(RequestId, HttpTransportResult)> {
            Vec::new()
        }
    }
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(Plain)).add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpDownloadResponse>();
    let id = client(&app).download_to("/save", dir.join("x.bin"));
    app.step_n(2);
    let error = app.all_messages::<HttpDownloadResponse>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err());
    assert!(matches!(&error, Some(BackendError::InvalidRequest(why)) if why.contains("does not write downloads")), "{error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
