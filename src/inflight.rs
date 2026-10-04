//! The pending-request bookkeeping ([`InFlight`]) and the plugin's three systems. The ECS side is
//! the ONLY thing that answers requests: every request gets exactly one answer, whatever the
//! transport does.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bevy_app::AppExit;
use bevy_ecs::message::{MessageReader, MessageWriter};
use bevy_ecs::resource::Resource;
use bevy_ecs::system::{Commands, Res, ResMut};
use bevy_time::{Real, Time};
use http::header::HeaderName;
use http::Method;

use crate::client::{HttpClient, Queued, Route};
use crate::config::HttpConfig;
use crate::credentials::BackendCredentials;
use crate::download::{HttpDownloadProgress, HttpDownloadResponse};
use crate::request::{build_uri, OutgoingRequest, PreparedRequest, RequestId};
use crate::response::{BackendError, HttpProgress, HttpResponse};
use crate::transport::{HttpTransportRes, HttpTransportResult};

/// How long after its own timeout a request is answered with a timeout by the plugin, in case the
/// transport never reports it (a stuck worker, a custom transport that forgets it).
pub const DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// The kind of connection a pending request belongs to. `#[non_exhaustive]`: match it with a
/// catch-all arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RequestKind {
    /// An HTTP request.
    Http,
    /// A request on a WebSocket connection (feature `ws`).
    WebSocket,
    /// A command on an SSH connection (feature `ssh`).
    Ssh,
    /// A file operation on an SSH connection (feature `sftp`).
    Sftp,
    /// A desktop sign-in at an OAuth / OpenID Connect provider (feature `oauth`).
    OAuth,
}

/// What a pending request is, for display (a "saving…" list, a debug overlay). Never holds a
/// query string, header value or body.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestInfo {
    /// The kind of connection.
    pub kind: RequestKind,
    /// The HTTP method, for [`RequestKind::Http`].
    pub method: Option<Method>,
    /// What the request targets: for HTTP the URL path without the query (as the game wrote it),
    /// for WebSocket and SSH the connection's name (never an SSH command line: it may hold a
    /// secret).
    pub target: String,
}

struct Entry {
    route: Route,
    info: RequestInfo,
    deadline: Duration,
    allowed: Duration,
    generation: u64,
}

type Answer = (RequestId, Route, HttpTransportResult);

/// Which protocol's systems own a set of rows in [`InFlight`] (HTTP keeps its own map). Each
/// protocol replaces only its own rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Protocol {
    #[cfg(feature = "ws")]
    WebSocket,
    #[cfg(feature = "ssh")]
    Ssh,
    #[cfg(feature = "oauth")]
    OAuth,
}

/// The cancel list every client shares: `HttpClient::cancel`, `WsClient::cancel`,
/// `SshClient::cancel` and `OAuthClient::cancel` push into it. Each protocol's systems CLAIM only
/// the ids they own (a request they hold or have queued), in any order, and leave the rest. An id
/// nobody claimed during two
/// `Send` phases in a row (an unknown or already answered id) is dropped by the core `Send`
/// system, which runs first; two phases, so an id pushed by a game system while a phase was
/// running still gets one full phase.
#[derive(Clone, Default)]
pub(crate) struct CancelList(Arc<Mutex<Vec<(RequestId, u8)>>>);

impl CancelList {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(RequestId, u8)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ask for `id` to be cancelled (any protocol).
    pub(crate) fn push(&self, id: RequestId) {
        self.lock().push((id, 0));
    }

    /// Take the ids `owns` recognises, oldest first; leave the others.
    pub(crate) fn claim(&self, mut owns: impl FnMut(RequestId) -> bool) -> Vec<RequestId> {
        let mut list = self.lock();
        let mut claimed = Vec::new();
        list.retain(|(id, _)| {
            if owns(*id) {
                claimed.push(*id);
                false
            } else {
                true
            }
        });
        claimed
    }

    /// Start of a `Send` phase: drop ids that stayed unclaimed through two phases.
    pub(crate) fn age(&self) {
        self.lock().retain_mut(|(_, passes)| {
            *passes = passes.saturating_add(1);
            *passes < 3
        });
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

/// Every request waiting for its answer, HTTP, WebSocket, SSH and sign-ins alike (optional
/// read-only tracking, e.g. for a "saving…" spinner). An HTTP request appears here in
/// `PostUpdate` ([`BackendSystems::Send`](crate::BackendSystems::Send)) of the frame it was made
/// in, a WebSocket request, SSH command or sign-in in the same place (also while it waits for its
/// connection). A request leaves it when it is answered; the answer message follows in `First`
/// (of that frame, or of the next one for answers decided in `PostUpdate`, such as a cancel).
#[derive(Resource)]
pub struct InFlight {
    entries: HashMap<RequestId, Entry>,
    /// Rows of the other protocols, each replaced only by its own systems.
    rows: HashMap<Protocol, HashMap<RequestId, RequestInfo>>,
    ready: Vec<Answer>,
    cancels: CancelList,
    epoch: Instant,
}

impl Default for InFlight {
    fn default() -> Self {
        Self { entries: HashMap::new(), rows: HashMap::new(), ready: Vec::new(), cancels: CancelList::default(), epoch: Instant::now() }
    }
}

impl fmt::Debug for InFlight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InFlight").field("pending", &self.len()).finish_non_exhaustive()
    }
}

impl InFlight {
    /// Whether `id` is still waiting for its answer.
    pub fn contains(&self, id: RequestId) -> bool {
        self.entries.contains_key(&id) || self.rows.values().any(|rows| rows.contains_key(&id))
    }

    /// How many requests are waiting.
    pub fn len(&self) -> usize {
        self.rows.values().fold(self.entries.len(), |n, rows| n.saturating_add(rows.len()))
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.rows.values().all(HashMap::is_empty)
    }

    /// The waiting ids, oldest first.
    pub fn ids(&self) -> Vec<RequestId> {
        let mut ids: Vec<RequestId> = self.entries.keys().chain(self.rows.values().flat_map(HashMap::keys)).copied().collect();
        ids.sort_unstable();
        ids
    }

    /// What a waiting request is (kind, method, target without query / connection name).
    pub fn describe(&self, id: RequestId) -> Option<&RequestInfo> {
        self.entries.get(&id).map(|e| &e.info).or_else(|| self.rows.values().find_map(|rows| rows.get(&id)))
    }

    fn now(&self, time: Option<&Time<Real>>) -> Duration {
        time.map_or_else(|| self.epoch.elapsed(), Time::elapsed)
    }

    pub(crate) fn cancel_list(&self) -> CancelList {
        self.cancels.clone()
    }

    /// Take the cancel ids `owns` recognises (see [`CancelList`]); the others stay for the other
    /// protocols.
    #[cfg_attr(not(any(feature = "ws", feature = "ssh", feature = "oauth")), allow(dead_code))]
    pub(crate) fn claim_cancels(&self, owns: impl FnMut(RequestId) -> bool) -> Vec<RequestId> {
        self.cancels.claim(owns)
    }

    /// Replace the rows of `protocol` (its systems call this after every change); the rows of the
    /// other protocols are untouched.
    #[cfg_attr(not(any(feature = "ws", feature = "ssh", feature = "oauth")), allow(dead_code))]
    pub(crate) fn set_rows(&mut self, protocol: Protocol, rows: impl IntoIterator<Item = (RequestId, RequestInfo)>) {
        let map = self.rows.entry(protocol).or_default();
        map.clear();
        map.extend(rows);
    }
}

/// Defaults, credentials and the URL applied to a game's request.
pub(crate) fn prepare(request: OutgoingRequest, config: &HttpConfig, credentials: Option<&BackendCredentials>) -> Result<PreparedRequest, BackendError> {
    let mut request = request;
    if let Some(error) = request.take_error() {
        return Err(error);
    }
    config.validate().map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
    check_method(request.method(), request.has_body())?;
    let own: HashSet<HeaderName> = request.headers().keys().cloned().collect();
    for (name, value) in config.headers() {
        if !own.contains(name) {
            request.headers_mut().append(name.clone(), value.clone());
        }
    }
    if request.uses_credentials() {
        if let Some(credentials) = credentials {
            credentials.apply(&mut request);
        }
    }
    if let Some(error) = request.take_error() {
        return Err(error);
    }
    if let Some(download) = request.download() {
        download.check()?;
    }
    let uri = build_uri(config.base_url(), request.path(), request.query(), config.insecure_http_allowed())?;
    let parts = request.into_parts();
    Ok(PreparedRequest {
        method: parts.method,
        uri,
        headers: parts.headers,
        body: parts.body,
        streaming_body: parts.stream,
        upload_progress: parts.progress,
        timeout: parts.timeout.unwrap_or(config.timeout()),
        max_body_bytes: config.max_body_bytes(),
        purpose: parts.purpose,
        download: parts.download,
    })
}

/// The methods sent: the standard ones except `CONNECT` (a tunnel, not an API call). `HEAD` has no
/// body. Anything else would fail inside the HTTP client after the request counted as "sent".
fn check_method(method: &Method, has_body: bool) -> Result<(), BackendError> {
    let standard = [Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE, Method::HEAD, Method::OPTIONS, Method::TRACE];
    if !standard.contains(method) {
        return Err(BackendError::InvalidRequest(format!("method `{method}` is not supported (standard methods only, no CONNECT)")));
    }
    if has_body && *method == Method::HEAD {
        return Err(BackendError::InvalidRequest("a HEAD request cannot have a body".into()));
    }
    Ok(())
}

/// `PostUpdate` ([`BackendSystems::Send`](crate::BackendSystems::Send)): hand every queued request
/// to the transport, or put its answer aside when it cannot be sent; apply cancels.
///
/// In a frame with an `AppExit` message nothing is submitted: the queue is left to
/// [`shutdown_on_exit`], which answers it with `Shutdown`, and a request answered `Shutdown` must
/// never reach the server.
#[allow(clippy::too_many_arguments)]
pub(crate) fn send_requests(
    client: Res<HttpClient>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<HttpTransportRes>>,
    config: Res<HttpConfig>,
    credentials: Option<Res<BackendCredentials>>,
    time: Option<Res<Time<Real>>>,
    mut exit: MessageReader<AppExit>,
) {
    // This system runs first in the phase: drop cancel ids nobody claimed for two phases.
    inflight.cancels.age();
    if exit.read().count() > 0 {
        return;
    }
    let queued = client.drain();
    // Only HTTP's own ids (a request queued this frame or waiting for its answer); the rest stays
    // in the shared list for the other protocols.
    let queued_ids: HashSet<RequestId> = queued.iter().map(|Queued::Send { id, .. }| *id).collect();
    let cancels = {
        let entries = &inflight.entries;
        inflight.cancels.claim(|id| queued_ids.contains(&id) || entries.contains_key(&id))
    };
    if queued.is_empty() && cancels.is_empty() {
        return;
    }
    let now = inflight.now(time.as_deref());
    // A cancel for a request queued in the same frame: answer it without ever submitting it.
    let cancelled: HashSet<RequestId> = cancels.iter().copied().collect();
    let mut claimed: HashSet<RequestId> = HashSet::new();
    for item in queued {
        match item {
            Queued::Send { id, route, .. } if cancelled.contains(&id) => {
                claimed.insert(id);
                inflight.ready.push((id, route, Err(BackendError::Cancelled)));
            }
            Queued::Send { id, request, route } => {
                let info = RequestInfo { kind: RequestKind::Http, method: Some(request.method().clone()), target: request.path().to_string() };
                match prepare(*request, &config, credentials.as_deref()) {
                    Err(error) => inflight.ready.push((id, route, Err(error))),
                    Ok(prepared) => match transport.as_mut() {
                        None => inflight.ready.push((id, route, Err(BackendError::NoTransport))),
                        Some(transport) if prepared.streaming_body.is_some() && !transport.get().streams_bodies() => inflight.ready.push((
                            id,
                            route,
                            Err(BackendError::InvalidRequest(
                                "the installed HTTP transport does not send streamed bodies (a multipart form with files from disk)".into(),
                            )),
                        )),
                        Some(transport) if prepared.download.is_some() && !transport.get().downloads_to_files() => inflight.ready.push((
                            id,
                            route,
                            Err(BackendError::InvalidRequest("the installed HTTP transport does not write downloads to files".into())),
                        )),
                        Some(transport) => {
                            let allowed = prepared.timeout.saturating_add(DEADLINE_GRACE);
                            if let Some(method) = &info.method {
                                tracing::debug!(">>> NET-BACKEND: {id} {method} {}", info.target);
                            }
                            inflight
                                .entries
                                .insert(id, Entry { route, info, deadline: now.saturating_add(allowed), allowed, generation: transport.generation() });
                            transport.get_mut().submit(id, prepared);
                        }
                    },
                }
            }
        }
    }
    for id in cancels {
        if claimed.contains(&id) || !inflight.entries.contains_key(&id) {
            continue;
        }
        // Too late when the transport already finished it (a download already put in place): its
        // result follows and is the answer.
        if transport.as_mut().is_none_or(|transport| transport.get_mut().try_cancel(id)) {
            if let Some(entry) = inflight.entries.remove(&id) {
                inflight.ready.push((id, entry.route, Err(BackendError::Cancelled)));
            }
        }
    }
}

/// The message writers answers go to.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct AnswerWriters<'w, 's> {
    raw: MessageWriter<'w, HttpResponse>,
    downloads: MessageWriter<'w, HttpDownloadResponse>,
    #[cfg_attr(not(feature = "json"), allow(dead_code))]
    commands: Commands<'w, 's>,
}

/// `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)): collect what the
/// transport reports, answer requests that can no longer be answered by it (deadline passed,
/// transport gone), and write every answer as a message for this frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn receive_answers(
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<HttpTransportRes>>,
    config: Res<HttpConfig>,
    time: Option<Res<Time<Real>>>,
    mut writers: AnswerWriters,
    mut progress: MessageWriter<HttpProgress>,
    mut download_progress: MessageWriter<HttpDownloadProgress>,
) {
    let mut answers = std::mem::take(&mut inflight.ready);
    let generation = transport.as_ref().map(|t| t.generation());
    if let Some(transport) = transport.as_mut() {
        // Results first, then progress: a worker sends its progress before its result, so every
        // progress of a request whose result is here is in the progress channel too. Progress is
        // written before the answers, only for requests that were still waiting.
        let results = transport.get_mut().poll();
        for (id, sent, total) in transport.get_mut().poll_progress() {
            if inflight.entries.get(&id).is_some_and(|entry| Some(entry.generation) == generation) {
                progress.write(HttpProgress { id, sent, total });
            }
        }
        for (id, received, total) in transport.get_mut().poll_download_progress() {
            if inflight.entries.get(&id).is_some_and(|entry| Some(entry.generation) == generation) {
                download_progress.write(HttpDownloadProgress { id, received, total });
            }
        }
        for (id, result) in results {
            match inflight.entries.get(&id) {
                Some(entry) if Some(entry.generation) == generation => {
                    if let Some(entry) = inflight.entries.remove(&id) {
                        answers.push((id, entry.route, result));
                    }
                }
                _ => tracing::debug!(">>> NET-BACKEND: {id}: a late result was discarded (already answered)"),
            }
        }
    }
    if !inflight.entries.is_empty() {
        let now = inflight.now(time.as_deref());
        let gone: Vec<(RequestId, Entry)> = inflight.entries.extract_if(|_, entry| Some(entry.generation) != generation).collect();
        answers.extend(gone.into_iter().map(|(id, entry)| (id, entry.route, Err(BackendError::NoTransport))));
        let late: Vec<(RequestId, Entry)> = inflight.entries.extract_if(|_, entry| entry.deadline <= now).collect();
        for (id, entry) in late {
            if transport.as_mut().is_none_or(|transport| transport.get_mut().try_cancel(id)) {
                answers.push((id, entry.route, Err(BackendError::Timeout(format!("no answer from the transport within {:?}", entry.allowed)))));
            } else {
                // The transport already finished it (a download already put in place): wait for
                // its result.
                inflight.entries.insert(id, entry);
            }
        }
    }
    deliver(answers, config.max_body_bytes(), &mut writers);
}

/// `Last` on `AppExit` ([`BackendSystems::Exit`](crate::BackendSystems::Exit)): shut the
/// transport down (the built-in one gives running downloads up to 1 s to remove their part
/// files; nothing waits for a network timeout), then answer everything still open: results that
/// arrived as they are (a download already put in place is answered with its file), the rest with
/// [`BackendError::Shutdown`] (or `Cancelled` when the game cancelled it).
pub(crate) fn shutdown_on_exit(
    client: Res<HttpClient>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<HttpTransportRes>>,
    config: Res<HttpConfig>,
    mut writers: AnswerWriters,
) {
    let mut answers = std::mem::take(&mut inflight.ready);
    let queued = client.drain();
    let queued_ids: HashSet<RequestId> = queued.iter().map(|Queued::Send { id, .. }| *id).collect();
    let cancelled: HashSet<RequestId> = {
        let entries = &inflight.entries;
        inflight.cancels.claim(|id| queued_ids.contains(&id) || entries.contains_key(&id)).into_iter().collect()
    };
    for Queued::Send { id, route, .. } in queued {
        let error = if cancelled.contains(&id) { BackendError::Cancelled } else { BackendError::Shutdown };
        answers.push((id, route, Err(error)));
    }
    let generation = transport.as_ref().map(|t| t.generation());
    if let Some(transport) = transport.as_mut() {
        // First the stop: from here on no download puts its file in place, so every result
        // polled below is final and matches what happened to the target.
        transport.get_mut().shutdown();
        for (id, result) in transport.get_mut().poll() {
            if inflight.entries.get(&id).is_some_and(|e| Some(e.generation) == generation) {
                if let Some(entry) = inflight.entries.remove(&id) {
                    // A cancel by the game wins, except over a download already put in place. A
                    // transfer the shutdown stopped reports `Cancelled`: the game did not ask.
                    let result = match result {
                        Ok(raw) if cancelled.contains(&id) && raw.file.is_none() => Err(BackendError::Cancelled),
                        Err(_) if cancelled.contains(&id) => Err(BackendError::Cancelled),
                        Err(BackendError::Cancelled) => Err(BackendError::Shutdown),
                        other => other,
                    };
                    answers.push((id, entry.route, result));
                }
            }
        }
    }
    for id in &cancelled {
        if let Some(entry) = inflight.entries.remove(id) {
            answers.push((*id, entry.route, Err(BackendError::Cancelled)));
        }
    }
    let open = inflight.entries.len();
    answers.extend(inflight.entries.drain().map(|(id, entry)| (id, entry.route, Err(BackendError::Shutdown))));
    if open > 0 {
        tracing::info!(">>> NET-BACKEND: app exit: {open} open request(s) answered with Shutdown");
    }
    deliver(answers, config.max_body_bytes(), &mut writers);
}

/// Turn transport results into final answers (status and body-limit rules) and write them, in
/// request order.
fn deliver(mut answers: Vec<Answer>, limit: u64, writers: &mut AnswerWriters) {
    answers.sort_by_key(|(id, _, _)| *id);
    for (id, route, result) in answers {
        let result = result.and_then(|response| {
            if u64::try_from(response.body.len()).unwrap_or(u64::MAX) > limit {
                Err(BackendError::BodyTooLarge { limit })
            } else if response.status.is_success() {
                Ok(response)
            } else {
                Err(BackendError::Status(Box::new(response)))
            }
        });
        match &result {
            Ok(response) => tracing::debug!(">>> NET-BACKEND: {id} -> {}", response.status),
            Err(
                error @ (BackendError::InvalidRequest(_)
                | BackendError::InsecureHttp { .. }
                | BackendError::Encode(_)
                | BackendError::RequestTooLarge { .. }
                | BackendError::NoTransport),
            ) => {
                tracing::warn!(">>> NET-BACKEND: {id} not sent: {error}")
            }
            Err(error) => tracing::debug!(">>> NET-BACKEND: {id} -> {error}"),
        }
        match route {
            Route::Raw => {
                writers.raw.write(HttpResponse { id, result });
            }
            #[cfg(feature = "json")]
            Route::Json(json) => json.deliver(id, result, &mut writers.commands),
            Route::Download => {
                let result = result.and_then(|response| match response.file {
                    Some(mut file) => {
                        file.status = response.status;
                        file.headers = response.headers;
                        Ok(file)
                    }
                    None => Err(BackendError::Network("the transport answered the download without writing the file".into())),
                });
                writers.downloads.write(HttpDownloadResponse { id, result });
            }
        }
    }
}
