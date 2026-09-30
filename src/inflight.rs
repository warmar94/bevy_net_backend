//! The pending-request bookkeeping ([`InFlight`]) and the plugin's three systems. The ECS side is
//! the ONLY thing that answers requests: every request gets exactly one answer, whatever the
//! transport does.

use std::collections::{HashMap, HashSet};
use std::fmt;
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
use crate::request::{build_uri, OutgoingRequest, PreparedRequest, RequestId};
use crate::response::{BackendError, HttpResponse};
use crate::transport::{HttpTransportRes, HttpTransportResult};

/// How long after its own timeout a request is answered with a timeout by the plugin, in case the
/// transport never reports it (a stuck worker, a custom transport that forgets it).
pub const DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// The kind of connection a pending request belongs to. `#[non_exhaustive]`: later versions add
/// kinds (a WebSocket request, an SSH command).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RequestKind {
    /// An HTTP request.
    Http,
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
    /// What the request targets: for HTTP the URL path without the query (as the game wrote it).
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

/// Requests handed to the transport and not answered yet (optional read-only tracking, e.g. for
/// a "saving…" spinner). A request appears here in `PostUpdate`
/// ([`BackendSystems::Send`](crate::BackendSystems::Send)) of the frame it was made in and
/// leaves it in the frame its answer is written.
#[derive(Resource)]
pub struct InFlight {
    entries: HashMap<RequestId, Entry>,
    ready: Vec<Answer>,
    epoch: Instant,
}

impl Default for InFlight {
    fn default() -> Self {
        Self { entries: HashMap::new(), ready: Vec::new(), epoch: Instant::now() }
    }
}

impl fmt::Debug for InFlight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InFlight").field("pending", &self.entries.len()).finish_non_exhaustive()
    }
}

impl InFlight {
    /// Whether `id` was sent and is still waiting for its answer.
    pub fn contains(&self, id: RequestId) -> bool {
        self.entries.contains_key(&id)
    }

    /// How many requests are waiting.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The waiting ids, oldest first.
    pub fn ids(&self) -> Vec<RequestId> {
        let mut ids: Vec<RequestId> = self.entries.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// What a waiting request is (kind, method, target without query).
    pub fn describe(&self, id: RequestId) -> Option<&RequestInfo> {
        self.entries.get(&id).map(|e| &e.info)
    }

    fn now(&self, time: Option<&Time<Real>>) -> Duration {
        time.map_or_else(|| self.epoch.elapsed(), Time::elapsed)
    }
}

/// Defaults, credentials and the URL applied to a game's request.
pub(crate) fn prepare(request: OutgoingRequest, config: &HttpConfig, credentials: Option<&BackendCredentials>) -> Result<PreparedRequest, BackendError> {
    let mut request = request;
    if let Some(error) = request.take_error() {
        return Err(error);
    }
    config.validate().map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
    check_method(request.method(), request.body().is_some())?;
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
    let uri = build_uri(config.base_url(), request.path(), request.query(), config.insecure_http_allowed())?;
    let (method, headers, body, timeout, purpose) = request.into_parts();
    Ok(PreparedRequest { method, uri, headers, body, timeout: timeout.unwrap_or(config.timeout()), max_body_bytes: config.max_body_bytes(), purpose })
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
    if exit.read().count() > 0 {
        return;
    }
    let queued = client.drain();
    if queued.is_empty() {
        return;
    }
    let now = inflight.now(time.as_deref());
    // A cancel for a request queued in the same frame: answer it without ever submitting it.
    let cancelled: HashSet<RequestId> = queued.iter().filter_map(|q| if let Queued::Cancel(id) = q { Some(*id) } else { None }).collect();
    for item in queued {
        match item {
            Queued::Send { id, route, .. } if cancelled.contains(&id) => {
                inflight.ready.push((id, route, Err(BackendError::Cancelled)));
            }
            Queued::Cancel(id) => {
                if let Some(entry) = inflight.entries.remove(&id) {
                    if let Some(transport) = transport.as_mut() {
                        transport.get_mut().cancel(id);
                    }
                    inflight.ready.push((id, entry.route, Err(BackendError::Cancelled)));
                }
            }
            Queued::Send { id, request, route } => {
                let info = RequestInfo { kind: RequestKind::Http, method: Some(request.method().clone()), target: request.path().to_string() };
                match prepare(*request, &config, credentials.as_deref()) {
                    Err(error) => inflight.ready.push((id, route, Err(error))),
                    Ok(prepared) => match transport.as_mut() {
                        None => inflight.ready.push((id, route, Err(BackendError::NoTransport))),
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
}

/// `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)): collect what the
/// transport reports, answer requests that can no longer be answered by it (deadline passed,
/// transport gone), and write every answer as a message for this frame.
pub(crate) fn receive_answers(
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<HttpTransportRes>>,
    config: Res<HttpConfig>,
    time: Option<Res<Time<Real>>>,
    mut raw: MessageWriter<HttpResponse>,
    mut commands: Commands,
) {
    let mut answers = std::mem::take(&mut inflight.ready);
    let generation = transport.as_ref().map(|t| t.generation());
    if let Some(transport) = transport.as_mut() {
        for (id, result) in transport.get_mut().poll() {
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
            if let Some(transport) = transport.as_mut() {
                transport.get_mut().cancel(id);
            }
            answers.push((id, entry.route, Err(BackendError::Timeout(format!("no answer from the transport within {:?}", entry.allowed)))));
        }
    }
    deliver(answers, config.max_body_bytes(), &mut raw, &mut commands);
}

/// `Last` on `AppExit` ([`BackendSystems::Exit`](crate::BackendSystems::Exit)): answer
/// everything still open (results that already arrived as they are, the rest with
/// [`BackendError::Shutdown`]) and shut the transport down without waiting for it.
pub(crate) fn shutdown_on_exit(
    client: Res<HttpClient>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<HttpTransportRes>>,
    config: Res<HttpConfig>,
    mut raw: MessageWriter<HttpResponse>,
    mut commands: Commands,
) {
    let mut answers = std::mem::take(&mut inflight.ready);
    for item in client.drain() {
        match item {
            Queued::Send { id, route, .. } => answers.push((id, route, Err(BackendError::Shutdown))),
            Queued::Cancel(id) => {
                if let Some(entry) = inflight.entries.remove(&id) {
                    answers.push((id, entry.route, Err(BackendError::Cancelled)));
                }
            }
        }
    }
    let generation = transport.as_ref().map(|t| t.generation());
    if let Some(transport) = transport.as_mut() {
        for (id, result) in transport.get_mut().poll() {
            if inflight.entries.get(&id).is_some_and(|e| Some(e.generation) == generation) {
                if let Some(entry) = inflight.entries.remove(&id) {
                    answers.push((id, entry.route, result));
                }
            }
        }
    }
    let open = inflight.entries.len();
    answers.extend(inflight.entries.drain().map(|(id, entry)| (id, entry.route, Err(BackendError::Shutdown))));
    if let Some(transport) = transport.as_mut() {
        transport.get_mut().shutdown();
    }
    if open > 0 {
        tracing::info!(">>> NET-BACKEND: app exit: {open} open request(s) answered with Shutdown");
    }
    deliver(answers, config.max_body_bytes(), &mut raw, &mut commands);
}

/// Turn transport results into final answers (status and body-limit rules) and write them, in
/// request order.
fn deliver(mut answers: Vec<Answer>, limit: u64, raw: &mut MessageWriter<HttpResponse>, commands: &mut Commands) {
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
            Err(error @ (BackendError::InvalidRequest(_) | BackendError::InsecureHttp { .. } | BackendError::Encode(_) | BackendError::NoTransport)) => {
                tracing::warn!(">>> NET-BACKEND: {id} not sent: {error}")
            }
            Err(error) => tracing::debug!(">>> NET-BACKEND: {id} -> {error}"),
        }
        match route {
            Route::Raw => {
                raw.write(HttpResponse { id, result });
            }
            #[cfg(feature = "json")]
            Route::Json(json) => json.deliver(id, result, commands),
        }
    }
    #[cfg(not(feature = "json"))]
    let _ = commands;
}
