# bevy_net_backend

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml/badge.svg)](https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml)
[![Bevy 0.19.0](https://img.shields.io/badge/Bevy-0.19.0-informational)](https://bevyengine.org)
[![ureq 3.4.2](https://img.shields.io/badge/ureq-3.4.2-orange)](https://crates.io/crates/ureq)
[![tungstenite 0.30.0 (optional)](https://img.shields.io/badge/tungstenite-0.30.0%20(optional)-orange)](https://crates.io/crates/tungstenite)
[![russh 0.63.3 (optional)](https://img.shields.io/badge/russh-0.63.3%20(optional)-orange)](https://crates.io/crates/russh)

Call **your game's own HTTPS JSON API** from [Bevy](https://bevyengine.org): accounts, save
games, leaderboards, inventories, matchmaking tickets, whatever your Laravel, Express, Go or
Django backend serves. With feature `ws`, also **named WebSocket connections** to it: live
chat, lobbies, match events and server pushes, with reconnect and heartbeat built in. With feature
`ssh`, for **admin and developer tools only**, named SSH connections that run commands on your
servers (and, with `sftp`, move files).

A system fires a request and gets a `RequestId` back at once. A few frames later **exactly one
answer** arrives as a Bevy message: the decoded value, or an error that says what happened
(network, TLS, timeout, an HTTP status with the server's body, a decode error, cancelled, or
shutdown when the app exits). The network never blocks a frame, nothing is dropped silently,
nothing panics.

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

- [Highlights](#highlights)
- [Cargo features](#cargo-features)
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
- [Backend compatibility](#backend-compatibility)
- [How it works](#how-it-works)
- [TLS exception: the default build is not pure Rust](#tls-exception-the-default-build-is-not-pure-rust)
- [API reference](#api-reference)
- [Limits and what it does not do](#limits-and-what-it-does-not-do)
- [Compatibility](#compatibility)
- [Examples](#examples)
- [Testing](#testing)
- [FAQ](#faq)
- [License](#license)
- [Contributing](#contributing)

## Highlights

- **Typed JSON in, typed JSON out:** `get_json::<T>`, `post_json::<T>`, `send_json::<T>` with
  your serde types; `JsonResponse<T>` messages out.
- **Exactly one answer per request**, always: success, `Status` (4xx/5xx with the server's
  status, headers and body), `Network`, `Tls`, `Timeout`, `Decode`, `Cancelled`, `Shutdown`,
  `NoTransport`, `BodyTooLarge`, `InvalidRequest`, `InsecureHttp`, `Encode`. A late result from
  the network after a cancel or timeout is discarded.
- **Never blocks a frame:** requests run on a small fixed pool of worker threads (default 2), not
  on Bevy's task pools. Answers are written in `First`, so `PreUpdate` and `Update` read them in
  the frame they arrived.
- **No ordering ceremony:** `HttpClient` is used through `Res<HttpClient>` (shared access),
  so any number of systems in any schedule can fire requests without `.before()` / `.after()`.
- **Game-controlled auth:** after your own login call, put a `BearerToken`, `ApiKeyHeader`,
  `ApiKeyQuery` or `JsonBodyField` (or your own `Credentials`) into `BackendCredentials`. Secrets
  are redacted from `Debug`, `Display` and the crate's logs.
- **Safe defaults:** HTTPS only (plain `http://` only to `localhost` / `127.x.x.x` / `[::1]`
  unless you allow it), a 15 s timeout, a 10 MiB response body limit, redirects not followed.
- **Testable offline:** a `FakeHttpTransport` answers from scripted routes; your tests need no server.
- **WebSocket (feature `ws`):** named connections (`connect("main", …)`), typed requests and
  pushes over a JSON envelope, reconnect with backoff and jitter, heartbeat and dead-peer
  detection, credentials on every handshake, one thread per connection.
- **SSH for admin / dev tools (feature `ssh`):** named connections, commands with streamed
  output and one answer each (exit status, timeout, cancel), strict known_hosts checking, key
  files, ssh-agent and `~/.ssh/config`; SFTP with `sftp`. Refuses to run in release builds unless
  you opt in.
- **Small, and no tokio unless you enable `ssh`:** ureq 3 (blocking HTTP/1.1), tungstenite (sync)
  + rustls; no hyper, no OpenSSL. Only `ssh` brings tokio, on one private thread of its own.

## Cargo features

| Feature | Default | What it adds |
|---|---|---|
| `http` | yes | The real transport, `UreqTransport`: ureq 3.4 on worker threads, rustls with ring's crypto and the Mozilla root certificates (webpki-roots). ring compiles C and assembly (see [TLS exception](#tls-exception-the-default-build-is-not-pure-rust)). |
| `json` | yes | `get_json` / `post_json` / `send_json`, `JsonResponse<T>`, `OutgoingRequest::with_json`, `RawResponse::json`, `JsonBodyField` (serde + serde_json). |
| `gzip` | no | Accept gzip-compressed responses (ureq's decoder, flate2). |
| `ws` | no | Named WebSocket connections: `WsClient`, `WsConnections`, the `Ws*` messages, `TungsteniteTransport` (tungstenite 0.30, sync, one thread per connection, rustls + ring, no permessage-deflate). With `json`: `JsonEnvelope`, `WsRequest`, `WsPushMessage`, `WsResponse<T>`, `WsPush<P>`. |
| `ssh` | no | **Admin / dev builds only.** Named SSH connections that run commands: `SshClient`, `SshConnections`, the `Ssh*` messages, `RusshTransport` (russh 0.63, ring for the AEAD ciphers and RustCrypto for the rest; tokio on one private thread), strict known_hosts, key files / ssh-agent / `~/.ssh/config`. |
| `sftp` | no | SFTP on SSH connections (implies `ssh`): upload, download, list, create / remove directory, remove file, rename (russh-sftp). |
| `ssh-rsa` | no | RSA host keys and RSA key files for SSH (implies `ssh`; rsa-sha2-256/512, never SHA-1). Off by default: the `rsa` crate carries the unfixed Marvin timing advisory RUSTSEC-2023-0071. Without it, ed25519 and ECDSA keys work. |

Crates in the build (normal + build dependencies, this crate excluded): default 90, `gzip` 95,
`ws` 104, `ssh` 209, `ssh` + `sftp` 219, `ssh` + `ssh-rsa` 212, everything 228; `ssh` without
default features 195.

Without `http` the crate still builds: every type, the `FakeHttpTransport` and your own
`HttpTransport` work, and requests without a transport are answered with `NoTransport`.

The default set is deliberately not empty: the crate exists to call an HTTPS JSON API, and it
should do that with no feature fiddling.

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

/// The request we wait for.
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
            Err(BackendError::Timeout(_) | BackendError::Network(_)) => warn!("offline? retry later"),
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
| `BodyTooLarge { limit }` | the response body is over the limit (SSH: the command's output or an SFTP transfer) | yes |
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
`status`, `headers`, `body`, `text()` and `json::<E>()`. `Display` of every error is safe to log:
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

- `Secret` prints as `<redacted>` in `Debug` and `Display`; read it with `expose()`.
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
- Methods added to `Credentials` later always come with a default implementation.
- Storing the token between sessions (keyring, file) and refreshing it are the game's job.

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
- **InFlight** lists every request not answered yet, HTTP and WebSocket (feature `ws`) alike. An
  HTTP request enters it in `PostUpdate` of the frame it was made in; a WebSocket request there
  too, also while it waits for its connection. A request leaves it when it is answered; the answer
  message follows in `First` (of the same frame, or of the next one for answers decided in
  `PostUpdate`, such as a cancel). `describe(id)` returns a `RequestInfo`: `kind`
  (`Http` or `WebSocket`), `method` (HTTP), `target` (the path without the query, or the
  connection's name).
- **One cancel for everything:** `HttpClient::cancel(id)` cancels HTTP and WebSocket requests
  alike (`WsClient::cancel` is the same call).
- **App exit:** in the frame an `AppExit` message is written, nothing new is sent:
  `BackendSystems::Send` hands no request to the transport, and `BackendSystems::Exit` (in `Last`)
  answers every open request with `Shutdown` (results that already arrived are delivered as they
  are) and stops the worker threads without waiting for busy ones. **A request answered `Shutdown`
  was never sent**, except one that was already on the wire before that frame (it may still reach
  the server). Systems ordered after `BackendSystems::Exit` in `Last` can read those answers.
  Write `AppExit` before `BackendSystems::Send` (anywhere in `Update` or earlier is fine; a
  `PostUpdate` writer must be ordered `.before(BackendSystems::Send)`): written later, requests
  of that frame may still go out, and written after `Exit` in `Last` it is seen by nobody in this
  crate.
- **Save on quit:** send the save, wait for its answer (`Ok` or an error), and only then write
  `AppExit`. A save fired in the same frame as `AppExit` is answered `Shutdown` and never sent.
  (Flushing pending requests on exit is not a feature of 0.1.0.)

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
  `shutdown_count()` let tests assert what the game sent.
- The crate's own tests use `bevy_headless_test`'s strict `TestApp` (ambiguity detection on every
  main schedule); the plugin's systems are ordered, and `Res<HttpClient>` never conflicts.

### 9. Your own transport

`HttpTransport` is the seam: `submit(id, PreparedRequest)` (never block), `poll()` (every result
since the last call), and optionally `cancel(id)` and `shutdown()`. Wrap it in
`HttpTransportRes::new(..)` and insert it; the plugin keeps doing all the bookkeeping (deadlines,
cancel, exit, status and body-limit rules). Report each request at most once; a late or unknown
result is discarded. Methods added to `HttpTransport` later always come with a default
implementation.

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
  `WsState` is `Connecting`, `Connected`, `Reconnecting { attempt, retry_in }` or `Disconnected`
  (`#[non_exhaustive]`). Not Bevy `States`: a game maps it to its own states if it wants.
- **`WsSettings`** (builder): read timeout (default 20 ms, 5–250 ms; it is also roughly the
  latency added to every frame you send, because the thread sends between reads: measured median
  request round trips through a TLS proxy were 30 ms at 5 ms, 43 ms at the default 20 ms and
  118 ms at 100 ms, against about 20 ms for HTTP), connect timeout (10 s, ONE
  deadline for TCP + TLS + handshake, at most 1 h), heartbeat (ping every 15 s, dead after 45 s
  without a single byte, at most 1 h), request timeout (10 s), message limit (1 MiB, incoming and
  outgoing), reconnect policy, handshake headers, `allow_insecure_ws`, `without_credentials`, the
  protocol, outbox (64 frames), resend (32) and waiting (64 requests) limits, `with_auth_ack`.
- **Reconnect:** exponential backoff with full jitter (`WsReconnect`: base 500 ms, cap 30 s,
  optional `with_max_attempts`, reset after 10 s connected, `never()`). Each attempt is a
  `WsStateChanged` with `Reconnecting { attempt, retry_in }` and the error that caused it.
  **Permanent (not retried, the connection goes `Disconnected` with the error):** a `401` / `403`
  handshake, a TLS / certificate error (unless `WsReconnect::with_tls_retry(true)`), a close code
  4000–4099, a refused first-message auth, a missing auth acknowledgement (`with_auth_ack`),
  invalid settings or a refused plain `ws://` URL, `disconnect`, exhausted attempts. Everything
  else (connect failures, drops, timeouts, dead peers, 5xx) is retried.
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
| `InvalidRequest` / `Encode` | unknown connection, no protocol, too large, unregistered type, bad payload | no |

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
  honest `started`; commands not sent yet wait for the new connection (until their own timeout).
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
  (an old RSA line, say) is `Unknown` for the new type. `@cert-authority` lines and host
  certificates are not supported.
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
  crash. `Match`, `%` tokens, `ProxyJump` / `ProxyCommand` and `UserKnownHostsFile` are not
  supported (the connection goes straight to the host).
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
| `InvalidRequest` | unknown connection, bad command (empty, NUL, over 64 KiB), too many requests, SSH disabled in a release build | `Some(false)` |

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
login directory. Transfers over `with_max_transfer_bytes` are `BodyTooLarge`; an upload over it
(from memory or from a file) is refused before anything is sent (`started: Some(false)`); a whole
operation is bounded by `with_sftp_timeout`. An
interrupted upload can leave a partial remote file. Errors carry the server's SFTP status text
(`Ssh("SFTP: No such file")`). The SFTP channel is opened on first use and shared by the
connection's operations. Local files are read and written on tokio's small blocking pool, never on
the SSH thread itself.

> **Listed names are untrusted input.** A hostile or broken server can list `../../.bashrc`,
> `C:\Windows\evil.dll` or `a/b`. Never join `SftpEntry::name` into a local path: use
> `entry.safe_file_name()`, which returns `None` for anything that is not one plain file name
> (`..`, separators, drive letters, control characters, Windows device names, …). The crate never
> turns a listed name into a local path itself; `download_file` writes only where you tell it to.

## Backend compatibility

- **HTTP** works with any backend that speaks HTTPS and JSON (or raw bytes): Laravel / PHP,
  Express / Node, Go, Rust, Django, ASP.NET, serverless functions.
- **WebSocket** (feature `ws`) works with any plain WebSocket server (RFC 6455), through the
  default JSON envelope or your own `WsProtocol` for another message layout.
- **Frameworks that run their own protocol on top of WebSocket** (Laravel Reverb / Pusher,
  Socket.IO, SignalR, Phoenix Channels) need an adapter for that protocol. Adapters are planned
  for a later version; until then use `WsProtocol` or raw frames if your server can also speak
  plain WebSocket.
- **SSH** (feature `ssh`, admin / dev tools) works with any standard SSH server: OpenSSH on Linux,
  BSD, macOS or Windows, and other servers speaking SSH-2 with ed25519 or ECDSA host keys (RSA with
  feature `ssh-rsa`). Servers without strict key exchange (OpenSSH before 9.6, unless the
  distribution backported it) connect through AES-GCM, which the client prefers; only a server that
  offers nothing but ChaCha20-Poly1305 or CBC + encrypt-then-MAC is refused (see Terrapin above).
  SFTP needs the server's `sftp` subsystem (OpenSSH's default).

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
  `Last` only in a frame with `AppExit`. The sets are `#[non_exhaustive]` and phase-named, so a
  later kind of connection can use the same three.
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
| `HttpClient` | resource | `send`, `request`, `get`, `cancel`; with `json`: `send_json`, `get_json`, `post_json`, `is_json_registered`. |
| `BackendAppExt` | trait on `App` | `add_json_response::<T>()` (feature `json`); `add_ws_request::<R>()`, `add_ws_push::<P>()` (features `ws` + `json`). Sealed. |
| `RequestId` | id | opaque, unique per process, `Copy + Eq + Hash + Ord + Display`. |
| `OutgoingRequest` | request | constructors, `with_*` builders, accessors (`method`, `path`, `query`, `headers`, `body`, `timeout`, `purpose`, `uses_credentials`, `error`), `query_mut`, `headers_mut`, `set_body`, `reject`. |
| `RequestPurpose` | enum | `Http`, `WebSocketHandshake`. |
| `HttpResponse` | message | `id`, `result: Result<RawResponse, BackendError>`. |
| `JsonResponse<T>` | message | `id`, `result: Result<T, BackendError>` (feature `json`). |
| `RawResponse` | struct | `status`, `headers`, `body`; `new`, `with_header`, `is_success`, `body()`, `text()`, `json()`. |
| `BackendError` | enum | see [Reading answers and errors](#4-reading-answers-and-errors); `status()`, `response()`, `is_invalid_request()`, `was_sent()`, `close_code()`, `host_key(..)` (a constructor for fakes). |
| `Credentials` | trait | `apply(&self, &mut OutgoingRequest)`; `ws_auth_message()` (default none: a first frame for WebSocket auth). |
| `BackendCredentials` | resource | `new`, `set`, `clear`, `is_set`. |
| `BearerToken`, `ApiKeyHeader`, `ApiKeyQuery`, `JsonBodyField` | credentials | ready-made `Credentials` (`JsonBodyField`: feature `json`). |
| `Secret` | string | redacted in `Debug` / `Display`; `new`, `expose`, `is_empty`. No comparison, no zeroing on drop. |
| `InFlight` | resource | HTTP, WebSocket and SSH: `contains`, `len`, `is_empty`, `ids`, `describe` (→ `RequestInfo`). |
| `RequestInfo` (struct), `RequestKind` (enum) | types | what a pending request is: `kind` (`Http`, `WebSocket`, `Ssh`, `Sftp`), `method`, `target` (never an SSH command line); `#[non_exhaustive]`. |
| `Rejection` | struct | the payload of `BackendError::Rejected`: `new`, `bytes`, `text`, `json` (json). |
| `HttpTransport` | trait | `submit`, `poll`, `cancel`, `shutdown`. |
| `HttpTransportResult` | type | `Result<RawResponse, BackendError>`. |
| `HttpTransportRes` | resource | `new(transport)`. |
| `PreparedRequest` | struct | what a transport receives: `method`, `uri`, `headers`, `body`, `timeout`, `max_body_bytes`, `purpose`; `path()`, `is_loopback()`, `is_https()`. |
| `FakeHttpTransport` | transport | `new`, `route`, `clear_routes`, `reply`, `requests`, `last_request`, `waiting`, `cancelled`, `shutdown_count`. |
| `UreqTransport` | transport | feature `http`: `new(&config)`, `workers()`. |
| `http` | crate | the `http` 1.x crate, re-exported (`Method`, `StatusCode`, `HeaderMap`, …). |
| `DEFAULT_TIMEOUT`, `MAX_TIMEOUT`, `DEFAULT_WORKERS`, `MAX_WORKERS`, `DEFAULT_MAX_BODY_BYTES`, `DEADLINE_GRACE` | consts | 15 s, 1 h, 2, 8, 10 MiB, 5 s. |
| `WsClient` | resource (`ws`) | `connect`, `disconnect`, `send`, `send_text`, `send_binary`, `request_raw`, `request` (json), `cancel` (the shared one). |
| `WsConnections`, `WsConnectionInfo` | resource / struct (`ws`) | `get`, `state`, `is_connected`, `iter`; info: `state`, `attempt`, `last_error`, `pending_requests`, `queued_frames`. |
| `WsName` | name (`ws`) | a connection's name; `From<&str>` / `From<String>`, `as_str`, compares with `&str`. |
| `WsState` | enum (`ws`) | `Connecting`, `Connected`, `Reconnecting { attempt, retry_in }`, `Disconnected`; `#[non_exhaustive]`. |
| `WsSettings`, `WsReconnect` | builders (`ws`) | per connection: timeouts, heartbeat, limits, reconnect, headers, `allow_insecure_ws`, `without_credentials`, protocol; backoff: base, cap, max attempts, stable-after, jitter, `never()`, `delay_bound`. |
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

## Limits and what it does not do

- **Native only** (Windows, Linux, macOS). No WebAssembly in this version.
- **HTTP/1.1 only** (ureq). No HTTP/2, no streaming bodies: a response is read whole (up to the
  body limit) before it is delivered.
- **No redirects followed**: a 3xx arrives as `Status` with its `Location` header.
- **No retries, no offline queue, no caching, no cookies.** Retry in your game if a request
  matters; the error kind and the "Sent?" column in
  [Reading answers and errors](#4-reading-answers-and-errors) tell you whether it may have been sent.
- **No token refresh, no keyring:** credentials are whatever the game puts into
  `BackendCredentials`.
- **One base URL** per app. Paths are relative to it; absolute URLs are refused.
- **A reverse proxy in front of your API must pass responses through unchanged**: no
  decompressing or recompressing on its own (Caddy: no `encode` directive for these routes; nginx:
  `gzip off`). The body limit and gzip handling assume the client sees exactly what your
  application sent; a proxy that re-encodes can turn a body under the limit into one over it, or
  add a `Content-Encoding` the client (without feature `gzip`) cannot read.
- **Root certificates** come from webpki-roots (Mozilla's list), not the OS store: a private CA
  or a corporate TLS-inspecting proxy is not trusted.
- **Cancel does not interrupt** a request already on the wire; it holds its worker thread until
  ureq's timeout at most.
- **WebSocket (feature `ws`):** no permessage-deflate (a server that requires compression cannot
  be used), no subprotocol negotiation helper (set `Sec-WebSocket-Protocol` with `with_header`),
  one thread per connection (fine for a few connections, not for hundreds). A large message you
  send occupies its connection thread until the socket takes it (that time does not count as the
  server's silence); if the server accepts no data for 30 s (or `dead_after`, if longer), the
  connection ends with a `Timeout` saying so. At `trace` level tungstenite
  prints the whole handshake request, `Authorization` and query included: keep `tungstenite`
  below `trace` like `ureq`. Received frames the game does not take are limited to 32 times the
  message limit per connection (then it closes with 1008).

- **SSH (feature `ssh`):** reconnect only when enabled and never for a running command; no
  agent or port forwarding, no PTY / interactive shell, no host certificates; ssh_config without
  `Match`, `%` tokens, `ProxyJump` / `ProxyCommand` or `UserKnownHostsFile`. Output arrives in
  chunks, not lines. A cancelled or timed-out remote process may keep running (see the SSH
  section). SFTP downloads read 64 KiB at a time, one request after the other (uploads keep 16
  writes of 32 KiB in flight), so a download's speed is bounded by the round trip. russh logs agent
  sign requests at `debug` (challenge bytes, not secrets) and packet details at `trace`: keep
  `russh` at `info` or below like `ureq`. A lost connection is noticed through russh's own
  disconnect report, backed by a once-a-second check of the session; a lost network without any
  reset is noticed by the keepalive (15 s, 3 misses).

## Compatibility

| bevy_net_backend | Bevy | ureq | tungstenite (`ws`) | russh (`ssh`) | rustls | Rust (MSRV) |
|---|---|---|---|---|---|---|
| 0.1.0 | 0.19.0 | 3.4.2 | 0.30.0 | 0.63.3 | 0.23.45 | 1.95 |

## Examples

All examples are headless and exit on their own. Without `BACKEND_URL` they start the mock server
from `examples/mock_server.rs` on 127.0.0.1 inside the example process.

| Example | Shows |
|---|---|
| `fetch_json` | `get_json::<Character>`, matching the answer by id, error bodies. |
| `post_with_token` | 401 before login, login `without_credentials`, `BearerToken`, a 422 validation error decoded from the error body, a successful authenticated `POST`. |
| `mock_server` | the mock API on its own and its JSON contract: `--seconds N` (maximum runtime, default 60; it exits by itself), `--bind ADDR` (default `127.0.0.1:0`). |
| `chat_client` (features `ws`, `json`) | a named connection, a typed request and its answer, typed pushes, state changes, disconnect. Starts `mock_ws_server` unless `BACKEND_WS_URL` is set. |
| `mock_ws_server` (features `ws`, `json`) | the mock WebSocket server and its envelope contract (echo, `chat.send` + push, `fail`, `close`, `drop`, `stall`, periodic `server.tick`, `/secure` needing a bearer token): `--seconds N`, `--bind ADDR` (default `127.0.0.1:0`), `--tick-ms N`. |
| `ssh_console` (feature `ssh`; SFTP steps with `sftp`) | connect with a known_hosts file, run commands and print their output and exit, then upload, list, download and remove a file one step after the other, disconnect. Starts `mock_ssh_server` with a throwaway key (written to `target/ssh-example/`) unless `SSH_HOST`, `SSH_USER`, `SSH_KEY` and `SSH_KNOWN_HOSTS` are set. |
| `mock_ssh_server` (feature `ssh`; SFTP with `sftp`) | the mock SSH server: canned commands (never executes anything), an in-memory SFTP file system, a throwaway host key; alone it writes a throwaway client key and a known_hosts file to `--out-dir` (default `target/mock-ssh`): `--seconds N`, `--bind ADDR` (default `127.0.0.1:0`), `--user NAME`, `--host NAME` (the name clients reach it by, for the known_hosts line; with `--bind 0.0.0.0:P` and no `--host` the line says `CHANGE-ME`, or pin the printed fingerprint), `--password PW` and `--kbd PW:CODE` (also accept a password / a keyboard-interactive `Password:` + `Verification code:` login; throwaway test values only, visible in the process list). |

```text
cargo run --example fetch_json
cargo run --example post_with_token
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

## Testing

- `cargo test` runs the unit tests, the `FakeHttpTransport` tests (every answer kind, exactly one
  answer each, strict ambiguity detection), a log-capture test proving no secret is logged, the
  loopback tests (the real `UreqTransport` against the mock server on 127.0.0.1: statuses,
  redirects, timeouts, body limit, TLS handshake failure, login flow, exit while busy). With
  `--features ws` (and `--all-features`) also the WebSocket tests: every lifecycle path on a
  `FakeWsTransport`, the real transport against `mock_ws_server` (large messages across many
  short read timeouts, reconnect, heartbeat, 401, 1009, exit), and a TLS test with large messages
  cut by read timeouts mid-record. With `--features ssh` (and `ssh,sftp`) also the SSH tests:
  every lifecycle path on a `FakeSshTransport` (including one app with HTTP, WebSocket and SSH
  cancelling each other's requests), the real `RusshTransport` against `mock_ssh_server` with
  throwaway keys generated at runtime (commands, timeouts, cancel, output limit, strict host keys,
  passphrases, ssh_config, SFTP) and hostile raw TCP peers (silent, trickling, huge banner) that
  must not stretch the connect deadline. `cargo test --all-features` also compiles every Rust block
  of this README.
- Tests never contact another host. CI runs the feature combinations on Linux, Windows and macOS
  with Rust 1.96.0, and a RustSec advisory check (cargo-deny) on every pull request, every push
  and weekly. RUSTSEC-2023-0071 (rsa, Marvin) is accepted in `deny.toml`: `rsa` is compiled only
  with the opt-in `ssh-rsa` feature, but `Cargo.lock` always lists it, and no fixed release exists.
- `tests/live.rs` holds live HTTPS checks, `#[ignore]`d: they run only with
  `cargo test --test live -- --ignored` and `BNB_TEST_HTTPS_URL` set to a server that serves the
  mock's contract over HTTPS (for example `mock_server` behind a TLS-terminating reverse proxy on
  a test machine).
- `tests/live_ws.rs` does the same for WebSocket: `cargo test --features ws --test live_ws -- --ignored`
  with `BNB_TEST_WSS_URL` set to `mock_ws_server` behind a TLS proxy (for example `wss://…/ws`).
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

**Can I call two different APIs?** Not in 0.1.0: one base URL per app.

**Is the answer delivered if my reader runs in `PostUpdate`?** Yes. Answers are written in
`First` before Bevy's message update, so they are readable in every schedule of that frame (and
in the next frame's `First` before the update), then dropped. A reader that runs only every
other frame can miss them.

**Can I build a `JsonResponse<T>` myself for a unit test?** No (it is `#[non_exhaustive]` and
`RequestId` has no public constructor). Drive your systems through the plugin with a
`FakeHttpTransport` instead (see [Testing your game](#8-testing-your-game-without-a-server)).

**Where does the token live between sessions?** Wherever your game keeps it; this crate only
sends what is in `BackendCredentials`.

**Can my game use SSH to talk to its servers?** Not a game you give to players: an SSH key in a
player build is shell access for anyone who extracts it. Use HTTP or WebSocket with per-player
tokens for that. SSH is for your own admin and developer tools, and release builds refuse it
unless the tool explicitly opts in (`SshSettings::allow_in_release`).

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
