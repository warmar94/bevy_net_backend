//! The SSH seam: [`SshTransport`] (one [`SshConnId`] = one connection attempt), its resource, and
//! the in-memory [`FakeSshTransport`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bevy_ecs::resource::Resource;

#[cfg(feature = "sftp")]
use super::{SftpOp, SftpOutcome};
use super::{SshCommand, SshExit, SshStream, SshTarget};
use crate::request::RequestId;
use crate::response::BackendError;

/// Identifies one connection attempt: each `connect` of a named connection is a new one. Opaque;
/// `Display` shows it for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SshConnId(u64);

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

impl SshConnId {
    pub(crate) fn next() -> Self {
        Self(NEXT_CONN.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for SshConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ssh#{}", self.0)
    }
}

/// What a transport reports. After `Closed` a connection reports nothing more; a request reports
/// at most one `Finished` / `SftpFinished`. `Debug` never shows output bytes.
#[derive(Clone)]
#[non_exhaustive]
pub enum SshEvent {
    /// Connected, host key verified, authenticated.
    Connected {
        /// The connection.
        conn: SshConnId,
        /// The server's host key fingerprint (`SHA256:…`).
        fingerprint: String,
    },
    /// The connection attempt failed, or an open connection was lost (`error: Some`), or it was
    /// closed as the plugin asked (`error: None`).
    Closed {
        /// The connection.
        conn: SshConnId,
        /// Why (the dependency's or the server's words).
        error: Option<BackendError>,
    },
    /// The server accepted a command's exec request: it runs.
    Started {
        /// The command.
        id: RequestId,
    },
    /// Output of a running command.
    Output {
        /// The command.
        id: RequestId,
        /// stdout or stderr.
        stream: SshStream,
        /// The bytes.
        data: Vec<u8>,
    },
    /// A command ended (or could not run).
    Finished {
        /// The command.
        id: RequestId,
        /// The exit, or the error. A command that was never sent says so with a `Timeout` text
        /// starting with `not sent:` or `Disconnected { sent: Some(false) }`.
        result: Result<SshExit, BackendError>,
    },
    /// Progress of an SFTP transfer (feature `sftp`).
    #[cfg(feature = "sftp")]
    Progress {
        /// The operation.
        id: RequestId,
        /// Bytes moved so far.
        done: u64,
        /// The size, when known.
        total: Option<u64>,
    },
    /// An SFTP operation ended (feature `sftp`).
    #[cfg(feature = "sftp")]
    SftpFinished {
        /// The operation.
        id: RequestId,
        /// The outcome, or the error.
        result: Result<SftpOutcome, BackendError>,
    },
}

impl fmt::Debug for SshEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshEvent::Connected { conn, fingerprint } => f.debug_struct("Connected").field("conn", conn).field("fingerprint", fingerprint).finish(),
            SshEvent::Closed { conn, error } => f.debug_struct("Closed").field("conn", conn).field("error", error).finish(),
            SshEvent::Started { id } => f.debug_struct("Started").field("id", id).finish(),
            SshEvent::Output { id, stream, data } => f.debug_struct("Output").field("id", id).field("stream", stream).field("bytes", &data.len()).finish(),
            SshEvent::Finished { id, result } => f.debug_struct("Finished").field("id", id).field("result", result).finish(),
            #[cfg(feature = "sftp")]
            SshEvent::Progress { id, done, total } => f.debug_struct("Progress").field("id", id).field("done", done).field("total", total).finish(),
            #[cfg(feature = "sftp")]
            SshEvent::SftpFinished { id, result } => f.debug_struct("SftpFinished").field("id", id).field("result", result).finish(),
        }
    }
}

/// Opens SSH connections and runs requests on them. The plugin owns the bookkeeping (pending
/// requests, deadlines, cancel, exit) and every answer; a transport only reports. All methods run
/// on the main thread and must never block or panic.
///
/// **Compatibility promise:** methods added to this trait in later versions always come with a
/// default implementation.
pub trait SshTransport: Send + Sync + 'static {
    /// Start connecting (resolve an ssh_config alias, TCP, key exchange, host key check,
    /// authentication, all under the target's connect timeout). Report `Connected`, later exactly
    /// one `Closed` (or just `Closed`).
    fn connect(&mut self, conn: SshConnId, target: SshTarget);

    /// Run `command` on a connected `conn`. Its timeout and output limit are always set. Report
    /// `Started`, `Output` and exactly one `Finished`.
    fn run(&mut self, conn: SshConnId, id: RequestId, command: SshCommand);

    /// An SFTP operation on a connected `conn` (feature `sftp`). Report `Progress` and exactly one
    /// `SftpFinished`. Only called when [`supports_sftp`](Self::supports_sftp) is `true`. Default:
    /// nothing.
    #[cfg(feature = "sftp")]
    fn sftp(&mut self, conn: SshConnId, id: RequestId, op: SftpOp) {
        let _ = (conn, id, op);
    }

    /// Whether `sftp` (feature `sftp`) is implemented. Default: `false` (SFTP requests are answered
    /// `InvalidRequest` and never handed over).
    fn supports_sftp(&self) -> bool {
        false
    }

    /// The plugin answered `id` (cancelled or timed out): stop it if it has not ended (close its
    /// channel). Default: nothing.
    fn cancel(&mut self, id: RequestId) {
        let _ = id;
    }

    /// Close a connection; the plugin stops listening to it at once. Default: nothing.
    fn close(&mut self, conn: SshConnId) {
        let _ = conn;
    }

    /// Everything that happened since the last call.
    fn poll(&mut self) -> Vec<SshEvent>;

    /// The app is exiting and the plugin has answered everything: start nothing new, close every
    /// connection, never wait. Default: nothing.
    fn shutdown(&mut self) {}
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The installed SSH transport. The plugin inserts a [`RusshTransport`](crate::RusshTransport)
/// when there is none; insert a [`FakeSshTransport`] for tests. Connections of a transport that
/// is removed or replaced count as lost.
#[derive(Resource)]
pub struct SshTransportRes {
    inner: Box<dyn SshTransport>,
    generation: u64,
}

impl fmt::Debug for SshTransportRes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTransportRes").field("generation", &self.generation).finish_non_exhaustive()
    }
}

impl SshTransportRes {
    /// Wrap a transport.
    pub fn new(transport: impl SshTransport) -> Self {
        Self { inner: Box::new(transport), generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed) }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn get_mut(&mut self) -> &mut dyn SshTransport {
        self.inner.as_mut()
    }

    #[cfg_attr(not(feature = "sftp"), allow(dead_code))]
    pub(crate) fn supports_sftp(&self) -> bool {
        self.inner.supports_sftp()
    }
}

/// A scripted command reply for [`FakeSshTransport::on_command`].
#[derive(Clone)]
struct Script {
    output: Vec<(SshStream, Vec<u8>)>,
    result: Result<SshExit, BackendError>,
}

#[derive(Default)]
struct FakeState {
    connects: Vec<(SshConnId, SshTarget)>,
    live: HashSet<SshConnId>,
    manual: bool,
    reject_next: VecDeque<BackendError>,
    scripts: HashMap<String, Script>,
    commands: Vec<(SshConnId, RequestId, SshCommand)>,
    running: HashSet<RequestId>,
    #[cfg(feature = "sftp")]
    sftp: Vec<(SshConnId, RequestId, SftpOp)>,
    #[cfg(feature = "sftp")]
    sftp_scripts: VecDeque<Result<SftpOutcome, BackendError>>,
    cancelled: Vec<RequestId>,
    closed: Vec<SshConnId>,
    events: VecDeque<SshEvent>,
    shutdowns: usize,
}

/// An in-memory [`SshTransport`] for tests: records every connect, command, SFTP operation,
/// cancel and close, and answers from a script. Clones share one state. Events are delivered on
/// the next poll (the next frame's `First`). Never touches the network or any file.
///
/// By default every connect succeeds (fingerprint [`FakeSshTransport::FINGERPRINT`]).
/// [`manual_connect`](Self::manual_connect) holds connects until [`accept`](Self::accept);
/// [`reject_next`](Self::reject_next) fails the next attempts. A command scripted with
/// [`on_command`](Self::on_command) starts, prints and ends at once; any other command starts and
/// keeps running until the test calls [`output`](Self::output) / [`finish`](Self::finish).
#[derive(Clone, Default)]
pub struct FakeSshTransport {
    state: Arc<Mutex<FakeState>>,
}

impl fmt::Debug for FakeSshTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeSshTransport").field("connects", &state.connects.len()).field("commands", &state.commands.len()).finish_non_exhaustive()
    }
}

impl FakeSshTransport {
    /// The fingerprint every fake connection reports (obviously fake).
    pub const FINGERPRINT: &'static str = "SHA256:fake0fake0fake0fake0fake0fake0fake0fake0fa";

    /// A fake that accepts every connect.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn event(&self, event: SshEvent) {
        self.lock().events.push_back(event);
    }

    /// Hold new connects until [`accept`](Self::accept).
    pub fn manual_connect(&self, manual: bool) -> &Self {
        self.lock().manual = manual;
        self
    }

    /// Fail the next connect with `error` (queue several for several connects).
    pub fn reject_next(&self, error: BackendError) -> &Self {
        self.lock().reject_next.push_back(error);
        self
    }

    /// Script `command` (the exact command line): it starts, writes `output` and ends with `result`.
    pub fn on_command(&self, command: &str, output: &[(SshStream, &str)], result: Result<SshExit, BackendError>) -> &Self {
        let output = output.iter().map(|(stream, text)| (*stream, text.as_bytes().to_vec())).collect();
        self.lock().scripts.insert(command.to_string(), Script { output, result });
        self
    }

    /// The next SFTP operation ends with `result` (queue several for several operations);
    /// unscripted operations stay open until [`sftp_finish`](Self::sftp_finish).
    #[cfg(feature = "sftp")]
    pub fn on_next_sftp(&self, result: Result<SftpOutcome, BackendError>) -> &Self {
        self.lock().sftp_scripts.push_back(result);
        self
    }

    /// Complete a held connect.
    pub fn accept(&self, conn: SshConnId) {
        self.event(SshEvent::Connected { conn, fingerprint: Self::FINGERPRINT.to_string() });
    }

    /// `conn` is lost (or its connect fails) with `error`.
    pub fn drop_conn(&self, conn: SshConnId, error: BackendError) {
        let mut state = self.lock();
        state.live.remove(&conn);
        state.events.push_back(SshEvent::Closed { conn, error: Some(error) });
    }

    /// The command `id` writes `data`.
    pub fn output(&self, id: RequestId, stream: SshStream, data: &[u8]) {
        self.event(SshEvent::Output { id, stream, data: data.to_vec() });
    }

    /// The command `id` ends.
    pub fn finish(&self, id: RequestId, result: Result<SshExit, BackendError>) {
        let mut state = self.lock();
        state.running.remove(&id);
        state.events.push_back(SshEvent::Finished { id, result });
    }

    /// Report SFTP progress for `id`.
    #[cfg(feature = "sftp")]
    pub fn sftp_progress(&self, id: RequestId, done: u64, total: Option<u64>) {
        self.event(SshEvent::Progress { id, done, total });
    }

    /// The SFTP operation `id` ends.
    #[cfg(feature = "sftp")]
    pub fn sftp_finish(&self, id: RequestId, result: Result<SftpOutcome, BackendError>) {
        self.event(SshEvent::SftpFinished { id, result });
    }

    /// Every connect so far with its target, in order.
    pub fn connects(&self) -> Vec<(SshConnId, SshTarget)> {
        self.lock().connects.clone()
    }

    /// The last connection id handed out.
    pub fn last_conn(&self) -> Option<SshConnId> {
        self.lock().connects.last().map(|(conn, _)| *conn)
    }

    /// Connections neither closed by the plugin nor dropped by the script.
    pub fn live_conns(&self) -> Vec<SshConnId> {
        let mut conns: Vec<SshConnId> = self.lock().live.iter().copied().collect();
        conns.sort_unstable();
        conns
    }

    /// Every command handed over, with its connection and id.
    pub fn commands(&self) -> Vec<(SshConnId, RequestId, SshCommand)> {
        self.lock().commands.clone()
    }

    /// Commands started and not finished yet.
    pub fn running(&self) -> Vec<RequestId> {
        let mut running: Vec<RequestId> = self.lock().running.iter().copied().collect();
        running.sort_unstable();
        running
    }

    /// Every SFTP operation handed over.
    #[cfg(feature = "sftp")]
    pub fn sftp_ops(&self) -> Vec<(SshConnId, RequestId, SftpOp)> {
        self.lock().sftp.clone()
    }

    /// Ids the plugin cancelled (cancel or timeout), in order.
    pub fn cancelled(&self) -> Vec<RequestId> {
        self.lock().cancelled.clone()
    }

    /// Connections the plugin closed.
    pub fn closed(&self) -> Vec<SshConnId> {
        self.lock().closed.clone()
    }

    /// How many times the plugin shut the transport down.
    pub fn shutdown_count(&self) -> usize {
        self.lock().shutdowns
    }
}

impl SshTransport for FakeSshTransport {
    fn connect(&mut self, conn: SshConnId, target: SshTarget) {
        let mut state = self.lock();
        state.connects.push((conn, target));
        if let Some(error) = state.reject_next.pop_front() {
            state.events.push_back(SshEvent::Closed { conn, error: Some(error) });
            return;
        }
        state.live.insert(conn);
        if !state.manual {
            state.events.push_back(SshEvent::Connected { conn, fingerprint: Self::FINGERPRINT.to_string() });
        }
    }

    fn run(&mut self, conn: SshConnId, id: RequestId, command: SshCommand) {
        let mut state = self.lock();
        let script = state.scripts.get(command.command()).cloned();
        state.commands.push((conn, id, command));
        if !state.live.contains(&conn) {
            state.events.push_back(SshEvent::Finished { id, result: Err(BackendError::disconnected("not connected", Some(false))) });
            return;
        }
        state.events.push_back(SshEvent::Started { id });
        match script {
            Some(script) => {
                for (stream, data) in script.output {
                    state.events.push_back(SshEvent::Output { id, stream, data });
                }
                state.events.push_back(SshEvent::Finished { id, result: script.result });
            }
            None => {
                state.running.insert(id);
            }
        }
    }

    #[cfg(feature = "sftp")]
    fn sftp(&mut self, conn: SshConnId, id: RequestId, op: SftpOp) {
        let mut state = self.lock();
        state.sftp.push((conn, id, op));
        if let Some(result) = state.sftp_scripts.pop_front() {
            state.events.push_back(SshEvent::SftpFinished { id, result });
        }
    }

    fn supports_sftp(&self) -> bool {
        cfg!(feature = "sftp")
    }

    fn cancel(&mut self, id: RequestId) {
        let mut state = self.lock();
        state.running.remove(&id);
        state.cancelled.push(id);
    }

    fn close(&mut self, conn: SshConnId) {
        let mut state = self.lock();
        state.live.remove(&conn);
        state.closed.push(conn);
    }

    fn poll(&mut self) -> Vec<SshEvent> {
        self.lock().events.drain(..).collect()
    }

    fn shutdown(&mut self) {
        let mut state = self.lock();
        state.shutdowns += 1;
        state.live.clear();
        state.running.clear();
    }
}
