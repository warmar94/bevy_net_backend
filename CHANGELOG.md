# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a Bevy / key dependency bump).

## [0.1.0] - Unreleased

### Added

- `BackendPlugin` and `HttpConfig` (base URL, timeout, default headers, worker count,
  `allow_insecure_http`, response body limit), with the public system sets
  `BackendSystems::{Receive, Send, Exit}`.
- `HttpClient` (used through `Res<HttpClient>`): `send`, `request`, `get`, `cancel`; with
  feature `json`: `get_json::<T>`, `post_json::<T>`, `send_json::<T>` and
  `App::add_json_response::<T>()`.
- Answers as messages: `HttpResponse` and `JsonResponse<T>`, exactly one per request, written
  in `First`.
- `BackendError` kinds: `InvalidRequest`, `InsecureHttp`, `Encode`, `Network`, `Tls`, `Timeout`,
  `BodyTooLarge`, `Status` (status + headers + body, with `RawResponse::json` for error bodies),
  `Decode`, `Cancelled`, `Shutdown`, `NoTransport`.
- Credentials: the `Credentials` trait, the `BackendCredentials` resource, `BearerToken`,
  `ApiKeyHeader`, `ApiKeyQuery`, `JsonBodyField` (feature `json`), and the redacted `Secret`.
- `InFlight`: read-only tracking of requests waiting for their answer, with `describe(id)` →
  `RequestInfo` (`RequestKind`, method, target).
- The transport seam: `HttpTransport`, `HttpTransportRes`, `PreparedRequest`, the in-memory
  `FakeHttpTransport`, and (feature `http`) `UreqTransport`: ureq 3.4.2 on a fixed pool of worker
  threads, rustls 0.23.45 with ring's crypto (always passed explicitly) and webpki-roots; gzip
  responses with feature `gzip`.
- Examples `fetch_json`, `post_with_token` and `mock_server` (a std-only mock API on 127.0.0.1,
  or a given address).
- Feature `ws`: named WebSocket connections. `WsClient` (`connect(name, WsSettings)`,
  `disconnect`, `send_text` / `send_binary`, `request_raw`, typed `request` with `json`, `cancel`),
  `WsConnections` / `WsState`, the messages `WsStateChanged`, `WsMessage`, `WsRawResponse`,
  `WsResponse<T>` and `WsPush<P>`, the `WsProtocol` trait and the default `JsonEnvelope`,
  `WsRequest` / `WsPushMessage` with `App::add_ws_request` / `add_ws_push`, reconnect with
  exponential backoff and jitter (`WsReconnect`), heartbeat and dead-peer detection, message size
  limits, credentials on every handshake plus `Credentials::ws_auth_message` for first-message
  auth, the `WsTransport` seam with `FakeWsTransport` and the real `TungsteniteTransport`
  (tungstenite 0.30.0, sync, one thread per connection, rustls + ring, no permessage-deflate).
- `BackendError::Disconnected { reason, sent }`, `BackendError::Closed { code, reason }` and
  `BackendError::Rejected(Box<Rejection>)` (bytes, `text()`, `json()`), plus
  `BackendError::was_sent()` and `close_code()`.
- WebSocket requests appear in `InFlight` (`RequestKind::WebSocket`), and one shared `cancel`
  (`HttpClient::cancel` = `WsClient::cancel`) works for HTTP and WebSocket requests.
- `WsSettings::with_auth_ack`: hold requests and frames until the server acknowledges the
  first-message auth (`WsIncoming::AuthOk`); without the acknowledgement in time the waiting
  requests are answered `Timeout` and the connection goes `Disconnected` (closed with 1008).
- WebSocket reconnect policy: TLS / certificate errors are permanent by default
  (`WsReconnect::with_tls_retry(true)` to retry them).
- WebSocket I/O deadlines: one deadline for TCP + TLS + handshake, a read budget per loop turn,
  dead-peer detection that does not count time blocked on our own writes.
- Examples `chat_client` and `mock_ws_server` (features `ws`, `json`).
- Features: `http` and `json` (default), `gzip`, `ws`.
