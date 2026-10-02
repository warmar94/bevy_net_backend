# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a Bevy / key dependency bump).

## [Unreleased]

### Added

- WebSocket credentials refresh (feature `ws`, off by default):
  `WsSettings::with_credentials_refresh(WsCredentialsRefresh)`. When the server refuses the
  credentials (a `401` handshake, a refused first-message authentication, or a close code added
  with `WsCredentialsRefresh::with_close_code`, e.g. 4001), the connection goes
  `WsState::WaitingForCredentials` and the plugin writes one `WsCredentialsRefused { name, error }`
  message per refused credentials, however many connections wait. New credentials set with
  `BackendCredentials::set` start ONE new connection; a second refusal, cleared credentials or
  no new credentials within the timeout (default 30 s) end it `Disconnected` with the server's
  refusal. A further refresh is allowed only after a connection stayed up for the reconnect
  policy's `stable_after`. Credentials that changed since the refused handshake reconnect at once
  without a message. The crate never calls a refresh route itself.
- `Multipart::file_from_path(name, filename, content_type, path)`: a file part read from disk on
  the HTTP worker thread while the request is sent (64 KiB at a time, exact `Content-Length`); a
  file that cannot be opened is `InvalidRequest` and a form over `with_max_bytes`
  `RequestTooLarge`, both before anything is sent; a file that changes size while it is sent cuts
  the request off with a `Network` error.
- `Multipart::part(name, content_type, bytes)` (a non-file part with its own content type) and
  `Multipart::json(name, &value)` (feature `json`: `Content-Type: application/json`).
- Upload progress: the `HttpProgress { id, sent, total }` message (at most about 10 per second per
  request, plus one when the whole body is out), on for multipart forms,
  `OutgoingRequest::with_upload_progress(bool)` / `upload_progress()` for any body.
- `StreamingBody` / `StreamingReader` (a body read while it is sent), `OutgoingRequest::streaming_body()`,
  `PreparedRequest::streaming_body` and `PreparedRequest::upload_progress`;
  `HttpTransport::streams_bodies()` (default `false`: such a request is answered `InvalidRequest`
  and never handed over) and `HttpTransport::poll_progress()` (default: none);
  `FakeHttpTransport::progress(id, sent, total)`.
- `BackendError::retry_after()`: the `Retry-After` header of a `Status` answer (delta-seconds) as a
  `Duration` (at most `MAX_TIMEOUT`), else `None`.
- WebSocket (feature `ws`): a handshake refused with `429` or `503` and a `Retry-After` header waits
  at least that long before the next reconnect attempt (`WsState::Reconnecting::retry_in` shows it;
  above the backoff cap too, at most `MAX_TIMEOUT`). The attempt limit is unchanged.
- `mock_ws_server`: a path ending in `/busy` refuses the handshake with `503` and `Retry-After: 2`.
- The mock server's `/upload` echo names the content type of a field part that has one; the mock
  SSH server's `MockOptions` can answer SFTP reads short, slowly or with an error from an offset.

### Changed

- SFTP downloads are pipelined: 64 KiB reads go out ahead of the answers, up to 1 MiB requested or
  received and not written at once, written in file order; short reads ask again for the rest; a
  file the server reports larger than the transfer limit is refused before any data is read.
  Measured over loopback in a debug build: 256 MiB to a file in 1.5 s.
- An SFTP transfer that is cancelled or times out closes its remote file handle in the background.
- Each SFTP request may wait for its answer as long as the whole operation (`with_sftp_timeout`)
  instead of a fixed 30 s, so a slow link is not cut off while 1 MiB of reads is in flight.
- `Secret` overwrites its whole allocation with zeros when it is dropped, and `BearerToken` wipes
  its temporary `Bearer …` text (copies that become part of a request are not wiped) (`zeroize`, now a dependency of every feature set; rustls already
  used it, so the default build has the same 90 crates, `default-features = false` one more).
- Documentation states what the crate does, without plans.

## [0.1.0] - 2026-10-01

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
  dead-peer detection that does not count time blocked on the client's own writes.
- Examples `chat_client` and `mock_ws_server` (features `ws`, `json`).
- Feature `ssh` (admin / dev builds only): named SSH connections that run commands. `SshClient`
  (`connect(name, SshTarget)`, `disconnect`, `run`, `cancel`), `SshTarget` (host, port, user,
  `SshAuth` key files / key files with a passphrase / ssh-agent, known_hosts files, pinned
  fingerprints with `trust_host_key_fingerprint`, `from_ssh_config` for `~/.ssh/config` aliases,
  timeouts, keepalive, limits), `SshCommand` (timeout, output limit, stdin), `SshSettings` given
  with `BackendPlugin::with_ssh` (`allow_in_release`, connection and request limits),
  `SshConnections` / `SshState`, the messages `SshStateChanged`, `SshOutput` (stdout / stderr
  chunks) and `SshFinished` (exactly one per command, with `SshExit` and an honest `started`), the
  `SshTransport` seam with `FakeSshTransport` and the real `RusshTransport` (russh 0.63.3 with ring,
  one private tokio current-thread runtime on its own thread, started on the first connect).
- SSH host keys are always checked by the crate's own strict known_hosts matcher (patterns,
  negation, hashed hosts, `[host]:port`, `@revoked`); no trust-on-first-use, known_hosts is never
  written. Strict key exchange (Terrapin) is required for the ciphers that need it. SHA-1 RSA
  signatures are never used.
- SSH refuses to run in release builds unless `SshSettings::allow_in_release(true)`.
- Feature `sftp`: `upload`, `upload_file`, `download`, `download_file`, `list_dir`, `create_dir`,
  `remove_file`, `remove_dir`, `rename`; `SftpProgress` and `SftpFinished` (`SftpOutcome`,
  `SftpEntry`).
- Feature `ssh-rsa`: RSA host keys and RSA key files (the `rsa` crate carries RUSTSEC-2023-0071).
- `BackendError::HostKey { host, fingerprint, problem }` (`HostKeyProblem`), `AuthFailed` and
  `Ssh`, and `BackendError::host_key(..)` for fakes; `BodyTooLarge` also covers SSH output and SFTP
  downloads.
- `RequestKind::Ssh` and `RequestKind::Sftp`: SSH requests appear in `InFlight`, and the one shared
  `cancel` works for HTTP, WebSocket and SSH requests alike.
- Examples `ssh_console` and `mock_ssh_server` (feature `ssh`; SFTP with `sftp`).
- SSH, opt-in runtime settings (no extra features): `SshAuth::password` and
  `SshAuth::keyboard_interactive` (multi-prompt, e.g. password + 2FA code, via
  `SshPromptResponder` / the ready-made `SshPromptAnswers`); `SshTarget::with_reconnect(SshReconnect)`
  (off by default, backoff with jitter; never re-runs a command) with `SshState::Reconnecting`;
  `SshTarget::allow_terrapin_vulnerable` (off by default); `RusshTransport::with_release_allowed`.
- SSH prefers AES-GCM and refuses only the Terrapin-vulnerable combination (no strict key
  exchange with ChaCha20-Poly1305 or CBC + EtM); the host key types already in known_hosts are
  preferred, and a different key type is `Unknown`, not `Changed`.
- ssh_config `Include` is expanded by the crate with limits (depth 16, 64 files, 1 MiB).
- `SftpEntry::safe_file_name()`; listed names are documented as untrusted and capped.
- CI: a RustSec advisory check (cargo-deny) on pull requests, pushes and weekly.
- `multipart/form-data` uploads (part of `http`, no new dependency): `Multipart` (`text`, `file`,
  repeated names, `with_max_bytes` default 32 MiB, `with_max_parts` default 256), a fresh random
  boundary per request checked against the content, browser-style escaping of names;
  `HttpClient::post_multipart`, `send_multipart`, `post_multipart_json::<T>` (json),
  `OutgoingRequest::with_multipart` / `is_multipart` (cleared again when `with_json`, `with_body`
  or `set_body` replaces the form). `JsonBodyField` refuses a multipart request. Refused before
  sending (`InvalidRequest`): a field name or file name that ends with a backslash (real servers
  drop such a part or turn the file into a text field) or contains a control character other
  than CR / LF.
- `BackendError::RequestTooLarge { limit, size }` (`was_sent() == Some(false)`, and
  `BackendError::request_too_large(..)`): a request refused before sending because it is too big
  (multipart forms, WebSocket requests over the message limit, SSH command lines over 64 KiB, SFTP
  uploads over the transfer limit). `BodyTooLarge` is always the answer being too big.
- Example `upload`; the mock server's `POST /upload` parses multipart and echoes what it received.
- Features: `http` and `json` (default), `gzip`, `ws`, `ssh`, `sftp`, `ssh-rsa`. No tokio unless
  `ssh` is enabled.
