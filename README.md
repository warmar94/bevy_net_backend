<p align="center">
  <img src="https://raw.githubusercontent.com/warmar94/bevy_net_backend/main/bevy_net_backend-cover.png"
       alt="bevy_net_backend: HTTP, WebSocket and SSH/SFTP for Bevy" width="100%">
</p>

<p align="center">
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <a href="https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://bevyengine.org"><img alt="Bevy 0.19.0" src="https://img.shields.io/badge/Bevy-0.19.0-informational"></a>
  <a href="https://crates.io/crates/ureq"><img alt="ureq 3.4.2" src="https://img.shields.io/badge/ureq-3.4.2-orange"></a>
  <a href="https://crates.io/crates/tungstenite"><img alt="tungstenite 0.30.0 (optional)" src="https://img.shields.io/badge/tungstenite-0.30.0%20(optional)-orange"></a>
  <a href="https://crates.io/crates/russh"><img alt="russh 0.63.3 (optional)" src="https://img.shields.io/badge/russh-0.63.3%20(optional)-orange"></a>
</p>

<p align="center"><b>HTTP, WebSocket and SSH/SFTP for Bevy</b>: fire a request, get exactly one typed answer back as a Bevy message.</p>

---

## What it is

`bevy_net_backend` connects a [Bevy](https://bevyengine.org) game to **its own backend**: the
servers behind accounts and logins, cloud saves, leaderboards, shops and inventories, friends
lists, chat, matchmaking and the other MMO-style services a game runs itself. It also lets admin
and developer tools reach those servers.

Three parts share one pattern: **a system fires a request and gets a `RequestId` back at once; a
few frames after that exactly one typed answer arrives as a Bevy message.**

- **HTTP** (default): calls to your HTTPS JSON API (Laravel, Express, Go, Django, FastAPI,
  ASP.NET, …) with your serde types, and file uploads as `multipart/form-data` (files streamed
  from disk, upload progress).
- **WebSocket** (feature `ws`): named long-lived connections for chat, lobbies, match events and
  server pushes, with reconnect and heartbeat built in.
- **SSH and SFTP** (features `ssh`, `sftp`), **for admin and developer tools only**: run commands on
  your servers and move files. Release builds refuse it unless the tool opts in.

The answer is the decoded value or an error that says what happened (network, TLS, timeout, an
HTTP status with the server's body, a decode error, cancelled, or shutdown when the app exits).
The network never blocks a frame, nothing is dropped silently, and bad input never panics.

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
struct Profile {
    name: String,
    level: u32,
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::new(HttpConfig::new("https://api.example.com"))))
        .add_json_response::<Profile>()
        .add_systems(Startup, |backend: Res<HttpClient>| {
            backend.get_json::<Profile>("/me");
        })
        .add_systems(Update, |mut answers: MessageReader<JsonResponse<Profile>>| {
            for answer in answers.read() {
                match &answer.result {
                    Ok(profile) => info!("{} is level {}", profile.name, profile.level),
                    Err(error) => warn!("could not load the profile: {error}"),
                }
            }
        })
        .run();
}
```

## Contents

- [What it is](#what-it-is)
- [Features at a glance](#features-at-a-glance)
- [What it guarantees](#what-it-guarantees)
- [Where it sits](#where-it-sits)
- [Backend compatibility](#backend-compatibility)
  - [Matching server](#matching-server)
- [Install](#install)
- [Quick start](#quick-start)
- [How to use it](#how-to-use-it)
  - [1. Configure the backend](#1-configure-the-backend)
  - [2. Typed JSON requests](#2-typed-json-requests)
  - [3. Raw requests and full control](#3-raw-requests-and-full-control)
  - [4. Reading answers and errors](#4-reading-answers-and-errors)
  - [5. Logging in: credentials](#5-logging-in-credentials)
  - [6. Cancel, in-flight tracking, app exit](#6-cancel-in-flight-tracking-app-exit)
  - [7. Plain http:// for local development](#7-plain-http-for-local-development)
  - [8. Testing your game without a server](#8-testing-your-game-without-a-server)
  - [9. Your own transport](#9-your-own-transport)
  - [10. WebSocket connections (feature `ws`)](#10-websocket-connections-feature-ws)
  - [11. SSH commands and SFTP (feature `ssh`, admin / dev builds only)](#11-ssh-commands-and-sftp-feature-ssh-admin--dev-builds-only)
  - [12. File uploads (multipart)](#12-file-uploads-multipart)
- [How it works](#how-it-works)
- [TLS exception: the default build is not pure Rust](#tls-exception-the-default-build-is-not-pure-rust)
- [API reference](#api-reference)
- [Good to know](#good-to-know)
- [Versions](#versions)
- [Examples](#examples)
- [How it's tested](#how-its-tested)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## Features at a glance

| Feature | Default | For | What it adds |
|---|---|---|---|
| `http` | yes | your HTTPS API, file uploads | The real transport, `UreqTransport`: ureq 3.4 on a few worker threads, rustls with ring's crypto and the Mozilla root certificates (webpki-roots). `multipart/form-data` uploads with `Multipart` (no extra dependency): bytes, files streamed from disk, typed parts (JSON), upload progress. ring compiles C and assembly (see [TLS exception](#tls-exception-the-default-build-is-not-pure-rust)). |
| `json` | yes | typed requests and answers | `get_json` / `post_json` / `send_json` / `post_multipart_json`, `JsonResponse<T>`, `OutgoingRequest::with_json`, `RawResponse::json`, `JsonBodyField` (serde + serde_json). |
| `gzip` | no | compressed answers | Accept gzip-compressed responses (ureq's decoder, flate2). |
| `ws` | no | live data: chat, lobbies, pushes | Named WebSocket connections: `WsClient`, `WsConnections`, the `Ws*` messages, `TungsteniteTransport` (tungstenite 0.30, sync, one thread per connection, rustls + ring, no permessage-deflate). With `json`: `JsonEnvelope`, `WsRequest`, `WsPushMessage`, `WsResponse<T>`, `WsPush<P>`. |
| `ssh` | no | **admin / dev tools only** | Named SSH connections that run commands: `SshClient`, `SshConnections`, the `Ssh*` messages, `RusshTransport` (russh 0.63, ring for the AEAD ciphers and RustCrypto for the rest; tokio on one private thread), strict known_hosts, key files / ssh-agent / `~/.ssh/config`. |
| `sftp` | no | admin / dev tools: files | SFTP on SSH connections (implies `ssh`): upload, download, list, create / remove directory, remove file, rename (russh-sftp). |
| `ssh-rsa` | no | old RSA-only servers | RSA host keys and RSA key files for SSH (implies `ssh`; rsa-sha2-256/512, never SHA-1). Off by default: the `rsa` crate carries the unfixed Marvin timing advisory RUSTSEC-2023-0071. Without it, ed25519 and ECDSA keys work. |

Crates in the build (normal and build dependencies, this crate excluded, measured with
`cargo tree` on Windows; other platforms differ by a few platform crates):

| Features | Crates |
|---|---|
| `default-features = false` (types and fake transports only) | 68 |
| default (`http`, `json`) | 90 |
| default + `gzip` | 95 |
| default + `ws` | 104 |
| default + `ssh` | 209 |
| default + `ssh`, `sftp` | 219 |
| default + `ssh`, `ssh-rsa` | 212 |
| `ssh` without default features | 195 |
| all features | 228 |

Without `http` the crate still builds: every type, the `FakeHttpTransport` and your own
`HttpTransport` work, and requests without a transport are answered with `NoTransport`. The
default set is deliberately not empty: the crate exists to call an HTTPS JSON API, and it should
do that with no feature fiddling.

## What it guarantees

- **Every request gets exactly one answer:** success, or an error such as `Status` (the server's
  status, headers and body), `Network`, `Tls`, `Timeout`, `Decode`, `Cancelled`, `Shutdown` (the
  app exited), `NoTransport`, `RequestTooLarge` or `InvalidRequest`. HTTP requests, WebSocket
  requests, SSH commands and SFTP operations alike. A late result from the network after a cancel
  or a timeout is discarded, never delivered twice.
- **An answered request is never sent afterwards.** A request cancelled, timed out or answered
  `Shutdown` while it still waited (for a worker, a connection or a free channel) never goes out
  afterwards. Errors say honestly whether the request went out: `error.was_sent()` is `Some(false)`
  (never sent), `Some(true)` (the server has it) or `None` (unknown), and SSH answers carry
  `started`. Retry decisions can rely on it.
- **Real deadlines, bounded buffers, size limits.** A timeout covers the whole call, waiting
  included; a WebSocket or SSH connect is ONE deadline over TCP, TLS / key exchange and the
  handshake, so a server that trickles bytes cannot stretch it. Answers are capped (HTTP body
  10 MiB after gzip decoding, so a gzip bomb stops at the limit; WebSocket messages 1 MiB; SSH
  output 8 MiB; SFTP transfers 256 MiB) and so are requests (uploads 32 MiB and 256 parts, checked
  before anything is sent). Every default can be changed.
- **Never blocks a frame, never panics on bad input.** Requests run on the crate's own threads,
  not on Bevy's task pools; answers are written in `First`, so `PreUpdate` and `Update` read them
  in the frame they arrived. An invalid path, header, name or form is an `InvalidRequest` answer.
- **No ordering ceremony:** `HttpClient`, `WsClient` and `SshClient` are used through `Res<..>`
  (shared access), so any number of systems in any schedule can fire requests.
- **Secrets stay out of logs:** tokens and passwords are redacted from `Debug`, `Display` and the
  crate's own log lines (a test captures every log event to prove it), and the crate's `Secret`
  overwrites its memory with zeros when it is dropped.
- **Safe defaults:** HTTPS only (plain `http://` only to `localhost` / `127.x.x.x` / `[::1]` unless
  you allow it), redirects not followed, a 15 s timeout, strict SSH host key checking with no
  trust-on-first-use.
- **No tokio unless you enable `ssh`**, and then only on one private thread. HTTPS uses rustls with
  ring, **no OpenSSL**, native-tls or aws-lc in any feature set. ring compiles C and assembly: see
  the [TLS exception](#tls-exception-the-default-build-is-not-pure-rust).
- **Testable offline:** `FakeHttpTransport`, `FakeWsTransport` and `FakeSshTransport` answer from
  scripts, so your game's systems can be tested headless without a server.

## Where it sits

This crate is the game talking to **your servers**, not players talking to each other.
Real-time gameplay between players (inputs, positions, replication at 30–60 Hz) belongs to UDP
netcode such as [bevy_replicon](https://crates.io/crates/bevy_replicon) with
[renet](https://crates.io/crates/renet). `bevy_net_backend` sits next to it: a typical online game
logs in, loads the save and joins matchmaking over HTTP, keeps a WebSocket open for chat and
lobby updates, plays the match over replicon, and posts the result over HTTP again. Nothing here
competes with the netcode for the frame or the socket.

## Backend compatibility

- **HTTP** works with any backend that speaks HTTPS and JSON (or raw bytes): Laravel / PHP,
  Express / Node, Go, Rust, Django, FastAPI, Spring, Rails, ASP.NET, serverless functions. Nothing
  is tailored to one framework. File uploads (`multipart/form-data`) were checked byte for byte
  against real PHP, Express + multer, FastAPI, Go and Django; frameworks differ in their limits
  and in how they read some file names, so read
  [what real backends do with an upload](#what-real-backends-do-with-an-upload).
- **WebSocket** (feature `ws`) works with any plain WebSocket server (RFC 6455), through the
  default JSON envelope or your own `WsProtocol` for another message layout.
- **Frameworks that run their own protocol on top of WebSocket** (Laravel Reverb / Pusher,
  Socket.IO, SignalR, Phoenix Channels): the crate speaks plain WebSocket frames, so such a
  protocol is implemented on top of it (`WsProtocol` or raw frames).
- **SSH** (feature `ssh`, admin / dev tools) works with any standard SSH server: OpenSSH on Linux,
  BSD, macOS or Windows, and other servers speaking SSH-2 with ed25519 or ECDSA host keys (RSA with
  feature `ssh-rsa`). Servers without strict key exchange (OpenSSH before 9.6, unless the
  distribution backported it) connect through AES-GCM, which the client prefers; only a server that
  offers nothing but ChaCha20-Poly1305 or CBC + encrypt-then-MAC is refused (see Terrapin in the
  SSH section). SFTP needs the server's `sftp` subsystem (OpenSSH's default).

### Matching server

For a ready-made server side, the [net_backend](https://github.com/warmar94/net_backend) stack (a
Rust game-server framework with its protocol and client crates) is built to pair with this crate.
Its `net_backend_protocol` crate has an optional `bevy_net_backend` feature that implements this
crate's `WsRequest`, `WsPushMessage` and `Credentials` for its messages and tokens.

## Install

```toml
[dependencies]
bevy_net_backend = { version = "0.1.0" }
```

Other sets:

```toml
# Also accept gzip-compressed answers.
bevy_net_backend = { version = "0.1.0", features = ["gzip"] }

# HTTP + WebSocket.
bevy_net_backend = { version = "0.1.0", features = ["ws"] }

# An admin / dev tool: SSH commands and SFTP (never in a build for players).
bevy_net_backend = { version = "0.1.0", features = ["ssh", "sftp"] }

# Only the types and the fake transport (e.g. a crate that brings its own transport).
bevy_net_backend = { version = "0.1.0", default-features = false }
```

The crate uses Bevy's sub-crates `bevy_app`, `bevy_ecs` and `bevy_time` 0.19.0 without default
features, so it adds no Bevy feature your game did not ask for.

## Quick start

1. Add `BackendPlugin` with your API's base URL.
2. Register each JSON answer type once: `app.add_json_response::<T>()`.
3. Fire requests from any system with `Res<HttpClient>`; keep the `RequestId` if you need to
   match the answer.
4. Read `JsonResponse<T>` (or `HttpResponse` for raw calls) with a `MessageReader`.

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Score {
    level: u32,
    points: u64,
}

#[derive(Deserialize, Clone, Debug)]
struct Rank {
    rank: u32,
}

/// The request being waited for.
#[derive(Resource)]
struct Submitting(RequestId);

fn submit_score(backend: Res<HttpClient>, mut commands: Commands) {
    let id = backend.post_json::<Rank>("/scores", &Score { level: 3, points: 12_500 });
    commands.insert_resource(Submitting(id));
}

fn show_rank(mut answers: MessageReader<JsonResponse<Rank>>, submitting: Option<Res<Submitting>>) {
    let Some(submitting) = submitting else { return };
    for answer in answers.read().filter(|a| a.id == submitting.0) {
        match &answer.result {
            Ok(rank) => info!("you are #{}", rank.rank),
            Err(error) => warn!("score not submitted: {error}"),
        }
    }
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::new(HttpConfig::new("https://api.example.com/v1"))))
        .add_json_response::<Rank>()
        .add_systems(Startup, submit_score)
        .add_systems(Update, show_rank)
        .run();
}
```

## How to use it

### 1. Configure the backend

`HttpConfig` holds the settings; `BackendPlugin::new(config)` inserts it as a resource.

```rust
use std::time::Duration;
use bevy_net_backend::{BackendPlugin, HttpConfig};

let config = HttpConfig::new("https://api.example.com/v1") // paths are appended to this
    .with_timeout(Duration::from_secs(10))                   // whole call; default 15 s
    .with_header("X-Game-Version", "1.4.2")                  // sent with every request
    .with_workers(2)                                         // worker threads; default 2, 1..=8
    .with_max_body_bytes(2 * 1024 * 1024);                   // default 10 MiB
assert!(config.validate().is_ok());
let plugin = BackendPlugin::new(config);
# let _ = plugin;
```

| Setting | Default | Read |
|---|---|---|
| base URL (`new`, `with_base_url`, `set_base_url`) | none: requests fail with `InvalidRequest` until set | per request |
| `with_timeout` | 15 s (`DEFAULT_TIMEOUT`), clamped to 1 ms ..= 1 h | per request |
| `with_header` / `without_header` | `User-Agent: bevy_net_backend/0.1.0` | per request |
| `allow_insecure_http` | `false` | per request |
| `with_max_body_bytes` | 10 MiB (`DEFAULT_MAX_BODY_BYTES`), at least 1 | per request |
| `with_workers` | 2 (`DEFAULT_WORKERS`), clamped to 1 ..= 8 (`MAX_WORKERS`) | once, at plugin build |

The base URL must be `http://` or `https://` with a host, and have no user name / password,
query or fragment. `HttpConfig::validate()` tells you what is wrong; the plugin logs it as a
warning. Change settings at runtime on the resource, for example after reading the game's own
settings file:

```rust
use bevy::prelude::*;
use bevy_net_backend::HttpConfig;

fn use_staging(mut config: ResMut<HttpConfig>) {
    config.set_base_url("https://staging.example.com/v1");
}
# let _ = use_staging;
```

### 2. Typed JSON requests

Register every answer type once, then use the typed calls. `T` is any
`serde::de::DeserializeOwned + Send + Sync + 'static` type; the request body is anything
`serde::Serialize`.

```rust
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Clone, Debug)]
struct Inventory {
    items: Vec<String>,
}

#[derive(Serialize)]
struct Craft<'a> {
    recipe: &'a str,
}

fn requests(backend: Res<HttpClient>) {
    // GET  /inventory               -> JsonResponse<Inventory>
    backend.get_json::<Inventory>("/inventory");
    // POST /craft  {"recipe":"axe"}  -> JsonResponse<Inventory>
    backend.post_json::<Inventory>("/craft", &Craft { recipe: "axe" });
    // Any method, with query, headers and a timeout -> JsonResponse<Inventory>
    let request = OutgoingRequest::patch("/inventory").with_query("merge", "true").with_json(&["torch"]);
    backend.send_json::<Inventory>(request);
}

let mut app = App::new();
app.add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")))
    .add_json_response::<Inventory>()
    .add_systems(Update, requests);
```

- The typed calls add `Accept: application/json` unless you set `Accept` yourself (Laravel
  answers validation errors as JSON only with it). `with_json` / `post_json` set
  `Content-Type: application/json`.
- A 2xx body is decoded into `T`. An empty body decodes as JSON `null`, so `()` and `Option<T>`
  accept a `204 No Content`.
- A type that was never registered is not sent: the request is answered on `HttpResponse`
  with `InvalidRequest` (naming the missing `add_json_response`) and an error is logged.
- A body that cannot be serialized is answered with `Encode` and never sent.

### 3. Raw requests and full control

Raw calls answer with `HttpResponse` (status, headers, bytes):

```rust
use bevy::prelude::*;
use bevy_net_backend::http::Method;
use bevy_net_backend::prelude::*;
use std::time::Duration;

fn raw(backend: Res<HttpClient>) {
    backend.get("/health");
    backend.request(Method::PUT, "/avatar", Some(vec![0x89, b'P', b'N', b'G']));
    backend.send(
        OutgoingRequest::post("/telemetry")
            .with_header("Content-Type", "text/csv")
            .with_body(b"frame,ms\n1,16.6\n".to_vec())
            .with_timeout(Duration::from_secs(30)),
    );
}
# let _ = raw;
```

`OutgoingRequest` has `new(method, path)` and `get` / `post` / `put` / `patch` / `delete`, plus
`with_query`, `with_header`, `with_body`, `with_json`, `with_timeout`, `without_credentials`.
Paths start with `/` and are appended to the base URL; absolute URLs and a `?` in the path are
refused (use `with_query`, which percent-encodes names and values). So is anything a server
could resolve outside the base URL's path, at every level of percent-decoding: `.` / `..`
segments (split on `/` and `\`, `..;` included), backslashes, encoded separators (`%2F`, `%5C`)
and control characters (`%00` included). A path is sent as
written: percent-encode user text you put into it. The methods sent are the standard ones except
`CONNECT` (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD` without a body, `OPTIONS`, `TRACE`).
An invalid header, method or path does not panic: the request is answered with `InvalidRequest`
and never sent.

### 4. Reading answers and errors

```rust
use bevy::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
struct Save {
    id: u64,
}

/// A typical error body (Laravel: `message` + `errors`).
#[derive(Deserialize, Debug)]
struct ApiError {
    message: String,
}

fn on_save(mut answers: MessageReader<JsonResponse<Save>>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(save) => info!("saved as #{}", save.id),
            Err(BackendError::Status(response)) if response.status == StatusCode::UNPROCESSABLE_ENTITY => {
                let message = response.json::<ApiError>().map(|e| e.message).unwrap_or_default();
                warn!("the server refused the save: {message}");
            }
            Err(error) if error.status() == Some(StatusCode::UNAUTHORIZED) => warn!("log in again"),
            Err(BackendError::Timeout(_) | BackendError::Network(_)) => warn!("offline? try again"),
            Err(error) => error!("save failed: {error}"),
        }
    }
}
# let _ = on_save;
```

| Error | When | Sent? |
|---|---|---|
| `InvalidRequest(why)` | bad path, header, base URL, unregistered JSON type, credentials that cannot apply | no |
| `InsecureHttp { host }` | plain text (`http://`) to a non-loopback host without `allow_insecure_http` | no |
| `Encode(why)` | the JSON body cannot be serialized | no |
| `Network(why)` | DNS, connect, reset, protocol error, worker threads not starting (ureq's words) | no for a DNS / connect / worker-start failure, else maybe |
| `Tls(why)` | TLS failure: handshake, certificate (rustls' / ureq's words) | no for a handshake or certificate failure (before any request byte), else maybe |
| `Timeout(why)` | the timeout ran out; it counts from hand-over to the transport, waiting for a free worker included | no if `why` starts with `not sent:`, else maybe |
| `BodyTooLarge { limit }` | the ANSWER is over its limit: the response body (SSH: the command's output or an SFTP download) | yes |
| `RequestTooLarge { limit, size }` | the REQUEST is over its limit and was refused before anything was sent: a multipart form over `Multipart::with_max_bytes`, a WebSocket request over the message limit, an SSH command line over 64 KiB, an SFTP upload over the transfer limit | no |
| `Status(response)` | a status outside 200–299, 3xx included (redirects are not followed) | yes |
| `Decode { message, response }` | a 2xx body that is not the expected JSON | yes |
| `Cancelled` | `HttpClient::cancel` | no if it was still waiting for a worker, else maybe |
| `Shutdown` | the app exited (`AppExit`) first | no, unless it was already on the wire before the exit frame |
| `NoTransport` | no `HttpTransportRes`, or it was removed / replaced first | no if it was still waiting for a worker, else maybe |
| `Disconnected { reason, sent }` | WebSocket and SSH: the connection went away, was closed by the game, or never opened (see [WebSocket](#10-websocket-connections-feature-ws), [SSH](#11-ssh-commands-and-sftp-feature-ssh-admin--dev-builds-only)) | as `sent` says: `Some(true)` it went out before, `Some(false)` never, `None` unknown |
| `Closed { code, reason }` | WebSocket only, on `WsStateChanged` / `WsConnectionInfo`: the server closed with a close frame | – |
| `Rejected(rejection)` | WebSocket only: the server answered the request with an error; `rejection.bytes()` / `text()` / `json()` (`Debug` / `Display` do not show it) | yes |
| `HostKey { host, fingerprint, problem }` | SSH only: the server's host key is unknown, changed or revoked (`HostKeyProblem`) | no |
| `AuthFailed(why)` | SSH only: no configured key was accepted (or none could be loaded) | no |
| `Ssh(why)` | SSH only: a protocol error, a refused channel / exec / subsystem, an SFTP status (the server's words) | see `SshFinished::started` |

`error.was_sent()` sums the column up: `Some(false)` never sent, `Some(true)` the server has it
(or had it before a loss), `None` maybe.

"Waiting for a worker" is the `UreqTransport` queue: a request answered while it waits there is
never sent afterwards. One already on the wire may still reach the server; its result is
discarded.

`BackendError` is `#[non_exhaustive]`: keep a catch-all arm. `error.status()` and
`error.response()` give the server's answer for `Status` and `Decode`; `RawResponse` has
`status`, `headers`, `body`, `text()` and `json::<E>()`. `error.retry_after()` is the `Retry-After`
header of a `Status` answer (delta-seconds, e.g. a 429 or 503) as a `Duration`, else `None`. `Display` of every error is safe to log:
it never shows a body, a header value or a query. The `message` field of `Decode` is serde_json's
text and may quote part of the body (a token, say); the crate never logs it, and neither should a
release build.

### 5. Logging in: credentials

Log in with an ordinary request, then store the token. From then on every request carries it
(applied last, after the config's default headers and the request's own headers):

```rust
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Login<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Deserialize, Clone, Debug)]
struct Token {
    token: String,
}

fn log_in(backend: Res<HttpClient>) {
    // The login call must not carry an old token.
    let request = OutgoingRequest::post("/login")
        .with_json(&Login { email: "player@example.com", password: "from-the-login-form" })
        .without_credentials();
    backend.send_json::<Token>(request);
}

fn on_login(mut answers: MessageReader<JsonResponse<Token>>, mut credentials: ResMut<BackendCredentials>) {
    for answer in answers.read() {
        if let Ok(login) = &answer.result {
            credentials.set(BearerToken::new(login.token.clone()));
        }
    }
}

fn log_out(mut credentials: ResMut<BackendCredentials>) {
    credentials.clear();
}
# let _ = (log_in, on_login, log_out);
```

| Credentials | Sends | Typical backend |
|---|---|---|
| `BearerToken::new(token)` | `Authorization: Bearer <token>` | Laravel Sanctum / Passport, JWT APIs, most Node / Go APIs |
| `ApiKeyHeader::new("X-Api-Key", key)` | `X-Api-Key: <key>` (any header name) | API gateways, simple game servers |
| `ApiKeyQuery::new("api_key", key)` | `?api_key=<key>` | APIs that only take a query key |
| `JsonBodyField::new("token", token)` (feature `json`) | `"token": "<token>"` added to JSON-object bodies | APIs that read the token from the body |

Anything else is one small trait:

```rust
use bevy_net_backend::{Credentials, OutgoingRequest, Secret};
use bevy_net_backend::http::HeaderValue;

/// Two headers: a player id and a session key.
struct Session {
    player: String,
    key: Secret,
}

impl Credentials for Session {
    fn apply(&self, request: &mut OutgoingRequest) {
        match (HeaderValue::try_from(self.player.as_str()), HeaderValue::try_from(self.key.expose())) {
            (Ok(player), Ok(mut key)) => {
                key.set_sensitive(true);
                request.headers_mut().insert("x-player", player);
                request.headers_mut().insert("x-session-key", key);
            }
            // Refused requests are answered with InvalidRequest; never put the secret in the reason.
            _ => request.reject("the session is not a valid header value"),
        }
    }
}
```

- `Secret` prints as `<redacted>` in `Debug` and `Display`; read it with `expose()`. When it is
  dropped, its whole allocation is overwritten with zeros first (the `zeroize` crate, which
  rustls already uses), and so are the crate's temporary `Bearer …` header text and the SSH key
  file text it reads. Not wiped: the copies that become part of a request (header values, the
  query value of `ApiKeyQuery` and the URL built from it, the body `JsonBodyField` writes,
  keyboard-interactive answers and passwords handed to russh, the first-message authentication
  frame), the `String` you built the secret from, and what the HTTP, WebSocket and SSH libraries
  copy while sending.
  `BackendCredentials`, `BearerToken`, `ApiKeyHeader`, `ApiKeyQuery` and `JsonBodyField` never
  show the secret in `Debug`; `OutgoingRequest` and `PreparedRequest` print header names but no
  header values, no query values and no body; `RawResponse` prints the body's length only.
- The crate's own log lines never contain a header value, a query string or a body.
- **Dependency logs at `trace` contain secrets.** At `trace` level the HTTP client logs raw request
  and response bytes (`ureq_proto`) and full paths with queries (`ureq`): `Authorization` headers,
  login bodies, tokens in answers. Keep those targets below `trace`, e.g.
  `RUST_LOG=trace,ureq=debug,ureq_proto=debug`, or in Bevy
  `LogPlugin { filter: "wgpu=error,naga=warn,ureq=debug,ureq_proto=debug".into(), ..default() }`.
  Bevy's default level (`info`) is safe.
- **`ApiKeyQuery` secrets end up in access logs.** A query key is part of the URL, so reverse
  proxies and servers log it: in testing Caddy's access log showed `api_key=…` (and an
  `X-Api-Key` header) in plain text while it masked `Authorization`. Prefer `BearerToken` (or a
  header your proxy redacts) wherever your API allows it.
- Compatibility rule: methods are only ever added to `Credentials` with a default
  implementation.
- Storing the token between sessions (keyring, file) and refreshing it are the game's job. A
  WebSocket connection can wait for that refresh after the server refused its token
  (`WsSettings::with_credentials_refresh`, see [WebSocket](#10-websocket-connections-feature-ws)).

### 6. Cancel, in-flight tracking, app exit

```rust
use bevy::prelude::*;
use bevy_net_backend::prelude::*;

#[derive(Resource)]
struct Search(RequestId);

fn new_search(backend: Res<HttpClient>, old: Option<Res<Search>>, mut commands: Commands) {
    if let Some(old) = old {
        backend.cancel(old.0); // answered with Cancelled; a late result is discarded
    }
    commands.insert_resource(Search(backend.get("/search")));
}

fn spinner(in_flight: Res<InFlight>) {
    if !in_flight.is_empty() {
        // show "saving…"; in_flight.contains(id), len(), ids(), describe(id) also exist
    }
}
# let _ = (new_search, spinner);
```

- **Cancel:** answered with `Cancelled` in the next frame's `First`. A request already on the
  wire keeps its worker thread until it finishes or times out (a blocking call cannot be
  interrupted); its result is discarded. Cancelling an answered id does nothing.
- **InFlight** lists every request still waiting for its answer: HTTP, WebSocket (feature `ws`) and SSH /
  SFTP (feature `ssh`) alike. An
  HTTP request enters it in `PostUpdate` of the frame it was made in; a WebSocket request there
  too, also while it waits for its connection. A request leaves it when it is answered; the answer
  message follows in `First` (of the same frame, or of the next one for answers decided in
  `PostUpdate`, such as a cancel). `describe(id)` returns a `RequestInfo`: `kind`
  (`Http`, `WebSocket`, `Ssh` or `Sftp`), `method` (HTTP), `target` (the path without the query,
  or the connection's name; never an SSH command line).
- **One cancel for everything:** `HttpClient::cancel(id)` cancels HTTP, WebSocket and SSH / SFTP
  requests alike (`WsClient::cancel` and `SshClient::cancel` are the same call).
- **App exit:** in the frame an `AppExit` message is written, nothing new is sent:
  `BackendSystems::Send` hands no request to the transport, and `BackendSystems::Exit` (in `Last`)
  answers every open request with `Shutdown` (results that already arrived are delivered as they
  are) and stops the worker threads without waiting for busy ones. **A request answered `Shutdown`
  was never sent**, except one that was already on the wire before that frame (it may still reach
  the server). Systems ordered after `BackendSystems::Exit` in `Last` can read those answers.
  Write `AppExit` before `BackendSystems::Send` (anywhere in `Update` or earlier is fine; a
  `PostUpdate` writer must be ordered `.before(BackendSystems::Send)`): written after that, requests
  of that frame may still go out, and written after `Exit` in `Last` it is seen by nobody in this
  crate.
- **Save on quit:** send the save, wait for its answer (`Ok` or an error), and only then write
  `AppExit`. A save fired in the same frame as `AppExit` is answered `Shutdown` and never sent.

### 7. Plain http:// for local development

`http://localhost`, `http://127.x.x.x` and `http://[::1]` work out of the box, for
`php artisan serve`, `npm run dev`, `go run .` and the like. Any other plain `http://` host is
refused with `InsecureHttp` (nothing is sent), because plain HTTP shows tokens to everyone on
the path. For a dev server on a trusted LAN:

```rust
use bevy_net_backend::HttpConfig;

let config = HttpConfig::new("http://192.168.1.20:8000/api").allow_insecure_http(true);
# let _ = config;
```

Loopback requests never use a proxy. Other requests use the proxy from `HTTPS_PROXY` /
`HTTP_PROXY` / `ALL_PROXY` (with `NO_PROXY`) when set (ureq's default).

### 8. Testing your game without a server

Insert a `FakeHttpTransport` before (or after) the plugin. It answers from scripted routes and
records every request, so your game's systems can be tested headless and offline:

```rust
use bevy::prelude::*;
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::prelude::*;
use bevy_net_backend::{HttpTransportRes, FakeHttpTransport, RawResponse};

let fake = FakeHttpTransport::new();
fake.route(Method::GET, "/v1/me", Ok(RawResponse::new(StatusCode::OK, r#"{"name":"Ayla"}"#)));

let mut app = App::new();
app.add_plugins(MinimalPlugins)
    .insert_resource(HttpTransportRes::new(fake.clone()))
    .add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com/v1")));

let id = app.world().resource::<HttpClient>().get("/me");
app.update(); // PostUpdate: handed to the fake
app.update(); // First: answered, readable in Update

let (_, request) = fake.last_request().expect("a request");
assert_eq!(request.uri.to_string(), "https://api.example.com/v1/me");
let answers = app.world().resource::<Messages<HttpResponse>>();
let mut cursor = answers.get_cursor();
let answer = cursor.read(answers).find(|a| a.id == id).expect("an answer");
assert_eq!(answer.result.as_ref().map(|r| r.text()).ok(), Some(r#"{"name":"Ayla"}"#.to_string()));
```

- `route(method, path, result)`: every matching request (method + URL path, newest route first)
  is answered with a copy of `result` on the next poll. `result` can be an error, e.g.
  `Err(BackendError::Network("reset".into()))`.
- Unrouted requests wait: answer them with `reply(id, result)`, or let the plugin's deadline
  answer them with `Timeout`. `waiting()`, `requests()`, `last_request()`, `cancelled()`,
  `shutdown_count()` let tests assert what the game sent; a form with files from disk is in
  `PreparedRequest::streaming_body` (`read_all()` gives its bytes). `progress(id, sent, total)`
  scripts an `HttpProgress` message.
- The crate's own tests use `bevy_headless_test`'s strict `TestApp` (ambiguity detection on every
  main schedule); the plugin's systems are ordered, and `Res<HttpClient>` never conflicts.

### 9. Your own transport

`HttpTransport` is the seam: `submit(id, PreparedRequest)` (never block), `poll()` (every result
since the last call), and optionally `cancel(id)`, `shutdown()`, `poll_progress()` (upload
progress for requests with `upload_progress`) and `streams_bodies()` (`true` if it sends a
`PreparedRequest::streaming_body`; with the default `false` the plugin answers such a request
`InvalidRequest` and never hands it over). Wrap it in `HttpTransportRes::new(..)` and insert it;
the plugin keeps doing all the bookkeeping (deadlines, cancel, exit, status and body-limit rules).
Report each request at most once; a late or unknown result is discarded. Compatibility rule:
methods are only ever added to `HttpTransport` with a default implementation.

### 10. WebSocket connections (feature `ws`)

For live data (chat, lobbies, match events, server pushes) open a **named** WebSocket
connection. Most games open one, `"main"`; a game that needs more simply opens another name.
Everything is keyed by that name: the state in `WsConnections`, the messages, the requests.

```toml
bevy_net_backend = { version = "0.1.0", features = ["ws"] }
```

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

/// A request: `{"id":7,"type":"chat.send","data":{"text":…}}` → `{"id":7,"ok":true,"data":{…}}`.
#[derive(Serialize)]
struct ChatSend {
    text: String,
}

#[derive(Deserialize, Clone, Debug)]
struct ChatAck {
    accepted: bool,
}

impl WsRequest for ChatSend {
    type Response = ChatAck;
    const KIND: &'static str = "chat.send";
}

/// A server push: `{"type":"chat.message","data":{"from":…,"text":…}}`.
#[derive(Deserialize, Clone, Debug)]
struct ChatMessage {
    from: String,
    text: String,
}

impl WsPushMessage for ChatMessage {
    const KIND: &'static str = "chat.message";
}

fn connect(ws: Res<WsClient>) {
    ws.connect("main", WsSettings::new("wss://game.example.com/ws"));
}

fn on_state(mut changes: MessageReader<WsStateChanged>, ws: Res<WsClient>) {
    for change in changes.read() {
        if change.state == WsState::Connected {
            ws.request(&change.name, &ChatSend { text: "hello".into() });
        }
    }
}

fn on_chat(mut pushes: MessageReader<WsPush<ChatMessage>>, mut acks: MessageReader<WsResponse<ChatAck>>) {
    for push in pushes.read() {
        info!("[{}] {}: {}", push.name, push.data.from, push.data.text);
    }
    for ack in acks.read() {
        if let Err(error) = &ack.result {
            warn!("chat.send failed: {error}");
        }
    }
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::default()))
        .add_ws_request::<ChatSend>()
        .add_ws_push::<ChatMessage>()
        .add_systems(Startup, connect)
        .add_systems(Update, (on_state, on_chat))
        .run();
}
```

- **`WsClient`** (a resource, `Res<WsClient>`, no ordering needed): `connect(name, settings)`,
  `disconnect(name)`, `send_text` / `send_binary` / `send` (fire and forget), `request::<R>`
  (typed, feature `json`), `request_raw(name, WsOutgoing)`, `cancel(id)`. Applied in
  `PostUpdate` (`BackendSystems::Send`).
- **Messages out**, all written in `First` of the frame they arrive and all carrying the
  connection's `name`: `WsStateChanged { name, state, error }`, `WsMessage { name, frame }` (every
  data frame the server sends, text or binary), `WsResponse<T>` / `WsRawResponse` (answers), and
  `WsPush<P>` (typed pushes).
- **State:** `WsConnections` (resource): `state(name)`, `is_connected(name)`, `get(name)` →
  `WsConnectionInfo` (state, failed attempts, last error, pending requests, queued frames).
  `WsState` is `Connecting`, `Connected`, `Reconnecting { attempt, retry_in }`,
  `WaitingForCredentials` (see below) or `Disconnected` (`#[non_exhaustive]`). Not Bevy
  `States`: a game maps it to its own states if it wants.
- **`WsSettings`** (builder): read timeout (default 20 ms, 5–250 ms; it is also roughly the
  latency added to every frame you send, because the thread sends between reads: measured median
  request round trips through a TLS proxy were 30 ms at 5 ms, 43 ms at the default 20 ms and
  118 ms at 100 ms, against about 20 ms for HTTP), connect timeout (10 s, ONE
  deadline for TCP + TLS + handshake, at most 1 h), heartbeat (ping every 15 s, dead after 45 s
  without a single byte, at most 1 h), request timeout (10 s), message limit (1 MiB, incoming and
  outgoing), reconnect policy, handshake headers, `allow_insecure_ws`, `without_credentials`, the
  protocol, outbox (64 frames), resend (32) and waiting (64 requests) limits, `with_auth_ack`,
  `with_credentials_refresh`.
- **Reconnect:** exponential backoff with full jitter (`WsReconnect`: base 500 ms, cap 30 s,
  optional `with_max_attempts`, reset after 10 s connected, `never()`). Each attempt is a
  `WsStateChanged` with `Reconnecting { attempt, retry_in }` and the error that caused it. A
  handshake refused with `429` / `503` and a `Retry-After` header waits at least that long (above
  the cap too, at most `MAX_TIMEOUT`).
  **Permanent (not retried, the connection goes `Disconnected` with the error):** a `401` / `403`
  handshake, a TLS / certificate error (unless `WsReconnect::with_tls_retry(true)`), a close code
  4000–4099, a refused first-message auth, a missing auth acknowledgement (`with_auth_ack`),
  invalid settings or a refused plain `ws://` URL, `disconnect`, exhausted attempts. Everything
  else (connect failures, drops, timeouts, dead peers, 5xx) is retried. With
  `with_credentials_refresh`, a `401` handshake, a refused first-message auth and the close codes
  you list first wait for refreshed credentials (below).
- **Credentials** from `BackendCredentials` go on **every** handshake (headers and query; the
  request's purpose is `RequestPurpose::WebSocketHandshake`). A token changed while connected is
  used on the next reconnect (no forced reconnect). `JsonBodyField` cannot authenticate a
  handshake (it has no body): that is a clear `InvalidRequest`; use first-message auth instead:
  `Credentials::ws_auth_message()` returns a text frame sent first on every connection. Requests
  follow it at once (frames on one socket stay in order); a server that authenticates
  asynchronously can opt in to `WsSettings::with_auth_ack(timeout)`: then nothing else goes out
  until the protocol reports `WsIncoming::AuthOk` (`{"type":"auth.ok"}` with `JsonEnvelope`); without
  it in time the waiting requests are answered `Timeout` (honest about an earlier send), the link
  closes with 1008 and the connection goes `Disconnected` with that error.
- **Refreshing credentials** (off by default; `WsSettings::with_credentials_refresh`): when the
  server refuses the credentials (a `401` handshake, a refused first-message auth, or a close
  code you list with `WsCredentialsRefresh::with_close_code`, e.g. a server's 4001 for a revoked
  session), the connection goes `WaitingForCredentials` and the plugin writes ONE
  `WsCredentialsRefused { name, error }` message, however many connections were refused with
  the same credentials. Your game refreshes with its own call and sets the new credentials;
  every waiting connection then makes ONE new connection with them. A second refusal, cleared
  credentials, or no new credentials within the timeout (default 30 s) end it `Disconnected`
  with the server's refusal; a further refresh is allowed only after a connection stayed up for
  the reconnect policy's `stable_after`, so it never loops. If the credentials already changed
  since the refused handshake (your game refreshed on its own meanwhile), no message is written
  and the connection connects again at once. The crate never calls a refresh route itself, and
  never logs a token. Requests made while waiting wait (until their own timeout).

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use bevy_net_backend::WsCredentialsRefresh;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Refresh {
    refresh_token: String,
}

#[derive(Deserialize, Clone, Debug)]
struct Tokens {
    access_token: String,
    refresh_token: String,
}

/// Your stored refresh token.
#[derive(Resource)]
struct RefreshToken(String);

fn connect(ws: Res<WsClient>) {
    let settings = WsSettings::new("wss://game.example.com/ws").with_credentials_refresh(WsCredentialsRefresh::new().with_close_code(4001));
    ws.connect("main", settings);
}

/// One refresh per message; your own refresh route, without the refused token.
fn refresh(mut refused: MessageReader<WsCredentialsRefused>, backend: Res<HttpClient>, token: Res<RefreshToken>) {
    if refused.read().count() > 0 {
        let request = OutgoingRequest::post("/auth/refresh").with_json(&Refresh { refresh_token: token.0.clone() }).without_credentials();
        backend.send_json::<Tokens>(request);
    }
}

/// New tokens: set them (the waiting connections connect again); refused: log out.
fn store(mut answers: MessageReader<JsonResponse<Tokens>>, mut credentials: ResMut<BackendCredentials>, mut token: ResMut<RefreshToken>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(tokens) => {
                token.0 = tokens.refresh_token.clone();
                credentials.set(BearerToken::new(tokens.access_token.clone()));
            }
            Err(_) => credentials.clear(),
        }
    }
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::new(HttpConfig::new("https://game.example.com/api"))))
        .add_json_response::<Tokens>()
        .insert_resource(RefreshToken("from-the-login".into()))
        .add_systems(Startup, connect)
        .add_systems(Update, (refresh, store))
        .run();
}
```

- **Protocol:** with feature `json` the default is `JsonEnvelope`: requests
  `{"id":…,"type":…,"data":…}`, answers `{"id":…,"ok":true,"data":…}` or `{"id":…,"ok":false,"error":…}`
  (answered `BackendError::Rejected`), pushes `{"type":…,"data":…}` (a push may carry its own
  `id` as long as it has no `ok`), auth `{"type":"auth.ok"}` / `{"type":"auth.failed",…}`.
  Implement `WsProtocol` for another layout (payloads are bytes, so binary formats work);
  `without_protocol()` for raw frames only.
- **Plain `ws://`** only to loopback hosts unless `allow_insecure_ws(true)`, as for HTTP. TLS is
  the same rustls + ring setup as HTTP. No permessage-deflate compression.

**WebSocket answers ("Sent?" as for HTTP):**

| Answer | When | Sent? |
|---|---|---|
| response / `Rejected` | the server answered | yes |
| `Timeout("not sent: …")` | the connection (or the auth acknowledgement) did not come in time | no |
| `Timeout("no answer …")` | sent, no answer within the request timeout | yes |
| `Timeout("sent before the connection was lost, …")` | a resend request that went out, then the link dropped and it timed out waiting | yes (on the earlier link) |
| `Disconnected { sent, .. }` | the connection went away or was replaced / closed by the game; not connected when asked; too many waiting | as `sent` says |
| `Cancelled` | `cancel` | maybe |
| `Shutdown` | the app exited first | no, unless it went out before the exit frame |
| `InvalidRequest` / `Encode` | unknown connection, no protocol, unregistered type, bad payload | no |
| `RequestTooLarge { limit, size }` | the request is larger than the message limit | no |

Requests made while a connection is opening or reconnecting wait for it (until their timeout,
64 at most). A server close frame is `BackendError::Closed { code, reason }` in `WsStateChanged` /
`WsConnectionInfo::last_error` (`error.close_code()`).
When a connection drops, requests already sent are answered `Disconnected`, except those marked
`resend_on_reconnect` (a `WsOutgoing` option, or `WsRequest::resend_on_reconnect`), which are sent
again after the reconnect; the default is off. Frames sent while not connected wait in an outbox
(64 by default) and go out when the connection opens.

### 11. SSH commands and SFTP (feature `ssh`, admin / dev builds only)

> **Never ship SSH to players.** An SSH key (or access to an ssh-agent) inside a build you give
> to players is **shell access to your server for anyone who extracts it** — and extracting it is
> easy. SSH is for admin and developer tools that stay on your own machines: a deploy button in an
> editor build, a server console in an internal tool. Keys are loaded at runtime from the admin's
> machine; there is deliberately no way to pass key bytes. **In a release build
> (`debug_assertions` off) the plugin refuses every SSH request** (answered `InvalidRequest`,
> nothing connects) unless the tool opts in with
> `BackendPlugin::default().with_ssh(SshSettings::default().allow_in_release(true))`. Give the key
> a narrow account on the server (`command="…"`, `from="…"`, `no-pty`, `no-port-forwarding` in
> `authorized_keys`, a dedicated user with narrow sudo rights).

```toml
bevy_net_backend = { version = "0.1.0", features = ["ssh", "sftp"] }
```

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use bevy_net_backend::{SshAuth, SshCommand, SshTarget};
use std::time::Duration;

fn connect(ssh: Res<SshClient>) {
    ssh.connect(
        "build",
        SshTarget::new("build.example.com", "deploy")
            .with_auth(SshAuth::agent())
            .with_auth(SshAuth::key_file("/home/admin/.ssh/id_ed25519"))
            .with_known_hosts_file("/home/admin/.ssh/known_hosts"),
    );
    // Waits for the connection; runs once it is up.
    ssh.run("build", SshCommand::new("systemctl restart game-api").with_timeout(Duration::from_secs(30)));
}

fn show(mut output: MessageReader<SshOutput>, mut finished: MessageReader<SshFinished>, mut states: MessageReader<SshStateChanged>) {
    for change in states.read() {
        if let Some(error) = &change.error {
            warn!("`{}` is {:?}: {error}", change.name, change.state);
        }
    }
    for chunk in output.read() {
        info!("{} {:?}: {}", chunk.id, chunk.stream, chunk.text());
    }
    for answer in finished.read() {
        match &answer.result {
            Ok(exit) if exit.success() => info!("{} done", answer.id),
            Ok(exit) => warn!("{} exited with {:?} / signal {:?}", answer.id, exit.status, exit.signal),
            Err(error) => warn!("{} failed: {error} (started: {:?})", answer.id, answer.started),
        }
    }
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::default()))
        .add_systems(Startup, connect)
        .add_systems(Update, show)
        .run();
}
```

- **`SshClient`** (a resource, `Res<SshClient>`, no ordering needed): `connect(name, SshTarget)`,
  `disconnect(name)`, `run(name, command)` (a `&str`, `String` or `SshCommand`), `cancel(id)` (the
  shared cancel of every protocol), and with `sftp` the file operations below. Applied in
  `PostUpdate` (`BackendSystems::Send`). A command made while its connection is connecting waits
  for it.
- **Messages out**, all written in `First`: `SshStateChanged { name, state, error }`,
  `SshOutput { id, name, stream, data }` (stdout / stderr chunks as they arrive; 0..n per command,
  all before or in the same frame as its answer; a chunk can end in the middle of a line or a UTF-8
  character, `text()` decodes lossily), and **exactly one** `SshFinished { id, name, started,
  result }` per command. A non-zero exit status is still `Ok(SshExit { status, signal, … })`: the
  command ran; `exit.success()` checks for 0.
- **State:** `SshConnections` (resource): `state(name)`, `is_connected(name)`, `get(name)` →
  `SshConnectionInfo` (state, the server's host key fingerprint, last error, open requests,
  reconnect attempts). `SshState` is `Connecting`, `Connected`, `Reconnecting { attempt, retry_in }`
  or `Disconnected`. Every name passed to `connect` gets an entry, a refused one too
  (`Disconnected` with the error). Only open connections count against `with_max_connections`;
  beyond 256 remembered names the oldest `Disconnected` ones are forgotten.
- **Reconnect is OFF by default.** `SshTarget::with_reconnect(SshReconnect::default())` turns it on:
  exponential backoff with full jitter as for WebSocket (base 1 s, cap 30 s, optional
  `with_max_attempts`, the counter resets after 10 s connected). **A reconnect never re-runs a
  command:** commands running when the connection was lost are answered `Disconnected` with their
  honest `started`; commands that were never sent wait for the new connection (until their own timeout).
  Host key, authentication, protocol (`Ssh`) and invalid-settings errors are not retried. Without
  it, a lost connection goes `Disconnected` with the error; `connect` again.
- **Host keys are always checked.** The server's key must be in a known_hosts file
  (`with_known_hosts_file`, any number; default `~/.ssh/known_hosts` when neither a file nor a
  pinned fingerprint is given) or match a fingerprint pinned in code with
  `trust_host_key_fingerprint("SHA256:…")` (only pin a fingerprint you read on the server itself,
  e.g. `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`). An unknown, changed or `@revoked` key
  is `BackendError::HostKey { host, fingerprint, problem }` before anything is sent; there is no
  trust-on-first-use and nothing is ever written to known_hosts. The matcher follows OpenSSH:
  patterns with `*` / `?` / `!`, hashed hosts, `[host]:port` for other ports, `@revoked` (wins
  over pins too). As OpenSSH does, the key types already listed for the host are asked for first,
  and only a different key **of the same type** is `Changed`; a host listed only with another type
  (an old RSA line, say) is `Unknown` for the new type. `@cert-authority` lines are ignored, and
  a server that presents a host certificate is refused with an `Ssh` error.
- **Authentication** (`SshAuth`, tried in order): `key_file(path)`,
  `key_file_with_passphrase(path, passphrase)` (OpenSSH, PKCS#8 or PuTTY format, at most 256 KiB;
  the passphrase is a redacted `Secret`), `agent()` (Unix: `SSH_AUTH_SOCK`; Windows: the OpenSSH
  agent's pipe, then Pageant; certificate identities are skipped). ed25519 and ECDSA keys always;
  RSA keys with feature `ssh-rsa` (SHA-2 signatures only). **Keys are the recommendation.** For
  servers that need it, opt in to `SshAuth::password(secret)` or
  `SshAuth::keyboard_interactive(responder)` (multi-prompt flows such as a password plus a 2FA
  code; `SshPromptAnswers::new().answer_containing("password", pw).answer_containing("code", otp)`
  answers prompts by word, or implement `SshPromptResponder`; it runs on the SSH thread, must not
  block, at most 8 rounds; prompt texts are server-supplied). Collect the values from the admin at
  runtime; they are held in `Secret` and never logged or shown in `Debug`. If every method fails,
  the answer is `BackendError::AuthFailed` naming the methods tried (key file names, never paths
  or secrets).
- **`~/.ssh/config`:** `SshTarget::from_ssh_config("alias")` (or `from_ssh_config_file(path,
  alias)`) takes `HostName`, `Port`, `User`, `IdentityFile` and `ConnectTimeout` from it when
  connecting; settings given in code win. `Include` is followed by the crate itself, with limits
  (nesting depth 16 like OpenSSH, 64 files, 1 MiB for all files together; `~` and relative paths
  as OpenSSH, relative to `~/.ssh`; globs): a config that includes itself is an error, not a
  crash. Other keys (`Match`, `%` tokens, `ProxyJump` / `ProxyCommand`, `UserKnownHostsFile`)
  are ignored: the connection goes straight to the host, with the known_hosts files given in code.
- **`SshTarget`** (builder, per connection): port (22), user, auth, known_hosts files, pins,
  connect timeout (15 s: ONE deadline for TCP + key exchange + host key + authentication, a server
  that trickles bytes cannot stretch it), keepalive (every 15 s of silence, lost after 3 unanswered),
  command timeout (60 s), output limit (8 MiB), SFTP timeout (5 min) and transfer limit (256 MiB),
  channels (8 commands at once; more wait, counted in their timeout). `SshCommand`: `with_timeout`,
  `with_max_output_bytes`, `with_stdin(bytes)` (then end-of-file; without it stdin is closed at
  once). `with_reconnect`, `allow_terrapin_vulnerable` (below). `SshSettings` (plugin):
  `allow_in_release`, `with_max_connections` (16 open at once), `with_max_requests_per_connection`
  (256). The release guard is also built into `RusshTransport` itself
  (`RusshTransport::new().with_release_allowed(..)`; the plugin passes `allow_in_release` on), so
  calling the transport directly does not bypass it.
- **Command lines may hold secrets:** `SshCommand`'s `Debug` shows only lengths, and the crate
  never logs a command or its output. Remember that a command line is visible in the server's
  process list: pass a secret through `with_stdin` instead.
- **Security (Terrapin, CVE-2023-48795):** strict key exchange is always offered, and AES-GCM is
  the preferred cipher (it is not affected), so servers without strict key exchange (OpenSSH before
  9.6 without a distribution backport) still connect through AES-GCM. Only the truly vulnerable
  combination is refused: no strict key exchange AND ChaCha20-Poly1305 or CBC with an
  encrypt-then-MAC MAC negotiated (a server that offers nothing else); the `Ssh` error says why.
  `SshTarget::allow_terrapin_vulnerable(true)` (off by default, logged as a warning) accepts it
  anyway, for an old server you cannot update. SHA-1 `ssh-rsa` signatures are never used.
- **Requests on one connection run at the same time** (commands and SFTP operations alike): when
  one step depends on another (upload, then move), start it after the first one's answer.

**SSH answers:**

| Answer | When | `started` |
|---|---|---|
| `Ok(SshExit)` | the command ended (any exit status or signal) | `Some(true)` |
| `Timeout("not sent: …")` | no free channel, or the connection did not open in time | `Some(false)` |
| `Timeout(…)` otherwise | ran longer than its timeout: TERM signal sent, channel closed (**the remote process may keep running**, see below) | `Some(true)` (or `None` if the exec reply never came) |
| `Cancelled` | `cancel(id)`: TERM and close as for a timeout | `Some(true)` if it was running, `Some(false)` if it was still waiting, `None` in between |
| `BodyTooLarge { limit }` | its output went over the limit; it was stopped | `Some(true)` |
| `Disconnected { sent, .. }` | the connection went away / was closed / was replaced (also with reconnect on: a running command is never re-run); or not connected when asked | as `sent` |
| `Shutdown` | the app exited first (a command of the exit frame is never sent) | as far as known |
| `Ssh(…)` | the server refused the channel or the exec request | `Some(false)` |
| `InvalidRequest` | unknown connection, bad command (empty, NUL), too many requests, SSH disabled in a release build | `Some(false)` |
| `RequestTooLarge { limit, size }` | the command line is over 64 KiB, or (SFTP) an upload is over the transfer limit | `Some(false)` |

Stopping a remote command is best effort: SSH has no reliable kill. The crate sends a `TERM`
signal and closes the channel; a server may ignore the signal, and a process without a terminal
may keep running after its channel is gone. Commands that must stop should
have their own timeout on the server (`timeout 30 ./deploy.sh`).

#### SFTP (feature `sftp`)

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;

fn upload(ssh: Res<SshClient>) {
    ssh.upload("build", "releases/notes.txt", b"version 1.2.3".to_vec());
    ssh.upload_file("build", "target/release/server.tar.gz", "releases/server.tar.gz");
    ssh.list_dir("build", "releases");
}

fn done(mut finished: MessageReader<SftpFinished>, mut progress: MessageReader<SftpProgress>) {
    for step in progress.read() {
        info!("{}: {} of {:?} bytes", step.id, step.done, step.total);
    }
    for answer in finished.read() {
        match &answer.result {
            Ok(SftpOutcome::Listing(entries)) => info!("{} entries", entries.len()),
            Ok(outcome) => info!("{}: {outcome:?}", answer.id),
            Err(error) => warn!("{}: {error}", answer.id),
        }
    }
}
# let _ = (upload, done);
```

`upload(name, remote, bytes)`, `upload_file(name, local, remote)` (both create or truncate the
remote file), `download(name, remote)` (into memory: `SftpOutcome::Data`),
`download_file(name, remote, local)` (written as `<local>.part`, renamed over `local` only when
complete; the part file's name is unique, `<local>.<process>-<id>-<n>.part`, so nothing else is
overwritten; it is removed on failure, but a transfer killed after the 1 s grace can leave it),
`list_dir` (`Listing(Vec<SftpEntry>)`, sorted, at most 10 000 entries, names at most 4 KiB, 4 MiB
of names in total), `create_dir`, `remove_file`, `remove_dir` (empty directories), `rename`, or any
`SftpOp` with `sftp(name, op)`. Each gets exactly one `SftpFinished { id, name, started, result }`;
transfers also report `SftpProgress` (about 10 per second). Relative remote paths start in the
login directory. A download over `with_max_transfer_bytes` is `BodyTooLarge`; an upload over it
(from memory or from a file) is `RequestTooLarge`, refused before anything is sent
(`started: Some(false)`); a whole operation is bounded by `with_sftp_timeout`. An
interrupted upload can leave a partial remote file; so can a local file that grows or shrinks
during `upload_file`, which is answered `Ssh("the local file changed size …")`. Errors carry the server's SFTP status text
(`Ssh("SFTP: No such file")`). The SFTP channel is opened on first use and shared by the
connection's operations. Local files are read and written on tokio's small blocking pool, never on
the SSH thread itself.

**Downloads are pipelined:** reads of 64 KiB go out ahead of the answers, up to 1 MiB requested
or received and waiting to be written at once (16 reads in flight), and the answers are written in file
order. That window is all the memory a download to a file uses, whatever the file's size (a
download into memory holds the file, up to the transfer limit). A short read (fewer bytes than
asked, which servers may send) asks again for the rest; a file the server reports larger than
the transfer limit is refused before any data is read; the first error stops the download. Uploads
keep 16 writes of 32 KiB in flight. Each read or write may wait for its answer as long as the
whole operation (`with_sftp_timeout`, default 5 min), so a link has to move about 1 MiB within that
time (about 3.5 KB/s with the default). After a cancel or a timeout the remote file handle is closed
in the background, so a long-lived connection collects no stale handles.

> **Listed names are untrusted input.** A hostile or broken server can list `../../.bashrc`,
> `C:\Windows\evil.dll` or `a/b`. Never join `SftpEntry::name` into a local path: use
> `entry.safe_file_name()`, which returns `None` for anything that is not one plain file name
> (`..`, separators, drive letters, control characters, Windows device names, …). The crate never
> turns a listed name into a local path itself; `download_file` writes only where you tell it to.

### 12. File uploads (multipart)

Upload files the way a browser form does: `multipart/form-data` (RFC 7578). Part of `http`: no
extra feature, no new dependency, nothing to configure app-wide. Avatars, screenshots, replays,
bug reports and cloud saves go this way (SFTP is for admin tools only).

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
struct AvatarSaved {
    url: String,
}

fn upload_avatar(backend: Res<HttpClient>) {
    # let png_bytes: Vec<u8> = Vec::new();
    let form = Multipart::new()
        .text("display_name", "Ayla")
        .file("avatar", "avatar.png", "image/png", png_bytes); // bytes you already loaded
    backend.post_multipart_json::<AvatarSaved>("/me/avatar", &form);
}
# fn main() { App::new().add_json_response::<AvatarSaved>().add_systems(Update, upload_avatar); }
```

A file from disk, a JSON part and upload progress:

```rust,no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct SaveInfo {
    slot: u32,
    play_time_s: u64,
}

fn upload_save(backend: Res<HttpClient>) {
    let form = Multipart::new()
        .json("info", &SaveInfo { slot: 2, play_time_s: 7_380 }) // Content-Type: application/json
        .file_from_path("save", "slot2.sav", "application/octet-stream", "saves/slot2.sav") // read while it is sent
        .with_max_bytes(512 * 1024 * 1024);
    // A big upload needs a longer timeout than the default 15 s.
    backend.send(OutgoingRequest::post("/saves").with_multipart(&form).with_timeout(std::time::Duration::from_secs(600)));
}

fn show_progress(mut progress: MessageReader<HttpProgress>) {
    for step in progress.read() {
        info!("{}: {} of {:?} bytes", step.id, step.sent, step.total);
    }
}
# let _ = (upload_save, show_progress);
```

- **Builder:** `Multipart::new()`, `.text(name, value)`, `.file(name, filename, content_type,
  bytes)` (an empty content type means `application/octet-stream`; text parts have no content
  type, as in a browser), `.file_from_path(name, filename, content_type, path)` (a file read
  from disk while the request is sent), `.part(name, content_type, bytes)` (a non-file part with
  its own content type), `.json(name, &value)` (feature `json`: a part with
  `Content-Type: application/json`, as Spring's `@RequestPart` reads it; frameworks that read
  forms by name give you its value as text), repeated names allowed (`photos[]` twice),
  `.with_max_bytes(n)` (the whole encoded body, part headers included, default 32 MiB: a single
  file of exactly 32 MiB is just over it), `.with_max_parts(n)` (default 256, at most 10 000),
  `len()`, `is_empty()`, `encoded_len()` (files from disk count 0 there: they are measured when
  the request is sent).
- **Sending:** `HttpClient::post_multipart(path, &form)` (raw `HttpResponse`),
  `post_multipart_json::<T>(path, &form)` (typed, feature `json`), `send_multipart(method, path,
  &form)` (`PUT`, `PATCH`, …), or `OutgoingRequest::with_multipart(&form)` for headers, query and
  a longer timeout on a big upload (`with_timeout`). Everything else is as for any HTTP request:
  one answer, the shared `cancel`, `InFlight`, the plain-http rule, and credentials applied as
  headers or query (`BearerToken`, `ApiKeyHeader`, `ApiKeyQuery`). `JsonBodyField` cannot go into
  a form: such a request is answered `InvalidRequest` and not sent (use a header credential, or add
  the field to the form yourself). A form replaced afterwards by `with_json`, `with_body` or
  `set_body` is an ordinary request again.
- **Refused before sending** (answered, never sent, `was_sent() == Some(false)`): a form over
  `with_max_bytes` is `RequestTooLarge { limit, size }`; more parts than `with_max_parts`, an
  empty field name, an invalid content type, and a name or file name that **ends with a
  backslash** or contains a **control character** (NUL, TAB, …; CR and LF are escaped instead) are
  `InvalidRequest`. A trailing backslash would turn the closing quote into an escaped one for most
  parsers: in the live test every real framework lost such a part (dropped it, or turned the file
  into a text field).
- **Files from disk (`file_from_path`)** are opened, measured and read on the HTTP worker
  thread when the request goes out, 64 KiB at a time: the main thread does no file I/O and the
  file is never in memory as a whole (measured: a 256 MiB upload peaked at 12 MB for the whole
  test process). The body goes out with its exact `Content-Length`. A file that cannot be opened
  is `InvalidRequest` and a form over `with_max_bytes` `RequestTooLarge`, both before anything is
  sent; a file that changes size while it is sent cuts the request off with a `Network` error
  that says so. It needs a transport that streams bodies: `UreqTransport` and
  `FakeHttpTransport` do.
- **Bytes (`file`, `part`, `text`)** are copied once and scanned for the boundary on the calling
  thread (measured: about 4 ms per 10 MiB and 13 ms for 32 MiB in a release build on a desktop
  PC). **Memory:** while it is sent, the form and its encoded body both exist, so the peak is
  about twice the in-memory parts.
- **Upload progress:** a request with a form reports `HttpProgress { id, sent, total }` messages
  (bytes read for sending, at most about 10 per second, plus one when the whole body is out),
  written in `First` before that frame's answers and only while the request waits for its
  answer. `OutgoingRequest::with_upload_progress(bool)` turns it on for any body (or off for a
  form).
- **Encoding details:** a fresh 128-bit random boundary per request (`bnb-` + 32 hex digits, from
  the operating system through ring), checked not to occur in any in-memory part (a file read
  from disk is not scanned: 128 random bits, like a browser's boundary); CRLF line breaks; the
  header `Content-Type: multipart/form-data; boundary=…`; a fixed `Content-Length` (never
  chunked). In `Content-Disposition`, names and file names are escaped as browsers do (WHATWG
  HTML): `"` → `%22`, CR → `%0D`, LF → `%0A`, everything else (backslashes, non-ASCII as UTF-8)
  unchanged, no `filename*`. Text values are sent exactly as given (line breaks are not
  rewritten). `Debug` of a form shows names and sizes only, never file names or paths.

**How backends read the same upload** (`display_name` text + `avatar` file):

| Backend | Text field | File (name / type / size) | Repeated names |
|---|---|---|---|
| Laravel | `$request->input('display_name')` | `$request->file('avatar')`: `getClientOriginalName()`, `getClientMimeType()`, `getSize()` | name them `photos[]`: `$request->file('photos')` is an array |
| Plain PHP | `$_POST['display_name']` | `$_FILES['avatar']['name']`, `['type']`, `['size']`, `['tmp_name']`, `['error']` | `photos[]` (without `[]` only the last one is kept) |
| Express + multer | `req.body.display_name` | `upload.single('avatar')` → `req.file.originalname`, `.mimetype`, `.size`, `.buffer` | `upload.array('photos')`: the name must match exactly (`photos` or `photos[]`) |
| Go (`net/http`) | `r.FormValue("display_name")` after `r.ParseMultipartForm(max)` | `f, h, _ := r.FormFile("avatar")`: `h.Filename`, `h.Header.Get("Content-Type")`, `h.Size` | `r.MultipartForm.File["photos"]` (same name, no brackets needed) |
| Django | `request.POST['display_name']` | `request.FILES['avatar']`: `.name`, `.content_type`, `.size` | `request.FILES.getlist('photos')` |
| FastAPI | `display_name: str = Form()` | `avatar: UploadFile`: `.filename`, `.content_type`, `await avatar.read()` (needs `python-multipart`) | `photos: list[UploadFile]` |
| Spring | `@RequestParam("display_name") String` | `@RequestParam("avatar") MultipartFile`: `getOriginalFilename()`, `getContentType()`, `getSize()` | `@RequestParam("photos") List<MultipartFile>` |
| Rails | `params[:display_name]` | `params[:avatar]`: `original_filename`, `content_type`, `size` | `photos[]`: `params[:photos]` is an array |
| ASP.NET Core | `[FromForm] string display_name` | `IFormFile avatar`: `FileName`, `ContentType`, `Length` | `List<IFormFile> photos` |
| axum | the `Multipart` extractor: `field.name()`, `field.text().await` | `field.file_name()`, `field.content_type()`, `field.bytes().await` | every part is its own field; group them yourself |

Array naming: PHP, Laravel and Rails turn `photos[]` into an array (without brackets they keep
only the last value); the others read repeated names as a list and see `photos[]` literally as
the name. Use what your backend expects.

#### What real backends do with an upload

Measured by sending the same 38 upload scenarios from this crate to real servers with their
default settings: PHP 8.3 (stock `php.ini`), Express 4.21 + multer 2.0.2, FastAPI 0.115
(Starlette 0.46, python-multipart 0.0.20), Go 1.22 `net/http` and Django 5.2. **Every file that
arrived had the right size and CRC-32.** What differed was limits, file names and parts a
framework dropped:

| | PHP 8.3 | Express + multer 2.0.2 | FastAPI / Starlette | Go 1.22 | Django 5.2 |
|---|---|---|---|---|---|
| **Over a size limit** | a file over `upload_max_filesize` (2 MB): **200**, the file has `error` 1 and no data; a body over `post_max_size` (8 MB): **200 with an empty form** | a text field over 1 MB (`fieldSize`): **500** (`MulterError`, Express's default handler); `limits.fileSize` likewise 500 | a text part over 1 MiB: **400** | **no default limit** (33 MB accepted): the handler must set one (`http.MaxBytesReader`) | text over 2.5 MiB (`DATA_UPLOAD_MAX_MEMORY_SIZE`): **400** |
| **Many files** | keeps **20** (`max_file_uploads`) and **silently drops the rest** (200) | all 25 kept | all kept | all kept | more than 100 files: **400** |
| **Repeated `photos` without `[]`** | **only the last** is kept | all kept | all kept | all kept | all kept |
| **Non-ASCII names** (`mentés.json`, `😀`) | exact | **mojibake** in file names AND field names (read as latin1) | exact | exact | exact |
| **Empty file name** `""` | a file with `error` 4 (no file), no data | **a text field** | a file named `""` | **a text field** | **a text field** |
| **CSRF** | – | – | – | – | **403** without `@csrf_exempt`, whatever the body |

- **multer mojibake:** convert each name back with `Buffer.from(name, 'latin1').toString('utf8')`
  (checked exact). multer's `defParamCharset` option has no effect in multer 2.0.2.
- **`"` in a file name** arrives as a literal `%22` on every one of them (nothing decodes it).
- **Everything else was read the same everywhere:** text before and after files, `tags[]` twice,
  an empty value, CRLF inside a value, a 0-byte file, binary data containing `--` and CRLF lines,
  an empty form, 20 uploads in parallel.
- **Paths and backslashes in file names** are read three different ways, so send a plain name:

  | File name sent | PHP | multer | FastAPI | Go | Django |
  |---|---|---|---|---|---|
  | `a\b.png` | `b.png` | `b.png` | `a\b.png` | `a\b.png` | `b.png` |
  | `C:\Users\me\a.png` | `a.png` | `a.png` | `a.png` | `C:\Users\me\a.png` | `a.png` |
  | `dir/file.png` | `file.png` | `file.png` | `dir/file.png` | `file.png` | `file.png` |
  | `x\` (the crate refuses it) | `x"` | file dropped | `x\` | file dropped | file dropped |

**Documented, not live-tested** (from the frameworks' documentation):
- **ASP.NET Core:** Kestrel's `MaxRequestBodySize` defaults to 30,000,000 bytes, **below this
  crate's 32 MiB default**: lower `with_max_bytes` or raise the server limit. `FormOptions`
  allows 1024 form values by default.
- **Spring Boot:** `spring.servlet.multipart.max-file-size` 1 MB, `max-request-size` 10 MB.
- **Laravel:** PHP's limits above apply (Laravel answers `413` for a body over `post_max_size`).
- **Rails (Rack):** percent-decodes file names (`%22` becomes `"`) and accepts at most 128 files.
- **nginx** `client_max_body_size` 1 MB and **axum** `DefaultBodyLimit` 2 MB answer `413`.
- **CSRF protection** refuses a game's POST whatever its body: Laravel `web` routes (419), Rails
  `protect_from_forgery` (422), like Django's measured 403 above. Use API routes (Laravel
  `routes/api.php`), or exempt the endpoint, and authenticate with a token instead.

**Advice:**
- For PHP and Laravel, name repeated fields `photos[]`, and keep at most 20 files per form (or
  raise `max_file_uploads`).
- Keep file names ASCII-safe and plain: letters, digits, `-`, `_`, `.`; no path, no `\`, no `"`.
  Keep the original name in a text field if you need it.
- Set the server's limits explicitly (upload size, body size, file count, text field size) and
  keep `with_max_bytes` at or below them. **Do not rely on a `413`:** only proxies with a body limit
  (nginx's default 1 MB, Caddy's `request_body` when configured) and some frameworks send one. PHP answers **200 with missing data** (check
  `$_FILES[..]['error']` and that the fields arrived), multer 500, FastAPI and Django 400, Go
  whatever the handler decides. A `413` or any other status is a normal `Status` answer
  (`error.status()`); a server that closes the connection mid-upload is a `Network` error.
- Put text fields before files if the server streams files to disk: multer documents that its
  disk storage only sees the fields sent before a file (not measured here; the live test used
  memory storage, where the order did not matter).

## How it works

```text
 game system ──Res<HttpClient>──▶ queue ─┐
                                            │ PostUpdate  BackendSystems::Send
                                            ▼   defaults + credentials + URL checks
                                     InFlight map ──submit──▶ HttpTransport (UreqTransport:
                                            ▲                   N worker threads, one ureq Agent)
                                            │ First  BackendSystems::Receive
                             poll ◀─────────┘   deadlines, status + body-limit rules
                                            │
                                            ▼
                  HttpResponse / JsonResponse<T>  ──▶ PreUpdate / Update readers
```

- **One owner of every answer.** The plugin's `InFlight` map (ECS side) is the only thing that
  answers requests. The transport only reports; a result for an id that is no longer waiting is
  dropped. So every request gets exactly one answer, whatever the network does.
- **Scheduling.** `BackendSystems::Receive` runs in `First`, after Bevy's `TimeSystems` and
  before `MessageUpdateSystems`, so answers are readable in `PreUpdate` / `Update` of the frame
  they arrive. `BackendSystems::Send` runs in `PostUpdate`, so a request made in `Update` goes
  out the same frame (one made after it goes out next frame). A game system in `PostUpdate` that
  fires requests should be ordered `.before(BackendSystems::Send)` if the frame matters: both only
  read `HttpClient`, so the strict ambiguity check cannot flag the race. `BackendSystems::Exit` runs in
  `Last` only in a frame with `AppExit`. The sets are `#[non_exhaustive]` and phase-named:
  HTTP, WebSocket and SSH all run in the same three.
- **Threads.** ureq is blocking. `UreqTransport` starts its worker threads (named
  `net-backend-N`) on the first request and shares one `ureq::Agent` (keep-alive connection
  pool) between them. At most `workers` requests are on the wire; the rest wait in a queue, and
  that wait counts against their timeout. A request answered while it waits (cancel, timeout,
  exit, transport removed or replaced) is dropped from the queue, never sent. Worker results come
  back over a channel that `poll` drains without blocking. A panic inside the HTTP client is
  caught and answered as `Network` (with the usual unwinding panics; a game built with
  `panic = "abort"` aborts instead).
- **Timeouts.** A request's timeout counts from the moment it is handed to the transport. The
  worker gives ureq what is left of it (a request that waited its whole timeout in the queue is
  answered `Timeout("not sent: …")`). As a backstop the plugin answers `Timeout` itself when a
  request is still waiting after its timeout + 5 s (`DEADLINE_GRACE`, measured on `Time<Real>`,
  or a monotonic clock without `TimePlugin`). `Time<Real>` follows Bevy's
  `TimeUpdateStrategy`: with a `Manual*` strategy (replays, some headless servers) the backstop
  fires on that clock, early or late; ureq's own timeout always uses the wall clock.
- **Status codes.** The transport returns every status; the plugin turns anything outside
  200–299 into `Status`. Redirects are not followed (ureq `max_redirects(0)`), so a redirect can
  never downgrade `https://` to `http://` or carry credentials to another host.
- **Body limit.** The response body is read with a cap (`max_body_bytes`) on the bytes on the
  wire AND on the bytes after gzip decoding (feature `gzip`), so a gzip bomb stops at the limit;
  the plugin checks it again for any transport.
- **JSON decoding** runs on the main thread in `Receive`, in the frame the answer arrives. A
  multi-megabyte answer can cost that frame a few milliseconds; keep big payloads raw
  (`HttpResponse`) or small.
- **WebSocket threads (feature `ws`).** Each connection attempt runs on its own std thread
  (`net-backend-ws-link#N`): TCP, then rustls for `wss://`, then the tungstenite handshake, then a
  loop: send what the game queued, ping when due, flush, one read whose socket reads together
  stop after one read timeout (Windows reports a read timeout as `TimedOut`, Unix as
  `WouldBlock`; both mean "no data"), check for a dead peer (no byte for `dead_after`). The
  handshake (TCP + TLS + upgrade) runs under ONE deadline. Both limits sit under rustls and
  tungstenite, so a peer that trickles bytes can neither stretch the handshake nor starve
  outgoing frames and pings. An idle connection wakes about 50 times a second, and a frame you
  send goes out within about one read timeout, plus the time the socket needs for earlier
  outgoing data. The heartbeat runs in the thread, so it keeps going while the game does not
  tick. Reconnects, backoff, credentials and every answer live on the ECS side; a reconnect is a
  new thread. On exit a close (1001) is queued to every link and no thread is joined (a process
  that exits right away usually wins that race).
- **SSH thread (feature `ssh`).** russh needs tokio, so `RusshTransport` owns ONE std thread
  (`net-backend-ssh`) with a tokio *current-thread* runtime, started on the first `connect` and
  stopped when the app exits (or the transport is dropped); tokio's blocking pool is capped at 2
  threads (`net-backend-ssh-io`, for DNS lookups and decrypting key files). Nothing runs on the
  game's threads or Bevy's task pools, and without feature `ssh` tokio is not even compiled.
  Every connection is a task on that thread; every command or SFTP operation is a task of its
  connection with its own deadline. The socket of each connection sits behind a kill switch tied
  to its task, so a connection that times out or is closed never lingers in the background. On
  exit the plugin answers everything `Shutdown` first; then the thread closes each connection
  (≤ 1 s to say goodbye) and ends, and the game does not wait for it.
- **TLS.** rustls with ring's crypto and the Mozilla root certificates (webpki-roots; the OS
  certificate store is not used). The ring provider is always handed to ureq explicitly and never
  installed process-wide, so a game that also links another rustls provider (for example
  aws-lc-rs through another crate) neither changes this crate's TLS nor triggers rustls'
  provider-selection panic.

## TLS exception: the default build is not pure Rust

`bevy_net_backend` is written in Rust, but its HTTPS stack is **not pure Rust**. TLS is done by
rustls, whose cryptography comes from **ring**, and ring compiles C code and ships assembly. A C
compiler is needed at build time (MSVC, clang or gcc, which Rust toolchains normally have at
hand). ring was picked because it is mature and widely deployed, needs no CMake or NASM, and adds
no system library. The crate never uses OpenSSL, native-tls or aws-lc, in any feature set.

Without the `http` feature nothing of this is compiled (and no C either).

SSH (feature `ssh`) uses the same ring crate for its AEAD ciphers (ChaCha20-Poly1305, AES-GCM);
everything else in russh (key exchange, ed25519 / ECDSA, AES-CTR, HMAC) is RustCrypto. It never
uses OpenSSL, libssh2 or aws-lc either.

## API reference

Everything is re-exported at the crate root; `prelude` holds the everyday items.

| Item | Kind | What it is |
|---|---|---|
| `BackendPlugin` | plugin | `new(config)`, `with_config`, `with_ssh` (feature `ssh`), `default()`. Inserts config, client, in-flight map, credentials, `HttpResponse`, and (feature `http`) a `UreqTransport` unless an `HttpTransportRes` exists. |
| `BackendSystems` | system sets | `Receive` (`First`), `Send` (`PostUpdate`), `Exit` (`Last`, on `AppExit`). `#[non_exhaustive]`. |
| `HttpConfig` | resource | base URL, timeout, default headers, workers, `allow_insecure_http`, body limit; `validate()`, getters, `set_base_url`, `set_timeout`. |
| `ConfigError` | enum | `NoBaseUrl`, `BadBaseUrl`, `BadHeader`. |
| `HttpClient` | resource | `send`, `request`, `get`, `cancel`; `post_multipart`, `send_multipart` (feature `http`); with `json`: `send_json`, `get_json`, `post_json`, `post_multipart_json`, `is_json_registered`. |
| `Multipart` | builder (`http`) | `new`, `text`, `file`, `file_from_path`, `part`, `json` (feature `json`), `with_max_bytes`, `with_max_parts`, `len`, `is_empty`, `encoded_len`. `Debug` shows names and sizes only. |
| `StreamingBody`, `StreamingReader` | body | a body read while it is sent (a form with files from disk): `open()` (exact length + reader, on a transport's thread), `read_all()`, `max_bytes()`. |
| `HttpProgress` | message | upload progress: `id`, `sent`, `total`. |
| `DEFAULT_MULTIPART_MAX_BYTES`, `DEFAULT_MULTIPART_MAX_PARTS` | consts (`http`) | 32 MiB, 256. |
| `BackendAppExt` | trait on `App` | `add_json_response::<T>()` (feature `json`); `add_ws_request::<R>()`, `add_ws_push::<P>()` (features `ws` + `json`). Sealed. |
| `RequestId` | id | opaque, unique per process, `Copy + Eq + Hash + Ord + Display`. |
| `OutgoingRequest` | request | constructors, `with_*` builders, accessors (`method`, `path`, `query`, `headers`, `body`, `timeout`, `purpose`, `uses_credentials`, `is_multipart`, `streaming_body`, `upload_progress`, `error`), `query_mut`, `headers_mut`, `set_body`, `reject`, `with_upload_progress`; `with_multipart` (feature `http`). |
| `RequestPurpose` | enum | `Http`, `WebSocketHandshake`. |
| `HttpResponse` | message | `id`, `result: Result<RawResponse, BackendError>`. |
| `JsonResponse<T>` | message | `id`, `result: Result<T, BackendError>` (feature `json`). |
| `RawResponse` | struct | `status`, `headers`, `body`; `new`, `with_header`, `is_success`, `body()`, `text()`, `json()`. |
| `BackendError` | enum | see [Reading answers and errors](#4-reading-answers-and-errors); `status()`, `response()`, `retry_after()`, `is_invalid_request()`, `was_sent()`, `close_code()`, `host_key(..)`, `request_too_large(..)` (constructors for fakes). |
| `Credentials` | trait | `apply(&self, &mut OutgoingRequest)`; `ws_auth_message()` (default none: a first frame for WebSocket auth). |
| `BackendCredentials` | resource | `new`, `set`, `clear`, `is_set`. A WebSocket connection waiting for refreshed credentials connects again on `set` and ends on `clear`. |
| `BearerToken`, `ApiKeyHeader`, `ApiKeyQuery`, `JsonBodyField` | credentials | ready-made `Credentials` (`JsonBodyField`: feature `json`). |
| `Secret` | string | redacted in `Debug` / `Display`, overwritten with zeros when dropped; `new`, `expose`, `is_empty`. |
| `InFlight` | resource | HTTP, WebSocket and SSH: `contains`, `len`, `is_empty`, `ids`, `describe` (→ `RequestInfo`). |
| `RequestInfo` (struct), `RequestKind` (enum) | types | what a pending request is: `kind` (`Http`, `WebSocket`, `Ssh`, `Sftp`), `method`, `target` (never an SSH command line); `#[non_exhaustive]`. |
| `Rejection` | struct | the payload of `BackendError::Rejected`: `new`, `bytes`, `text`, `json` (json). |
| `HttpTransport` | trait | `submit`, `poll`, `cancel`, `shutdown`, `streams_bodies`, `poll_progress`. |
| `HttpTransportResult` | type | `Result<RawResponse, BackendError>`. |
| `HttpTransportRes` | resource | `new(transport)`. |
| `PreparedRequest` | struct | what a transport receives: `method`, `uri`, `headers`, `body`, `streaming_body`, `upload_progress`, `timeout`, `max_body_bytes`, `purpose`; `path()`, `is_loopback()`, `is_https()`. |
| `FakeHttpTransport` | transport | `new`, `route`, `clear_routes`, `reply`, `progress`, `requests`, `last_request`, `waiting`, `cancelled`, `shutdown_count`. |
| `UreqTransport` | transport | feature `http`: `new(&config)`, `workers()`. |
| `http` | crate | the `http` 1.x crate, re-exported (`Method`, `StatusCode`, `HeaderMap`, …). |
| `DEFAULT_TIMEOUT`, `MAX_TIMEOUT`, `DEFAULT_WORKERS`, `MAX_WORKERS`, `DEFAULT_MAX_BODY_BYTES`, `DEADLINE_GRACE` | consts | 15 s, 1 h, 2, 8, 10 MiB, 5 s. |
| `WsClient` | resource (`ws`) | `connect`, `disconnect`, `send`, `send_text`, `send_binary`, `request_raw`, `request` (json), `cancel` (the shared one). |
| `WsConnections`, `WsConnectionInfo` | resource / struct (`ws`) | `get`, `state`, `is_connected`, `iter`; info: `state`, `attempt`, `last_error`, `pending_requests`, `queued_frames`. |
| `WsName` | name (`ws`) | a connection's name; `From<&str>` / `From<String>`, `as_str`, compares with `&str`. |
| `WsState` | enum (`ws`) | `Connecting`, `Connected`, `Reconnecting { attempt, retry_in }`, `WaitingForCredentials`, `Disconnected`; `#[non_exhaustive]`. |
| `WsSettings`, `WsReconnect` | builders (`ws`) | per connection: timeouts, heartbeat, limits, reconnect, headers, `allow_insecure_ws`, `without_credentials`, protocol, `with_auth_ack`, `with_credentials_refresh`; backoff: base, cap, max attempts, stable-after, jitter, `never()`, `delay_bound`. |
| `WsCredentialsRefresh` | builder (`ws`) | `new`, `with_timeout` (default 30 s), `with_close_code`, `timeout`, `close_codes`. |
| `WsCredentialsRefused` | message (`ws`) | `name`, `error`: refresh the credentials (one per refused credentials). |
| `WsFrame`, `WsOutgoing` | types (`ws`) | a text / binary frame (`Debug` shows the length only); a raw request (`kind`, payload, `resend_on_reconnect`, `with_timeout`). |
| `WsStateChanged`, `WsMessage`, `WsRawResponse` | messages (`ws`) | state changes (with the error), every data frame, raw answers; all with `name`. |
| `WsRequest`, `WsPushMessage`, `WsResponse<T>`, `WsPush<P>`, `JsonEnvelope` | (`ws` + `json`) | typed requests (`Response`, `KIND`, `resend_on_reconnect`), typed pushes (`KIND`), their messages, the default protocol. |
| `WsProtocol`, `WsIncoming` | trait / enum (`ws`) | `encode_request`, `decode`, `retry_after_close`; `Response { wire_id, result: Result<bytes, bytes> }`, `Push`, `AuthOk`, `AuthFailed`, `Ignore`. |
| `DEFAULT_WS_READ_TIMEOUT`, `DEFAULT_WS_MAX_MESSAGE_BYTES` | consts (`ws`) | 20 ms, 1 MiB. |
| `WsTransport`, `WsTransportRes`, `WsLinkId`, `WsLinkEvent`, `WsHandshake` | seam (`ws`) | `open`, `send`, `close`, `poll`, `shutdown`; one link = one connection attempt. |
| `FakeWsTransport` | transport (`ws`) | `manual_accept`, `accept`, `reject_next`, `echo_envelope`, `push`, `drop_link`, `fail_link`, `opened`, `last_link`, `live_links`, `sent`, `all_sent`, `closed`, `shutdown_count`. |
| `TungsteniteTransport` | transport (`ws`) | the real one; `new()`. |
| `SshClient` | resource (`ssh`) | `connect`, `disconnect`, `run`, `cancel` (the shared one); with `sftp`: `upload`, `upload_file`, `download`, `download_file`, `list_dir`, `create_dir`, `remove_file`, `remove_dir`, `rename`, `sftp`. |
| `SshTarget` | builder (`ssh`) | `new(host, user)`, `from_ssh_config`, `from_ssh_config_file`, `with_port`, `with_user`, `with_auth`, `with_known_hosts_file`, `trust_host_key_fingerprint`, timeouts, keepalive, limits, `with_max_channels`, `with_reconnect`, `allow_terrapin_vulnerable`, `validate`, getters. |
| `SshAuth` | auth (`ssh`) | `key_file`, `key_file_with_passphrase`, `agent`; opt-in `password`, `keyboard_interactive`. `Debug` shows file names only, never secrets. |
| `SshPromptResponder`, `SshPromptAnswers`, `SshPromptRequest`, `SshPrompt` | auth (`ssh`) | keyboard-interactive: the responder trait (`respond(&request) -> Option<Vec<Secret>>`), a ready-made word-matching responder (`answer_containing`), one round of server prompts (`name`, `instructions`, `prompts`: `text`, `echo`). |
| `SshReconnect` | builder (`ssh`) | `with_base`, `with_cap`, `with_max_attempts`, `with_stable_after`, `with_jitter`, `delay_bound`; given with `SshTarget::with_reconnect`. |
| `SshCommand`, `SshExit`, `SshStream` | types (`ssh`) | a command (`new`, `with_timeout`, `with_max_output_bytes`, `with_stdin`; `From<&str>`); how it ended (`status`, `signal`, byte counts, `success()`); stdout / stderr. |
| `SshSettings` | builder (`ssh`) | plugin-wide: `allow_in_release`, `with_max_connections`, `with_max_requests_per_connection`, `is_allowed`. Given with `BackendPlugin::with_ssh`. |
| `SshConnections`, `SshConnectionInfo`, `SshState`, `SshName` | state (`ssh`) | as for WebSocket; info: `state`, `fingerprint`, `last_error`, `pending_requests`, `attempt`; `SshState::Reconnecting { attempt, retry_in }`. |
| `SshStateChanged`, `SshOutput`, `SshFinished` | messages (`ssh`) | state changes; output chunks; the one answer per command (`started`, `result`). |
| `SftpOp`, `SftpOutcome`, `SftpEntry`, `SftpEntryKind`, `SftpProgress`, `SftpFinished` | types / messages (`sftp`) | an operation, its outcome (`Uploaded`, `Downloaded`, `Data`, `Listing`, `Done`), a listing entry (`safe_file_name()`; `name` is untrusted), progress, the one answer. |
| `HostKeyProblem` | enum | `Unknown`, `Changed`, `Revoked` (in `BackendError::HostKey`). |
| `SshTransport`, `SshTransportRes`, `SshEvent`, `SshConnId` | seam (`ssh`) | `connect`, `run`, `sftp` + `supports_sftp` (default: none), `cancel`, `close`, `poll`, `shutdown`. |
| `FakeSshTransport` | transport (`ssh`) | `manual_connect`, `accept`, `reject_next`, `on_command`, `output`, `finish`, `drop_conn`, `on_next_sftp`, `sftp_progress`, `sftp_finish`, `connects`, `last_conn`, `live_conns`, `commands`, `running`, `sftp_ops`, `cancelled`, `closed`, `shutdown_count`. Never touches the network. |
| `RusshTransport` | transport (`ssh`) | the real one; `new()`, `with_release_allowed`. |
| `DEFAULT_SSH_CONNECT_TIMEOUT`, `DEFAULT_SSH_COMMAND_TIMEOUT`, `DEFAULT_SSH_MAX_OUTPUT_BYTES`, `DEFAULT_SFTP_TIMEOUT`, `DEFAULT_SFTP_MAX_BYTES`, `MAX_SSH_COMMAND_BYTES` | consts (`ssh`) | 15 s, 60 s, 8 MiB, 5 min, 256 MiB, 64 KiB. |

## Good to know

- **Platforms:** native Windows, Linux and macOS.
- **HTTP/1.1** (ureq). A response is read whole (up to the body limit) before it is delivered.
- **Redirects:** a 3xx arrives as `Status` with its `Location` header.
- **Each request is sent once.** Retrying, queueing while offline, caching and cookies are your
  game's decisions; the error kind and the "Sent?" column in
  [Reading answers and errors](#4-reading-answers-and-errors) tell you whether it may have been sent.
- **Credentials** are whatever the game puts into `BackendCredentials`; the game stores and
  refreshes them (a WebSocket connection can wait for that refresh, see
  [WebSocket](#10-websocket-connections-feature-ws)).
- **One base URL** per app (`HttpConfig`, changeable at runtime with `set_base_url`). Paths are
  relative to it; absolute URLs are refused.
- **A reverse proxy in front of your API must pass responses through unchanged**: no
  decompressing or recompressing on its own (Caddy: no `encode` directive for these routes; nginx:
  `gzip off`). The body limit and gzip handling assume the client sees exactly what your
  application sent; a proxy that re-encodes can turn a body under the limit into one over it, or
  add a `Content-Encoding` the client (without feature `gzip`) cannot read.
- **Root certificates** come from webpki-roots (Mozilla's list), not the OS store: a private CA
  or a corporate TLS-inspecting proxy is not trusted.
- **Cancel does not interrupt** a request already on the wire; it holds its worker thread until
  ureq's timeout at most.
- **Uploads:** in-memory parts are built into one body (at most `with_max_bytes`, about twice
  the in-memory parts at the peak while sending); files from disk (`file_from_path`) are streamed.
- **WebSocket (feature `ws`):** messages go uncompressed (permessage-deflate is not negotiated,
  so a server that requires compression refuses the connection); a subprotocol is set as a
  `Sec-WebSocket-Protocol` header with `with_header`; one thread per connection (fine for a few
  connections, not for hundreds). A large message you send occupies its connection thread until
  the socket takes it (that time does not count as the server's silence); if the server accepts
  no data for 30 s (or `dead_after`, if longer), the connection ends with a `Timeout` saying so.
  At `trace` level tungstenite prints the whole handshake request, `Authorization` and query
  included: keep `tungstenite` below `trace` like `ureq`. Received frames the game does not take
  are limited to 32 times the message limit per connection (then it closes with 1008).
- **SSH (feature `ssh`):** connections run commands (exec channels, no terminal) and SFTP;
  reconnect only when enabled and never for a running command. Output arrives in chunks, not
  lines. A cancelled or timed-out remote process may keep running (see the SSH section). SFTP
  downloads keep 16 reads of 64 KiB in flight and uploads 16 writes of 32 KiB. russh logs agent
  sign requests at `debug` (challenge bytes, not secrets) and packet details at `trace`: keep
  `russh` at `info` or below like `ureq`. A lost connection is noticed through russh's own
  disconnect report, backed by a once-a-second check of the session; a lost network without any
  reset is noticed by the keepalive (15 s, 3 misses).

## Versions

| bevy_net_backend | Bevy | ureq | tungstenite (`ws`) | russh (`ssh`) | rustls | Rust (MSRV) |
|---|---|---|---|---|---|---|
| 0.1.0 | 0.19.0 | 3.4.2 | 0.30.0 | 0.63.3 | 0.23.45 | 1.95 |

## Examples

All examples are headless and exit on their own. Without `BACKEND_URL` they start the mock server
from `examples/mock_server.rs` on 127.0.0.1 inside the example process.

| Example | Shows |
|---|---|
| `fetch_json` | `get_json::<Character>`, matching the answer by id, error bodies. |
| `upload` | a `multipart/form-data` avatar upload (a text field + an image) with `post_multipart_json`; the mock parses it and answers what it received. |
| `post_with_token` | 401 before login, login `without_credentials`, `BearerToken`, a 422 validation error decoded from the error body, a successful authenticated `POST`. |
| `mock_server` | the mock API on its own and its JSON contract (including `POST /upload`, a real multipart parser that echoes what it received): `--seconds N` (maximum runtime, default 60; it exits by itself), `--bind ADDR` (default `127.0.0.1:0`). |
| `chat_client` (features `ws`, `json`) | a named connection, a typed request and its answer, typed pushes, state changes, disconnect. Starts `mock_ws_server` unless `BACKEND_WS_URL` is set. |
| `mock_ws_server` (features `ws`, `json`) | the mock WebSocket server and its envelope contract (echo, `chat.send` + push, `fail`, `close`, `drop`, `stall`, periodic `server.tick`, `/secure` needing a bearer token, `/busy` refusing with `503` and `Retry-After: 2`): `--seconds N`, `--bind ADDR` (default `127.0.0.1:0`), `--tick-ms N`. |
| `ssh_console` (feature `ssh`; SFTP steps with `sftp`) | connect with a known_hosts file, run commands and print their output and exit, then upload, list, download and remove a file one step after the other, disconnect. Starts `mock_ssh_server` with a throwaway key (written to `target/ssh-example/`) unless `SSH_HOST`, `SSH_USER`, `SSH_KEY` and `SSH_KNOWN_HOSTS` are set. |
| `mock_ssh_server` (feature `ssh`; SFTP with `sftp`) | the mock SSH server: canned commands (never executes anything), an in-memory SFTP file system, a throwaway host key; alone it writes a throwaway client key and a known_hosts file to `--out-dir` (default `target/mock-ssh`): `--seconds N`, `--bind ADDR` (default `127.0.0.1:0`), `--user NAME`, `--host NAME` (the name clients reach it by, for the known_hosts line; with `--bind 0.0.0.0:P` and no `--host` the line says `CHANGE-ME`, or pin the printed fingerprint), `--password PW` and `--kbd PW:CODE` (also accept a password / a keyboard-interactive `Password:` + `Verification code:` login; throwaway test values only, visible in the process list). |

```text
cargo run --example fetch_json
cargo run --example post_with_token
cargo run --example upload
cargo run --example mock_server -- --seconds 120
cargo run --example mock_server -- --seconds 1800 --bind 127.0.0.1:8080
BACKEND_URL=http://127.0.0.1:8000/api cargo run --example fetch_json
cargo run --example chat_client --features ws,json
cargo run --example mock_ws_server --features ws,json -- --seconds 1800 --bind 127.0.0.1:9001
cargo run --example ssh_console --features ssh,sftp
cargo run --example mock_ssh_server --features ssh,sftp -- --seconds 600
```

Pointed at your own backend (`BACKEND_URL`), `fetch_json` expects `GET /characters/1` →
`{"id":1,"name":"…","class":"…","level":7}`, and `post_with_token` expects the routes in its
header comment (the login reads `BACKEND_USERNAME` / `BACKEND_PASSWORD`).

## How it's tested

- **312 tests with all features** (unit tests, integration tests and every Rust block of this
  README), plus 12 live tests that are `#[ignore]`d by default. CI runs the tests for 15
  feature combinations on Linux, Windows and macOS with Rust 1.96.0, clippy with `-D warnings` for
  every combination, rustfmt, the docs with `-D warnings`, a build with the minimum Rust version
  (1.95), dependency-tree checks (one ring, one rustls, no tokio without `ssh`, no OpenSSL /
  native-tls / aws-lc / libssh2) and a RustSec advisory check (cargo-deny) on every pull request,
  every push and weekly. RUSTSEC-2023-0071 (rsa, Marvin) is accepted in `deny.toml`: `rsa` is
  compiled only with the opt-in `ssh-rsa` feature, but `Cargo.lock` always lists it, and no fixed
  release exists.
- **An adversarial review every round:** each part (core + HTTP, WebSocket, SSH / SFTP, uploads)
  was reviewed line by line against its specification and this README before it was accepted,
  and every finding was fixed or documented as a known limit.
- **Hostile and slow servers are part of the regular suite:** the real transports run against
  mock servers on 127.0.0.1 inside the test process: gzip bombs, bodies over the limit, servers
  that answer too late, trickle a handshake or a TLS record byte by byte, stop reading while the
  client sends 32 MiB, send a 2 MB SSH banner or never say anything. Tests never contact another
  host.
- **Live-tested against a real server:** an Ubuntu machine on the internet behind Caddy with a real
  Let's Encrypt certificate.
  - **HTTP:** the real certificate chain accepted, and a wrong-name and an untrusted certificate
    rejected; every credential type; timeouts, including a request that waited for a worker;
    body limits; redirects not followed; a gzip bomb stopped at the limit; 50 parallel requests,
    each answered exactly once; cancel and exit mid-flight.
  - **WebSocket over `wss://`:** typed requests and pushes, named connections, handshake
    credentials, close codes, message limits; the server was stopped and restarted in the middle
    of a session, and the client reconnected, authenticated again and answered the lost request
    honestly as "sent".
  - **SSH and SFTP against real OpenSSH:** strict key exchange and AES-GCM confirmed in the
    server's own log; cancelled and timed-out commands gone from the server within 1.5 s; a 50 MB
    upload; a reconnect that never re-ran a command; connection resets noticed within about half a
    second.
  - **Uploads:** byte for byte (CRC-32) against real PHP, Express + multer, FastAPI, Go and Django
    (see [what real backends do with an upload](#what-real-backends-do-with-an-upload)); limits
    refused before sending really never reached the server.

Run it yourself:

- `cargo test` runs the unit tests, the `FakeHttpTransport` tests (every answer kind, exactly one
  answer each, strict ambiguity detection), a log-capture test proving no secret is logged, the
  loopback tests (the real `UreqTransport` against the mock server on 127.0.0.1: statuses,
  redirects, timeouts, body limit, TLS handshake failure, login flow, exit while busy) and the
  upload tests (the real transport against the mock's multipart parser; files streamed from
  disk, a JSON part, upload progress, refusals before sending, and a 256 MiB file streamed to a
  local server that hashes it on the fly). With `--features ws` (and `--all-features`) also the
  WebSocket tests: every lifecycle path on a `FakeWsTransport` (credentials refresh included:
  one message for several refused connections, one new connection, a second refusal final), the
  real transport against `mock_ws_server` (large messages across many short read timeouts,
  reconnect, heartbeat, 401, a 503 with `Retry-After`, a refused token refreshed by the game, 1009, exit), and a TLS test
  with large messages cut by read timeouts mid-record. With `--features ssh` (and `ssh,sftp`) also
  the SSH tests: every lifecycle path on a `FakeSshTransport` (including one app with HTTP,
  WebSocket and SSH cancelling each other's requests), the real `RusshTransport` against
  `mock_ssh_server` with throwaway keys generated at runtime (commands, timeouts, cancel, output
  limit, strict host keys, passphrases, ssh_config, SFTP: a 256 MiB download compared by SHA-1,
  sizes from 0 bytes up, short reads, a server error, cancel and timeout in the middle with no
  file left behind), the download pipeline against an in-memory file that answers out of order,
  and hostile raw TCP peers (silent, trickling, huge banner) that must not stretch the connect
  deadline. `cargo test --all-features` also compiles every Rust block of this README.
- `tests/live.rs` holds live HTTPS checks, `#[ignore]`d: they run only with
  `cargo test --test live -- --ignored` and `BNB_TEST_HTTPS_URL` set to a server that serves the
  mock's contract over HTTPS (for example `mock_server` behind a TLS-terminating reverse proxy on
  a test machine).
- `tests/live_ws.rs` does the same for WebSocket: `cargo test --features ws --test live_ws -- --ignored`
  with `BNB_TEST_WSS_URL` set to `mock_ws_server` behind a TLS proxy (for example `wss://…/ws`).
- `tests/live_multipart.rs` uploads to the mock (`BNB_TEST_HTTPS_URL` + `/upload`) and to any echo
  servers listed in `BNB_TEST_MULTIPART_URLS` (comma-separated upload URLs, e.g. small PHP /
  Express / FastAPI / Go / Django servers that answer the mock's JSON echo shape, documented in
  `examples/mock_server.rs`), and checks the names, values, content types, sizes and CRC-32 they
  report, for a simple form and for the cases every tested framework reads the same way:
  `cargo test --test live_multipart -- --ignored --test-threads 1`.
- `tests/live_ssh.rs` checks a real OpenSSH server: `cargo test --features ssh,sftp --test live_ssh
  -- --ignored --test-threads 1` with `BNB_TEST_HOST`, `BNB_TEST_SSH_USER`, `BNB_TEST_SSH_KEY` (a key
  file) and `BNB_TEST_SSH_KNOWN_HOSTS` (a known_hosts file) set, optionally `BNB_TEST_SSH_PORT` and
  `BNB_TEST_SSH_PASSPHRASE`. It runs harmless commands only (`echo`, `uname`, `whoami`, `sleep`)
  and SFTP inside a new temporary directory in the user's home that it removes again.
- Against your real API, run the examples with `BACKEND_URL` (see [Examples](#examples)).

## FAQ

**Why not reqwest?** reqwest needs tokio (an async runtime next to Bevy's) and is a much larger
tree. A game API client does a handful of requests; blocking ureq on two threads is plenty.

**Why not Bevy's `IoTaskPool`?** It has 1–4 threads shared with asset loading. A 15-second
request would stall asset IO; a dedicated pool cannot.

**How many APIs can one app call?** One base URL per app (`HttpConfig`); `set_base_url` changes
it at runtime.

**Is the answer delivered if my reader runs in `PostUpdate`?** Yes. Answers are written in
`First` before Bevy's message update, so they are readable in every schedule of that frame (and
in the next frame's `First` before the update), then dropped. A reader that runs only every
other frame can miss them.

**Can I build a `JsonResponse<T>` myself for a unit test?** No (it is `#[non_exhaustive]` and
`RequestId` has no public constructor). Drive your systems through the plugin with a
`FakeHttpTransport` instead (see [Testing your game](#8-testing-your-game-without-a-server)).

**Where does the token live between sessions?** Wherever your game keeps it; this crate only
sends what is in `BackendCredentials`, and its `Secret` wipes its memory when it is dropped.

**Can my game use SSH to talk to its servers?** Not a game you give to players: an SSH key in a
player build is shell access for anyone who extracts it. Use HTTP or WebSocket with per-player
tokens for that. SSH is for your own admin and developer tools, and release builds refuse it
unless the tool explicitly opts in (`SshSettings::allow_in_release`).

**How do I upload a screenshot or a save file?** `HttpClient::post_multipart` with a
`Multipart` form (see [File uploads](#12-file-uploads-multipart)): `file` for bytes you have,
`file_from_path` for a file on disk (read on the worker thread while it is sent, with
`HttpProgress` messages).

**Can SSH log in with a password or a 2FA code?** Yes, opt-in: `SshAuth::password` and
`SshAuth::keyboard_interactive` (see the SSH section). Keys stay the recommendation; the values are
typed by the admin at runtime and never logged.

**Why does `ssh` bring tokio when nothing else does?** Every maintained, complete SSH client in
Rust that needs neither C nor OpenSSL is built on tokio (russh). The crate keeps it contained: one
current-thread runtime on one thread of its own, started on the first connect, and nothing of it
without the feature.

**Does it follow the system proxy?** The `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`
environment variables, yes (not for loopback hosts). Windows' registry proxy settings, no.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt`, `cargo clippy --all-targets
--all-features -- -D warnings` and `cargo test` (with and without default features) before
opening a pull request. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
