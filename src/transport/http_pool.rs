//! [`UreqTransport`] (feature `http`): ureq 3 on a fixed pool of std worker threads.
//!
//! ureq is blocking, so requests run on the crate's own threads (named `net-backend-N`), never on Bevy's
//! task pools. The threads start on the first request, share one `ureq::Agent` (one connection
//! pool, keep-alive) and hand `(id, result)` back over a channel that `poll` drains without
//! blocking. The plugin owns every answer; a worker's late result is simply discarded there.

use std::any::Any;
use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ureq::tls::{TlsConfig, TlsProvider};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{ConnectProxyConnector, Connector, TcpConnector};
use ureq::{Agent, AsSendBody, SendBody};

use super::tls_connector::{ConfiguredTls, TlsSetupError};
use super::{HttpTransport, HttpTransportResult};
use crate::body::LocalFileChanged;
use crate::config::HttpConfig;
use crate::download::{PartFile, ReadyFile};
use crate::proxy::{ProxyRoute, ProxySettings, Via};
use crate::request::{PreparedRequest, RequestId};
use crate::response::{BackendError, RawResponse};
use crate::tls::TlsSettings;

type Answer = (RequestId, HttpTransportResult);
type Progress = (RequestId, u64, Option<u64>);

/// Upload progress is reported at most this often per request (plus once at the end).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// On the app's exit ([`HttpTransport::shutdown`]) running downloads get this long to stop at
/// their next piece and remove their part files.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Waiting for a free worker shorter than this does not shorten ureq's timeout (a request-level
/// timeout costs ureq a rebuild of its TLS config per new connection).
const QUEUE_SLACK: Duration = Duration::from_millis(100);

struct Job {
    id: RequestId,
    request: PreparedRequest,
    queued: Instant,
}

/// The marks one pool's workers and the plugin agree on, under one lock: cancels, downloads that
/// were put in place, and how many downloads run.
#[derive(Default)]
struct Marks {
    /// Requests the plugin answered without the transport (cancel, timeout).
    cancelled: HashSet<RequestId>,
    /// Downloads whose file was renamed over the target; their result follows from `poll`.
    committed: HashSet<RequestId>,
    /// Downloads a worker is running (their part file may exist).
    downloads: usize,
}

/// The lock and the signal for [`Marks`]. `stopping` is only set while the lock is held, so a
/// worker that checks it under the lock sees the same state as the plugin.
#[derive(Default)]
struct Gate {
    marks: Mutex<Marks>,
    /// Signalled when a download ends (its part file removed or put in place).
    idle: Condvar,
    stopping: AtomicBool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, Marks> {
        lock(&self.marks)
    }

    fn stopped(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// What every worker of one pool shares.
struct Shared {
    agent: HttpAgent,
    default_timeout: Duration,
    jobs: Mutex<Receiver<Job>>,
    results: Sender<Answer>,
    progress: Sender<Progress>,
    download_progress: Sender<Progress>,
    gate: Arc<Gate>,
}

/// One running pool. Stopping it (shutdown, drop, replacement) sets `stopping`: from then on its
/// workers send nothing that is still queued, so a request the plugin already answered is never
/// sent afterwards. Each pool has its own marks, so they never leak into a new pool.
struct Pool {
    jobs: Sender<Job>,
    gate: Arc<Gate>,
}

/// Counts one running download; dropped (after its result was sent) it signals the gate.
struct Running<'a>(&'a Gate);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut marks = self.0.lock();
        marks.downloads = marks.downloads.saturating_sub(1);
        drop(marks);
        self.0.idle.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The real HTTP transport: ureq 3.4 (HTTP/1.1, blocking) with rustls (ring crypto) and the
/// Mozilla root certificates (or the trust of a [`TlsSettings`], see [`with_tls`](Self::with_tls)),
/// on [`HttpConfig::workers`] threads. The plugin creates it from the
/// config when the app has no [`HttpTransportRes`](crate::HttpTransportRes); insert your own
/// to use other settings.
///
/// - TLS crypto is ring's, handed to ureq explicitly (never a process-wide default), so another
///   crate's rustls setup cannot change or break it.
/// - Redirects are not followed: a 3xx arrives as [`BackendError::Status`] with its `Location`
///   header.
/// - The proxy comes from a [`ProxySettings`] (default: the `HTTPS_PROXY` / `HTTP_PROXY` /
///   `ALL_PROXY` / `NO_PROXY` environment variables, read when the transport is created; see
///   [`with_settings`](Self::with_settings)); loopback hosts never use one. `http://` and
///   `https://` proxies carry requests (`CONNECT`); a request that would have to use a SOCKS
///   proxy is answered [`BackendError::InvalidRequest`] and never sent around the proxy.
/// - The response body limit caps the bytes kept in memory after gzip decoding (feature `gzip`)
///   as well as the bytes on the wire.
/// - A request's timeout counts from the moment it is handed to this transport, time spent
///   waiting for a free worker included. A request still waiting when its timeout is over is
///   answered with a [`BackendError::Timeout`] whose text starts with `not sent:`; it never goes
///   out.
/// - A request that was cancelled, timed out, or answered because the app exits or this
///   transport is shut down, dropped or replaced is never sent afterwards if it was still waiting
///   for a worker. One already on the wire keeps its thread until ureq's timeout ends it (a
///   blocking call cannot be interrupted); its result is discarded.
/// - A download stops at its next 64 KiB piece after a cancel, a timeout or the shutdown and
///   removes its part file. Its file is renamed over the target only under the lock the cancel
///   takes ([`HttpTransport::try_cancel`]): a download answered `Cancelled`, `Timeout` or
///   `Shutdown` never replaces the target, and one that already replaced it is answered with its
///   file. On [`shutdown`](HttpTransport::shutdown) (the app's exit) running downloads get up to
///   1 s to remove their part files; one still waiting for the server after that can leave its
///   part file when the process ends (the next download to that target removes it).
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub struct UreqTransport {
    agent: HttpAgent,
    default_timeout: Duration,
    workers: usize,
    pool: Option<Pool>,
    results_tx: Sender<Answer>,
    results_rx: Mutex<Receiver<Answer>>,
    progress_tx: Sender<Progress>,
    progress_rx: Mutex<Receiver<Progress>>,
    download_tx: Sender<Progress>,
    download_rx: Mutex<Receiver<Progress>>,
    immediate: Vec<Answer>,
    running: usize,
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqTransport")
            .field("workers", &self.workers)
            .field("running", &self.running)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl UreqTransport {
    /// A transport for `config` (its worker count and default timeout) with the default TLS
    /// trust (Mozilla's root certificates). No thread starts until the first request.
    pub fn new(config: &HttpConfig) -> Self {
        Self::with_tls(config, &TlsSettings::default())
    }

    /// A transport for `config` that trusts the server certificates `tls` names (extra root
    /// certificates, the operating system's store). The PEM files are read now; when `tls` cannot
    /// be used, a warning is logged and every `https://` request is answered with its error
    /// (`InvalidRequest` or `Tls`), never sent. No thread starts until the first request.
    pub fn with_tls(config: &HttpConfig, tls: &TlsSettings) -> Self {
        Self::with_settings(config, tls, &ProxySettings::from_env())
    }

    /// A transport for `config` with these TLS settings (see [`with_tls`](Self::with_tls)) and
    /// this proxy (see [`ProxySettings`]; `from_env` reads the environment variables now). No
    /// thread starts until the first request.
    pub fn with_settings(config: &HttpConfig, tls: &TlsSettings, proxy: &ProxySettings) -> Self {
        let agent = build_agent(config.timeout(), tls, Arc::new(proxy.resolve()), true);
        let (results_tx, results_rx) = mpsc::channel();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (download_tx, download_rx) = mpsc::channel();
        Self {
            agent,
            default_timeout: config.timeout(),
            workers: config.workers(),
            pool: None,
            results_tx,
            results_rx: Mutex::new(results_rx),
            progress_tx,
            progress_rx: Mutex::new(progress_rx),
            download_tx,
            download_rx: Mutex::new(download_rx),
            immediate: Vec::new(),
            running: 0,
        }
    }

    /// How many worker threads this transport runs at most.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Start the pool: as many of the configured threads as the OS gives. `Err` when none.
    fn start(&mut self) -> Result<(), String> {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let gate = Arc::new(Gate::default());
        let shared = Arc::new(Shared {
            agent: self.agent.clone(),
            default_timeout: self.default_timeout,
            jobs: Mutex::new(jobs_rx),
            results: self.results_tx.clone(),
            progress: self.progress_tx.clone(),
            download_progress: self.download_tx.clone(),
            gate: Arc::clone(&gate),
        });
        let mut started = 0;
        let mut last_error = String::new();
        for n in 0..self.workers {
            let shared = Arc::clone(&shared);
            match thread::Builder::new().name(format!("net-backend-{n}")).spawn(move || worker(&shared)) {
                Ok(_) => started += 1,
                Err(e) => last_error = e.to_string(),
            }
        }
        if started == 0 {
            return Err(format!("could not start a worker thread: {last_error}"));
        }
        if started < self.workers {
            tracing::warn!(">>> NET-BACKEND: started {started} of {} worker threads ({last_error})", self.workers);
        } else {
            tracing::info!(">>> NET-BACKEND: started {started} worker threads");
        }
        self.running = started;
        self.pool = Some(Pool { jobs: jobs_tx, gate });
        Ok(())
    }

    /// Stop the pool: queued jobs are dropped unsent, idle workers exit, busy ones exit after
    /// their call; no download puts its file in place from now on. Returns the pool's gate (to
    /// wait for running downloads). Never joined: nothing waits for a network timeout.
    fn stop_pool(&mut self) -> Option<Arc<Gate>> {
        self.running = 0;
        let pool = self.pool.take()?;
        let marks = pool.gate.lock();
        pool.gate.stopping.store(true, Ordering::SeqCst);
        drop(marks);
        tracing::info!(">>> NET-BACKEND: worker threads stopping");
        Some(pool.gate)
    }
}

impl Drop for UreqTransport {
    /// A dropped (removed or replaced) transport sends nothing that is still queued.
    fn drop(&mut self) {
        let _ = self.stop_pool();
    }
}

impl HttpTransport for UreqTransport {
    fn submit(&mut self, id: RequestId, request: PreparedRequest) {
        if self.pool.is_none() {
            if let Err(why) = self.start() {
                self.immediate.push((id, Err(BackendError::Network(why))));
                return;
            }
        }
        let sent = match &self.pool {
            Some(pool) => pool.jobs.send(Job { id, request, queued: Instant::now() }).is_ok(),
            None => false,
        };
        if !sent {
            // Every worker is gone (they only exit when the pool stops, so this is not
            // expected); start a fresh pool next time.
            let _ = self.stop_pool();
            self.immediate.push((id, Err(BackendError::Network("the worker threads stopped".into()))));
        }
    }

    fn poll(&mut self) -> Vec<Answer> {
        let mut out = std::mem::take(&mut self.immediate);
        let rx = lock(&self.results_rx);
        // Empty or disconnected: nothing more this frame.
        while let Ok(answer) = rx.try_recv() {
            out.push(answer);
        }
        drop(rx);
        // A result means its job left the queue: its marks can go.
        if let (false, Some(pool)) = (out.is_empty(), &self.pool) {
            let mut marks = pool.gate.lock();
            for (id, _) in &out {
                marks.cancelled.remove(id);
                marks.committed.remove(id);
            }
        }
        out
    }

    fn cancel(&mut self, id: RequestId) {
        let _ = self.try_cancel(id);
    }

    fn try_cancel(&mut self, id: RequestId) -> bool {
        let Some(pool) = &self.pool else { return true };
        let mut marks = pool.gate.lock();
        if marks.committed.contains(&id) {
            // The file is already in place: the plugin waits for this result.
            return false;
        }
        marks.cancelled.insert(id);
        true
    }

    fn shutdown(&mut self) {
        let Some(gate) = self.stop_pool() else { return };
        // Running downloads see the stop at their next piece and remove their part files.
        let deadline = Instant::now() + EXIT_GRACE;
        let mut marks = gate.lock();
        while marks.downloads > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::info!(">>> NET-BACKEND: {} download(s) still waiting for the server at exit; their part files may stay", marks.downloads);
                break;
            }
            marks = gate.idle.wait_timeout(marks, left).map_or_else(|e| e.into_inner().0, |(marks, _)| marks);
        }
    }

    fn streams_bodies(&self) -> bool {
        true
    }

    fn poll_progress(&mut self) -> Vec<Progress> {
        let rx = lock(&self.progress_rx);
        let mut out = Vec::new();
        while let Ok(progress) = rx.try_recv() {
            out.push(progress);
        }
        out
    }

    fn downloads_to_files(&self) -> bool {
        true
    }

    fn poll_download_progress(&mut self) -> Vec<Progress> {
        let rx = lock(&self.download_rx);
        let mut out = Vec::new();
        while let Ok(progress) = rx.try_recv() {
            out.push(progress);
        }
        out
    }
}

/// The ureq agent of this crate and the proxy route it follows.
#[derive(Clone)]
pub(crate) struct HttpAgent {
    pub(crate) agent: Agent,
    proxy: Arc<ProxyRoute>,
    /// Whether the agent itself was given a proxy (ureq never reads the environment here).
    has_proxy: bool,
}

impl HttpAgent {
    /// Whether a request to `uri` must bypass the agent's proxy (`true`: send it with
    /// `proxy(None)`). `Err` when it would have to use a proxy ureq is not given (SOCKS, a URL
    /// ureq cannot read): answered, never sent, never sent around the proxy.
    pub(crate) fn bypass(&self, uri: &http::Uri) -> Result<bool, BackendError> {
        match self.proxy.route(uri, Via::Http)? {
            None => Ok(self.has_proxy),
            Some(_) if self.has_proxy => Ok(false),
            Some(_) => Err(BackendError::InvalidRequest("the proxy URL cannot be used by the HTTP client; the request is not sent around the proxy".into())),
        }
    }
}

/// The ureq agent of this crate: ring handed over explicitly, statuses as answers, no redirects,
/// `timeout` for the whole call, the trust of `tls` (the default keeps ureq's own TLS chain) and
/// the proxy of `proxy` given explicitly (`None` when there is none or it is not `http://` /
/// `https://`: ureq never reads the environment itself). `warn` logs settings that cannot be used.
pub(crate) fn build_agent(timeout: Duration, tls: &TlsSettings, proxy: Arc<ProxyRoute>, warn: bool) -> HttpAgent {
    let tls_config = TlsConfig::builder().provider(TlsProvider::Rustls).unversioned_rustls_crypto_provider(crate::tls::provider()).build();
    let ureq_proxy = match proxy.proxy() {
        Some(Ok(target)) => target.ureq(),
        _ => None,
    };
    let has_proxy = ureq_proxy.is_some();
    let agent_config =
        Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(timeout)).tls_config(tls_config).proxy(ureq_proxy).build();
    let agent = match tls.build() {
        // The default trust: ureq's own chain, unchanged.
        None => agent_config.new_agent(),
        Some(built) => {
            if let (Err(error), true) = (&built, warn) {
                tracing::warn!(">>> NET-BACKEND: the TLS settings cannot be used, every https:// request is answered with an error: {error}");
            }
            let connector = ().chain(ConnectProxyConnector::default()).chain(TcpConnector::default()).chain(ConfiguredTls::new(built));
            Agent::with_parts(agent_config, connector, DefaultResolver::default())
        }
    };
    HttpAgent { agent, proxy, has_proxy }
}

fn worker(shared: &Shared) {
    loop {
        let job = lock(&shared.jobs).recv();
        let Ok(Job { id, request, queued }) = job else { return };
        let download = request.download.is_some();
        {
            let mut marks = shared.gate.lock();
            if shared.gate.stopped() {
                // Everything still queued was already answered by the plugin: send none of it.
                return;
            }
            if marks.cancelled.remove(&id) {
                continue;
            }
            if download {
                marks.downloads = marks.downloads.saturating_add(1);
            }
        }
        // Counted until its result is sent (a shutdown waits for it, bounded).
        let running = download.then(|| Running(&shared.gate));
        let waited = queued.elapsed();
        let result = if waited >= request.timeout {
            Err(BackendError::Timeout(format!("not sent: no free worker within {:?}", request.timeout)))
        } else {
            let timeout = if waited < QUEUE_SLACK { request.timeout } else { request.timeout.saturating_sub(waited) };
            catch_unwind(AssertUnwindSafe(|| execute(shared, id, request, timeout)))
                .unwrap_or_else(|panic| Err(BackendError::Network(format!("the HTTP client panicked: {}", panic_text(panic.as_ref())))))
        };
        let sent = shared.results.send((id, result)).is_ok();
        drop(running);
        if !sent {
            return;
        }
    }
}

fn panic_text(panic: &(dyn Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

fn execute(shared: &Shared, id: RequestId, request: PreparedRequest, timeout: Duration) -> HttpTransportResult {
    let PreparedRequest { method, uri, mut headers, body, streaming_body, upload_progress, max_body_bytes, download, .. } = request;
    let (agent, default_timeout) = (&shared.agent.agent, shared.default_timeout);
    // A request that would have to use a proxy the client cannot use is answered here, before
    // anything (a part file included) is made: never sent, never sent around the proxy.
    let bypass = shared.agent.bypass(&uri)?;
    let report = upload_progress.then(|| (id, shared.progress.clone()));
    // A download's part file is created before anything is sent: a folder that cannot be written
    // is answered `InvalidRequest`, never sent.
    let part = download.as_ref().map(|download| PartFile::create(download, id)).transpose()?;
    let response = match (streaming_body, body) {
        (Some(stream), _) => {
            // The files are opened and measured here, on the worker: nothing is sent if one is
            // missing or the form is over its limit.
            let (len, reader) = stream.open()?;
            headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(len));
            let body = SendBody::from_owned_reader(Counting::new(reader, len, report));
            call(agent, default_timeout, http_request(method, uri, headers, body), timeout, bypass, max_body_bytes)
        }
        (None, Some(body)) if report.is_some() => {
            let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
            headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(len));
            // The reader owns the wiped body: it is overwritten when ureq drops the reader.
            let body = SendBody::from_owned_reader(Counting::new(std::io::Cursor::new(body), len, report));
            call(agent, default_timeout, http_request(method, uri, headers, body), timeout, bypass, max_body_bytes)
        }
        // ureq reads the bytes from the wiped buffer, which is overwritten when it drops here.
        (None, Some(body)) => call(agent, default_timeout, http_request(method, uri, headers, body.as_slice()), timeout, bypass, max_body_bytes),
        (None, None) => call(agent, default_timeout, http_request(method, uri, headers, ()), timeout, bypass, max_body_bytes),
    }?;
    match part {
        Some(part) if response.status().is_success() => write_download(shared, id, part, response),
        // No download, or an answer outside 200-299 (its body is read into memory as for any
        // request; the part file is removed when `part` drops).
        _ => read_body(response, max_body_bytes),
    }
}

/// Stream a 2xx answer into the download's part file; stop at the next piece on a cancel or
/// shutdown (the part file is removed).
fn write_download(shared: &Shared, id: RequestId, part: PartFile, response: http::Response<ureq::Body>) -> HttpTransportResult {
    let (parts, body) = response.into_parts();
    // A decoded body (gzip) has another length than the server's `Content-Length`.
    let length = if parts.headers.contains_key(http::header::CONTENT_ENCODING) {
        None
    } else {
        parts.headers.get(http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok())
    };
    let mut reader = body.into_with_config().limit(u64::MAX).reader();
    let sender = shared.download_progress.clone();
    let mut progress = |received: u64, total: Option<u64>| {
        // The plugin may be gone (exit): progress is only a report.
        let _ = sender.send((id, received, total));
    };
    let stop = || {
        let marks = shared.gate.lock();
        shared.gate.stopped() || marks.cancelled.contains(&id)
    };
    let ready = part.write_from(&mut reader, length, &mut progress, &stop, &|e| map_error(ureq::Error::from(e), u64::MAX))?;
    // The rename happens under the gate: after a cancel, timeout or shutdown mark the target is
    // never replaced, and once it is replaced the plugin waits for this result.
    let file = {
        let mut marks = shared.gate.lock();
        if shared.gate.stopped() || marks.cancelled.contains(&id) {
            // `ready` drops here: the part file is removed.
            return Err(BackendError::Cancelled);
        }
        let file = ready.rename()?;
        marks.committed.insert(id);
        file
    };
    ReadyFile::sync_folder(&file);
    Ok(RawResponse { status: parts.status, headers: parts.headers, body: Vec::new(), file: Some(file) })
}

/// A body reader that counts what ureq reads for sending and reports it (throttled, and once
/// when the whole body is read).
struct Counting<R> {
    inner: R,
    sent: u64,
    total: u64,
    report: Option<(RequestId, Sender<Progress>)>,
    last: Option<Instant>,
}

impl<R: Read> Counting<R> {
    fn new(inner: R, total: u64, report: Option<(RequestId, Sender<Progress>)>) -> Self {
        Self { inner, sent: 0, total, report, last: None }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.total == 0 && self.last.is_none() {
            // An empty body: its one "whole body is out" report.
            self.last = Some(Instant::now());
            if let Some((id, sender)) = &self.report {
                let _ = sender.send((*id, 0, Some(0)));
            }
        }
        if n > 0 {
            self.sent = self.sent.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            if let Some((id, sender)) = &self.report {
                let now = Instant::now();
                if self.sent >= self.total || self.last.is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_EVERY) {
                    self.last = Some(now);
                    // The plugin may be gone (exit): progress is only a report.
                    let _ = sender.send((*id, self.sent, Some(self.total)));
                }
            }
        }
        Ok(n)
    }
}

fn http_request<B>(method: http::Method, uri: http::Uri, headers: http::HeaderMap, body: B) -> http::Request<B> {
    let mut request = http::Request::new(body);
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    request
}

fn call<B: AsSendBody>(
    agent: &Agent,
    default_timeout: Duration,
    request: http::Request<B>,
    timeout: Duration,
    bypass: bool,
    limit: u64,
) -> Result<http::Response<ureq::Body>, BackendError> {
    // A request-level config only when needed: with it ureq rebuilds its TLS config per new
    // connection instead of using the agent's cached one. `bypass`: a loopback host or a
    // `NO_PROXY` host while the agent has a proxy.
    let request = if bypass {
        agent.configure_request(request).timeout_global(Some(timeout)).proxy(None).build()
    } else if timeout != default_timeout {
        agent.configure_request(request).timeout_global(Some(timeout)).build()
    } else {
        request
    };
    agent.run(request).map_err(|e| map_error(e, limit))
}

/// Read an answer body into memory, capped at `limit` bytes.
pub(crate) fn read_body(response: http::Response<ureq::Body>, limit: u64) -> HttpTransportResult {
    let (parts, body) = response.into_parts();
    // ureq's limit counts the bytes on the wire (before gzip decoding) and refuses the read AFTER
    // `limit` bytes even at the end of the body, so it gets one byte more. `take` then caps the
    // DECODED bytes: a gzip bomb stops at `limit + 1` bytes in memory. The exact limit (a body of
    // exactly `limit` bytes is fine) is checked here and again by the plugin.
    let cap = limit.saturating_add(1);
    let mut reader = body.into_with_config().limit(cap).reader().take(cap);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|e| map_error(ureq::Error::from(e), limit))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(BackendError::BodyTooLarge { limit });
    }
    Ok(RawResponse { status: parts.status, headers: parts.headers, body: bytes, file: None })
}

/// ureq's error in the crate's kinds, with ureq's own words.
pub(crate) fn map_error(error: ureq::Error, limit: u64) -> BackendError {
    match error {
        // `timed out (global limit)`, `timed out (connect limit)`, …
        ureq::Error::Timeout(which) => BackendError::Timeout(format!("{which} limit")),
        ureq::Error::BodyExceedsLimit(_) => BackendError::BodyTooLarge { limit },
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => BackendError::Tls(error.to_string()),
        // The TlsSettings could not be used: their own error (nothing was sent).
        ureq::Error::Io(ref io) if io.get_ref().is_some_and(|inner| inner.is::<TlsSetupError>()) => {
            io.get_ref().and_then(|inner| inner.downcast_ref::<TlsSetupError>()).map_or_else(|| BackendError::Tls(error.to_string()), |setup| setup.0.clone())
        }
        // A local file of a streamed form changed while it was sent: our own words.
        ureq::Error::Io(ref io) if io.get_ref().is_some_and(|inner| inner.is::<LocalFileChanged>()) => {
            BackendError::Network(io.get_ref().map(ToString::to_string).unwrap_or_default())
        }
        ureq::Error::Io(ref io) if io.kind() == std::io::ErrorKind::TimedOut => BackendError::Timeout(format!("socket: {io}")),
        ureq::Error::Io(ref io) if io.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) => BackendError::Tls(error.to_string()),
        ureq::Error::Http(_) => BackendError::InvalidRequest(error.to_string()),
        // ureq's text repeats the URL, whose query may hold a key: say only what it is.
        ureq::Error::BadUri(_) => BackendError::InvalidRequest("ureq rejected the URL (bad uri)".into()),
        _ => BackendError::Network(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{panic_text, UreqTransport};
    use crate::config::HttpConfig;
    use crate::download::HttpDownload;
    use crate::request::{OutgoingRequest, RequestId};
    use crate::response::BackendError;
    use crate::transport::{HttpTransport, HttpTransportResult};

    #[test]
    fn panic_payloads_become_text() {
        assert_eq!(panic_text(&"static"), "static");
        assert_eq!(panic_text(&String::from("owned")), "owned");
        assert_eq!(panic_text(&42u8), "no message");
    }

    /// A server on 127.0.0.1 that answers every connection with `pieces` pieces of 64 KiB, `pause`
    /// apart.
    fn serve(pieces: usize, pause: Duration) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| panic!("{e}"));
        thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { return };
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut byte).map_or(true, |n| n == 0) {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", pieces * 65_536);
                    for _ in 0..pieces {
                        if stream.write_all(&[7u8; 65_536]).is_err() {
                            return;
                        }
                        thread::sleep(pause);
                    }
                });
            }
        });
        port
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bnb-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).map(|it| it.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
    }

    fn submit(transport: &mut UreqTransport, port: u16, target: &Path) -> RequestId {
        let config = HttpConfig::new(format!("http://127.0.0.1:{port}")).with_timeout(Duration::from_secs(10));
        let mut request = OutgoingRequest::get("/file");
        request.set_download(HttpDownload::to(target).with_progress(false));
        let prepared = crate::inflight::prepare(request, &config, None).unwrap_or_else(|e| panic!("{e}"));
        let id = RequestId::next();
        transport.submit(id, prepared);
        id
    }

    fn result(transport: &mut UreqTransport, id: RequestId) -> HttpTransportResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some((_, result)) = transport.poll().into_iter().find(|(got, _)| *got == id) {
                return result;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no result for {id}")
    }

    /// The race between a cancel and the rename: whichever comes first decides, and the answer says
    /// what happened to the target.
    #[test]
    fn a_cancel_and_the_rename_never_both_happen() {
        let dir = folder("race");
        let mut transport = UreqTransport::new(&HttpConfig::new("http://127.0.0.1:1"));

        // The rename first: the file is in place before anyone polls, so the cancel is too late
        // and the result is the file.
        let port = serve(2, Duration::ZERO);
        let target = dir.join("done.bin");
        let id = submit(&mut transport, port, &target);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !target.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(target.exists(), "the download finished");
        assert!(!transport.try_cancel(id), "too late: the file is in place");
        let file = result(&mut transport, id).unwrap_or_else(|e| panic!("{e}")).file;
        assert_eq!(file.map(|f| f.bytes), Some(2 * 65_536));

        // The cancel first: the transfer stops, the target never appears, no part file stays.
        let port = serve(40, Duration::from_millis(30));
        let target = dir.join("cancelled.bin");
        let id = submit(&mut transport, port, &target);
        let deadline = Instant::now() + Duration::from_secs(10);
        while names(&dir).len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(transport.try_cancel(id), "in time");
        assert!(matches!(result(&mut transport, id), Err(BackendError::Cancelled)));
        assert_eq!(names(&dir), vec!["done.bin".to_string()]);

        // The shutdown waits (bounded) until a running download removed its part file.
        let port = serve(40, Duration::from_millis(30));
        let id = submit(&mut transport, port, &dir.join("exit.bin"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while names(&dir).len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        let started = Instant::now();
        transport.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2), "bounded");
        assert_eq!(names(&dir), vec!["done.bin".to_string()], "no part file once shutdown returns");
        assert!(matches!(result(&mut transport, id), Err(BackendError::Cancelled)), "the plugin turns this into Shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
