# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0: a minor bump for any API
change or a Bevy / key dependency bump).

## [0.2.0] - 2026-10-04

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
- WebSocket connections (feature `ws`) go through a proxy like HTTP requests (`ProxySettings`
  below; by default the `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` environment variables with the
  same `NO_PROXY` rules, read when the transport is created; loopback hosts always direct): a
  `CONNECT` tunnel through an `http://` proxy, `Proxy-Authorization: Basic` from a user and password
  in its URL. A refused or unreachable proxy is a `Network` error; with an `https://` or SOCKS proxy
  set, a connection that would use it fails with `InvalidRequest`.
- `WsTransport::send_auth(link, text)`: sends the first-message authentication (default: `send`
  with `WsFrame::Text`).
- `TlsSettings` (features `http` / `ws`), given with `BackendPlugin::with_tls` or to
  `UreqTransport::with_tls` / `TungsteniteTransport::with_tls`: which server certificates
  `https://` and `wss://` trust. `with_root_certificates_pem` / `with_root_certificates_file` add
  root certificates (e.g. a self-signed development server's) next to Mozilla's; `validate()`
  checks them (`ConfigError::Tls`). Settings that cannot be used answer every `https://` request
  and `wss://` connection with `InvalidRequest`, never sent. The default is unchanged (Mozilla's
  roots through ureq's own TLS step).
- Feature `os-certificates` (off by default; rustls-platform-verifier with ring):
  `TlsSettings::with_os_certificates(true)` uses the operating system's certificate store and
  checks instead of Mozilla's roots, extra roots on top.
- `SecretFile`: one `Secret` in a file between runs (`new`, `path`, `load`, `save`, `remove`):
  written atomically (new file, flushed, renamed over the old one), owner-only on Unix (`0600`,
  a new folder `0700`; Windows: the folder's inherited permissions), buffers wiped, the content
  never in an error or a log line; a damaged or foreign file is `InvalidData`. The crate does not
  log in or refresh with it.
- Downloads streamed to a file (`http`): `HttpClient::download(request, HttpDownload)` and
  `HttpClient::download_to(path, file)` write a 2xx answer body to a part file next to the target as
  it arrives (never held in memory as a whole; the in-memory answer limit does not apply), sync it
  and rename it over the target. `HttpDownload::to(path)` with `with_sha256`, `with_size`,
  `with_max_bytes` (default `DEFAULT_DOWNLOAD_MAX_BYTES`, 256 MiB) and `with_progress`. The answer
  is the `HttpDownloadResponse` message (`DownloadedFile`: `path`, `bytes`, `sha256`, `status`,
  `headers`), progress the `HttpDownloadProgress` message (`received`, `total`). The part file
  (`<name>.<process>-<request>-<n>.part`) is created before the request is sent (an unwritable
  folder or a target that is a folder is `InvalidRequest`, never sent) and removed on any error,
  cancel (the transfer stops at its next 64 KiB piece) or timeout; an existing target is replaced
  only on success. The rename happens under the lock the cancel takes: a download answered
  `Cancelled`, `Timeout` or `Shutdown` never replaced the target, and one whose file was already
  put in place is answered with the file. On app exit the HTTP transport gives running downloads
  up to 1 s to stop and remove their part files; a transfer still waiting for the server after
  that can leave its part file when the process ends, and the next download to that target removes
  part files other processes left. An answer outside 200–299 writes no file and arrives as
  `Status`. Seam: `PreparedRequest::download`, `RawResponse::file`,
  `HttpTransport::downloads_to_files()` (default `false`: such a request is answered
  `InvalidRequest` and never handed over), `HttpTransport::poll_download_progress()` (default:
  none) and `HttpTransport::try_cancel(id)` (`false` when the request already completed and its
  result is the answer; default: `cancel`, then `true`), `HttpDownload::receive` (feature `http`)
  for a transport's own writing, `OutgoingRequest::download()`. `FakeHttpTransport` writes a
  scripted 2xx answer's body to the file.
- Feature `oauth` (implies `http` and `json`; no new crate): the desktop sign-in at an OAuth 2.0 /
  OpenID Connect provider. `OAuthClient::sign_in(&OAuthFlow)` runs the authorization code flow
  with PKCE (S256), `state` and nonce on its own thread with a one-time loopback redirect listener
  on `127.0.0.1` (a free port); the sign-in page URL arrives as `OAuthSignInUrl` (the game opens the
  browser); the code is exchanged at the token endpoint over the crate's HTTP stack (the plugin's
  `TlsSettings`); the answer is `OAuthSignedIn` with `OAuthTokens` (`id_token`, `access_token`,
  `refresh_token` as `Secret`, `token_type`, `expires_in`, `scope`, `nonce`). A redirect with
  another `state` gets an error page and is ignored; the listener closes after the redirect with
  this sign-in's `state`; `OAuthClient::cancel` (or the shared `HttpClient::cancel`), the time
  limit (`OAuthFlow::with_timeout`, default 5 minutes) and app exit end it. `OAuthFlow::google`,
  `OAuthFlow::new` (any provider's endpoints; `https://`, `http://` only for loopback),
  `with_client_secret`, `with_scopes`, `with_param`; `GOOGLE_AUTHORIZATION_ENDPOINT`,
  `GOOGLE_TOKEN_ENDPOINT`, `DEFAULT_SIGN_IN_TIMEOUT`. Codes, tokens, the verifier and the client
  secret are never logged. The crate does not check the ID token or log in to a server with it.
- `BackendError::OAuth` (a sign-in the provider or its token endpoint ended) and
  `RequestKind::OAuth` (a running sign-in in `InFlight`).
- `ProxySettings` (features `http` / `ws`), given with `BackendPlugin::with_proxy` or to
  `UreqTransport::with_settings(&config, &tls, &proxy)` / `TungsteniteTransport::with_settings(&tls,
  &proxy)`: the proxy HTTP requests, WebSocket connections and the sign-in's code exchange use.
  `from_env()` (the default: `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` with `NO_PROXY`, read when
  the transport is created), `url("http://user:password@host:port")` (this proxy, the environment
  not read), `direct()` (none); loopback hosts always direct; `validate()` (`ConfigError::Proxy`);
  `Debug` without the user or password. A `url` that is not a proxy URL answers every request that
  would use it with `InvalidRequest`, never sent.
- `WipedBytes`: request body bytes overwritten with zeros (the whole allocation) when dropped
  (`Deref<Target = [u8]>`, `as_slice`, `From<Vec<u8>>`, `Debug` with the length only).
- The mock server's `/upload` echo names the content type of a field part that has one; the mock
  SSH server's `MockOptions` can answer SFTP reads short, slowly or with an error from an offset,
  close the SFTP channel from an offset, or report another size (or none) for open files, and
  `MockSshServer::drop_connections` closes every open connection.

### Changed

- **Breaking:** request bodies are wiped: `OutgoingRequest` holds its body (`with_body`, `with_json`,
  `set_body`, an in-memory `with_multipart` form, the body `JsonBodyField` rewrites) in a
  `WipedBytes`, and so do the in-memory pieces of a streamed form; they are overwritten with zeros
  once the request is answered and dropped. JSON is written into an exactly sized buffer (no
  reallocation copy), and `JsonBodyField` wipes the parsed copy it edits. `PreparedRequest::body` is
  now `Option<WipedBytes>` (it was `Option<Vec<u8>>`; read it as `&[u8]`, e.g.
  `request.body.as_deref()`).
- **Changed (behaviour):** WebSocket connections follow the proxy environment variables
  (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` with `NO_PROXY`) like HTTP requests; in 0.1.0 they
  always connected directly. A SOCKS proxy in the environment now answers HTTP requests and
  WebSocket connections that would use it `InvalidRequest` (never sent) instead of going around
  it. `BackendPlugin::with_proxy(ProxySettings::direct())` restores 0.1.0's direct connections.
- On `AppExit` the plugin calls `HttpTransport::shutdown` first and then polls the transport's
  last results (a result that arrived is delivered as it is, the rest answered `Shutdown`); it
  polled before the shutdown in 0.1.0. A cancel or deadline the transport reports as too late
  (`try_cancel`) leaves the request waiting for its result.
- `Secret` overwrites its whole allocation with zeros when it is dropped, and `BearerToken` wipes
  its temporary `Bearer …` text (copies that become part of a request are not wiped) (`zeroize`, now
  a dependency of every feature set; rustls already used it, so the default build has the same 90
  crates, `default-features = false` one more).
- `TungsteniteTransport` writes the WebSocket handshake request and the first-message
  authentication frame itself (from buffers it wipes after the write) and hands the stream to
  tungstenite after the `101` answer, which it checks as tungstenite does. Credential headers, an
  `ApiKeyQuery` key in the URL and the first-message authentication no longer appear in
  tungstenite's `trace` log lines.
- The HTTP transport resolves the proxy itself (the same variables and `NO_PROXY` rules as before)
  and hands ureq an explicit proxy or none; ureq no longer reads the environment.
- SFTP downloads are pipelined: 64 KiB reads go out ahead of the answers, up to 1 MiB requested or
  received and not written at once, written in file order; short reads ask again for the rest; a
  file the server reports larger than the transfer limit is refused before any data is read.
- An SFTP download into memory reserves the size the server reports for the file once (at most
  the transfer limit) instead of growing by doubling.
- SFTP `download_file` syncs the part file to disk before the rename (and, on Unix, the folder
  after it), as the HTTP download and `SecretFile` do; refuses a local target that is a folder
  before the transfer (`InvalidRequest`); and removes part files of that target other processes
  left.
- Each SFTP request may wait for its answer as long as the whole operation (`with_sftp_timeout`)
  instead of a fixed 30 s, so a slow link is not cut off while 1 MiB of reads is in flight.
- An SFTP operation whose connection or SFTP channel is lost while it runs is answered
  `Disconnected` (`sent: Some(true)` once it had started, `Some(false)` when it never went out)
  instead of `Ssh("SFTP: sender dropped")`; the next operation opens a new SFTP channel.
- Documentation states what the crate does.

### Fixed

- HTTP requests with a SOCKS proxy in the environment (`ALL_PROXY=socks5://…`,
  `HTTPS_PROXY=socks5h://…`) were sent directly, around the proxy. A request that would use a SOCKS
  proxy is now answered `InvalidRequest` and never sent, as WebSocket connections already were.
- An SFTP download whose remote file ends before the size the server reported when it was opened
  is an error (`Ssh("SFTP: the remote file was cut short during the download (expected N bytes, its
  size when it was opened; received M)")`) instead of a shorter success; no final file is written and
  the part file is removed. Files that report size 0 or no size are read to their end as before.
- An SFTP transfer that is cancelled or times out closes its remote file handle in the background.
- A local file error after an SFTP transfer started (reading the file during `upload_file`;
  writing, syncing or renaming the file of `download_file`) is `Ssh` with the file's name; it was
  `InvalidRequest`, whose `was_sent()` said `Some(false)` although data had already gone over the
  connection. Opening or creating the local file before the transfer stays `InvalidRequest`.
- A WebSocket handshake answer that is not valid HTTP (a status or header value the parser
  refuses, more than 124 headers) is a `Network` error, retried by the reconnect policy; it was
  `InvalidRequest` (permanent, "never sent") or a message about a 1009 close that never happened.

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
