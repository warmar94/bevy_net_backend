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
- Features: `http` and `json` (default), `gzip`.
