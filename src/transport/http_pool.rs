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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ureq::tls::{TlsConfig, TlsProvider};
use ureq::{Agent, AsSendBody};

use super::{HttpTransport, HttpTransportResult};
use crate::config::HttpConfig;
use crate::request::{PreparedRequest, RequestId};
use crate::response::{BackendError, RawResponse};

type Answer = (RequestId, HttpTransportResult);

/// Waiting for a free worker shorter than this does not shorten ureq's timeout (a request-level
/// timeout costs ureq a rebuild of its TLS config per new connection).
const QUEUE_SLACK: Duration = Duration::from_millis(100);

struct Job {
    id: RequestId,
    request: PreparedRequest,
    queued: Instant,
}

/// What every worker of one pool shares.
struct Shared {
    agent: Agent,
    default_timeout: Duration,
    jobs: Mutex<Receiver<Job>>,
    results: Sender<Answer>,
    cancelled: Arc<Mutex<HashSet<RequestId>>>,
    stopping: Arc<AtomicBool>,
}

/// One running pool. Stopping it (shutdown, drop, replacement) sets `stopping`: from then on its
/// workers send nothing that is still queued, so a request the plugin already answered is never
/// sent afterwards. Each pool has its own cancel marks, so marks never leak into a new pool.
struct Pool {
    jobs: Sender<Job>,
    cancelled: Arc<Mutex<HashSet<RequestId>>>,
    stopping: Arc<AtomicBool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The real HTTP transport: ureq 3.4 (HTTP/1.1, blocking) with rustls (ring crypto) and the
/// Mozilla root certificates, on [`HttpConfig::workers`] threads. The plugin creates it from the
/// config when the app has no [`HttpTransportRes`](crate::HttpTransportRes); create one yourself
/// to install it later.
///
/// - TLS crypto is ring's, handed to ureq explicitly (never a process-wide default), so another
///   crate's rustls setup cannot change or break it.
/// - Redirects are not followed: a 3xx arrives as [`BackendError::Status`] with its `Location`
///   header.
/// - Proxies from the `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` environment
///   variables are used (ureq's default), except for loopback hosts.
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
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub struct UreqTransport {
    agent: Agent,
    default_timeout: Duration,
    workers: usize,
    pool: Option<Pool>,
    results_tx: Sender<Answer>,
    results_rx: Mutex<Receiver<Answer>>,
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
    /// A transport for `config` (its worker count and default timeout). No thread starts until
    /// the first request.
    pub fn new(config: &HttpConfig) -> Self {
        let tls = TlsConfig::builder().provider(TlsProvider::Rustls).unversioned_rustls_crypto_provider(crate::tls::provider()).build();
        let agent =
            Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(config.timeout())).tls_config(tls).build().new_agent();
        let (results_tx, results_rx) = mpsc::channel();
        Self {
            agent,
            default_timeout: config.timeout(),
            workers: config.workers(),
            pool: None,
            results_tx,
            results_rx: Mutex::new(results_rx),
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
        let cancelled = Arc::new(Mutex::new(HashSet::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            agent: self.agent.clone(),
            default_timeout: self.default_timeout,
            jobs: Mutex::new(jobs_rx),
            results: self.results_tx.clone(),
            cancelled: Arc::clone(&cancelled),
            stopping: Arc::clone(&stopping),
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
        self.pool = Some(Pool { jobs: jobs_tx, cancelled, stopping });
        Ok(())
    }

    /// Stop the pool: queued jobs are dropped unsent, idle workers exit, busy ones exit after
    /// their call. Never joined: nothing waits for a network timeout.
    fn stop_pool(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.stopping.store(true, Ordering::SeqCst);
            tracing::info!(">>> NET-BACKEND: worker threads stopping");
        }
        self.running = 0;
    }
}

impl Drop for UreqTransport {
    /// A dropped (removed or replaced) transport sends nothing that is still queued.
    fn drop(&mut self) {
        self.stop_pool();
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
            self.stop_pool();
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
        // A result means its job left the queue: a cancel mark for it can go.
        if let (false, Some(pool)) = (out.is_empty(), &self.pool) {
            let mut cancelled = lock(&pool.cancelled);
            for (id, _) in &out {
                cancelled.remove(id);
            }
        }
        out
    }

    fn cancel(&mut self, id: RequestId) {
        if let Some(pool) = &self.pool {
            lock(&pool.cancelled).insert(id);
        }
    }

    fn shutdown(&mut self) {
        self.stop_pool();
    }
}

fn worker(shared: &Shared) {
    loop {
        let job = lock(&shared.jobs).recv();
        let Ok(Job { id, request, queued }) = job else { return };
        if shared.stopping.load(Ordering::SeqCst) {
            // Everything still queued was already answered by the plugin: send none of it.
            return;
        }
        if lock(&shared.cancelled).remove(&id) {
            continue;
        }
        let waited = queued.elapsed();
        let result = if waited >= request.timeout {
            Err(BackendError::Timeout(format!("not sent: no free worker within {:?}", request.timeout)))
        } else {
            let timeout = if waited < QUEUE_SLACK { request.timeout } else { request.timeout.saturating_sub(waited) };
            catch_unwind(AssertUnwindSafe(|| execute(&shared.agent, shared.default_timeout, request, timeout)))
                .unwrap_or_else(|panic| Err(BackendError::Network(format!("the HTTP client panicked: {}", panic_text(panic.as_ref())))))
        };
        if shared.results.send((id, result)).is_err() {
            return;
        }
    }
}

fn panic_text(panic: &(dyn Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

fn execute(agent: &Agent, default_timeout: Duration, request: PreparedRequest, timeout: Duration) -> HttpTransportResult {
    let PreparedRequest { method, uri, headers, body, max_body_bytes, .. } = request;
    let loopback = uri.host().is_some_and(crate::request::is_loopback_host);
    match body {
        Some(body) => run(agent, default_timeout, http_request(method, uri, headers, body), timeout, loopback, max_body_bytes),
        None => run(agent, default_timeout, http_request(method, uri, headers, ()), timeout, loopback, max_body_bytes),
    }
}

fn http_request<B>(method: http::Method, uri: http::Uri, headers: http::HeaderMap, body: B) -> http::Request<B> {
    let mut request = http::Request::new(body);
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    request
}

fn run<B: AsSendBody>(
    agent: &Agent,
    default_timeout: Duration,
    request: http::Request<B>,
    timeout: Duration,
    loopback: bool,
    limit: u64,
) -> HttpTransportResult {
    // A request-level config only when needed: with it ureq rebuilds its TLS config per new
    // connection instead of using the agent's cached one.
    let request = if loopback {
        agent.configure_request(request).timeout_global(Some(timeout)).proxy(None).build()
    } else if timeout != default_timeout {
        agent.configure_request(request).timeout_global(Some(timeout)).build()
    } else {
        request
    };
    let response = agent.run(request).map_err(|e| map_error(e, limit))?;
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
    Ok(RawResponse { status: parts.status, headers: parts.headers, body: bytes })
}

/// ureq's error in the crate's kinds, with ureq's own words.
fn map_error(error: ureq::Error, limit: u64) -> BackendError {
    match error {
        // `timed out (global limit)`, `timed out (connect limit)`, …
        ureq::Error::Timeout(which) => BackendError::Timeout(format!("{which} limit")),
        ureq::Error::BodyExceedsLimit(_) => BackendError::BodyTooLarge { limit },
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => BackendError::Tls(error.to_string()),
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
    use super::panic_text;

    #[test]
    fn panic_payloads_become_text() {
        assert_eq!(panic_text(&"static"), "static");
        assert_eq!(panic_text(&String::from("owned")), "owned");
        assert_eq!(panic_text(&42u8), "no message");
    }
}
