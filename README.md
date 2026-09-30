# bevy_net_backend

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml/badge.svg)](https://github.com/warmar94/bevy_net_backend/actions/workflows/ci.yml)
[![Bevy 0.19.0](https://img.shields.io/badge/Bevy-0.19.0-informational)](https://bevyengine.org)
[![ureq 3.4.2](https://img.shields.io/badge/ureq-3.4.2-orange)](https://crates.io/crates/ureq)

Call **your game's own HTTPS JSON API** from [Bevy](https://bevyengine.org): accounts, save
games, leaderboards, inventories, matchmaking tickets, whatever your Laravel, Express, Go or
Django backend serves.

A system fires a request and gets a `RequestId` back at once. A few frames later **exactly one
answer** arrives as a Bevy message: the decoded value, or an error that says what happened
(network, TLS, timeout, an HTTP status with the server's body, a decode error, cancelled, or
shutdown when the app exits). Nothing blocks a frame, nothing is dropped silently, nothing panics.

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
        .add_systems(Startup, |backend: Res<BackendClient>| {
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
- **No ordering ceremony:** `BackendClient` is used through `Res<BackendClient>` (shared access),
  so any number of systems in any schedule can fire requests without `.before()` / `.after()`.
- **Game-controlled auth:** after your own login call, put a `BearerToken`, `ApiKeyHeader`,
  `ApiKeyQuery` or `JsonBodyField` (or your own `Credentials`) into `BackendCredentials`. Secrets
  are redacted from `Debug`, `Display` and the crate's logs.
- **Safe defaults:** HTTPS only (plain `http://` only to `localhost` / `127.x.x.x` / `[::1]`
  unless you allow it), a 15 s timeout, a 10 MiB response body limit, redirects not followed.
- **Testable offline:** a `FakeHttpTransport` answers from scripted routes; your tests need no server.
- **Small and runtime-free:** ureq 3 (blocking HTTP/1.1) + rustls; no tokio, no hyper, no OpenSSL.

## Cargo features

| Feature | Default | What it adds |
|---|---|---|
| `http` | yes | The real transport, `UreqTransport`: ureq 3.4 on worker threads, rustls with ring's crypto and the Mozilla root certificates (webpki-roots). ring compiles C and assembly (see [TLS exception](#tls-exception-the-default-build-is-not-pure-rust)). |
| `json` | yes | `get_json` / `post_json` / `send_json`, `JsonResponse<T>`, `OutgoingRequest::with_json`, `RawResponse::json`, `JsonBodyField` (serde + serde_json). |
| `gzip` | no | Accept gzip-compressed responses (ureq's decoder, flate2). |

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

# Only the types and the fake transport (e.g. a crate that brings its own transport).
bevy_net_backend = { version = "0.1.0", default-features = false }
```

The crate uses Bevy's sub-crates `bevy_app`, `bevy_ecs` and `bevy_time` 0.19.0 without default
features, so it adds no Bevy feature your game did not ask for.

## Quick start

1. Add `BackendPlugin` with your API's base URL.
2. Register each JSON answer type once: `app.add_json_response::<T>()`.
3. Fire requests from any system with `Res<BackendClient>`; keep the `RequestId` if you need to
   match the answer.
4. Read `JsonResponse<T>` (or `BackendResponse` for raw calls) with a `MessageReader`.

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

fn submit_score(backend: Res<BackendClient>, mut commands: Commands) {
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

fn requests(backend: Res<BackendClient>) {
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
- A type that was never registered is not sent: the request is answered on `BackendResponse`
  with `InvalidRequest` (naming the missing `add_json_response`) and a warning is logged.
- A body that cannot be serialized is answered with `Encode` and never sent.

### 3. Raw requests and full control

Raw calls answer with `BackendResponse` (status, headers, bytes):

```rust
use bevy::prelude::*;
use bevy_net_backend::http::Method;
use bevy_net_backend::prelude::*;
use std::time::Duration;

fn raw(backend: Res<BackendClient>) {
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
refused (use `with_query`, which percent-encodes names and values). A path is sent as written:
percent-encode user text you put into it. An invalid header does not panic: the request is
answered with `InvalidRequest` and never sent.

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
| `InsecureHttp { host }` | plain `http://` to a non-loopback host without `allow_insecure_http` | no |
| `Encode(why)` | the JSON body cannot be serialized | no |
| `Network(why)` | DNS, connect, reset, protocol error (ureq's words) | yes |
| `Tls(why)` | TLS failure: handshake, certificate (rustls' / ureq's words) | yes |
| `Timeout(why)` | ureq's timeout, or the plugin's deadline (timeout + 5 s) | yes |
| `BodyTooLarge { limit }` | the response body is over the limit | yes |
| `Status(response)` | a status outside 200–299, 3xx included (redirects are not followed) | yes |
| `Decode { message, response }` | a 2xx body that is not the expected JSON | yes |
| `Cancelled` | `BackendClient::cancel` | maybe |
| `Shutdown` | the app exited (`AppExit`) first | maybe |
| `NoTransport` | no `HttpTransportRes`, or it was removed / replaced first | no / maybe |

`BackendError` is `#[non_exhaustive]`: keep a catch-all arm. `error.status()` and
`error.response()` give the server's answer for `Status` and `Decode`; `RawResponse` has
`status`, `headers`, `body`, `text()` and `json::<E>()`. The `Decode` message is serde_json's and
may quote part of the body; the crate itself never logs it.

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

fn log_in(backend: Res<BackendClient>) {
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
- A query key ends up in server access logs, and ureq logs full paths and queries at `trace`
  level: prefer a header where your API allows it.
- Methods added to `Credentials` later always come with a default implementation.
- Storing the token between sessions (keyring, file) and refreshing it are the game's job.

### 6. Cancel, in-flight tracking, app exit

```rust
use bevy::prelude::*;
use bevy_net_backend::prelude::*;

#[derive(Resource)]
struct Search(RequestId);

fn new_search(backend: Res<BackendClient>, old: Option<Res<Search>>, mut commands: Commands) {
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
- **InFlight** lists requests handed to the transport and not answered yet. A request enters it
  in `PostUpdate` of the frame it was made in.
- **App exit:** in the frame an `AppExit` message is written, `BackendSystems::Exit` (in `Last`)
  answers every open request with `Shutdown` (results that already arrived are delivered as they
  are) and stops the worker threads without waiting for busy ones. Systems ordered after
  `BackendSystems::Exit` in `Last` can read those answers.

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

let id = app.world().resource::<BackendClient>().get("/me");
app.update(); // PostUpdate: handed to the fake
app.update(); // First: answered, readable in Update

let (_, request) = fake.last_request().expect("a request");
assert_eq!(request.uri.to_string(), "https://api.example.com/v1/me");
let answers = app.world().resource::<Messages<BackendResponse>>();
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
  main schedule); the plugin's systems are ordered, and `Res<BackendClient>` never conflicts.

### 9. Your own transport

`HttpTransport` is the seam: `submit(id, PreparedRequest)` (never block), `poll()` (every result
since the last call), and optionally `cancel(id)` and `shutdown()`. Wrap it in
`HttpTransportRes::new(..)` and insert it; the plugin keeps doing all the bookkeeping (deadlines,
cancel, exit, status and body-limit rules). Report each request at most once; a late or unknown
result is discarded. Methods added to `HttpTransport` later always come with a default
implementation.

## How it works

```text
 game system ──Res<BackendClient>──▶ queue ─┐
                                            │ PostUpdate  BackendSystems::Send
                                            ▼   defaults + credentials + URL checks
                                     InFlight map ──submit──▶ HttpTransport (UreqTransport:
                                            ▲                   N worker threads, one ureq Agent)
                                            │ First  BackendSystems::Receive
                             poll ◀─────────┘   deadlines, status + body-limit rules
                                            │
                                            ▼
                  BackendResponse / JsonResponse<T>  ──▶ PreUpdate / Update readers
```

- **One owner of every answer.** The plugin's `InFlight` map (ECS side) is the only thing that
  answers requests. The transport only reports; a result for an id that is no longer waiting is
  dropped. So every request gets exactly one answer, whatever the network does.
- **Scheduling.** `BackendSystems::Receive` runs in `First`, after Bevy's `TimeSystems` and
  before `MessageUpdateSystems`, so answers are readable in `PreUpdate` / `Update` of the frame
  they arrive. `BackendSystems::Send` runs in `PostUpdate`, so a request made in `Update` goes
  out the same frame (one made after it goes out next frame). `BackendSystems::Exit` runs in
  `Last` only in a frame with `AppExit`. The sets are `#[non_exhaustive]` and phase-named, so a
  later kind of connection can use the same three.
- **Threads.** ureq is blocking. `UreqTransport` starts its worker threads (named
  `net-backend-N`) on the first request and shares one `ureq::Agent` (keep-alive connection
  pool) between them. At most `workers` requests are on the wire; the rest wait in a queue.
  Worker results come back over a channel that `poll` drains without blocking. A panic inside
  the HTTP client is caught and answered as `Network`.
- **Timeouts.** ureq's global timeout (per request) ends the call. As a backstop the plugin
  answers `Timeout` itself when a request is still waiting after its timeout + 5 s
  (`DEADLINE_GRACE`, measured on `Time<Real>`, or a monotonic clock without `TimePlugin`).
- **Status codes.** The transport returns every status; the plugin turns anything outside
  200–299 into `Status`. Redirects are not followed (ureq `max_redirects(0)`), so a redirect can
  never downgrade `https://` to `http://` or carry credentials to another host.
- **Body limit.** The response body is read with a cap (`max_body_bytes`); the plugin checks it
  again for any transport.
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

## API reference

Everything is re-exported at the crate root; `prelude` holds the everyday items.

| Item | Kind | What it is |
|---|---|---|
| `BackendPlugin` | plugin | `new(config)`, `with_config`, `default()`. Inserts config, client, in-flight map, credentials, `BackendResponse`, and (feature `http`) a `UreqTransport` unless an `HttpTransportRes` exists. |
| `BackendSystems` | system sets | `Receive` (`First`), `Send` (`PostUpdate`), `Exit` (`Last`, on `AppExit`). `#[non_exhaustive]`. |
| `HttpConfig` | resource | base URL, timeout, default headers, workers, `allow_insecure_http`, body limit; `validate()`, getters, `set_base_url`, `set_timeout`. |
| `ConfigError` | enum | `NoBaseUrl`, `BadBaseUrl`, `BadHeader`. |
| `BackendClient` | resource | `send`, `request`, `get`, `cancel`; with `json`: `send_json`, `get_json`, `post_json`, `is_json_registered`. |
| `BackendAppExt` | trait on `App` | `add_json_response::<T>()` (feature `json`). |
| `RequestId` | id | opaque, unique per process, `Copy + Eq + Hash + Ord + Display`. |
| `OutgoingRequest` | request | constructors, `with_*` builders, accessors (`method`, `path`, `query`, `headers`, `body`, `timeout`, `purpose`, `uses_credentials`, `error`), `query_mut`, `headers_mut`, `set_body`, `reject`. |
| `RequestPurpose` | enum | `Http` (and `WebSocketHandshake`, reserved). |
| `BackendResponse` | message | `id`, `result: Result<RawResponse, BackendError>`. |
| `JsonResponse<T>` | message | `id`, `result: Result<T, BackendError>` (feature `json`). |
| `RawResponse` | struct | `status`, `headers`, `body`; `new`, `with_header`, `is_success`, `body()`, `text()`, `json()`. |
| `BackendError` | enum | see [Reading answers and errors](#4-reading-answers-and-errors); `status()`, `response()`, `is_invalid_request()`. |
| `Credentials` | trait | `apply(&self, &mut OutgoingRequest)`. |
| `BackendCredentials` | resource | `new`, `set`, `clear`, `is_set`. |
| `BearerToken`, `ApiKeyHeader`, `ApiKeyQuery`, `JsonBodyField` | credentials | ready-made `Credentials` (`JsonBodyField`: feature `json`). |
| `Secret` | string | redacted in `Debug` / `Display`; `new`, `expose`, `is_empty`. |
| `InFlight` | resource | `contains`, `len`, `is_empty`, `ids`, `describe`. |
| `HttpTransport` | trait | `submit`, `poll`, `cancel`, `shutdown`. |
| `HttpTransportResult` | type | `Result<RawResponse, BackendError>`. |
| `HttpTransportRes` | resource | `new(transport)`. |
| `PreparedRequest` | struct | what a transport receives: `method`, `uri`, `headers`, `body`, `timeout`, `max_body_bytes`, `purpose`; `path()`, `is_loopback()`, `is_https()`. |
| `FakeHttpTransport` | transport | `new`, `route`, `clear_routes`, `reply`, `requests`, `last_request`, `waiting`, `cancelled`, `shutdown_count`. |
| `UreqTransport` | transport | feature `http`: `new(&config)`, `workers()`. |
| `http` | crate | the `http` 1.x crate, re-exported (`Method`, `StatusCode`, `HeaderMap`, …). |
| `DEFAULT_TIMEOUT`, `MAX_TIMEOUT`, `DEFAULT_WORKERS`, `MAX_WORKERS`, `DEFAULT_MAX_BODY_BYTES`, `DEADLINE_GRACE` | consts | 15 s, 1 h, 2, 8, 10 MiB, 5 s. |

## Limits and what it does not do

- **Native only** (Windows, Linux, macOS). No WebAssembly in this version.
- **HTTP/1.1 only** (ureq). No HTTP/2, no streaming bodies: a response is read whole (up to the
  body limit) before it is delivered.
- **No redirects followed**: a 3xx arrives as `Status` with its `Location` header.
- **No retries, no offline queue, no caching, no cookies.** Retry in your game if a request
  matters; the error kind tells you whether it was sent.
- **No token refresh, no keyring:** credentials are whatever the game puts into
  `BackendCredentials`.
- **One base URL** per app. Paths are relative to it; absolute URLs are refused.
- **Root certificates** come from webpki-roots (Mozilla's list), not the OS store: a private CA
  or a corporate TLS-inspecting proxy is not trusted.
- **Cancel does not interrupt** a request already on the wire; it holds its worker thread until
  ureq's timeout at most.
- **No WebSocket** in this version.

## Compatibility

| bevy_net_backend | Bevy | ureq | rustls | Rust (MSRV) |
|---|---|---|---|---|
| 0.1.0 | 0.19.0 | 3.4.2 | 0.23.45 | 1.95 |

## Examples

All examples are headless and exit on their own. Without `BACKEND_URL` they start the mock server
from `examples/mock_server.rs` on 127.0.0.1 inside the example process.

| Example | Shows |
|---|---|
| `fetch_json` | `get_json::<Character>`, matching the answer by id, error bodies. |
| `post_with_token` | 401 before login, login `without_credentials`, `BearerToken`, a 422 validation error decoded from the error body, a successful authenticated `POST`. |
| `mock_server` | the mock API on its own (60 s or the seconds given; 127.0.0.1 and a free port, or the address given), and its JSON contract. |

```text
cargo run --example fetch_json
cargo run --example post_with_token
cargo run --example mock_server -- 120
cargo run --example mock_server -- 3600 127.0.0.1:8080
BACKEND_URL=http://127.0.0.1:8000/api cargo run --example fetch_json
```

Pointed at your own backend (`BACKEND_URL`), `fetch_json` expects `GET /characters/1` →
`{"id":1,"name":"…","class":"…","level":7}`, and `post_with_token` expects the routes in its
header comment (the login reads `BACKEND_USERNAME` / `BACKEND_PASSWORD`).

## Testing

- `cargo test` runs the unit tests, the `FakeHttpTransport` tests (every answer kind, exactly one
  answer each, strict ambiguity detection), a log-capture test proving no secret is logged, the
  loopback tests (the real `UreqTransport` against the mock server on 127.0.0.1: statuses,
  redirects, timeouts, body limit, TLS handshake failure, login flow, exit while busy) and every
  Rust block of this README.
- Tests never contact another host. CI runs the feature combinations on Linux, Windows and macOS
  with Rust 1.96.0.
- `tests/live.rs` holds live HTTPS checks, `#[ignore]`d: they run only with
  `cargo test --test live -- --ignored` and `BNB_TEST_HTTPS_URL` set to a server that serves the
  mock's contract over HTTPS (for example `mock_server` behind a TLS-terminating reverse proxy on
  a test machine).
- Against your real API, run the examples with `BACKEND_URL` (see [Examples](#examples)).

## FAQ

**Why not reqwest?** reqwest needs tokio (an async runtime next to Bevy's) and is a much larger
tree. A game API client does a handful of requests; blocking ureq on two threads is plenty.

**Why not Bevy's `IoTaskPool`?** It has 1–4 threads shared with asset loading. A 15-second
request would stall asset IO; a dedicated pool cannot.

**Can I call two different APIs?** Not in 0.1.0: one base URL per app.

**Is the answer delivered if my reader runs in `PostUpdate`?** Yes: messages stay readable for
two frames; `PreUpdate` and `Update` just see them first.

**Where does the token live between sessions?** Wherever your game keeps it; this crate only
sends what is in `BackendCredentials`.

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
#   b e v y _ n e t _ b a c k e n d  
 