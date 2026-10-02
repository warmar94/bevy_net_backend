//! A tiny mock game WebSocket server on 127.0.0.1, std + tungstenite's server side only (features
//! `ws` + `json`). The `chat_client` example and the tests start it in their own process. Run it alone:
//!
//! ```text
//! cargo run --example mock_ws_server --features ws,json                                           # 60 s
//! cargo run --example mock_ws_server --features ws,json -- --seconds 1800 --bind 127.0.0.1:9001   # behind a TLS proxy
//! ```
//!
//! `--bind` defaults to `127.0.0.1:0` (loopback, a free port), `--seconds` (default 60) is the
//! maximum runtime, `--tick-ms` (default 1000, 0 = off) the push interval. Never bind it to
//! `0.0.0.0`. It sends close frames (1001) when `--seconds` ends; on SIGTERM it just exits (std
//! has no signal handling), so clients see a dropped connection. Behind a TLS-terminating proxy
//! (Caddy) it is the live WSS test server for
//! `tests/live_ws.rs`. Limits: 32 connections at once (more get `503` before the handshake), 10 s
//! per read during the handshake, 60 s without any frame, 10 min per connection, 1 MiB messages.
//!
//! The JSON envelope it speaks (the crate's default `JsonEnvelope`):
//!
//! | Client sends `{"id":N,"type":…,"data":…}` with type | Server answers |
//! |---|---|
//! | `echo` | `{"id":N,"ok":true,"data":<data>}` |
//! | `chat.send` (`{"text":…}`) | `{"id":N,"ok":true,"data":{"accepted":true}}`, then the push `{"type":"chat.message","data":{"from":"mock","text":…}}` |
//! | `fail` | `{"id":N,"ok":false,"error":{"code":"refused","message":"as asked"}}` |
//! | `big` (`{"bytes":B}`) | `{"id":N,"ok":true,"data":"xxx…"}` about B bytes (at most 4 MiB) |
//! | `close` (`{"code":C}`) | closes the connection with code C |
//! | `drop` | drops the TCP connection without a close handshake |
//! | `stall` (`{"ms":M}`) | stops reading and writing for M ms (at most 10 s) |
//! | anything else | `{"id":N,"ok":false,"error":{"code":"unknown"}}` |
//!
//! Anything that is not an envelope request (plain text, binary) is echoed back as it came.
//! Every `tick_ms` it pushes `{"type":"server.tick","data":{"n":…}}` (per connection:
//! `?tick_ms=N` in the URL, at least 10, or 0 for none). A path ending in `/secure` requires
//! `Authorization: Bearer mock-token-123` (else `401`). A path ending in `/busy` refuses every
//! handshake with `503` and `Retry-After: 2`.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, WebSocket};

/// The token `/secure` requires (obviously fake).
pub const TOKEN: &str = "mock-token-123";

const MAX_CONNECTIONS: usize = 32;
const MAX_MESSAGE: usize = 1024 * 1024;
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
const IDLE_DEADLINE: Duration = Duration::from_secs(60);
const CONNECTION_LIFETIME: Duration = Duration::from_secs(600);

/// A running mock WebSocket server; stops when dropped.
pub struct MockWsServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accepted: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl MockWsServer {
    /// Start on 127.0.0.1 with a free port, pushing a tick every `tick_ms` (0 = none).
    pub fn start(tick_ms: u64) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", tick_ms)
    }

    /// Start on `addr`.
    pub fn start_on(addr: &str, tick_ms: u64) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (flag, count) = (Arc::clone(&stop), Arc::clone(&accepted));
        let thread = thread::Builder::new().name("mock-ws-server".into()).spawn(move || accept_loop(&listener, &flag, &count, tick_ms))?;
        Ok(Self { addr, stop, accepted, thread: Some(thread) })
    }

    /// The base URL, e.g. `ws://127.0.0.1:50123`.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// How many WebSocket handshakes succeeded so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for MockWsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, stop: &Arc<AtomicBool>, accepted: &Arc<AtomicUsize>, tick_ms: u64) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    use std::io::Write;
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let (slot, stop, accepted) = (Arc::clone(&active), Arc::clone(stop), Arc::clone(accepted));
                let spawned = thread::Builder::new().name("mock-ws-conn".into()).spawn(move || {
                    let _ = serve(stream, &stop, &accepted, tick_ms);
                    slot.fetch_sub(1, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn close(ws: &mut WebSocket<TcpStream>, code: u16) {
    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: "".into() }));
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if ws.read().is_err() {
            break;
        }
    }
}

// tungstenite's callback type fixes the error type.
#[allow(clippy::result_large_err)]
fn serve(stream: TcpStream, stop: &AtomicBool, accepted: &AtomicUsize, default_tick: u64) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(HANDSHAKE_DEADLINE)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(HANDSHAKE_DEADLINE)).map_err(|e| e.to_string())?;
    let mut tick_ms = default_tick;
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        if let Some(query) = request.uri().query() {
            if let Some(ms) = query.split('&').filter_map(|p| p.split_once('=')).find(|(n, _)| *n == "tick_ms").and_then(|(_, v)| v.parse::<u64>().ok()) {
                tick_ms = if ms == 0 { 0 } else { ms.max(10) };
            }
        }
        if request.uri().path().ends_with("/busy") {
            let mut busy = ErrorResponse::new(None);
            *busy.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
            busy.headers_mut().insert(http::header::RETRY_AFTER, http::HeaderValue::from_static("2"));
            return Err(busy);
        }
        let authorized = request.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(format!("Bearer {TOKEN}").as_str());
        if request.uri().path().ends_with("/secure") && !authorized {
            let mut refused = ErrorResponse::new(Some(r#"{"message":"Unauthenticated."}"#.to_string()));
            *refused.status_mut() = http::StatusCode::UNAUTHORIZED;
            return Err(refused);
        }
        Ok(response)
    };
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    let mut ws = tungstenite::accept_hdr_with_config(stream, callback, Some(config)).map_err(|e| e.to_string())?;
    accepted.fetch_add(1, Ordering::SeqCst);
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(20))).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut last_seen = Instant::now();
    let mut last_tick = Instant::now();
    let mut ticks = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) || started.elapsed() > CONNECTION_LIFETIME {
            close(&mut ws, 1001);
            return Ok(());
        }
        if last_seen.elapsed() > IDLE_DEADLINE {
            close(&mut ws, 1000);
            return Ok(());
        }
        if tick_ms > 0 && last_tick.elapsed() >= Duration::from_millis(tick_ms) {
            ticks += 1;
            last_tick = Instant::now();
            let _ = ws.send(Message::text(format!(r#"{{"type":"server.tick","data":{{"n":{ticks}}}}}"#)));
        }
        let message = match ws.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => return Ok(()),
        };
        last_seen = Instant::now();
        match message {
            Message::Text(text) => {
                let request = serde_json::from_str::<serde_json::Value>(text.as_str()).ok().filter(|v| v.get("id").is_some_and(serde_json::Value::is_u64));
                let Some(request) = request else {
                    ws.send(Message::Text(text)).map_err(|e| e.to_string())?;
                    continue;
                };
                let id = request.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let kind = request.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
                let data = request.get("data").cloned().unwrap_or(serde_json::Value::Null);
                let reply = |value: serde_json::Value| Message::text(value.to_string());
                match kind {
                    "echo" => ws.send(reply(serde_json::json!({"id": id, "ok": true, "data": data}))).map_err(|e| e.to_string())?,
                    "chat.send" => {
                        ws.send(reply(serde_json::json!({"id": id, "ok": true, "data": {"accepted": true}}))).map_err(|e| e.to_string())?;
                        let text = data.get("text").cloned().unwrap_or(serde_json::Value::Null);
                        ws.send(reply(serde_json::json!({"type": "chat.message", "data": {"from": "mock", "text": text}}))).map_err(|e| e.to_string())?;
                    }
                    "fail" => ws
                        .send(reply(serde_json::json!({"id": id, "ok": false, "error": {"code": "refused", "message": "as asked"}})))
                        .map_err(|e| e.to_string())?,
                    "big" => {
                        let bytes = data.get("bytes").and_then(serde_json::Value::as_u64).unwrap_or(1024).min(4 * 1024 * 1024);
                        let filler = "x".repeat(usize::try_from(bytes).unwrap_or(1024));
                        // Bypass the server's own message limit for this one: the point is the client's limit.
                        let text = serde_json::json!({"id": id, "ok": true, "data": filler}).to_string();
                        ws.send(Message::text(text)).map_err(|e| e.to_string())?;
                    }
                    "close" => {
                        let code = data.get("code").and_then(serde_json::Value::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(1000);
                        close(&mut ws, code);
                        return Ok(());
                    }
                    "drop" => return Ok(()),
                    "stall" => {
                        let ms = data.get("ms").and_then(serde_json::Value::as_u64).unwrap_or(1000).min(10_000);
                        thread::sleep(Duration::from_millis(ms));
                    }
                    _ => ws.send(reply(serde_json::json!({"id": id, "ok": false, "error": {"code": "unknown"}}))).map_err(|e| e.to_string())?,
                }
            }
            Message::Binary(bytes) => ws.send(Message::Binary(bytes)).map_err(|e| e.to_string())?,
            Message::Close(_) => {
                let _ = ws.flush();
                return Ok(());
            }
            _ => {}
        }
    }
}

/// `--seconds N` (maximum runtime, default 60), `--bind ADDR` (default `127.0.0.1:0`),
/// `--tick-ms N` (default 1000, 0 = off).
fn main() -> std::io::Result<()> {
    let (mut seconds, mut addr, mut tick_ms) = (60u64, "127.0.0.1:0".to_string(), 1000u64);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--seconds", Some(value)) if value.parse::<u64>().is_ok() => seconds = value.parse().unwrap_or(60),
            ("--tick-ms", Some(value)) if value.parse::<u64>().is_ok() => tick_ms = value.parse().unwrap_or(1000),
            ("--bind", Some(value)) => addr = value,
            _ => {
                eprintln!("usage: mock_ws_server [--seconds N] [--bind ADDR] [--tick-ms N]   (defaults: 60, 127.0.0.1:0, 1000)");
                return Err(std::io::Error::other("bad arguments"));
            }
        }
    }
    let server = MockWsServer::start_on(&addr, tick_ms)?;
    println!("mock WebSocket server on {} for {seconds} s", server.url());
    println!("try: BACKEND_WS_URL={} cargo run --example chat_client --features ws,json", server.url());
    thread::sleep(Duration::from_secs(seconds));
    drop(server);
    println!("mock WebSocket server stopped");
    Ok(())
}
