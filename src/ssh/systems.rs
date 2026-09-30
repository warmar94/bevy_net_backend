//! The ECS side of `ssh`: one entry per named connection, the requests (commands and SFTP
//! operations), optional reconnect with backoff, and the three systems. Like HTTP and WebSocket,
//! the ECS side is the only thing that answers requests (exactly one answer each), a request
//! answered in the exit frame is never sent, a command is never re-run, and every answer says
//! honestly whether the command started.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

use bevy_app::AppExit;
use bevy_ecs::message::{MessageReader, MessageWriter};
use bevy_ecs::resource::Resource;
use bevy_ecs::system::{Res, ResMut};
use bevy_time::{Real, Time};

use super::transport::{SshConnId, SshEvent, SshTransportRes};
#[cfg(feature = "sftp")]
use super::{SftpFinished, SftpOp, SftpOutcome, SftpProgress};
use super::{
    SshClient, SshCommand, SshConnectionInfo, SshConnections, SshExit, SshFinished, SshName, SshOutput, SshQueued, SshSettings, SshState, SshStateChanged,
    SshTarget,
};
use crate::inflight::{InFlight, Protocol, RequestInfo, RequestKind, DEADLINE_GRACE};
use crate::request::RequestId;
use crate::response::BackendError;

/// How many names are remembered in total; beyond it the oldest `Disconnected` names are
/// forgotten (their `SshConnections` entry goes away).
const MAX_REMEMBERED_NAMES: usize = 256;

/// The error every SSH request gets in a release build without `allow_in_release`.
pub(crate) fn release_refusal() -> BackendError {
    BackendError::InvalidRequest(
        "SSH is disabled in release builds; an admin tool opts in with BackendPlugin::with_ssh(SshSettings::default().allow_in_release(true))".into(),
    )
}

enum Work {
    Command(Option<SshCommand>),
    #[cfg(feature = "sftp")]
    Sftp(Option<SftpOp>),
}

impl Work {
    fn kind(&self) -> RequestKind {
        match self {
            Work::Command(_) => RequestKind::Ssh,
            #[cfg(feature = "sftp")]
            Work::Sftp(_) => RequestKind::Sftp,
        }
    }
}

/// A command or SFTP operation waiting for its answer.
struct Op {
    name: SshName,
    work: Work,
    /// Handed to the transport.
    handed: bool,
    /// The transport reported that it started.
    started: bool,
    deadline: Duration,
    /// How long it may take once handed over.
    timeout: Duration,
    /// Output limit (commands) and output seen so far (checked here too, for any transport).
    max_output: u64,
    output: u64,
}

/// One named connection.
struct Conn {
    target: SshTarget,
    state: SshState,
    conn: Option<SshConnId>,
    generation: u64,
    fingerprint: Option<String>,
    last_error: Option<BackendError>,
    connect_deadline: Option<Duration>,
    /// Reconnect bookkeeping (only with `SshTarget::with_reconnect`).
    attempt: u32,
    retry_at: Option<Duration>,
    connected_at: Option<Duration>,
    /// When this name was last used (for forgetting old disconnected names).
    touched: Duration,
}

impl Conn {
    fn new(target: SshTarget, now: Duration) -> Self {
        Self {
            target,
            state: SshState::Disconnected,
            conn: None,
            generation: 0,
            fingerprint: None,
            last_error: None,
            connect_deadline: None,
            attempt: 0,
            retry_at: None,
            connected_at: None,
            touched: now,
        }
    }

    /// Counts against `max_connections`.
    fn active(&self) -> bool {
        self.state != SshState::Disconnected
    }
}

enum Answer {
    Command {
        id: RequestId,
        name: SshName,
        started: Option<bool>,
        result: Result<SshExit, BackendError>,
    },
    #[cfg(feature = "sftp")]
    Sftp {
        id: RequestId,
        name: SshName,
        started: Option<bool>,
        result: Result<SftpOutcome, BackendError>,
    },
}

impl Answer {
    fn id(&self) -> RequestId {
        match self {
            Answer::Command { id, .. } => *id,
            #[cfg(feature = "sftp")]
            Answer::Sftp { id, .. } => *id,
        }
    }
}

/// Private bookkeeping.
#[derive(Resource)]
pub(crate) struct SshRuntime {
    settings: SshSettings,
    conns: HashMap<SshName, Conn>,
    by_conn: HashMap<SshConnId, SshName>,
    ops: HashMap<RequestId, Op>,
    ready: Vec<Answer>,
    outputs: Vec<SshOutput>,
    #[cfg(feature = "sftp")]
    progress: Vec<SftpProgress>,
    changes: Vec<SshStateChanged>,
    epoch: Instant,
    rng: u64,
}

impl SshRuntime {
    pub(crate) fn new(settings: SshSettings) -> Self {
        let seed = RandomState::new().hash_one(Instant::now());
        Self {
            settings,
            conns: HashMap::new(),
            by_conn: HashMap::new(),
            ops: HashMap::new(),
            ready: Vec::new(),
            outputs: Vec::new(),
            #[cfg(feature = "sftp")]
            progress: Vec::new(),
            changes: Vec::new(),
            epoch: Instant::now(),
            rng: seed | 1,
        }
    }

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

    /// The SSH rows of `InFlight`: every request not answered yet.
    fn publish(&self, inflight: &mut InFlight) {
        inflight
            .set_rows(Protocol::Ssh, self.ops.iter().map(|(id, op)| (*id, RequestInfo { kind: op.work.kind(), method: None, target: op.name.to_string() })));
    }

    /// Answer `id` with `error` (routed by the kind of work it was).
    fn answer(&mut self, id: RequestId, op: Op, error: BackendError) {
        let started = started_on_error(&op, &error);
        self.push(id, op, started, error);
    }

    fn push(&mut self, id: RequestId, op: Op, started: Option<bool>, error: BackendError) {
        match op.work {
            Work::Command(_) => self.ready.push(Answer::Command { id, name: op.name, started, result: Err(error) }),
            #[cfg(feature = "sftp")]
            Work::Sftp(_) => self.ready.push(Answer::Sftp { id, name: op.name, started, result: Err(error) }),
        }
    }

    /// Answer the requests of `name` that `pick` selects with the error `make(op)` gives.
    fn fail_where(&mut self, name: &SshName, pick: impl Fn(&Op) -> bool, make: impl Fn(&Op) -> BackendError) {
        let ids: Vec<RequestId> = self.ops.iter().filter(|(_, op)| &op.name == name && pick(op)).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some(op) = self.ops.remove(&id) {
                let error = make(&op);
                self.answer(id, op, error);
            }
        }
    }

    fn set_state(&mut self, name: &SshName, state: SshState, error: Option<BackendError>) {
        if let Some(conn) = self.conns.get_mut(name) {
            if error.is_some() {
                conn.last_error = error.clone();
            }
            if conn.state != state || error.is_some() {
                conn.state = state;
                self.changes.push(SshStateChanged { name: name.clone(), state, error });
            }
        }
    }

    /// Close a connection (transport side), stop reconnecting, and answer its requests.
    fn close(&mut self, name: &SshName, transport: Option<&mut SshTransportRes>, reason: &str) {
        if let Some(conn) = self.conns.get_mut(name) {
            conn.connect_deadline = None;
            conn.fingerprint = None;
            conn.retry_at = None;
            conn.connected_at = None;
            if let Some(id) = conn.conn.take() {
                self.by_conn.remove(&id);
                if let Some(transport) = transport {
                    transport.get_mut().close(id);
                }
            }
        }
        self.fail_where(name, |_| true, |op| lost(reason, op));
    }

    /// Start a connection attempt for `name` (the first one, or a reconnect).
    fn start_attempt(&mut self, name: &SshName, transport: &mut SshTransportRes, now: Duration) {
        let Some(conn) = self.conns.get_mut(name) else { return };
        let id = SshConnId::next();
        conn.conn = Some(id);
        conn.retry_at = None;
        conn.generation = transport.generation();
        conn.connect_deadline = Some(now.saturating_add(conn.target.connect_timeout).saturating_add(DEADLINE_GRACE));
        let target = conn.target.clone();
        self.by_conn.insert(id, name.clone());
        tracing::debug!(">>> NET-BACKEND: ssh `{name}`: {id} connecting");
        self.set_state(name, SshState::Connecting, None);
        transport.get_mut().connect(id, target);
    }

    /// Hand every waiting request of `name` to the transport (the connection is up).
    fn hand_over(&mut self, name: &SshName, transport: &mut SshTransportRes, now: Duration) {
        let Some(conn_id) = self.conns.get(name).and_then(|c| c.conn) else { return };
        let mut ids: Vec<RequestId> = self.ops.iter().filter(|(_, op)| &op.name == name && !op.handed).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        for id in ids {
            let Some(op) = self.ops.get_mut(&id) else { continue };
            op.handed = true;
            op.deadline = now.saturating_add(op.timeout).saturating_add(DEADLINE_GRACE);
            match &mut op.work {
                Work::Command(command) => {
                    if let Some(command) = command.take() {
                        transport.get_mut().run(conn_id, id, command);
                    }
                }
                #[cfg(feature = "sftp")]
                Work::Sftp(sftp) => {
                    if let Some(sftp) = sftp.take() {
                        transport.get_mut().sftp(conn_id, id, sftp);
                    }
                }
            }
        }
    }

    /// A connection ended (lost, failed, or its transport went away): answer what ran on it, then
    /// reconnect (if set and allowed) or give up.
    fn connection_ended(&mut self, name: &SshName, error: Option<BackendError>, now: Duration) {
        let Some(conn) = self.conns.get_mut(name) else { return };
        let was_connected = conn.state == SshState::Connected;
        conn.conn = None;
        conn.connect_deadline = None;
        conn.fingerprint = None;
        conn.connected_at = None;
        let reason = match &error {
            Some(BackendError::Disconnected { reason, .. }) => reason.clone(),
            Some(other) => other.to_string(),
            None => "the connection was closed".to_string(),
        };
        let permanent = error.as_ref().is_none_or(is_permanent);
        let next = conn.attempt.saturating_add(1);
        let retry = match &conn.target.reconnect {
            Some(policy) if !permanent && policy.may_retry(next) => Some(policy.clone()),
            _ => None,
        };
        if was_connected {
            tracing::warn!(">>> NET-BACKEND: ssh `{name}` lost: {reason}");
        } else {
            tracing::warn!(">>> NET-BACKEND: ssh `{name}` could not connect: {reason}");
        }
        // What was handed over ran (or may have run) on the old connection: answered now, never
        // re-run. What was never sent keeps waiting when a reconnect follows.
        self.fail_where(name, |op| op.handed, |op| lost(&reason, op));
        match retry {
            Some(policy) => {
                let random = self.random();
                let delay = policy.delay(next, random);
                if let Some(conn) = self.conns.get_mut(name) {
                    conn.attempt = next;
                    conn.retry_at = Some(now.saturating_add(delay));
                }
                tracing::info!(">>> NET-BACKEND: ssh `{name}`: reconnect attempt {next} in {delay:?}");
                self.set_state(name, SshState::Reconnecting { attempt: next, retry_in: delay }, error);
            }
            None => {
                if let Some(conn) = self.conns.get_mut(name) {
                    conn.retry_at = None;
                }
                self.fail_where(name, |_| true, |op| lost(&reason, op));
                self.set_state(name, SshState::Disconnected, error);
            }
        }
    }

    /// Forget the oldest `Disconnected` names once too many are remembered.
    fn forget_old_names(&mut self) {
        while self.conns.len() > MAX_REMEMBERED_NAMES {
            let oldest = self.conns.iter().filter(|(_, c)| !c.active()).min_by_key(|(_, c)| c.touched).map(|(n, _)| n.clone());
            let Some(name) = oldest else { break };
            self.conns.remove(&name);
        }
    }

    /// Apply what the transport reported.
    fn apply(&mut self, events: Vec<SshEvent>, mut transport: Option<&mut SshTransportRes>, now: Duration) {
        for event in events {
            match event {
                SshEvent::Connected { conn, fingerprint } => {
                    let Some(name) = self.by_conn.get(&conn).cloned() else { continue };
                    if let Some(entry) = self.conns.get_mut(&name) {
                        entry.fingerprint = Some(fingerprint.clone());
                        entry.connect_deadline = None;
                        entry.last_error = None;
                        entry.connected_at = Some(now);
                    }
                    tracing::info!(">>> NET-BACKEND: ssh `{name}` connected (host key {fingerprint})");
                    self.set_state(&name, SshState::Connected, None);
                    if let Some(transport) = transport.as_deref_mut() {
                        self.hand_over(&name, transport, now);
                    }
                }
                SshEvent::Closed { conn, error } => {
                    let Some(name) = self.by_conn.remove(&conn) else { continue };
                    self.connection_ended(&name, error, now);
                }
                SshEvent::Started { id } => {
                    if let Some(op) = self.ops.get_mut(&id) {
                        op.started = true;
                    }
                }
                SshEvent::Output { id, stream, data } => {
                    let Some(op) = self.ops.get_mut(&id) else { continue };
                    if !matches!(op.work, Work::Command(_)) {
                        continue;
                    }
                    op.output = op.output.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                    if op.output > op.max_output {
                        // A transport that does not enforce the limit itself: stop it here.
                        let limit = op.max_output;
                        if let Some(op) = self.ops.remove(&id) {
                            if let Some(transport) = transport.as_deref_mut() {
                                transport.get_mut().cancel(id);
                            }
                            self.push(id, op, Some(true), BackendError::BodyTooLarge { limit });
                        }
                        continue;
                    }
                    self.outputs.push(SshOutput { id, name: op.name.clone(), stream, data });
                }
                SshEvent::Finished { id, result } => {
                    if !self.ops.get(&id).is_some_and(|op| matches!(op.work, Work::Command(_))) {
                        continue; // late, or not a command: already answered
                    }
                    let Some(op) = self.ops.remove(&id) else { continue };
                    match result {
                        Ok(exit) => self.ready.push(Answer::Command { id, name: op.name, started: Some(true), result: Ok(exit) }),
                        Err(error) => self.answer(id, op, error),
                    }
                }
                #[cfg(feature = "sftp")]
                SshEvent::Progress { id, done, total } => {
                    if let Some(op) = self.ops.get_mut(&id) {
                        op.started = true;
                        self.progress.push(SftpProgress { id, name: op.name.clone(), done, total });
                    }
                }
                #[cfg(feature = "sftp")]
                SshEvent::SftpFinished { id, result } => {
                    if !self.ops.get(&id).is_some_and(|op| matches!(op.work, Work::Sftp(_))) {
                        continue;
                    }
                    let Some(op) = self.ops.remove(&id) else { continue };
                    match result {
                        Ok(outcome) => self.ready.push(Answer::Sftp { id, name: op.name, started: Some(true), result: Ok(outcome) }),
                        Err(error) => self.answer(id, op, error),
                    }
                }
            }
        }
    }
}

/// Errors a reconnect cannot fix.
fn is_permanent(error: &BackendError) -> bool {
    matches!(error, BackendError::HostKey { .. } | BackendError::AuthFailed(_) | BackendError::Ssh(_)) || error.is_invalid_request()
}

/// `started` for an answer decided by the plugin (cancel, timeout, loss, exit).
fn started_by_plugin(op: &Op) -> Option<bool> {
    if op.started {
        Some(true)
    } else if op.handed {
        None
    } else {
        Some(false)
    }
}

/// `started` for an error: what the plugin knows, refined by what the error says.
fn started_on_error(op: &Op, error: &BackendError) -> Option<bool> {
    if op.started {
        return Some(true);
    }
    if !op.handed || error.was_sent() == Some(false) {
        return Some(false);
    }
    match error {
        // Handed over, never confirmed: it may have started.
        BackendError::Timeout(_) | BackendError::Disconnected { .. } | BackendError::Cancelled | BackendError::Shutdown | BackendError::NoTransport => None,
        // The transport reports it could not run it (no channel, refused, …).
        _ => Some(false),
    }
}

/// The error of a request whose connection went away.
fn lost(reason: &str, op: &Op) -> BackendError {
    let sent = if op.started {
        Some(true)
    } else if op.handed {
        None
    } else {
        Some(false)
    };
    BackendError::disconnected(reason, sent)
}

fn refresh_info(connections: &mut SshConnections, runtime: &SshRuntime) {
    let mut pending: HashMap<&SshName, usize> = HashMap::new();
    for op in runtime.ops.values() {
        *pending.entry(&op.name).or_default() += 1;
    }
    connections.map.retain(|name, _| runtime.conns.contains_key(name));
    for (name, conn) in &runtime.conns {
        let info = SshConnectionInfo {
            state: conn.state,
            fingerprint: conn.fingerprint.clone(),
            last_error: conn.last_error.clone(),
            pending_requests: pending.get(name).copied().unwrap_or(0),
            attempt: conn.attempt,
        };
        if connections.map.get(name) != Some(&info) {
            connections.map.insert(name.clone(), info);
        }
    }
}

/// The messages this side writes.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct Writers<'w> {
    states: MessageWriter<'w, SshStateChanged>,
    outputs: MessageWriter<'w, SshOutput>,
    finished: MessageWriter<'w, SshFinished>,
    #[cfg(feature = "sftp")]
    progress: MessageWriter<'w, SftpProgress>,
    #[cfg(feature = "sftp")]
    sftp: MessageWriter<'w, SftpFinished>,
}

fn flush(runtime: &mut SshRuntime, writers: &mut Writers) {
    for change in std::mem::take(&mut runtime.changes) {
        writers.states.write(change);
    }
    for output in std::mem::take(&mut runtime.outputs) {
        writers.outputs.write(output);
    }
    #[cfg(feature = "sftp")]
    for progress in std::mem::take(&mut runtime.progress) {
        writers.progress.write(progress);
    }
    let mut answers = std::mem::take(&mut runtime.ready);
    answers.sort_by_key(Answer::id);
    for answer in answers {
        match answer {
            Answer::Command { id, name, started, result } => {
                match &result {
                    Ok(exit) => tracing::debug!(">>> NET-BACKEND: ssh `{name}` {id} -> exit {:?} signal {:?}", exit.status, exit.signal),
                    Err(error) => tracing::debug!(">>> NET-BACKEND: ssh `{name}` {id} -> {error}"),
                }
                writers.finished.write(SshFinished { id, name, started, result });
            }
            #[cfg(feature = "sftp")]
            Answer::Sftp { id, name, started, result } => {
                if let Err(error) = &result {
                    tracing::debug!(">>> NET-BACKEND: sftp `{name}` {id} -> {error}");
                }
                writers.sftp.write(SftpFinished { id, name, started, result });
            }
        }
    }
}

/// Whether `id` is an SSH request: queued this frame or waiting / running.
fn owns(runtime: &SshRuntime, queued: &[SshQueued], id: RequestId) -> bool {
    runtime.ops.contains_key(&id) || queued.iter().any(|item| item.request_id() == Some(id))
}

/// A new request waiting for its connection: how long until it gives up unsent.
fn waiting_limit(target: &SshTarget, timeout: Duration) -> Duration {
    target.connect_timeout.max(timeout).saturating_add(DEADLINE_GRACE)
}

/// `PostUpdate` (`BackendSystems::Send`, after the HTTP and WebSocket send): apply connects,
/// disconnects, commands, SFTP operations and cancels; start due reconnect attempts. In a frame
/// with `AppExit` nothing is sent (the exit system answers the queue).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ssh_send(
    client: Res<SshClient>,
    mut runtime: ResMut<SshRuntime>,
    mut connections: ResMut<SshConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<SshTransportRes>>,
    time: Option<Res<Time<Real>>>,
    mut exit: MessageReader<AppExit>,
) {
    if exit.read().count() > 0 {
        return;
    }
    let now = runtime.now(time.as_deref());
    let runtime = &mut *runtime;
    let queued = client.drain();
    // SSH's ids of the shared cancel list, taken before the queue is applied: a request cancelled
    // in the frame it was made in is never sent.
    let cancels: HashSet<RequestId> = inflight.claim_cancels(|id| owns(runtime, &queued, id)).into_iter().collect();
    let allowed = runtime.settings.is_allowed();
    for item in queued {
        match item {
            SshQueued::Connect(name, target) => {
                if runtime.conns.contains_key(&name) {
                    runtime.close(&name, transport.as_deref_mut(), "replaced by a new connect");
                }
                let active = runtime.conns.iter().filter(|(n, c)| **n != name && c.active()).count();
                let error = if !allowed {
                    Some(release_refusal())
                } else if active >= runtime.settings.max_connections {
                    Some(BackendError::InvalidRequest(format!("too many open SSH connections (limit {})", runtime.settings.max_connections)))
                } else if let Err(error) = target.validate() {
                    Some(error)
                } else if transport.is_none() {
                    Some(BackendError::NoTransport)
                } else {
                    None
                };
                // Every name gets an entry, refused ones too (`Disconnected` with the error).
                let mut conn = Conn::new(*target, now);
                conn.last_error = error.clone();
                runtime.conns.insert(name.clone(), conn);
                if let Some(error) = error {
                    tracing::warn!(">>> NET-BACKEND: ssh `{name}`: {error}");
                    runtime.changes.push(SshStateChanged { name: name.clone(), state: SshState::Disconnected, error: Some(error) });
                } else if let Some(transport) = transport.as_mut() {
                    runtime.start_attempt(&name, transport, now);
                }
                runtime.forget_old_names();
            }
            SshQueued::Disconnect(name) => {
                if runtime.conns.contains_key(&name) {
                    runtime.close(&name, transport.as_deref_mut(), "disconnected by the game");
                    runtime.set_state(&name, SshState::Disconnected, None);
                }
            }
            SshQueued::Run { name, id, command } => {
                let op = Op {
                    name: name.clone(),
                    work: Work::Command(None),
                    handed: false,
                    started: false,
                    deadline: now,
                    timeout: Duration::ZERO,
                    max_output: u64::MAX,
                    output: 0,
                };
                if cancels.contains(&id) {
                    runtime.answer(id, op, BackendError::Cancelled);
                    continue;
                }
                let checked = if allowed { command.validate() } else { Err(release_refusal()) };
                match checked.and_then(|()| admit(runtime, &name)) {
                    Err(error) => runtime.answer(id, op, error),
                    Ok(target) => {
                        let mut command = command;
                        let timeout = *command.timeout.get_or_insert(target.command_timeout);
                        let max_output = *command.max_output_bytes.get_or_insert(target.max_output_bytes);
                        let deadline = now.saturating_add(waiting_limit(&target, timeout));
                        runtime.ops.insert(id, Op { work: Work::Command(Some(command)), deadline, timeout, max_output, ..op });
                    }
                }
            }
            #[cfg(feature = "sftp")]
            SshQueued::Sftp { name, id, op: sftp } => {
                let op = Op {
                    name: name.clone(),
                    work: Work::Sftp(None),
                    handed: false,
                    started: false,
                    deadline: now,
                    timeout: Duration::ZERO,
                    max_output: u64::MAX,
                    output: 0,
                };
                if cancels.contains(&id) {
                    runtime.answer(id, op, BackendError::Cancelled);
                    continue;
                }
                let supported = transport.as_ref().is_none_or(|t| t.supports_sftp());
                let checked = if !allowed {
                    Err(release_refusal())
                } else if !supported {
                    Err(BackendError::InvalidRequest("the installed SSH transport does not support SFTP".into()))
                } else {
                    admit(runtime, &name).and_then(|target| sftp.validate(target.max_transfer_bytes).map(|()| target))
                };
                match checked {
                    Err(error) => runtime.answer(id, op, error),
                    Ok(target) => {
                        let deadline = now.saturating_add(waiting_limit(&target, target.sftp_timeout));
                        runtime.ops.insert(id, Op { work: Work::Sftp(Some(sftp)), deadline, timeout: target.sftp_timeout, ..op });
                    }
                }
            }
        }
    }
    for id in cancels {
        if let Some(op) = runtime.ops.remove(&id) {
            if op.handed {
                if let Some(transport) = transport.as_mut() {
                    transport.get_mut().cancel(id);
                }
            }
            let started = started_by_plugin(&op);
            runtime.push(id, op, started, BackendError::Cancelled);
        }
    }
    if let Some(transport) = transport.as_mut() {
        // Due reconnect attempts.
        let due: Vec<SshName> =
            runtime.conns.iter().filter(|(_, c)| c.conn.is_none() && c.retry_at.is_some_and(|at| at <= now)).map(|(n, _)| n.clone()).collect();
        for name in due {
            runtime.start_attempt(&name, transport, now);
        }
        // Requests for connections that are up go out now.
        let connected: Vec<SshName> = runtime.conns.iter().filter(|(_, c)| c.state == SshState::Connected).map(|(n, _)| n.clone()).collect();
        for name in connected {
            runtime.hand_over(&name, transport, now);
        }
    }
    for conn in runtime.conns.values_mut() {
        if conn.active() {
            conn.touched = now;
        }
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
}

/// Whether a new request may be queued on `name`; the connection's target when it may.
fn admit(runtime: &SshRuntime, name: &SshName) -> Result<SshTarget, BackendError> {
    let Some(conn) = runtime.conns.get(name) else {
        return Err(BackendError::InvalidRequest(format!("no SSH connection named `{name}`")));
    };
    if conn.state == SshState::Disconnected {
        return Err(BackendError::disconnected("not connected", Some(false)));
    }
    let open = runtime.ops.values().filter(|op| &op.name == name).count();
    if open >= runtime.settings.max_requests {
        return Err(BackendError::InvalidRequest(format!("too many open SSH requests on `{name}` (limit {})", runtime.settings.max_requests)));
    }
    Ok(conn.target.clone())
}

/// `First` (`BackendSystems::Receive`, after the HTTP and WebSocket receive): transport events,
/// output, answers, deadlines, reconnect bookkeeping, state changes.
pub(crate) fn ssh_receive(
    mut runtime: ResMut<SshRuntime>,
    mut connections: ResMut<SshConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<SshTransportRes>>,
    time: Option<Res<Time<Real>>>,
    mut writers: Writers,
) {
    let now = runtime.now(time.as_deref());
    let generation = transport.as_ref().map(|t| t.generation());
    let events = transport.as_mut().map(|t| t.get_mut().poll()).unwrap_or_default();
    let runtime = &mut *runtime;
    runtime.apply(events, transport.as_deref_mut(), now);
    // Connections of a transport that is gone or replaced.
    let stale: Vec<SshName> = runtime.conns.iter().filter(|(_, c)| c.conn.is_some() && Some(c.generation) != generation).map(|(n, _)| n.clone()).collect();
    for name in stale {
        if let Some(id) = runtime.conns.get(&name).and_then(|c| c.conn) {
            runtime.by_conn.remove(&id);
        }
        runtime.connection_ended(&name, Some(BackendError::NoTransport), now);
    }
    // Request deadlines: waiting for the connection, or the plugin's backstop for a transport
    // that never answers (the transport enforces the real timeout itself).
    let due: Vec<RequestId> = runtime.ops.iter().filter(|(_, op)| op.deadline <= now).map(|(id, _)| *id).collect();
    for id in due {
        let Some(op) = runtime.ops.remove(&id) else { continue };
        let error = if op.handed {
            if let Some(transport) = transport.as_mut() {
                transport.get_mut().cancel(id);
            }
            BackendError::Timeout(format!("no answer from the SSH transport within {:?}", op.timeout.saturating_add(DEADLINE_GRACE)))
        } else {
            let limit = runtime.conns.get(&op.name).map(|c| waiting_limit(&c.target, op.timeout).saturating_sub(DEADLINE_GRACE)).unwrap_or_default();
            BackendError::Timeout(format!("not sent: the connection did not open within {limit:?}"))
        };
        runtime.answer(id, op, error);
    }
    // A transport that never reports on a connect.
    let late: Vec<SshName> = runtime.conns.iter().filter(|(_, c)| c.connect_deadline.is_some_and(|at| at <= now)).map(|(n, _)| n.clone()).collect();
    for name in late {
        let limit = runtime.conns.get(&name).map(|c| c.target.connect_timeout.saturating_add(DEADLINE_GRACE)).unwrap_or_default();
        let error = BackendError::Timeout(format!("the SSH transport did not report on the connect within {limit:?}"));
        if let Some(id) = runtime.conns.get_mut(&name).and_then(|c| c.conn.take()) {
            runtime.by_conn.remove(&id);
            if let Some(transport) = transport.as_mut() {
                transport.get_mut().close(id);
            }
        }
        runtime.connection_ended(&name, Some(error), now);
    }
    // The attempt counter resets once a connection stayed up long enough.
    for conn in runtime.conns.values_mut() {
        let stable = conn.target.reconnect.as_ref().map_or(Duration::ZERO, |r| r.stable_after);
        if conn.connected_at.is_some_and(|at| now.saturating_sub(at) >= stable) {
            conn.attempt = 0;
        }
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
    flush(runtime, &mut writers);
}

/// `Last` on `AppExit` (`BackendSystems::Exit`, after the HTTP and WebSocket exit): deliver what
/// already arrived, answer everything else `Shutdown` (or `Cancelled` when cancelled in that
/// frame; requests of the exit frame are never sent), close every connection, and shut the
/// transport down.
pub(crate) fn ssh_exit(
    client: Res<SshClient>,
    mut runtime: ResMut<SshRuntime>,
    mut connections: ResMut<SshConnections>,
    mut inflight: ResMut<InFlight>,
    mut transport: Option<ResMut<SshTransportRes>>,
    time: Option<Res<Time<Real>>>,
    mut writers: Writers,
) {
    let now = runtime.now(time.as_deref());
    let runtime = &mut *runtime;
    let queued = client.drain();
    let cancelled: HashSet<RequestId> = inflight.claim_cancels(|id| owns(runtime, &queued, id)).into_iter().collect();
    // Results that already arrived are delivered as they are (as for HTTP); no reconnect now.
    for conn in runtime.conns.values_mut() {
        conn.target.reconnect = None;
    }
    let events = transport.as_mut().map(|t| t.get_mut().poll()).unwrap_or_default();
    runtime.apply(events, None, now);
    let error_for = |id: RequestId| if cancelled.contains(&id) { BackendError::Cancelled } else { BackendError::Shutdown };
    for item in queued {
        match item {
            SshQueued::Run { name, id, .. } => {
                runtime.ready.push(Answer::Command { id, name, started: Some(false), result: Err(error_for(id)) });
            }
            #[cfg(feature = "sftp")]
            SshQueued::Sftp { name, id, .. } => {
                runtime.ready.push(Answer::Sftp { id, name, started: Some(false), result: Err(error_for(id)) });
            }
            SshQueued::Connect(..) | SshQueued::Disconnect(_) => {}
        }
    }
    let ops: Vec<(RequestId, Op)> = runtime.ops.drain().collect();
    let open = ops.len();
    for (id, op) in ops {
        let started = started_by_plugin(&op);
        runtime.push(id, op, started, error_for(id));
    }
    let names: Vec<SshName> = runtime.conns.keys().cloned().collect();
    for name in names {
        runtime.close(&name, transport.as_deref_mut(), "the app is exiting");
        runtime.set_state(&name, SshState::Disconnected, None);
    }
    runtime.by_conn.clear();
    if let Some(transport) = transport.as_mut() {
        transport.get_mut().shutdown();
    }
    if open > 0 {
        tracing::info!(">>> NET-BACKEND: app exit: {open} open SSH request(s) answered with Shutdown");
    }
    refresh_info(&mut connections, runtime);
    runtime.publish(&mut inflight);
    flush(runtime, &mut writers);
}
