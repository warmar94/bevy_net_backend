//! A tiny mock game API on 127.0.0.1, std only: the other examples and the tests start it in
//! their own process. Run it on its own to point an example at it from another terminal:
//!
//! ```text
//! cargo run --example mock_server                                         # 127.0.0.1, free port, 60 s
//! cargo run --example mock_server -- --seconds 300                         # stops by itself after 300 s
//! cargo run --example mock_server -- --seconds 1800 --bind 127.0.0.1:8080  # fixed address, behind a TLS proxy
//! ```
//!
//! `--bind` defaults to `127.0.0.1:0` (loopback, a free port); `--seconds` (default 60) is the
//! maximum runtime, after which the server exits by itself. Never bind it to `0.0.0.0`.
//!
//! Behind a TLS-terminating reverse proxy (Caddy, nginx) on a test machine it doubles as a live
//! HTTPS test server for `tests/live.rs`. Test data only: `/echo` sends back every request header.
//! It serves at most 32 connections at once (more get a `503`), gives each connection 15 s in
//! total, caps every generated body at 16 MiB and counts hits only for a few fixed routes, but it
//! is still a test tool: keep the proxy in front of it restricted to testers and to the routes the
//! live tests use.
//!
//! It speaks just enough HTTP/1.1 (one request per connection) and JSON for the examples. It is
//! NOT a server to copy: a real backend is Laravel, Express, Go, … The JSON contract it serves:
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /characters/1` | `200 {"id":1,"name":"Ayla","class":"ranger","level":7}` |
//! | `GET /characters/<other>` | `404 {"message":"character not found"}` |
//! | `POST /login` with `{"username":"…","password":"correct-horse"}` | `200 {"token":"mock-token-123"}` |
//! | `POST /login` with any other password | `401 {"message":"invalid credentials"}` |
//! | `POST /saves` with `Authorization: Bearer mock-token-123` and a JSON object | `201 {"id":42,"stored_bytes":N}` |
//! | `POST /saves` without that header | `401 {"message":"Unauthenticated."}` |
//! | `POST /saves` with a body that is not a JSON object | `422 {"message":"The given data was invalid.","errors":{"body":["must be a JSON object"]}}` |
//! | `GET /echo` | `200` with the method, path, query and request headers as JSON |
//! | `GET /slow?ms=N` | waits N ms (default 2000, at most 10000), then `200 {"slept_ms":N}` |
//! | `GET /big?bytes=N` | `200` with a JSON string of N bytes (at most 16 MiB) |
//! | `GET /gzip` | `{"compressed":true}`, gzip-encoded when the client accepts gzip |
//! | `GET /gzip-bomb?bytes=N` | always gzip-encoded: N zero bytes (default 1 MiB, at most 16 MiB); about 6.6 KB of gzip per MiB (16 MiB ≈ 106 KB) |
//! | `POST /purchase` | `201 {"ok":true}` (counts hits, for "sent exactly once" tests) |
//! | `POST /upload` (`multipart/form-data`) | `200` with what it parsed: `{"fields":[{"name":…,"value":…}],"files":[{"name":…,"filename":…,"content_type":…,"size":N,"crc32":"8 hex"}]}` in body order (counts hits); a body that is not valid multipart `400`, over 8 MiB `413` |
//! | `GET /empty` | `204` with no body |
//! | `GET /redirect` | `302` to `/characters/1` |
//! | anything else | `404 {"message":"not found"}` |
//!
//! Request bodies: `Content-Length` or `Transfer-Encoding: chunked`, at most 1 MiB (8 MiB for
//! `/upload`). A bigger body is answered `413` (never silently cut), a malformed length or chunk
//! `400`. The multipart parser follows RFC 7578 (boundary from the `Content-Type` header, part
//! headers, `name`, `filename`, `filename*` with RFC 5987 percent-decoding) and allows at most 1000
//! parts; the `crc32` is the IEEE CRC-32 (zlib's) of the file's bytes. Names and file names are
//! echoed exactly as sent (a backslash is an ordinary character, no path is stripped, `%22` stays
//! `%22`): the mock shows what the client put on the wire, not how a given framework reads it.
//! Every connection has a 15 s deadline in total, so an 8 MiB upload over a link slower than about
//! 5 Mbit/s is cut (the client sees a `Network` error, not the `413`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The password `/login` accepts (obviously fake).
pub const PASSWORD: &str = "correct-horse";
/// The token `/login` hands out and `/saves` requires (obviously fake).
pub const TOKEN: &str = "mock-token-123";

/// The most connections served at once; more are answered `503` at once.
const MAX_CONNECTIONS: usize = 32;
/// The largest generated body.
const MAX_GENERATED: usize = 16 * 1024 * 1024;
/// How long one connection may take in total (reading, waiting, writing).
const CONNECTION_DEADLINE: Duration = Duration::from_secs(15);
/// The only paths whose hits are counted (a fixed set, so random paths cannot grow memory).
const COUNTED: [&str; 5] = ["/purchase", "/slow", "/saves", "/login", "/upload"];

/// Requests seen per path.
type Hits = Arc<Mutex<HashMap<String, usize>>>;

/// A running mock server; stops when dropped.
pub struct MockServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    hits: Hits,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Start on 127.0.0.1 with a free port chosen by the OS.
    pub fn start() -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0")
    }

    /// Start on `addr` (e.g. `127.0.0.1:8080`).
    pub fn start_on(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let hits = Hits::default();
        let counter = Arc::clone(&hits);
        let thread = thread::Builder::new().name("mock-server".into()).spawn(move || accept_loop(&listener, &flag, &counter))?;
        Ok(Self { addr, stop, hits, thread: Some(thread) })
    }

    /// How many requests reached `path` (without query) so far; counted only for `/purchase`,
    /// `/slow`, `/saves` and `/login`.
    pub fn hits(&self, path: &str) -> usize {
        self.hits.lock().map(|h| h.get(path).copied().unwrap_or(0)).unwrap_or(0)
    }

    /// The base URL, e.g. `http://127.0.0.1:50123`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, stop: &AtomicBool, hits: &Hits) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                // One short-lived thread per connection, so `/slow` never blocks the others.
                active.fetch_add(1, Ordering::SeqCst);
                let (slot, hits) = (Arc::clone(&active), Arc::clone(hits));
                let spawned = thread::Builder::new().name("mock-conn".into()).spawn(move || {
                    let _ = handle(stream, &hits);
                    slot.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    // The closure never ran: release its slot here.
                    active.fetch_sub(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// The largest request body accepted (`/upload`: [`MAX_UPLOAD_BODY`]).
const MAX_REQUEST_BODY: usize = 1024 * 1024;
/// The largest `/upload` body accepted.
const MAX_UPLOAD_BODY: usize = 8 * 1024 * 1024;
/// The most parts `/upload` accepts.
const MAX_UPLOAD_PARTS: usize = 1000;

fn body_limit(path: &str) -> usize {
    if path == "/upload" {
        MAX_UPLOAD_BODY
    } else {
        MAX_REQUEST_BODY
    }
}

fn too_large_message(path: &str) -> &'static str {
    if path == "/upload" {
        "request body larger than 8 MiB"
    } else {
        "request body larger than 1 MiB"
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Why the body could not be read (status + message); answered instead of routing.
    refused: Option<(u16, &'static str)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn query_number(&self, name: &str) -> Option<u64> {
        self.query.split('&').filter_map(|pair| pair.split_once('=')).find(|(n, _)| *n == name).and_then(|(_, v)| v.parse().ok())
    }
}

/// One `read` that must finish before `deadline` (a trickling client cannot hold a slot forever).
fn read_before(stream: &mut TcpStream, chunk: &mut [u8], deadline: Instant) -> std::io::Result<usize> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "connection deadline"));
    }
    stream.set_read_timeout(Some(left.min(Duration::from_secs(5))))?;
    stream.read(chunk)
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let deadline = Instant::now() + CONNECTION_DEADLINE;
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(CONNECTION_DEADLINE))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::other("request head too large"));
        }
        let n = read_before(stream, &mut chunk, deadline)?;
        if n == 0 {
            return Err(std::io::Error::other("connection closed"));
        }
        buf.extend_from_slice(&chunk[..n]);
        // Not HTTP (e.g. a TLS ClientHello sent to this plain-HTTP server): refuse at once.
        if !buf[0].is_ascii_uppercase() {
            stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return Err(std::io::Error::other("not an HTTP request"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target, String::new()));
    let headers: Vec<(String, String)> =
        lines.filter_map(|line| line.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let rest = buf[head_end + 4..].to_vec();
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.to_ascii_lowercase());
    let limit = body_limit(&path);
    let body = if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        read_chunked(stream, rest, deadline, limit, too_large_message(&path))?
    } else {
        match header("content-length").map(|v| v.parse::<usize>()) {
            None => Ok(Vec::new()),
            Some(Err(_)) => Err((400, "bad Content-Length")),
            Some(Ok(length)) if length > limit => {
                // Read (and drop) up to 4 times the limit first (bounded by the connection
                // deadline), so the client sees the 413 instead of a reset while it is still writing.
                let mut left = length.min(limit.saturating_mul(4)).saturating_sub(rest.len());
                while left > 0 {
                    let n = read_before(stream, &mut chunk, deadline)?;
                    if n == 0 {
                        break;
                    }
                    left = left.saturating_sub(n);
                }
                Err((413, too_large_message(&path)))
            }
            Some(Ok(length)) => {
                let mut body = rest;
                while body.len() < length {
                    let n = read_before(stream, &mut chunk, deadline)?;
                    if n == 0 {
                        return Err(std::io::Error::other("body cut short"));
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                body.truncate(length);
                Ok(body)
            }
        }
    };
    let (body, refused) = match body {
        Ok(body) => (body, None),
        Err(why) => (Vec::new(), Some(why)),
    };
    Ok(Request { method, path, query, headers, body, refused })
}

/// A `Transfer-Encoding: chunked` body (`pending` holds what was already read after the head).
/// `Err((status, why))` for a malformed or too large body.
fn read_chunked(
    stream: &mut TcpStream,
    mut pending: Vec<u8>,
    deadline: Instant,
    limit: usize,
    too_large: &'static str,
) -> std::io::Result<Result<Vec<u8>, (u16, &'static str)>> {
    let mut chunk = [0u8; 4096];
    let mut body = Vec::new();
    // Make `pending` hold at least `n` bytes.
    let mut fill = |pending: &mut Vec<u8>, n: usize| -> std::io::Result<bool> {
        while pending.len() < n {
            let got = read_before(stream, &mut chunk, deadline)?;
            if got == 0 {
                return Ok(false);
            }
            pending.extend_from_slice(&chunk[..got]);
        }
        Ok(true)
    };
    loop {
        // The size line.
        let line_end = loop {
            if let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
                break pos;
            }
            if pending.len() > 1024 {
                return Ok(Err((400, "chunk size line too long")));
            }
            let want = pending.len() + 1;
            if !fill(&mut pending, want)? {
                return Ok(Err((400, "chunked body cut short")));
            }
        };
        let line = String::from_utf8_lossy(&pending[..line_end]).into_owned();
        let size_text = line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return Ok(Err((400, "bad chunk size")));
        };
        pending.drain(..line_end + 2);
        if size == 0 {
            // Trailers until an empty line; ignored.
            loop {
                if let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
                    let empty = pos == 0;
                    pending.drain(..pos + 2);
                    if empty {
                        return Ok(Ok(body));
                    }
                } else {
                    if pending.len() > 8192 {
                        return Ok(Err((400, "trailer too long")));
                    }
                    let want = pending.len() + 1;
                    if !fill(&mut pending, want)? {
                        return Ok(Err((400, "chunked body cut short")));
                    }
                }
            }
        }
        if body.len().saturating_add(size) > limit {
            return Ok(Err((413, too_large)));
        }
        if !fill(&mut pending, size + 2)? {
            return Ok(Err((400, "chunked body cut short")));
        }
        if &pending[size..size + 2] != b"\r\n" {
            return Ok(Err((400, "chunk not followed by CRLF")));
        }
        body.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2);
    }
}

fn handle(mut stream: TcpStream, hits: &Hits) -> std::io::Result<()> {
    let request = read_request(&mut stream)?;
    if let Some((status, why)) = request.refused {
        let reason = if status == 413 { "Content Too Large" } else { "Bad Request" };
        let body = format!(r#"{{"message":"{why}"}}"#);
        let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        return stream.flush();
    }
    if COUNTED.contains(&request.path.as_str()) {
        if let Ok(mut hits) = hits.lock() {
            *hits.entry(request.path.clone()).or_insert(0) += 1;
        }
    }
    let body_text = String::from_utf8_lossy(&request.body).into_owned();
    let mut extra: Vec<(&str, String)> = Vec::new();
    let (status, body): (u16, Vec<u8>) = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/characters/1") => (200, br#"{"id":1,"name":"Ayla","class":"ranger","level":7}"#.to_vec()),
        ("GET", p) if p.starts_with("/characters/") => (404, br#"{"message":"character not found"}"#.to_vec()),
        ("POST", "/login") if body_text.contains(&format!("\"{PASSWORD}\"")) => (200, format!(r#"{{"token":"{TOKEN}"}}"#).into_bytes()),
        ("POST", "/login") => (401, br#"{"message":"invalid credentials"}"#.to_vec()),
        ("POST", "/saves") if request.header("authorization") != Some(&format!("Bearer {TOKEN}")) => (401, br#"{"message":"Unauthenticated."}"#.to_vec()),
        ("POST", "/saves") if !body_text.trim_start().starts_with('{') => {
            (422, br#"{"message":"The given data was invalid.","errors":{"body":["must be a JSON object"]}}"#.to_vec())
        }
        ("POST", "/saves") => (201, format!(r#"{{"id":42,"stored_bytes":{}}}"#, request.body.len()).into_bytes()),
        ("GET", "/echo") => {
            let headers: Vec<String> = request.headers.iter().map(|(n, v)| format!("{}:{}", json_string(n), json_string(v))).collect();
            let json = format!(
                r#"{{"method":{},"path":{},"query":{},"headers":{{{}}},"body":{}}}"#,
                json_string(&request.method),
                json_string(&request.path),
                json_string(&request.query),
                headers.join(","),
                json_string(&body_text)
            );
            (200, json.into_bytes())
        }
        ("GET", "/slow") => {
            let ms = request.query_number("ms").unwrap_or(2000).min(10_000);
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
            (200, format!(r#"{{"slept_ms":{ms}}}"#).into_bytes())
        }
        ("GET", "/big") => {
            let n = usize::try_from(request.query_number("bytes").unwrap_or(1024)).unwrap_or(1024).min(MAX_GENERATED);
            let mut body = Vec::with_capacity(n + 2);
            body.push(b'"');
            body.resize(n.max(2) - 1, b'a');
            body.push(b'"');
            (200, body)
        }
        ("GET", "/gzip") => {
            let plain = br#"{"compressed":true}"#;
            if request.header("accept-encoding").is_some_and(|v| v.contains("gzip")) {
                extra.push(("Content-Encoding", "gzip".into()));
                (200, gzip_stored(plain))
            } else {
                (200, plain.to_vec())
            }
        }
        ("GET", "/gzip-bomb") => {
            let n = usize::try_from(request.query_number("bytes").unwrap_or(1024 * 1024)).unwrap_or(0).min(MAX_GENERATED);
            extra.push(("Content-Encoding", "gzip".into()));
            (200, gzip_zeros(n))
        }
        ("POST", "/purchase") => (201, br#"{"ok":true}"#.to_vec()),
        ("POST", "/upload") => match multipart::parse(request.header("content-type").unwrap_or(""), &request.body) {
            Ok(parts) => (200, multipart::echo(&parts).into_bytes()),
            Err(why) => (400, format!(r#"{{"message":{}}}"#, json_string(why)).into_bytes()),
        },
        ("GET", "/empty") => (204, Vec::new()),
        ("GET", "/redirect") => {
            extra.push(("Location", "/characters/1".into()));
            (302, Vec::new())
        }
        _ => (404, br#"{"message":"not found"}"#.to_vec()),
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        422 => "Unprocessable Content",
        _ => "Other",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// A std-only `multipart/form-data` parser (RFC 7578 / RFC 2046) for `/upload`.
mod multipart {
    use super::{crc32, json_string, MAX_UPLOAD_PARTS};

    /// One part; its data borrows the request body (no second copy of an 8 MiB upload).
    pub struct Part<'a> {
        pub name: String,
        pub filename: Option<String>,
        pub content_type: Option<String>,
        pub data: &'a [u8],
    }

    /// The boundary parameter of a `multipart/form-data` content type.
    fn boundary(content_type: &str) -> Result<String, &'static str> {
        let mut params = content_type.split(';');
        let kind = params.next().unwrap_or("").trim().to_ascii_lowercase();
        if kind != "multipart/form-data" {
            return Err("not multipart/form-data");
        }
        for param in params {
            if let Some((key, value)) = param.split_once('=') {
                if key.trim().eq_ignore_ascii_case("boundary") {
                    let value = value.trim().trim_matches('"').to_string();
                    if value.is_empty() || value.len() > 70 {
                        return Err("bad boundary");
                    }
                    return Ok(value);
                }
            }
        }
        Err("no boundary")
    }

    fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
    }

    /// `key="value"` / `key=value` parameters of a header, quotes removed. Read the way browsers
    /// write them (WHATWG: `"` is sent as `%22`, never as `\"`): a quoted value ends at the next
    /// `"`, and a backslash is an ordinary character. So the echo shows the name exactly as the
    /// client sent it (`C:\Users\me\a.png` stays whole; nothing is path-stripped or decoded), which
    /// is what the crate's tests compare. Real frameworks differ here (see the README).
    fn params(value: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = value;
        while let Some((key, after)) = rest.split_once('=') {
            let key = key.rsplit(';').next().unwrap_or("").trim().to_ascii_lowercase();
            let after = after.trim_start();
            let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').unwrap_or(quoted.len());
                (quoted[..end].to_string(), quoted.get(end + 1..).unwrap_or(""))
            } else {
                let end = after.find(';').unwrap_or(after.len());
                (after[..end].trim().to_string(), &after[end..])
            };
            out.push((key, value));
            rest = next;
        }
        out
    }

    /// RFC 5987 `UTF-8''percent-encoded`.
    fn ext_value(value: &str) -> Option<String> {
        let (charset, rest) = value.split_once('\'')?;
        let (_, encoded) = rest.split_once('\'')?;
        if !charset.eq_ignore_ascii_case("utf-8") {
            return None;
        }
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    pub fn parse<'a>(content_type: &str, body: &'a [u8]) -> Result<Vec<Part<'a>>, &'static str> {
        let boundary = boundary(content_type)?;
        let delimiter = format!("--{boundary}");
        let delimiter = delimiter.as_bytes();
        // Skip a preamble, then expect the first delimiter.
        let mut at = find(body, delimiter, 0).ok_or("no opening boundary")?;
        let mut parts = Vec::new();
        loop {
            at += delimiter.len();
            let tail = body.get(at..at + 2).ok_or("cut short after a boundary")?;
            if tail == b"--" {
                return Ok(parts);
            }
            if tail != b"\r\n" {
                return Err("a boundary is not followed by CRLF or --");
            }
            at += 2;
            let head_end = find(body, b"\r\n\r\n", at).ok_or("part headers not terminated")?;
            let head = std::str::from_utf8(&body[at..head_end]).map_err(|_| "part headers are not UTF-8")?;
            let mut name = None;
            let mut filename = None;
            let mut filename_ext = None;
            let mut content_type = None;
            for line in head.split("\r\n") {
                let (key, value) = line.split_once(':').ok_or("bad part header")?;
                match key.trim().to_ascii_lowercase().as_str() {
                    "content-disposition" => {
                        if !value.trim().to_ascii_lowercase().starts_with("form-data") {
                            return Err("a part is not form-data");
                        }
                        for (key, value) in params(value) {
                            match key.as_str() {
                                "name" => name = Some(value),
                                "filename" => filename = Some(value),
                                "filename*" => filename_ext = ext_value(&value),
                                _ => {}
                            }
                        }
                    }
                    "content-type" => content_type = Some(value.trim().to_string()),
                    _ => {}
                }
            }
            let data_start = head_end + 4;
            let mut end_marker = b"\r\n".to_vec();
            end_marker.extend_from_slice(delimiter);
            let data_end = find(body, &end_marker, data_start).ok_or("a part is not closed by a boundary")?;
            let name = name.ok_or("a part has no name")?;
            parts.push(Part { name, filename: filename_ext.or(filename), content_type, data: &body[data_start..data_end] });
            if parts.len() > MAX_UPLOAD_PARTS {
                return Err("too many parts");
            }
            at = data_end + 2;
        }
    }

    /// `{"fields":[…],"files":[…]}`, both in body order.
    pub fn echo(parts: &[Part]) -> String {
        let mut fields = Vec::new();
        let mut files = Vec::new();
        for part in parts {
            match &part.filename {
                None => fields.push(format!(r#"{{"name":{},"value":{}}}"#, json_string(&part.name), json_string(&String::from_utf8_lossy(part.data)))),
                Some(filename) => files.push(format!(
                    r#"{{"name":{},"filename":{},"content_type":{},"size":{},"crc32":"{:08x}"}}"#,
                    json_string(&part.name),
                    json_string(filename),
                    part.content_type.as_deref().map_or_else(|| "null".to_string(), json_string),
                    part.data.len(),
                    crc32(part.data)
                )),
            }
        }
        format!(r#"{{"fields":[{}],"files":[{}]}}"#, fields.join(","), files.join(","))
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A gzip stream holding `data` in one uncompressed ("stored") deflate block: valid gzip without
/// a compression library.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).unwrap_or(u16::MAX);
    let data = &data[..usize::from(len)];
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.push(1); // final block, stored
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&u32::from(len).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xffff_ffff, data.iter().copied()) ^ 0xffff_ffff
}

fn crc32_update(mut crc: u32, bytes: impl Iterator<Item = u8>) -> u32 {
    for byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}

/// Bits into bytes, least significant bit first (deflate's order).
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    used: u32,
}

impl Bits {
    /// `count` bits of `value`, low bit first.
    fn put(&mut self, value: u32, count: u32) {
        for i in 0..count {
            self.acc |= ((value >> i) & 1) << self.used;
            self.used += 1;
            if self.used == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.used = 0;
            }
        }
    }

    /// A Huffman code: `count` bits of `code`, high bit first.
    fn code(&mut self, code: u32, count: u32) {
        for i in (0..count).rev() {
            self.put((code >> i) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// A gzip stream of `n` zero bytes, a few KiB long: one literal zero, then back-references of
/// 258 bytes at distance 1 (fixed Huffman codes). For "the body limit holds after decoding" tests.
fn gzip_zeros(n: usize) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(1, 1); // final block
    bits.put(1, 2); // fixed Huffman
    let mut left = n;
    if left > 0 {
        bits.code(0x30, 8); // literal 0
        left -= 1;
    }
    while left >= 258 {
        bits.code(0xc5, 8); // length 258 (code 285)
        bits.code(0, 5); // distance 1 (code 0)
        left -= 258;
    }
    for _ in 0..left {
        bits.code(0x30, 8);
    }
    bits.code(0, 7); // end of block (code 256)
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend_from_slice(&bits.finish());
    let crc = crc32_update(0xffff_ffff, std::iter::repeat_n(0u8, n)) ^ 0xffff_ffff;
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&u32::try_from(n).unwrap_or(0).to_le_bytes());
    out
}

/// `--seconds N` (maximum runtime, default 60) and `--bind ADDR` (default `127.0.0.1:0`).
fn main() -> std::io::Result<()> {
    let mut seconds: u64 = 60;
    let mut addr = "127.0.0.1:0".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--seconds", Some(value)) if value.parse::<u64>().is_ok() => seconds = value.parse().unwrap_or(60),
            ("--bind", Some(value)) => addr = value,
            _ => {
                eprintln!("usage: mock_server [--seconds N] [--bind ADDR]   (defaults: 60, 127.0.0.1:0)");
                return Err(std::io::Error::other("bad arguments"));
            }
        }
    }
    let server = MockServer::start_on(&addr)?;
    println!("mock game API on {} for {seconds} s", server.url());
    println!("try: BACKEND_URL={} cargo run --example fetch_json", server.url());
    thread::sleep(Duration::from_secs(seconds));
    drop(server);
    println!("mock server stopped");
    Ok(())
}
