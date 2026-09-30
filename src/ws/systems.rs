//! The ECS side of `ws`: one entry per named connection, backoff, requests, and the three
//! systems. Like HTTP, the ECS side is the only thing that answers requests (exactly one answer
//! each), and a request answered `Shutdown` in the exit frame is never sent. Every answer says
//! honestly whether the request went out (`BackendError::was_sent`).

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

use bevy_app::AppExit;
use bevy_ecs::message::{MessageReader, MessageWriter};
use bevy_ecs::resource::Resource;
use bevy_ecs::system::{Commands, Res, ResMut};
use bevy_time::{Real, Time};
use http::Uri;

use super::transport::{WsHandshake, WsLinkEvent, WsLinkId, WsTransportRes};
use super::{
    WsClient, WsConnectionInfo, WsConnections, WsFrame, WsIncoming, WsMessage, WsName, WsQueued, WsRawResponse, WsRoute, WsSettings, WsState, WsStateChanged,
};
use crate::credentials::BackendCredentials;
use crate::inflight::{InFlight, Protocol, RequestInfo, RequestKind};
use crate::request::{check_path, check_scheme, encode_component, OutgoingRequest, RequestId, RequestPurpose};
use crate::response::{BackendError, Rejection};

/// A request waiting for its answer.
struct Request {
    id: RequestId,
    kind: String,
    payload: Vec<u8>,
    route: WsRoute,
    resend: bool,
    deadline: Duration,
    timeout: Duration,
    /// Sent on the current link.
    sent: bool,
    /// Sent on some link (this one or an earlier one): the server may have it.
    ever_sent: bool,
}

/// One named connection.
struct Conn {
    settings: WsSettings,
    state: WsState,
    link: Option<WsLinkId>,
    generation: u64,
    attempt: u32,
    retry_at: Option<Duration>,
    connected_at: Option<Duration>,
    last_error: Option<BackendError>,
    outbox: VecDeque<WsFrame>,
    requests: Vec<Request>,
    no_retry: bool,
    /// Requests and frames may go out (false while waiting for the auth acknowledgement).
    authed: bool,
    auth_deadline: Option<Duration>,
}

impl Conn {
    fn can_send(&self) -> bool {
        self.state == WsState::Connected && self.authed
    }

    fn waiting(&self) -> usize {
        self.requests.iter().filter(|r| !r.sent).count()
    }
}

type Answer = (RequestId, WsName, WsRoute, Result<Vec<u8>, BackendError>);

/// Private bookkeeping.
#[derive(Resource)]
pub(crate) struct WsRuntime {
    conns: HashMap<WsName, Conn>,
    links: HashMap<WsLinkId, WsName>,
    ready: Vec<Answer>,
    changes: Vec<WsStateChanged>,
    epoch: Instant,
    rng: u64,
}

impl Default for WsRuntime {
    fn default() -> Self {
        let seed = RandomState::new().hash_one(Instant::now());
        Self { conns: HashMap::new(), links: HashMap::new(), ready: Vec::new(), changes: Vec::new(), epoch: Instant::now(), rng: seed | 1 }
    }
}

impl WsRuntime {
    fn now(&self, time: Option<&Time<Real>>) -> Duration {
        time.map_or_else(|| self.epoch.elapsed(), Time::elapsed)
    }

    /// xorshift64: jitter only, not security.
    fn random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// The WebSocket rows of `InFlight`: every request not answered yet.
    fn publish(&self, inflight: &mut InFlight) {
        inflight.set_rows(
            Protocol::WebSocket,
            self.conns.iter().flat_map(|(name, conn)| {
                conn.requests.iter().map(move |r| (r.id, RequestInfo { kind: RequestKind::WebSocket, method: None, target: name.to_string() }))
            }),
        );
    }
}

/// Whether `id` is a WebSocket request: queued this frame or waiting on a connection.
fn owns(runtime: &WsRuntime, queued: &[WsQueued], id: RequestId) -> bool {
    queued.iter().any(|item| matches!(item, WsQueued::Request { id: q, .. } | WsQueued::Fail { id: q, .. } if *q == id))
        || runtime.conns.values().any(|conn| conn.requests.iter().any(|r| r.id == id))
}

/// Check a `ws://` / `wss://` URL (scheme, host, no user info or fragment, no path tricks).
pub(crate) fn parse_ws_url(url: &str) -> Result<Uri, BackendError> {
    let url = url.trim();
    if url.contains('#') {
        return Err(BackendError::InvalidRequest("the WebSocket URL must not have a fragment".into()));
    }
    let uri = Uri::try_from(url).map_err(|e| BackendError::InvalidRequest(format!("bad WebSocket URL ({e})")))?;
    match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("ws" | "wss") => {}
        _ => return Err(BackendError::InvalidRequest("the WebSocket URL must start with ws:// or wss://".into())),
    }
    let Some(authority) = uri.authority() else { return Err(BackendError::InvalidRequest("the WebSocket URL needs a host".into())) };
    if authority.as_str().contains('@') {
        return Err(BackendError::InvalidRequest("a user name or password in the URL is not supported; use Credentials".into()));
    }
    if authority.host().is_empty() {
        return Err(BackendError::InvalidRequest("the WebSocket URL needs a host".into()));
    }
    check_path(uri.path())?;
    Ok(uri)
}

/// The handshake for the next attempt: settings headers, then the game's credentials
/// (`RequestPurpose::WebSocketHandshake`), then the URL policy.
fn prepare_handshake(settings: &WsSettings, credentials: Option<&BackendCredentials>) -> Result<WsHandshake, BackendError> {
    settings.validate()?;
    let uri = parse_ws_url(&settings.url)?;
    let mut request = OutgoingRequest::get("/");
    request.set_purpose(RequestPurpose::WebSocketHandshake);
    for (name, value) in &settings.headers {
        request.headers_mut().append(name.clone(), value.clone());
    }
    if settings.credentials {
        if let Some(credentials) = credentials {
            credentials.apply(&mut request);
        }
    }
    if let Some(error) = request.take_error() {
        return Err(error);
    }
    if request.body().is_some() {
        return Err(BackendError::InvalidRequest(
            "credentials set a body on a WebSocket handshake; use first-message auth (Credentials::ws_auth_message)".into(),
        ));
    }
    let uri = if request.query().is_empty() {
        uri
    } else {
        let mut url = uri.to_string();
        let mut first = uri.query().is_none();
        for (name, value) in request.query() {
            url.push(if first { '?' } else { '&' });
            first = false;
            encode_component(name, &mut url);
            url.push('=');
            encode_component(value, &mut url);
        }
        Uri::try_from(url.as_str()).map_err(|e| BackendError::InvalidRequest(format!("bad WebSocket URL after credentials ({e})")))?
    };
    check_scheme(&uri, settings.allow_insecure)?;
    let (_, headers, _, _, _) = request.into_parts();
    Ok(WsHandshake {
        uri,
        headers,
        connect_timeout: settings.connect_timeout,
        read_timeout: settings.read_timeout,
        ping_interval: settings.ping_interval,
        dead_after: settings.dead_after,
        max_message_bytes: settings.max_message_bytes,
    })
}

fn set_state(changes: &mut Vec<WsStateChanged>, name: &WsName, conn: &mut Conn, state: WsState, error: Option<BackendError>) {
    if error.is_some() {
        conn.last_error = error.clone();
    }
    if conn.state != state || error.is_some() {
        conn.state = state;
        changes.push(WsStateChanged { name: name.clone(), state, error });
    }
}

/// Encode and send the request at `index` on an open link.
fn send_request(transport: &mut WsTransportRes, link: WsLinkId, conn: &mut Conn, index: usize, ready: &mut Vec<Answer>, name: &WsName) {
    let Some(protocol) = conn.settings.protocol.clone() else { return };
    let Some(request) = conn.requests.get_mut(index) else { return };
    match protocol.encode_request(request.id.wire(), &request.kind, &request.payload) {
        Ok(frame) if frame.len() > conn.settings.max_message_bytes => {
            let request = conn.requests.remove(index);
            let (limit, size) = (u64::try_from(conn.settings.max_message_bytes).unwrap_or(u64::MAX), u64::try_from(frame.len()).unwrap_or(u64::MAX));
            ready.push((request.id, name.clone(), request.route, Err(BackendError::RequestTooLarge { limit, size })));
        }
        Ok(frame) => {
            request.sent = true;
            request.ever_sent = true;
            transport.get_mut().send(link, frame);
        }
        Err(why) => {
            let request = conn.requests.remove(index);
            ready.push((
                request.id,
                name.clone(),
                request.route,
                Err(BackendError::InvalidRequest(format!("the protocol could not encode the request: {why}"))),
            ));
        }
    }
}

/// Send everything that waited: queued frames, then requests not sent on this link.
fn flush_waiting(transport: &mut WsTransportRes, link: WsLinkId, conn: &mut Conn, ready: &mut Vec<Answer>, name: &WsName) {
    while let Some(frame) = conn.outbox.pop_front() {
        transport.get_mut().send(link, frame);
    }
    let mut index = 0;
    while index < conn.requests.len() {
        let before = conn.requests.len();
        if conn.requests.get(index).is_some_and(|r| !r.sent) {
            send_request(transport, link, conn, index, ready, name);
        }
        if conn.requests.len() == before {
            index += 1;
        }
    }
}

/// The link opened: first-message auth first; with `with_auth_ack`, everything else waits for
/// the acknowledgement.
#[allow(clippy::too_many_arguments)]
fn on_open(
    transport: &mut WsTransportRes,
    link: WsLinkId,
    conn: &mut Conn,
    credentials: Option<&BackendCredentials>,
    ready: &mut Vec<Answer>,
    name: &WsName,
    now: Duration,
) {
    let auth = if conn.settings.credentials { credentials.and_then(BackendCredentials::ws_auth_message) } else { None };
    conn.authed = true;
    conn.auth_deadline = None;
    if let Some(auth) = auth {
        transport.get_mut().send(link, WsFrame::Text(auth));
        if let Some(ack) = conn.settings.auth_ack {
            conn.authed = false;
            conn.auth_deadline = Some(now.saturating_add(ack));
            // "Stable" counts from the acknowledgement, not from the socket opening.
            conn.connected_at = None;
            return;
        }
    }
    flush_waiting(transport, link, conn, ready, name);
}

/// Answer every waiting request of `conn` (disconnected, the given reason) and drop its frames.
fn fail_all(conn: &mut Conn, name: &WsName, reason: &str, ready: &mut Vec<Answer>) {
    for request in conn.requests.drain(..) {
        ready.push((request.id, name.clone(), request.route, Err(BackendError::disconnected(reason, Some(request.ever_sent)))));
    }
    if !conn.outbox.is_empty() {
        tracing::warn!(">>> NET-BACKEND: ws `{name}`: {} queued frame(s) dropped ({reason})", conn.outbox.len());
        conn.outbox.clear();
    }
}

/// A link is gone (closed, failed, or its transport replaced): answer or keep the requests, then
/// reconnect or give up. `error` is the connection-level error (for `WsStateChanged`).
#[allow(clippy::too_many_arguments)]
fn link_lost(
    changes: &mut Vec<WsStateChanged>,
    ready: &mut Vec<Answer>,
    name: &WsName,
    conn: &mut Conn,
    error: BackendError,
    retry_allowed: bool,
    now: Duration,
    random: u64,
) {
    conn.link = None;
    conn.connected_at = None;
    conn.authed = false;
    conn.auth_deadline = None;
    let reason = match &error {
        BackendError::Disconnected { reason, .. } => reason.clone(),
        other => other.to_string(),
    };
    // Requests sent on this link: answered (they may have reached the server), unless marked
    // resend (bounded). Requests not sent yet keep waiting.
    let mut kept = 0;
    let mut remaining = Vec::with_capacity(conn.requests.len());
    for mut request in conn.requests.drain(..) {
        if !request.sent {
            remaining.push(request);
        } else if request.resend && kept < conn.settings.resend_limit {
            kept += 1;
            request.sent = false;
            remaining.push(request);
        } else {
            let why = if request.resend { "the resend queue is full".to_string() } else { format!("the connection was lost: {reason}") };
            ready.push((request.id, name.clone(), request.route, Err(BackendError::disconnected(why, Some(true)))));
        }
    }
    conn.requests = remaining;
    let next_attempt = conn.attempt.saturating_add(1);
    if retry_allowed && !conn.no_retry && conn.settings.reconnect.may_retry(next_attempt) {
        conn.attempt = next_attempt;
        let delay = conn.settings.reconnect.delay(next_attempt, random);
        conn.retry_at = Some(now.saturating_add(delay));
        tracing::info!(">>> NET-BACKEND: ws `{name}`: {reason}; reconnect attempt {next_attempt} in {delay:?}");
        set_state(changes, name, conn, WsState::Reconnecting { attempt: next_attempt, retry_in: delay }, Some(error));
    } else {
        conn.retry_at = None;
        tracing::warn!(">>> NET-BACKEND: ws `{name}`: {reason}; not reconnecting");
        fail_all(conn, name, &reason, ready);
        set_state(changes, name, conn, WsState::Disconnected, Some(error));
    }
}

fn refresh_info(connections: &mut WsConnections, runtime: &WsRuntime) {
    for (name, conn) in &runtime.conns {
        let info = WsConnectionInfo {
            state: conn.state,
            attempt: conn.attempt,
            last_error: conn.last_error.clone(),
            pending_requests: conn.requests.len(),
            queued_frames: conn.outbox.len(),
        };
        if connections.map.get(name) != Some(&info) {
            connections.map.insert(name.clone(), info);
        }
    }
}

fn deliver(
    mut answers: Vec<Answer>,
    raw: &mut MessageWriter<WsRawResponse>,
    #[cfg_attr(not(feature = "json"), allow(unused_variables))] commands: &mut Commands,
) {
    answers.sort_by_key(|(id, ..)| *id);
    for (id, name, route, result) in answers {
        if let Err(error) = &result {
            tracing::debug!(">>> NET-BACKEND: ws `{name}` {id} -> {error}");
        }
        match route {
            WsRoute::Raw => {
                raw.write(WsRawResponse { id, name, result });
            }
            #[cfg(feature = "json")]
            WsRoute::Typed(route) => route.deliver(id, name, result, commands),
        }
    }
}

/// `PostUpdate` (`BackendSystems::Send`, after the HTTP send): apply connects, disconnects,
/// frames, requests and cancels; start due (re)connect attempts. In a frame with `AppExit` nothing
/// is sent (the exit system answers the queue).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ws_send(
    client: Res<WsClient>,
    mut runtime: ResMut<WsRuntime>,
    mut connections: ResMut<WsConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<WsTransportRes>>,
    credentials: Option<Res<BackendCredentials>>,
    time: Option<Res<Time<Real>>>,
    mut exit: MessageReader<AppExit>,
) {
    if exit.read().count() > 0 {
        return;
    }
    let now = runtime.now(time.as_deref());
    let runtime = &mut *runtime;
    // The WebSocket ids of the shared cancel list (a request queued this frame or waiting), taken
    // before the queue is applied, so a request cancelled in the frame it was made in is never
    // sent (as for HTTP).
    let queued = client.drain();
    let mut cancels: Vec<RequestId> = inflight.claim_cancels(|id| owns(runtime, &queued, id));
    for item in queued {
        match item {
            WsQueued::Request { name, id, route, .. } if cancels.contains(&id) => {
                cancels.retain(|c| *c != id);
                runtime.ready.push((id, name, route, Err(BackendError::Cancelled)));
            }
            WsQueued::Connect(name, settings) => {
                if let Some(mut old) = runtime.conns.remove(&name) {
                    if let (Some(link), Some(transport)) = (old.link.take(), transport.as_mut()) {
                        runtime.links.remove(&link);
                        transport.get_mut().close(link, 1000);
                    }
                    fail_all(&mut old, &name, "replaced by a new connect", &mut runtime.ready);
                }
                // Connecting from the start, so frames and requests made in the same frame wait
                // for the link instead of being refused.
                runtime.changes.push(WsStateChanged { name: name.clone(), state: WsState::Connecting, error: None });
                let conn = Conn {
                    settings: *settings,
                    state: WsState::Connecting,
                    link: None,
                    generation: 0,
                    attempt: 0,
                    retry_at: Some(now),
                    connected_at: None,
                    last_error: None,
                    outbox: VecDeque::new(),
                    requests: Vec::new(),
                    no_retry: false,
                    authed: false,
                    auth_deadline: None,
                };
                runtime.conns.insert(name, conn);
            }
            WsQueued::Disconnect(name) => {
                if let Some(conn) = runtime.conns.get_mut(&name) {
                    if let Some(link) = conn.link.take() {
                        runtime.links.remove(&link);
                        if let Some(transport) = transport.as_mut() {
                            transport.get_mut().close(link, 1000);
                        }
                    }
                    conn.retry_at = None;
                    conn.connected_at = None;
                    fail_all(conn, &name, "disconnected by the game", &mut runtime.ready);
                    set_state(&mut runtime.changes, &name, conn, WsState::Disconnected, None);
                }
            }
            WsQueued::Send(name, frame) => match runtime.conns.get_mut(&name) {
                Some(conn) if frame.len() > conn.settings.max_message_bytes => {
                    tracing::warn!(">>> NET-BACKEND: ws `{name}`: a {}-byte frame is over the message limit; dropped", frame.len());
                }
                Some(conn) if conn.can_send() => {
                    if let (Some(link), Some(transport)) = (conn.link, transport.as_mut()) {
                        transport.get_mut().send(link, frame);
                    }
                }
                Some(conn) if conn.state != WsState::Disconnected && conn.outbox.len() < conn.settings.outbox_limit => conn.outbox.push_back(frame),
                Some(_) => tracing::warn!(">>> NET-BACKEND: ws `{name}`: a frame was dropped (not connected, or the outbox is full)"),
                None => tracing::warn!(">>> NET-BACKEND: ws: no connection named `{name}`; a frame was dropped"),
            },
            WsQueued::Request { name, id, out, route } => {
                let Some(conn) = runtime.conns.get_mut(&name) else {
                    runtime.ready.push((id, name.clone(), route, Err(BackendError::InvalidRequest(format!("no WebSocket connection named `{name}`")))));
                    continue;
                };
                if conn.settings.protocol.is_none() {
                    runtime.ready.push((id, name, route, Err(BackendError::InvalidRequest("this connection has no request protocol".into()))));
                    continue;
                }
                if conn.state == WsState::Disconnected {
                    runtime.ready.push((id, name, route, Err(BackendError::disconnected("not connected", Some(false)))));
                    continue;
                }
                if !conn.can_send() && conn.waiting() >= conn.settings.waiting_limit {
                    runtime.ready.push((id, name, route, Err(BackendError::disconnected("too many requests are waiting for the connection", Some(false)))));
                    continue;
                }
                let timeout = out.timeout.unwrap_or(conn.settings.request_timeout);
                conn.requests.push(Request {
                    id,
                    kind: out.kind,
                    payload: out.payload,
                    route,
                    resend: out.resend,
                    deadline: now.saturating_add(timeout),
                    timeout,
                    sent: false,
                    ever_sent: false,
                });
                if conn.can_send() {
                    if let (Some(link), Some(transport)) = (conn.link, transport.as_mut()) {
                        let index = conn.requests.len().saturating_sub(1);
                        send_request(transport, link, conn, index, &mut runtime.ready, &name);
                    }
                }
            }
            WsQueued::Fail { name, id, route, error } => runtime.ready.push((id, name, route, Err(error))),
        }
    }
    for id in cancels {
        for (name, conn) in &mut runtime.conns {
            if let Some(index) = conn.requests.iter().position(|r| r.id == id) {
                let request = conn.requests.remove(index);
                runtime.ready.push((id, name.clone(), request.route, Err(BackendError::Cancelled)));
                break;
            }
        }
    }
    // Due (re)connect attempts, with the credentials as they are now.
    let due: Vec<WsName> = runtime.conns.iter().filter(|(_, c)| c.link.is_none() && c.retry_at.is_some_and(|at| at <= now)).map(|(n, _)| n.clone()).collect();
    for name in due {
        let Some(conn) = runtime.conns.get_mut(&name) else { continue };
        conn.retry_at = None;
        match (prepare_handshake(&conn.settings, credentials.as_deref()), transport.as_mut()) {
            (Err(error), _) => {
                tracing::warn!(">>> NET-BACKEND: ws `{name}`: {error}");
                fail_all(conn, &name, &error.to_string(), &mut runtime.ready);
                set_state(&mut runtime.changes, &name, conn, WsState::Disconnected, Some(error));
            }
            (Ok(_), None) => {
                fail_all(conn, &name, "no WebSocket transport is installed", &mut runtime.ready);
                set_state(&mut runtime.changes, &name, conn, WsState::Disconnected, Some(BackendError::NoTransport));
            }
            (Ok(handshake), Some(transport)) => {
                let link = WsLinkId::next();
                tracing::debug!(">>> NET-BACKEND: ws `{name}`: {link} connecting");
                conn.link = Some(link);
                conn.generation = transport.generation();
                runtime.links.insert(link, name.clone());
                set_state(&mut runtime.changes, &name, conn, WsState::Connecting, None);
                transport.get_mut().open(link, handshake);
            }
        }
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
}

/// `First` (`BackendSystems::Receive`, after the HTTP receive): link events, answers, pushes,
/// deadlines, state changes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ws_receive(
    client: Res<WsClient>,
    mut runtime: ResMut<WsRuntime>,
    mut connections: ResMut<WsConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<WsTransportRes>>,
    credentials: Option<Res<BackendCredentials>>,
    time: Option<Res<Time<Real>>>,
    mut states: MessageWriter<WsStateChanged>,
    mut frames: MessageWriter<WsMessage>,
    mut raw: MessageWriter<WsRawResponse>,
    mut commands: Commands,
) {
    let now = runtime.now(time.as_deref());
    let generation = transport.as_ref().map(|t| t.generation());
    let events = transport.as_mut().map(|t| t.get_mut().poll()).unwrap_or_default();
    let runtime = &mut *runtime;
    for (link, event) in events {
        let Some(name) = runtime.links.get(&link).cloned() else {
            continue; // a link the plugin already let go of
        };
        let Some(conn) = runtime.conns.get_mut(&name) else { continue };
        if conn.link != Some(link) {
            continue;
        }
        match event {
            WsLinkEvent::Opened => {
                conn.connected_at = Some(now);
                conn.last_error = None;
                tracing::info!(">>> NET-BACKEND: ws `{name}` connected");
                set_state(&mut runtime.changes, &name, conn, WsState::Connected, None);
                if let Some(transport) = transport.as_mut() {
                    on_open(transport, link, conn, credentials.as_deref(), &mut runtime.ready, &name, now);
                }
            }
            WsLinkEvent::Frame(frame) => {
                if let Some(protocol) = conn.settings.protocol.clone() {
                    match protocol.decode(&frame) {
                        WsIncoming::Response { wire_id, result } => {
                            if let Some(index) = conn.requests.iter().position(|r| r.sent && wire_id == r.id.wire()) {
                                let request = conn.requests.remove(index);
                                let result = result.map_err(|payload| BackendError::Rejected(Box::new(Rejection::new(payload))));
                                runtime.ready.push((request.id, name.clone(), request.route, result));
                            }
                        }
                        WsIncoming::Push { kind, data } => {
                            #[cfg(feature = "json")]
                            if let Some(routes) = client.push_routes.get(kind.as_str()) {
                                for route in routes {
                                    route.deliver(name.clone(), &data, &mut commands);
                                }
                            }
                            #[cfg(not(feature = "json"))]
                            let _ = (kind, data, &client);
                        }
                        WsIncoming::AuthOk => {
                            if !conn.authed {
                                conn.authed = true;
                                conn.auth_deadline = None;
                                conn.connected_at = Some(now);
                                if let Some(transport) = transport.as_mut() {
                                    flush_waiting(transport, link, conn, &mut runtime.ready, &name);
                                }
                            }
                        }
                        WsIncoming::AuthFailed(_) => {
                            conn.no_retry = true;
                            if let Some(transport) = transport.as_mut() {
                                transport.get_mut().close(link, 1008);
                            }
                            runtime.links.remove(&link);
                            let error = BackendError::disconnected("the server refused the authentication message", None);
                            let random = runtime.rng;
                            link_lost(&mut runtime.changes, &mut runtime.ready, &name, conn, error, false, now, random);
                        }
                        WsIncoming::Ignore => {}
                    }
                }
                frames.write(WsMessage { name: name.clone(), frame });
            }
            WsLinkEvent::Closed { code, reason } => {
                runtime.links.remove(&link);
                let retry = code.is_none_or(|code| conn.settings.protocol.as_ref().is_none_or(|p| p.retry_after_close(code)));
                let error = match code {
                    Some(code) => BackendError::Closed { code, reason },
                    None => BackendError::disconnected("the connection closed", None),
                };
                let random = runtime.rng;
                link_lost(&mut runtime.changes, &mut runtime.ready, &name, conn, error, retry, now, random);
                runtime.random();
            }
            WsLinkEvent::Failed(error) => {
                runtime.links.remove(&link);
                // Permanent: 401 / 403, invalid settings, and TLS (certificate) errors unless the
                // game opted in with `WsReconnect::with_tls_retry(true)`.
                let refused = matches!(error.status().map(|s| s.as_u16()), Some(401 | 403))
                    || error.is_invalid_request()
                    || (matches!(error, BackendError::Tls(_)) && !conn.settings.reconnect.retry_tls);
                let random = runtime.rng;
                link_lost(&mut runtime.changes, &mut runtime.ready, &name, conn, error, !refused, now, random);
                runtime.random();
            }
        }
    }
    // Links of a transport that is gone or replaced.
    let stale: Vec<WsName> = runtime.conns.iter().filter(|(_, c)| c.link.is_some() && Some(c.generation) != generation).map(|(n, _)| n.clone()).collect();
    for name in stale {
        let random = runtime.random();
        if let Some(conn) = runtime.conns.get_mut(&name) {
            if let Some(link) = conn.link {
                runtime.links.remove(&link);
            }
            link_lost(&mut runtime.changes, &mut runtime.ready, &name, conn, BackendError::NoTransport, true, now, random);
        }
    }
    // An auth acknowledgement that did not come.
    let unacked: Vec<WsName> = runtime.conns.iter().filter(|(_, c)| c.auth_deadline.is_some_and(|at| at <= now)).map(|(n, _)| n.clone()).collect();
    for name in unacked {
        let random = runtime.random();
        let Some(conn) = runtime.conns.get_mut(&name) else { continue };
        let limit = conn.settings.auth_ack.unwrap_or_default();
        let mut index = 0;
        while index < conn.requests.len() {
            if conn.requests.get(index).is_some_and(|r| !r.sent) {
                let request = conn.requests.remove(index);
                // Honest about an earlier link: a resend request may already be on the server.
                let why = if request.ever_sent {
                    format!("sent before the connection was lost; the server did not acknowledge the authentication message on the reconnect within {limit:?}")
                } else {
                    format!("not sent: the server did not acknowledge the authentication message within {limit:?}")
                };
                runtime.ready.push((request.id, name.clone(), request.route, Err(BackendError::Timeout(why))));
            } else {
                index += 1;
            }
        }
        if let Some(link) = conn.link {
            runtime.links.remove(&link);
            if let Some(transport) = transport.as_mut() {
                transport.get_mut().close(link, 1008);
            }
        }
        // Not retried: a server that does not acknowledge will not start to on a retry. The game
        // sees `Disconnected` with this error (not a silent `Connected`).
        let error = BackendError::disconnected(format!("no authentication acknowledgement within {limit:?} (closed with 1008)"), None);
        link_lost(&mut runtime.changes, &mut runtime.ready, &name, conn, error, false, now, random);
    }
    // Deadlines, and the attempt counter reset after a stable connection.
    for (name, conn) in &mut runtime.conns {
        if conn.connected_at.is_some_and(|at| now.saturating_sub(at) >= conn.settings.reconnect.stable_after) {
            conn.attempt = 0;
        }
        let mut index = 0;
        while index < conn.requests.len() {
            if conn.requests.get(index).is_some_and(|r| r.deadline <= now) {
                let request = conn.requests.remove(index);
                let why = if request.sent {
                    format!("no answer within {:?}", request.timeout)
                } else if request.ever_sent {
                    format!("sent before the connection was lost, no answer within {:?}", request.timeout)
                } else if conn.state == WsState::Connected && !conn.authed {
                    format!("not sent: the authentication was not acknowledged within {:?}", request.timeout)
                } else {
                    format!("not sent: the connection did not open within {:?}", request.timeout)
                };
                runtime.ready.push((request.id, name.clone(), request.route, Err(BackendError::Timeout(why))));
            } else {
                index += 1;
            }
        }
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
    for change in std::mem::take(&mut runtime.changes) {
        states.write(change);
    }
    deliver(std::mem::take(&mut runtime.ready), &mut raw, &mut commands);
}

/// `Last` on `AppExit` (`BackendSystems::Exit`, after the HTTP exit): close every link (1001),
/// answer every request `Shutdown` (or `Cancelled` when cancelled in that frame; requests of the
/// exit frame were never sent), and shut the transport down.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ws_exit(
    client: Res<WsClient>,
    mut runtime: ResMut<WsRuntime>,
    mut connections: ResMut<WsConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<WsTransportRes>>,
    mut states: MessageWriter<WsStateChanged>,
    mut raw: MessageWriter<WsRawResponse>,
    mut commands: Commands,
) {
    let runtime = &mut *runtime;
    let queued = client.drain();
    let cancelled: std::collections::HashSet<RequestId> = inflight.claim_cancels(|id| owns(runtime, &queued, id)).into_iter().collect();
    for item in queued {
        match item {
            WsQueued::Request { name, id, route, .. } | WsQueued::Fail { name, id, route, .. } => {
                let error = if cancelled.contains(&id) { BackendError::Cancelled } else { BackendError::Shutdown };
                runtime.ready.push((id, name, route, Err(error)));
            }
            _ => {}
        }
    }
    let mut open = 0;
    for (name, conn) in &mut runtime.conns {
        if let Some(link) = conn.link.take() {
            if let Some(transport) = transport.as_mut() {
                transport.get_mut().close(link, 1001);
            }
        }
        for request in conn.requests.drain(..) {
            open += 1;
            let error = if cancelled.contains(&request.id) { BackendError::Cancelled } else { BackendError::Shutdown };
            runtime.ready.push((request.id, name.clone(), request.route, Err(error)));
        }
        conn.outbox.clear();
        conn.retry_at = None;
        conn.auth_deadline = None;
        set_state(&mut runtime.changes, name, conn, WsState::Disconnected, None);
    }
    runtime.links.clear();
    if let Some(transport) = transport.as_mut() {
        transport.get_mut().shutdown();
    }
    if open > 0 {
        tracing::info!(">>> NET-BACKEND: app exit: {open} open WebSocket request(s) answered with Shutdown");
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
    for change in std::mem::take(&mut runtime.changes) {
        states.write(change);
    }
    deliver(std::mem::take(&mut runtime.ready), &mut raw, &mut commands);
}
