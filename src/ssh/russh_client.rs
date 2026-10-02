//! [`RusshTransport`]: the real SSH transport. ONE private std thread (`net-backend-ssh`) owns a
//! tokio current-thread runtime (russh needs tokio); it starts on the first connect and stops on
//! `shutdown` / drop. Every connection is a task on that thread, every command or SFTP operation a
//! task of its connection. Nothing runs on the game's threads or on Bevy's task pools; tokio's
//! blocking pool (DNS lookups, key file decoding) is capped at 2 threads.
//!
//! Deadlines: connect + key exchange + host key check + authentication under ONE absolute
//! deadline; each command / SFTP operation under its own. A connection's socket is wrapped in a
//! kill switch tied to its task, so a timed-out or closed connection never lingers.

use std::any::Any;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use russh::client::{self, DisconnectReason, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, Preferred, Sig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc as tmpsc, oneshot, Notify, Semaphore};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use zeroize::Zeroizing;

use super::known_hosts::{lookup_name, same_family, KnownHosts, MAX_KNOWN_HOSTS_BYTES};
#[cfg(feature = "sftp")]
use super::sftp::ops as sftp_ops;
use super::ssh_config::{home, read_limited, resolve, Resolved};
use super::transport::{SshConnId, SshEvent, SshTransport};
#[cfg(feature = "sftp")]
use super::SftpOp;
use super::{
    file_name, AuthKind, SshCommand, SshExit, SshPrompt, SshPromptRequest, SshStream, SshTarget, DEFAULT_SSH_COMMAND_TIMEOUT, DEFAULT_SSH_MAX_OUTPUT_BYTES,
};
use crate::request::RequestId;
use crate::response::BackendError;

/// The largest private key file read.
const MAX_KEY_FILE_BYTES: u64 = 256 * 1024;
/// How long a closing connection may take to say goodbye (disconnect message, channel closes).
const GOODBYE: Duration = Duration::from_secs(1);
/// How often a connection checks that its russh session is still alive (a backstop in case russh
/// ends the session without calling `disconnected`).
const WATCHDOG: Duration = Duration::from_secs(1);
/// The most keyboard-interactive rounds answered for one login.
const MAX_PROMPT_ROUNDS: usize = 8;

/// What the main thread tells the SSH thread.
enum Command {
    Connect(SshConnId, Box<SshTarget>),
    Run(SshConnId, RequestId, SshCommand),
    #[cfg(feature = "sftp")]
    Sftp(SshConnId, RequestId, SftpOp),
    Cancel(RequestId),
    Close(SshConnId),
    Shutdown,
}

/// Where the SSH thread reports.
#[derive(Clone)]
struct EventSink(mpsc::Sender<SshEvent>);

impl EventSink {
    fn send(&self, event: SshEvent) {
        let _ = self.0.send(event);
    }
}

struct Worker {
    commands: tmpsc::UnboundedSender<Command>,
    /// In a mutex only to make the transport `Sync` (a resource); only `poll` locks it.
    events: Mutex<mpsc::Receiver<SshEvent>>,
    stopping: Arc<AtomicBool>,
}

/// The real [`SshTransport`] (feature `ssh`): russh 0.63.3 with ring for the AEAD ciphers, on one
/// private thread with a tokio current-thread runtime, started on the first connect. The plugin
/// inserts one unless an [`SshTransportRes`](crate::SshTransportRes) exists.
pub struct RusshTransport {
    release_allowed: bool,
    worker: Option<Worker>,
    /// Events decided on the main thread (the SSH thread could not start or stopped).
    local: Vec<SshEvent>,
    /// Connections handed to the thread and not reported closed.
    open: HashSet<SshConnId>,
}

impl fmt::Debug for RusshTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RusshTransport").field("thread_running", &self.worker.is_some()).field("connections", &self.open.len()).finish()
    }
}

impl Default for RusshTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl RusshTransport {
    /// A transport; its thread starts on the first connect. Like the plugin, it refuses to
    /// connect in a release build unless [`with_release_allowed`](Self::with_release_allowed)
    /// (the plugin passes `SshSettings::allow_in_release` on).
    pub fn new() -> Self {
        Self { release_allowed: false, worker: None, local: Vec::new(), open: HashSet::new() }
    }

    /// Allow connecting in a release build (default `false`). Admin / dev tools only.
    pub fn with_release_allowed(mut self, allow: bool) -> Self {
        self.release_allowed = allow;
        self
    }

    fn start(&mut self) -> Result<(), String> {
        if self.worker.is_some() {
            return Ok(());
        }
        let (commands, receiver) = tmpsc::unbounded_channel();
        let (sender, events) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);
        std::thread::Builder::new()
            .name("net-backend-ssh".into())
            .spawn(move || thread_main(receiver, EventSink(sender), flag))
            .map_err(|e| format!("could not start the SSH thread: {e}"))?;
        tracing::debug!(">>> NET-BACKEND: SSH thread started");
        self.worker = Some(Worker { commands, events: Mutex::new(events), stopping });
        Ok(())
    }

    /// Send to the thread; `false` when it is not running.
    fn send(&mut self, command: Command) -> bool {
        match &self.worker {
            Some(worker) => worker.commands.send(command).is_ok(),
            None => false,
        }
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stopping.store(true, Ordering::SeqCst);
            let _ = worker.commands.send(Command::Shutdown);
        }
        self.open.clear();
    }
}

impl Drop for RusshTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

impl SshTransport for RusshTransport {
    fn connect(&mut self, conn: SshConnId, target: SshTarget) {
        if !super::allowed(cfg!(debug_assertions), self.release_allowed) {
            self.local.push(SshEvent::Closed { conn, error: Some(super::systems::release_refusal()) });
            return;
        }
        if let Err(why) = self.start() {
            tracing::warn!(">>> NET-BACKEND: {why}");
            self.local.push(SshEvent::Closed { conn, error: Some(BackendError::Network(why)) });
            return;
        }
        if self.send(Command::Connect(conn, Box::new(target))) {
            self.open.insert(conn);
        } else {
            self.worker = None;
            self.local.push(SshEvent::Closed { conn, error: Some(BackendError::Network("the SSH thread is not running".into())) });
        }
    }

    fn run(&mut self, conn: SshConnId, id: RequestId, command: SshCommand) {
        if !self.send(Command::Run(conn, id, command)) {
            self.local.push(SshEvent::Finished { id, result: Err(BackendError::disconnected("the SSH thread is not running", Some(false))) });
        }
    }

    #[cfg(feature = "sftp")]
    fn sftp(&mut self, conn: SshConnId, id: RequestId, op: SftpOp) {
        if !self.send(Command::Sftp(conn, id, op)) {
            self.local.push(SshEvent::SftpFinished { id, result: Err(BackendError::disconnected("the SSH thread is not running", Some(false))) });
        }
    }

    fn supports_sftp(&self) -> bool {
        cfg!(feature = "sftp")
    }

    fn cancel(&mut self, id: RequestId) {
        self.send(Command::Cancel(id));
    }

    fn close(&mut self, conn: SshConnId) {
        self.open.remove(&conn);
        self.send(Command::Close(conn));
    }

    fn poll(&mut self) -> Vec<SshEvent> {
        let mut events = std::mem::take(&mut self.local);
        let mut gone = false;
        if let Some(worker) = &self.worker {
            let receiver = worker.events.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
        }
        for event in &events {
            if let SshEvent::Closed { conn, .. } = event {
                self.open.remove(conn);
            }
        }
        if gone {
            // The thread ended (it panicked, or its runtime could not start): every connection is
            // lost; the next connect starts a new thread.
            self.worker = None;
            for conn in self.open.drain() {
                events.push(SshEvent::Closed { conn, error: Some(BackendError::Network("the SSH thread stopped".into())) });
            }
        }
        events
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

fn panic_text(panic: &(dyn Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

fn thread_main(mut commands: tmpsc::UnboundedReceiver<Command>, events: EventSink, stopping: Arc<AtomicBool>) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(2).thread_name("net-backend-ssh-io").build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(e) => {
            // Answer what is already queued; later commands find the thread gone.
            while let Ok(command) = commands.try_recv() {
                if let Command::Connect(conn, _) = command {
                    events.send(SshEvent::Closed { conn, error: Some(BackendError::Network(format!("could not start the SSH runtime: {e}"))) });
                }
            }
            return;
        }
    };
    runtime.block_on(dispatch(commands, events, stopping));
    runtime.shutdown_timeout(Duration::from_millis(500));
    tracing::debug!(">>> NET-BACKEND: SSH thread stopped");
}

/// Spawn `future` and watch it: a panic becomes `on_panic(text)`. Returns the task's abort handle.
fn spawn_guarded(
    future: impl Future<Output = ()> + Send + 'static,
    on_panic: impl FnOnce(String) + Send + 'static,
) -> (AbortHandle, tokio::task::JoinHandle<()>) {
    let task = tokio::spawn(future);
    let abort = task.abort_handle();
    let watcher = tokio::spawn(async move {
        if let Err(error) = task.await {
            if error.is_panic() {
                on_panic(panic_text(&*error.into_panic()).to_string());
            }
        }
    });
    (abort, watcher)
}

enum ConnCommand {
    Run(RequestId, SshCommand),
    #[cfg(feature = "sftp")]
    Sftp(RequestId, SftpOp),
    Cancel(RequestId),
    Close,
}

struct ConnHandle {
    commands: tmpsc::UnboundedSender<ConnCommand>,
    watcher: tokio::task::JoinHandle<()>,
}

async fn dispatch(mut commands: tmpsc::UnboundedReceiver<Command>, events: EventSink, stopping: Arc<AtomicBool>) {
    let mut conns: HashMap<SshConnId, ConnHandle> = HashMap::new();
    let mut closing: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    while let Some(command) = commands.recv().await {
        conns.retain(|_, c| !c.watcher.is_finished());
        closing.retain(|task| !task.is_finished());
        match command {
            Command::Connect(conn, target) => {
                let (sender, receiver) = tmpsc::unbounded_channel();
                let sink = events.clone();
                let (_, watcher) = spawn_guarded(connection(conn, *target, receiver, events.clone(), Arc::clone(&stopping)), move |panic| {
                    sink.send(SshEvent::Closed { conn, error: Some(BackendError::Network(format!("the SSH connection task panicked: {panic}"))) });
                });
                conns.insert(conn, ConnHandle { commands: sender, watcher });
            }
            Command::Run(conn, id, command) => {
                if conns.get(&conn).is_none_or(|c| c.commands.send(ConnCommand::Run(id, command)).is_err()) {
                    events.send(SshEvent::Finished { id, result: Err(BackendError::disconnected("the connection is closed", Some(false))) });
                }
            }
            #[cfg(feature = "sftp")]
            Command::Sftp(conn, id, op) => {
                if conns.get(&conn).is_none_or(|c| c.commands.send(ConnCommand::Sftp(id, op)).is_err()) {
                    events.send(SshEvent::SftpFinished { id, result: Err(BackendError::disconnected("the connection is closed", Some(false))) });
                }
            }
            Command::Cancel(id) => {
                for conn in conns.values() {
                    let _ = conn.commands.send(ConnCommand::Cancel(id));
                }
            }
            Command::Close(conn) => {
                if let Some(handle) = conns.remove(&conn) {
                    let _ = handle.commands.send(ConnCommand::Close);
                    closing.push(handle.watcher);
                }
            }
            Command::Shutdown => break,
        }
    }
    // Shutdown, or the transport is gone: close everything, give the connections a moment to say
    // goodbye, then the runtime is dropped (which closes whatever is left).
    for conn in conns.values() {
        let _ = conn.commands.send(ConnCommand::Close);
    }
    closing.extend(conns.into_values().map(|c| c.watcher));
    let _ = tokio::time::timeout(GOODBYE, async {
        for task in closing {
            let _ = task.await;
        }
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// The socket kill switch.

/// A TCP stream that fails every read and write once its owner's `oneshot::Sender` is dropped
/// (or used): the owner task ended, so the russh session task must end too.
struct Guarded {
    inner: TcpStream,
    kill: oneshot::Receiver<()>,
    killed: bool,
}

impl Guarded {
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.killed && Pin::new(&mut self.kill).poll(cx).is_ready() {
            self.killed = true;
        }
        if self.killed {
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the connection was closed by the plugin"))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for Guarded {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Guarded {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.killed {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------------------------
// The russh handler: host key check, strict key exchange, loss notification.

/// Why the connection ended, set once by the handler.
#[derive(Default)]
struct Lost {
    reason: Mutex<Option<String>>,
    notify: Notify,
}

impl Lost {
    fn set(&self, reason: String) {
        let mut slot = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.notify.notify_one();
    }

    async fn wait(&self) -> String {
        loop {
            if let Some(reason) = self.reason.lock().unwrap_or_else(PoisonError::into_inner).clone() {
                return reason;
            }
            self.notify.notified().await;
        }
    }
}

#[derive(Debug)]
enum HandlerError {
    Russh(russh::Error),
    Refused(BackendError),
}

impl From<russh::Error> for HandlerError {
    fn from(error: russh::Error) -> Self {
        HandlerError::Russh(error)
    }
}

impl HandlerError {
    fn into_backend(self) -> BackendError {
        match self {
            HandlerError::Refused(error) => error,
            HandlerError::Russh(error) => map_russh(error),
        }
    }
}

/// russh's words, in the matching kind.
fn map_russh(error: russh::Error) -> BackendError {
    match error {
        russh::Error::IO(e) => BackendError::Network(e.to_string()),
        russh::Error::KeepaliveTimeout => BackendError::Timeout("the server stopped answering keepalives".into()),
        russh::Error::ConnectionTimeout | russh::Error::InactivityTimeout => BackendError::Timeout(error.to_string()),
        russh::Error::Disconnect | russh::Error::HUP => BackendError::disconnected(error.to_string(), None),
        russh::Error::Version => BackendError::Ssh("the server did not send a valid SSH banner (is it an SSH server?)".into()),
        other => BackendError::Ssh(other.to_string()),
    }
}

/// Only printable characters of a server-supplied text, at most 200.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

struct ClientHandler {
    known: Arc<KnownHosts>,
    pinned: Vec<String>,
    allow_terrapin_vulnerable: bool,
    name: String,
    fingerprint: Arc<Mutex<Option<String>>>,
    lost: Arc<Lost>,
}

impl client::Handler for ClientHandler {
    type Error = HandlerError;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => match self.known.verify(&self.name, key, &self.pinned) {
                Ok(fingerprint) => {
                    *self.fingerprint.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
                    Ok(true)
                }
                Err(error) => Err(HandlerError::Refused(error)),
            },
            // Certificates are never negotiated (none are offered); refuse one anyway.
            PublicKeyOrCertificate::Certificate(_) => {
                Err(HandlerError::Refused(BackendError::Ssh("the server presented a host certificate; not supported".into())))
            }
        }
    }

    async fn kex_done(&mut self, _shared_secret: Option<&[u8]>, names: &russh::Names, _session: &mut client::Session) -> Result<(), Self::Error> {
        match terrapin_refusal(names.strict_kex(), names.cipher.as_ref(), [names.client_mac.as_ref(), names.server_mac.as_ref()]) {
            Some(error) if !self.allow_terrapin_vulnerable => Err(HandlerError::Refused(error)),
            Some(_) => {
                tracing::warn!(
                    ">>> NET-BACKEND: ssh: `{}` does not support strict key exchange and uses `{}`: Terrapin-vulnerable, accepted because of allow_terrapin_vulnerable",
                    self.name,
                    names.cipher.as_ref()
                );
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn disconnected(&mut self, reason: DisconnectReason<Self::Error>) -> Result<(), Self::Error> {
        let text = match reason {
            DisconnectReason::ReceivedDisconnect(info) => format!("the server closed the connection ({:?}: {})", info.reason_code, clean(&info.message)),
            DisconnectReason::Error(error) => error.into_backend().to_string(),
        };
        self.lost.set(text);
        Ok(())
    }
}

/// Terrapin (CVE-2023-48795): without strict key exchange, ChaCha20-Poly1305 and CBC with an
/// encrypt-then-MAC MAC are practically attackable (an attacker on the path can drop messages at
/// the start of the connection); AES-GCM and CTR are not. The client prefers AES-GCM, so this only
/// hits a server without strict key exchange that offers none of the safe ciphers. OpenSSH 9.6
/// and later (and most distributions' backports) support strict key exchange.
fn terrapin_refusal(strict_kex: bool, cipher: &str, macs: [&str; 2]) -> Option<BackendError> {
    let etm = macs.iter().any(|mac| mac.ends_with("-etm@openssh.com"));
    let exposed = cipher.starts_with("chacha20-poly1305") || (cipher.contains("-cbc") && etm);
    (!strict_kex && exposed).then(|| {
        BackendError::Ssh(format!(
            "refused: the server does not support strict key exchange and only agreed on `{cipher}`, which is then open to the Terrapin attack (CVE-2023-48795); enable AES-GCM or strict key exchange on the server (OpenSSH 9.6+), or accept the risk with SshTarget::allow_terrapin_vulnerable(true)"
        ))
    })
}

/// The ciphers offered, most preferred first: AES-GCM first (never Terrapin-exposed), then
/// ChaCha20-Poly1305 (fine with strict key exchange), then AES-CTR.
const CIPHERS: &[russh::cipher::Name] =
    &[russh::cipher::AES_256_GCM, russh::cipher::CHACHA20_POLY1305, russh::cipher::AES_256_CTR, russh::cipher::AES_192_CTR, russh::cipher::AES_128_CTR];

/// russh's client settings for a target: the crate's keepalive, no inactivity timeout, Nagle off, AES-GCM
/// first, and host key algorithms without SHA-1 RSA (and without RSA at all unless `ssh-rsa`),
/// the types already in known_hosts for this host first (as OpenSSH does).
fn client_config(target: &SshTarget, known_types: &[Algorithm]) -> client::Config {
    let usable: Vec<Algorithm> = Preferred::DEFAULT
        .key
        .iter()
        .filter(|algorithm| match algorithm {
            Algorithm::Rsa { hash } => cfg!(feature = "ssh-rsa") && hash.is_some(),
            _ => true,
        })
        .cloned()
        .collect();
    let (mut keys, rest): (Vec<Algorithm>, Vec<Algorithm>) = usable.into_iter().partition(|a| known_types.iter().any(|k| same_family(k, a)));
    keys.sort_by_key(|a| known_types.iter().position(|k| same_family(k, a)).unwrap_or(usize::MAX));
    keys.extend(rest);
    client::Config {
        keepalive_interval: Some(target.keepalive_interval),
        keepalive_max: usize::try_from(target.keepalive_max).unwrap_or(3),
        inactivity_timeout: None,
        nodelay: true,
        preferred: Preferred { key: Cow::Owned(keys), cipher: Cow::Borrowed(CIPHERS), ..Preferred::DEFAULT },
        ..client::Config::default()
    }
}

// ---------------------------------------------------------------------------------------------
// One connection.

struct Established {
    handle: Handle<ClientHandler>,
    fingerprint: String,
}

/// Read the known_hosts files the target names (or `~/.ssh/known_hosts` when it names neither a
/// file nor a pinned fingerprint; a missing default file is just empty).
fn load_known_hosts(target: &SshTarget) -> Result<KnownHosts, BackendError> {
    let mut known = KnownHosts::default();
    if target.known_hosts.is_empty() && target.pinned.is_empty() {
        if let Some(path) = home().map(|h| h.join(".ssh").join("known_hosts")) {
            if path.exists() {
                known.add(&read_limited(&path, MAX_KNOWN_HOSTS_BYTES, "the known_hosts file")?);
            }
        }
    }
    for path in &target.known_hosts {
        known.add(&read_limited(path, MAX_KNOWN_HOSTS_BYTES, "the known_hosts file")?);
    }
    Ok(known)
}

async fn establish(target: &SshTarget, kill: oneshot::Receiver<()>, lost: Arc<Lost>) -> Result<Established, BackendError> {
    // File I/O (ssh_config with its includes, known_hosts) on the blocking pool: a slow share or
    // a FIFO must not freeze the other connections on the SSH thread.
    let files = target.clone();
    let (resolved, known) = tokio::task::spawn_blocking(move || Ok::<_, BackendError>((resolve(&files)?, load_known_hosts(&files)?)))
        .await
        .map_err(|e| BackendError::Network(format!("reading the SSH settings failed ({e})")))??;
    let name = lookup_name(&resolved.host, resolved.port);
    let known_types = known.known_algorithms(&name);
    let tcp = TcpStream::connect((resolved.host.as_str(), resolved.port));
    let tcp = match resolved.connect_timeout {
        Some(limit) => tokio::time::timeout(limit, tcp)
            .await
            .map_err(|_| BackendError::Timeout(format!("TCP connect to `{name}` took longer than the ssh_config ConnectTimeout ({limit:?})")))?,
        None => tcp.await,
    }
    .map_err(|e| BackendError::Network(format!("could not connect to `{name}`: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let fingerprint = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        known: Arc::new(known),
        pinned: target.pinned.clone(),
        allow_terrapin_vulnerable: target.allow_terrapin_vulnerable,
        name,
        fingerprint: Arc::clone(&fingerprint),
        lost,
    };
    let stream = Guarded { inner: tcp, kill, killed: false };
    let mut handle = client::connect_stream(Arc::new(client_config(target, &known_types)), stream, handler).await.map_err(HandlerError::into_backend)?;
    let fingerprint = fingerprint.lock().unwrap_or_else(PoisonError::into_inner).take();
    let Some(fingerprint) = fingerprint else {
        return Err(BackendError::Ssh("the server's host key was never checked".into()));
    };
    authenticate(&mut handle, &resolved, target).await?;
    Ok(Established { handle, fingerprint })
}

/// Load and decode a private key file on the blocking pool (decryption is slow on purpose).
async fn load_key(path: PathBuf, passphrase: Option<crate::Secret>) -> Result<PrivateKey, String> {
    let task = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let file = std::fs::File::open(&path).map_err(|e| format!("cannot be read ({e})"))?;
        let mut text = Zeroizing::new(String::new());
        file.take(MAX_KEY_FILE_BYTES.saturating_add(1)).read_to_string(&mut text).map_err(|e| format!("cannot be read ({e})"))?;
        if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_KEY_FILE_BYTES {
            return Err(format!("is larger than {MAX_KEY_FILE_BYTES} bytes"));
        }
        russh::keys::decode_secret_key(&text, passphrase.as_ref().map(crate::Secret::expose)).map_err(|e| match passphrase {
            Some(_) => format!("could not be decoded with the passphrase ({e})"),
            None => format!("could not be decoded ({e}; an encrypted key needs SshAuth::key_file_with_passphrase)"),
        })
    });
    task.await.map_err(|e| format!("could not be loaded ({e})"))?
}

/// The SSH agent of this platform.
async fn connect_agent() -> Result<AgentClient<Box<dyn AgentStream + Send + Unpin>>, String> {
    #[cfg(unix)]
    {
        AgentClient::connect_env().await.map(AgentClient::dynamic).map_err(|e| match e {
            russh::keys::Error::EnvVar(_) => "SSH_AUTH_SOCK is not set".to_string(),
            _ => "the agent socket could not be opened".to_string(),
        })
    }
    #[cfg(windows)]
    {
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            return Ok(agent.dynamic());
        }
        AgentClient::connect_pageant().await.map(AgentClient::dynamic).map_err(|_| "neither the OpenSSH agent nor Pageant is running".to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err("no SSH agent support on this platform".to_string())
    }
}

/// The RSA signature hash to use: SHA-2 only (never SHA-1 `ssh-rsa`).
async fn rsa_hash(handle: &Handle<ClientHandler>) -> Result<Option<HashAlg>, String> {
    match handle.best_supported_rsa_hash().await {
        Ok(Some(Some(hash))) => Ok(Some(hash)),
        Ok(Some(None)) => Err("the server only accepts SHA-1 RSA signatures (ssh-rsa), which are not used".to_string()),
        // The server did not say (no server-sig-algs): every OpenSSH since 7.2 takes SHA-256.
        Ok(None) | Err(_) => Ok(Some(HashAlg::Sha256)),
    }
}

async fn authenticate(handle: &mut Handle<ClientHandler>, resolved: &Resolved, target: &SshTarget) -> Result<(), BackendError> {
    let mut methods: Vec<AuthKind> = target.auth.iter().map(|a| a.0.clone()).collect();
    methods.extend(resolved.identity_files.iter().map(|path| AuthKind::KeyFile { path: path.clone(), passphrase: None }));
    if methods.is_empty() {
        return Err(BackendError::AuthFailed(
            "no authentication method (SshAuth::key_file, SshAuth::agent, SshAuth::password, SshAuth::keyboard_interactive, or IdentityFile in ssh_config)"
                .into(),
        ));
    }
    let user = resolved.user.as_str();
    let mut tried: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for method in methods {
        match method {
            AuthKind::KeyFile { path, passphrase } => {
                let label = format!("key file `{}`", file_name(&path));
                let key = match load_key(path, passphrase).await {
                    Ok(key) => key,
                    Err(why) => {
                        problems.push(format!("{label} {why}"));
                        continue;
                    }
                };
                let hash = if key.algorithm().is_rsa() {
                    if !cfg!(feature = "ssh-rsa") {
                        problems.push(format!("{label}: RSA keys need the `ssh-rsa` feature"));
                        continue;
                    }
                    match rsa_hash(handle).await {
                        Ok(hash) => hash,
                        Err(why) => {
                            problems.push(format!("{label}: {why}"));
                            continue;
                        }
                    }
                } else {
                    None
                };
                tried.push(label);
                if handle.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(map_russh)?.success() {
                    return Ok(());
                }
            }
            AuthKind::Agent => {
                let mut agent = match connect_agent().await {
                    Ok(agent) => agent,
                    Err(why) => {
                        problems.push(format!("agent: {why}"));
                        continue;
                    }
                };
                let identities = match agent.request_identities().await {
                    Ok(identities) => identities,
                    Err(e) => {
                        problems.push(format!("agent: could not list its keys ({e})"));
                        continue;
                    }
                };
                let mut offered = 0usize;
                for identity in identities {
                    let AgentIdentity::PublicKey { key, .. } = identity else { continue };
                    let hash = if key.algorithm().is_rsa() {
                        if !cfg!(feature = "ssh-rsa") {
                            continue;
                        }
                        match rsa_hash(handle).await {
                            Ok(hash) => hash,
                            Err(_) => continue,
                        }
                    } else {
                        None
                    };
                    offered = offered.saturating_add(1);
                    match handle.authenticate_publickey_with(user, key, hash, &mut agent).await {
                        Ok(result) if result.success() => return Ok(()),
                        Ok(_) => {}
                        Err(e) => problems.push(format!("agent: signing failed ({e})")),
                    }
                }
                tried.push(format!("agent ({offered} key(s))"));
            }
            AuthKind::Password(password) => {
                tried.push("password".into());
                if handle.authenticate_password(user, password.expose()).await.map_err(map_russh)?.success() {
                    return Ok(());
                }
            }
            AuthKind::KeyboardInteractive(responder) => {
                tried.push("keyboard-interactive".into());
                let mut reply = handle.authenticate_keyboard_interactive_start(user, None::<String>).await.map_err(map_russh)?;
                let mut rounds = 0usize;
                loop {
                    match reply {
                        KeyboardInteractiveAuthResponse::Success => return Ok(()),
                        KeyboardInteractiveAuthResponse::Failure { .. } => break,
                        KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                            rounds = rounds.saturating_add(1);
                            if rounds > MAX_PROMPT_ROUNDS {
                                problems.push(format!("keyboard-interactive: more than {MAX_PROMPT_ROUNDS} rounds of prompts"));
                                break;
                            }
                            let request = SshPromptRequest {
                                name: clean(&name),
                                instructions: clean(&instructions),
                                prompts: prompts.into_iter().map(|p| SshPrompt { text: clean(&p.prompt), echo: p.echo }).collect(),
                            };
                            let wanted = request.prompts.len();
                            let answers = match responder.respond(&request) {
                                Some(answers) if answers.len() == wanted => answers,
                                Some(_) => {
                                    problems.push("keyboard-interactive: the responder gave a wrong number of answers".into());
                                    break;
                                }
                                None => {
                                    problems.push(format!("keyboard-interactive: no answer for the server's {wanted} prompt(s)"));
                                    break;
                                }
                            };
                            // russh takes plain strings; they live only for this call.
                            let answers: Vec<String> = answers.iter().map(|a| a.expose().to_string()).collect();
                            reply = handle.authenticate_keyboard_interactive_respond(answers).await.map_err(map_russh)?;
                        }
                    }
                }
            }
        }
    }
    let mut why = if tried.is_empty() { "no method could be offered".to_string() } else { format!("the server accepted none of: {}", tried.join(", ")) };
    if !problems.is_empty() {
        why.push_str("; ");
        why.push_str(&problems.join("; "));
    }
    Err(BackendError::AuthFailed(why))
}

/// A running request of a connection.
struct Running {
    cancel: Option<oneshot::Sender<()>>,
    abort: AbortHandle,
}

/// Everything a request task needs.
#[derive(Clone)]
struct Ctx {
    handle: Arc<Handle<ClientHandler>>,
    events: EventSink,
    stopping: Arc<AtomicBool>,
    channels: Arc<Semaphore>,
    done: tmpsc::UnboundedSender<RequestId>,
    #[cfg(feature = "sftp")]
    sftp: Arc<tokio::sync::Mutex<Option<Arc<russh_sftp::client::RawSftpSession>>>>,
    #[cfg(feature = "sftp")]
    sftp_timeout: Duration,
    #[cfg(feature = "sftp")]
    max_transfer: u64,
}

/// Reports a request as done to its connection when dropped (every way out of a task).
struct DoneGuard(tmpsc::UnboundedSender<RequestId>, RequestId);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}

async fn wait_for_close(commands: &mut tmpsc::UnboundedReceiver<ConnCommand>) {
    loop {
        match commands.recv().await {
            None | Some(ConnCommand::Close) => return,
            Some(_) => {}
        }
    }
}

async fn connection(conn: SshConnId, target: SshTarget, mut commands: tmpsc::UnboundedReceiver<ConnCommand>, events: EventSink, stopping: Arc<AtomicBool>) {
    let connect_timeout = target.connect_timeout;
    let deadline = Instant::now().checked_add(connect_timeout).unwrap_or_else(Instant::now);
    // Dropping `_kill` (this task ends) kills the socket under the russh session.
    let (_kill, kill) = oneshot::channel::<()>();
    let lost = Arc::new(Lost::default());
    let established = tokio::select! {
        biased;
        () = wait_for_close(&mut commands) => return,
        result = tokio::time::timeout_at(deadline, establish(&target, kill, Arc::clone(&lost))) => result,
    };
    let Established { handle, fingerprint } = match established {
        Err(_) => {
            let why = format!("not connected within {connect_timeout:?} (TCP connect, key exchange, host key check and authentication together)");
            events.send(SshEvent::Closed { conn, error: Some(BackendError::Timeout(why)) });
            return;
        }
        Ok(Err(error)) => {
            events.send(SshEvent::Closed { conn, error: Some(error) });
            return;
        }
        Ok(Ok(established)) => established,
    };
    if stopping.load(Ordering::SeqCst) {
        return;
    }
    events.send(SshEvent::Connected { conn, fingerprint });
    let (done, mut finished) = tmpsc::unbounded_channel::<RequestId>();
    let ctx = Ctx {
        handle: Arc::new(handle),
        events: events.clone(),
        stopping: Arc::clone(&stopping),
        channels: Arc::new(Semaphore::new(target.max_channels)),
        done,
        #[cfg(feature = "sftp")]
        sftp: Arc::new(tokio::sync::Mutex::new(None)),
        #[cfg(feature = "sftp")]
        sftp_timeout: target.sftp_timeout,
        #[cfg(feature = "sftp")]
        max_transfer: target.max_transfer_bytes,
    };
    let mut running: HashMap<RequestId, Running> = HashMap::new();
    let mut watchdog = tokio::time::interval(WATCHDOG);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let lost_reason = loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                None | Some(ConnCommand::Close) => break None,
                Some(ConnCommand::Run(id, command)) => {
                    let (cancel, cancelled) = oneshot::channel();
                    let sink = events.clone();
                    let (abort, _) = spawn_guarded(exec(ctx.clone(), id, command, cancelled), move |panic| {
                        sink.send(SshEvent::Finished { id, result: Err(BackendError::Network(format!("the SSH command task panicked: {panic}"))) });
                    });
                    running.insert(id, Running { cancel: Some(cancel), abort });
                }
                #[cfg(feature = "sftp")]
                Some(ConnCommand::Sftp(id, op)) => {
                    let (cancel, cancelled) = oneshot::channel();
                    let sink = events.clone();
                    let (abort, _) = spawn_guarded(sftp(ctx.clone(), id, op, cancelled), move |panic| {
                        sink.send(SshEvent::SftpFinished { id, result: Err(BackendError::Network(format!("the SFTP task panicked: {panic}"))) });
                    });
                    running.insert(id, Running { cancel: Some(cancel), abort });
                }
                Some(ConnCommand::Cancel(id)) => {
                    if let Some(cancel) = running.get_mut(&id).and_then(|r| r.cancel.take()) {
                        let _ = cancel.send(());
                    }
                }
            },
            Some(id) = finished.recv() => {
                running.remove(&id);
            }
            reason = lost.wait() => break Some(reason),
            // Backstop: russh can end its session without calling `disconnected` (when shutting
            // the socket down fails after a reset).
            _ = watchdog.tick() => {
                if ctx.handle.is_closed() {
                    break Some("the connection was lost (the SSH session ended)".to_string());
                }
            }
        }
    };
    // Stop what still runs: a cancel lets each task send TERM and close its channel; after a short
    // grace the rest is aborted.
    for run in running.values_mut() {
        if let Some(cancel) = run.cancel.take() {
            let _ = cancel.send(());
        }
    }
    let _ = tokio::time::timeout(GOODBYE, async {
        while !running.is_empty() {
            match finished.recv().await {
                Some(id) => {
                    running.remove(&id);
                }
                None => break,
            }
        }
    })
    .await;
    for run in running.values() {
        run.abort.abort();
    }
    match lost_reason {
        None => {
            let _ = tokio::time::timeout(GOODBYE, ctx.handle.disconnect(Disconnect::ByApplication, "", "en")).await;
            events.send(SshEvent::Closed { conn, error: None });
        }
        Some(reason) => events.send(SshEvent::Closed { conn, error: Some(BackendError::disconnected(reason, None)) }),
    }
}

// ---------------------------------------------------------------------------------------------
// Requests.

enum Race<T> {
    Done(T),
    TimedOut,
    Cancelled,
}

/// Run `future` until it finishes, the deadline passes, or the request is cancelled (or its
/// connection ends). After `Cancelled`, `cancel` must not be polled again.
async fn race<F: Future>(future: F, deadline: Instant, cancel: &mut oneshot::Receiver<()>) -> Race<F::Output> {
    tokio::select! {
        biased;
        _ = cancel => Race::Cancelled,
        () = tokio::time::sleep_until(deadline) => Race::TimedOut,
        output = future => Race::Done(output),
    }
}

/// Open a session channel under the request's deadline without leaking it: when the request is
/// cancelled or times out while the server is still opening it, a small task waits (bounded) for
/// the channel and closes it, so the server's channel slots (`MaxSessions`) are not used up.
async fn open_channel(ctx: &Ctx, deadline: Instant, cancel: &mut oneshot::Receiver<()>) -> Race<Result<russh::Channel<client::Msg>, russh::Error>> {
    let handle = Arc::clone(&ctx.handle);
    let mut opening = tokio::spawn(async move { handle.channel_open_session().await });
    let outcome = race(&mut opening, deadline, cancel).await;
    match outcome {
        Race::Done(Ok(result)) => Race::Done(result),
        Race::Done(Err(join)) => Race::Done(Err(russh::Error::IO(io::Error::other(join.to_string())))),
        other => {
            tokio::spawn(async move {
                if let Ok(Ok(Ok(channel))) = tokio::time::timeout(GOODBYE.saturating_mul(5), opening).await {
                    let _ = tokio::time::timeout(GOODBYE, channel.close()).await;
                }
            });
            match other {
                Race::TimedOut => Race::TimedOut,
                _ => Race::Cancelled,
            }
        }
    }
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout.min(crate::config::MAX_TIMEOUT)).unwrap_or(now)
}

/// Best effort: ask the remote process to stop and close the channel (bounded).
async fn stop_channel(channel: &russh::Channel<client::Msg>) {
    let _ = tokio::time::timeout(GOODBYE, async {
        let _ = channel.signal(Sig::TERM).await;
        let _ = channel.eof().await;
        let _ = channel.close().await;
    })
    .await;
}

fn signal_name(signal: &Sig) -> String {
    match signal {
        Sig::Custom(name) => clean(name),
        other => format!("{other:?}"),
    }
}

async fn exec(ctx: Ctx, id: RequestId, command: SshCommand, mut cancel: oneshot::Receiver<()>) {
    let _done = DoneGuard(ctx.done.clone(), id);
    let timeout = command.timeout.unwrap_or(DEFAULT_SSH_COMMAND_TIMEOUT);
    let limit = command.max_output_bytes.unwrap_or(DEFAULT_SSH_MAX_OUTPUT_BYTES);
    let deadline = deadline_after(timeout);
    let finish = |result| ctx.events.send(SshEvent::Finished { id, result });
    // 1. A free channel slot, then a channel: nothing is sent to the shell yet.
    let _permit = match race(Arc::clone(&ctx.channels).acquire_owned(), deadline, &mut cancel).await {
        Race::Done(Ok(permit)) => permit,
        Race::Done(Err(_)) => return finish(Err(BackendError::disconnected("the connection is closing", Some(false)))),
        Race::TimedOut => return finish(Err(BackendError::Timeout(format!("not sent: no free channel on this connection within {timeout:?}")))),
        Race::Cancelled => return,
    };
    let mut channel = match open_channel(&ctx, deadline, &mut cancel).await {
        Race::Done(Ok(channel)) => channel,
        Race::Done(Err(e)) => return finish(Err(BackendError::Ssh(format!("could not open a channel: {e}")))),
        Race::TimedOut => return finish(Err(BackendError::Timeout(format!("not sent: the server did not open a channel within {timeout:?}")))),
        Race::Cancelled => return,
    };
    if ctx.stopping.load(Ordering::SeqCst) {
        // The app is exiting: already answered `Shutdown`, never sent; give the channel back.
        let _ = tokio::time::timeout(GOODBYE, channel.close()).await;
        return;
    }
    // 2. The exec request.
    match race(channel.exec(true, command.command.as_bytes()), deadline, &mut cancel).await {
        Race::Done(Ok(())) => {}
        Race::Done(Err(e)) => return finish(Err(BackendError::Ssh(format!("could not send the command: {e}")))),
        Race::TimedOut => {
            stop_channel(&channel).await;
            return finish(Err(BackendError::Timeout(format!("the exec request could not be written within {timeout:?}; the command may have started"))));
        }
        Race::Cancelled => {
            stop_channel(&channel).await;
            return;
        }
    }
    // 3. Its answer, the output, the exit.
    let mut started = false;
    let mut gone = false;
    let (mut stdout, mut stderr) = (0u64, 0u64);
    let (mut status, mut signal) = (None, None);
    let mut stdin = command.stdin;
    loop {
        let message = match race(channel.wait(), deadline, &mut cancel).await {
            Race::Done(message) => message,
            Race::TimedOut => {
                stop_channel(&channel).await;
                let why = if started {
                    format!("the command ran longer than {timeout:?}; its channel was closed after a TERM signal (the remote process may keep running)")
                } else {
                    format!("the server did not answer the exec request within {timeout:?}; the command may have started")
                };
                return finish(Err(BackendError::Timeout(why)));
            }
            Race::Cancelled => {
                stop_channel(&channel).await;
                return;
            }
        };
        // The exec reply, or (from a server that skips it) the first output or exit: it runs.
        let runs = matches!(
            message,
            Some(
                ChannelMsg::Success | ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. } | ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. }
            )
        );
        if runs && !started {
            started = true;
            ctx.events.send(SshEvent::Started { id });
            // stdin, then end-of-file (a command reading stdin must not wait forever).
            let input = stdin.take();
            let write = async {
                if let Some(input) = input {
                    channel.data(input.as_slice()).await?;
                }
                channel.eof().await
            };
            match race(write, deadline, &mut cancel).await {
                Race::Done(_) => {}
                Race::TimedOut => {
                    stop_channel(&channel).await;
                    return finish(Err(BackendError::Timeout(format!("stdin could not be written within {timeout:?}; the command was started"))));
                }
                Race::Cancelled => {
                    stop_channel(&channel).await;
                    return;
                }
            }
        }
        match message {
            Some(ChannelMsg::Failure) if !started => {
                stop_channel(&channel).await;
                return finish(Err(BackendError::Ssh("the server refused to run the command".into())));
            }
            Some(ChannelMsg::Data { data }) => {
                stdout = stdout.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if stdout.saturating_add(stderr) > limit {
                    stop_channel(&channel).await;
                    return finish(Err(BackendError::BodyTooLarge { limit }));
                }
                ctx.events.send(SshEvent::Output { id, stream: SshStream::Stdout, data: data.to_vec() });
            }
            Some(ChannelMsg::ExtendedData { data, ext }) => {
                stderr = stderr.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                if stdout.saturating_add(stderr) > limit {
                    stop_channel(&channel).await;
                    return finish(Err(BackendError::BodyTooLarge { limit }));
                }
                if ext == 1 {
                    ctx.events.send(SshEvent::Output { id, stream: SshStream::Stderr, data: data.to_vec() });
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name: sig, .. }) => signal = Some(signal_name(&sig)),
            Some(ChannelMsg::Close) => break,
            // The channel's sender is gone: the session ended under it.
            None => {
                gone = true;
                break;
            }
            Some(_) => {}
        }
    }
    let _ = tokio::time::timeout(GOODBYE, channel.close()).await;
    if status.is_none() && signal.is_none() && (gone || ctx.handle.is_closed()) {
        return finish(Err(BackendError::disconnected("the connection was lost before the command ended", Some(true))));
    }
    finish(Ok(SshExit { status, signal, stdout_bytes: stdout, stderr_bytes: stderr }));
}

#[cfg(feature = "sftp")]
struct Reporter<'a> {
    id: RequestId,
    events: &'a EventSink,
    started: bool,
}

#[cfg(feature = "sftp")]
impl sftp_ops::Reporter for Reporter<'_> {
    fn started(&mut self) {
        if !self.started {
            self.started = true;
            self.events.send(SshEvent::Started { id: self.id });
        }
    }

    fn progress(&mut self, done: u64, total: Option<u64>) {
        self.events.send(SshEvent::Progress { id: self.id, done, total });
    }
}

/// The connection's SFTP session, opened on first use (a channel + the `sftp` subsystem).
#[cfg(feature = "sftp")]
async fn sftp_session(ctx: &Ctx) -> Result<Arc<russh_sftp::client::RawSftpSession>, BackendError> {
    let mut slot = ctx.sftp.lock().await;
    if let Some(session) = slot.as_ref() {
        return Ok(Arc::clone(session));
    }
    let mut channel = ctx.handle.channel_open_session().await.map_err(|e| {
        if ctx.handle.is_closed() {
            // Nothing of the operation went out.
            BackendError::disconnected(format!("the connection is gone: could not open a channel for SFTP ({e})"), Some(false))
        } else {
            BackendError::Ssh(format!("could not open a channel for SFTP: {e}"))
        }
    })?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| BackendError::Ssh(format!("could not request the SFTP subsystem: {e}")))?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) => return Err(BackendError::Ssh("the server refused the SFTP subsystem".into())),
            Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err(BackendError::Ssh("the SFTP channel closed before it started".into())),
            Some(_) => {}
        }
    }
    // Each SFTP request may wait as long as a whole operation (`with_sftp_timeout`): with 16 reads
    // in flight the last one is answered only after 1 MiB crossed the link, so a fixed 30 s would
    // fail links slower than about 35 KB/s. The operation's own deadline still bounds it.
    let request_timeout_secs = ctx.sftp_timeout.as_secs().max(1);
    let config = russh_sftp::client::Config { request_timeout_secs, ..russh_sftp::client::Config::default() };
    let session = russh_sftp::client::RawSftpSession::new_with_config(channel.into_stream(), config);
    session.init().await.map_err(sftp_ops::map_error)?;
    let session = Arc::new(session);
    *slot = Some(Arc::clone(&session));
    Ok(session)
}

#[cfg(feature = "sftp")]
async fn sftp(ctx: Ctx, id: RequestId, op: SftpOp, mut cancel: oneshot::Receiver<()>) {
    let _done = DoneGuard(ctx.done.clone(), id);
    let timeout = ctx.sftp_timeout;
    let deadline = deadline_after(timeout);
    let finish = |result| ctx.events.send(SshEvent::SftpFinished { id, result });
    let _permit = match race(Arc::clone(&ctx.channels).acquire_owned(), deadline, &mut cancel).await {
        Race::Done(Ok(permit)) => permit,
        Race::Done(Err(_)) => return finish(Err(BackendError::disconnected("the connection is closing", Some(false)))),
        Race::TimedOut => return finish(Err(BackendError::Timeout(format!("not sent: no free channel on this connection within {timeout:?}")))),
        Race::Cancelled => return,
    };
    let session = match race(sftp_session(&ctx), deadline, &mut cancel).await {
        Race::Done(Ok(session)) => session,
        // The operation itself never went out.
        Race::Done(Err(BackendError::Disconnected { reason, .. })) => return finish(Err(BackendError::disconnected(reason, Some(false)))),
        Race::Done(Err(error)) => return finish(Err(error)),
        Race::TimedOut => return finish(Err(BackendError::Timeout(format!("not sent: the SFTP subsystem did not start within {timeout:?}")))),
        Race::Cancelled => return,
    };
    if ctx.stopping.load(Ordering::SeqCst) {
        return;
    }
    // A download to a file goes through a part file with a unique name next to it.
    let part = match &op {
        SftpOp::DownloadFile { local, .. } => Some(sftp_ops::part_path(local, id)),
        _ => None,
    };
    let mut reporter = Reporter { id, events: &ctx.events, started: false };
    let outcome = race(sftp_ops::run(&session, op, ctx.max_transfer, part.clone(), &mut reporter), deadline, &mut cancel).await;
    let started = reporter.started;
    let cleanup = || {
        if let Some(part) = part.clone() {
            // On the blocking pool, like every local file operation.
            drop(tokio::task::spawn_blocking(move || std::fs::remove_file(part)));
        }
    };
    match outcome {
        Race::Done(Err(BackendError::Disconnected { reason, .. })) => {
            // The SFTP channel died (with its connection, or alone): the next operation opens a
            // new one. Whether the operation went out is known here: `started` is set just before
            // its first request.
            *ctx.sftp.lock().await = None;
            let reason = if ctx.handle.is_closed() { format!("the connection was lost during the SFTP operation: {reason}") } else { reason };
            finish(Err(BackendError::disconnected(reason, Some(started))));
        }
        Race::Done(result) => finish(result),
        Race::TimedOut => {
            cleanup();
            let why = if started {
                format!("the SFTP operation took longer than {timeout:?} (an interrupted upload may leave a partial file)")
            } else {
                format!("not sent: the SFTP operation could not start within {timeout:?}")
            };
            finish(Err(BackendError::Timeout(why)));
        }
        Race::Cancelled => cleanup(),
    }
}

#[cfg(test)]
mod tests {
    use russh::keys::Algorithm;

    use super::{client_config, terrapin_refusal};
    use crate::ssh::{SshAuth, SshTarget};

    #[test]
    fn terrapin_exposed_combinations_need_strict_kex() {
        let plain = ["hmac-sha2-256", "hmac-sha2-256"];
        let etm = ["hmac-sha2-256-etm@openssh.com", "hmac-sha2-256-etm@openssh.com"];
        assert!(terrapin_refusal(false, "chacha20-poly1305@openssh.com", plain).is_some());
        assert!(terrapin_refusal(false, "aes256-cbc", etm).is_some());
        assert!(terrapin_refusal(false, "aes256-cbc", plain).is_none());
        assert!(terrapin_refusal(false, "aes256-ctr", etm).is_none(), "CTR-EtM is not practically exploitable");
        assert!(terrapin_refusal(false, "aes256-gcm@openssh.com", etm).is_none());
        assert!(terrapin_refusal(true, "chacha20-poly1305@openssh.com", etm).is_none());
        let why = terrapin_refusal(false, "chacha20-poly1305@openssh.com", plain).map(|e| e.to_string()).unwrap_or_default();
        assert!(why.contains("Terrapin") && why.contains("allow_terrapin_vulnerable"), "{why}");
    }

    #[test]
    fn aes_gcm_is_preferred_and_known_key_types_come_first() {
        let target = SshTarget::new("h", "u").with_auth(SshAuth::agent());
        let config = client_config(&target, &[]);
        assert_eq!(config.preferred.cipher.first().map(AsRef::as_ref), Some("aes256-gcm@openssh.com"));
        assert_eq!(config.preferred.key.first(), Some(&Algorithm::Ed25519));
        let p256 = Algorithm::Ecdsa { curve: russh::keys::EcdsaCurve::NistP256 };
        let config = client_config(&target, std::slice::from_ref(&p256));
        assert_eq!(config.preferred.key.first(), Some(&p256));
        assert!(config.preferred.key.contains(&Algorithm::Ed25519), "the other types are still offered");
    }

    #[test]
    fn the_client_offers_no_sha1_rsa_and_rsa_only_with_the_feature() {
        let config = client_config(&SshTarget::new("h", "u").with_auth(SshAuth::agent()), &[]);
        let keys = config.preferred.key.to_vec();
        assert!(!keys.contains(&Algorithm::Rsa { hash: None }));
        assert_eq!(keys.iter().any(|k| matches!(k, Algorithm::Rsa { .. })), cfg!(feature = "ssh-rsa"));
        assert!(keys.contains(&Algorithm::Ed25519));
        assert!(config.preferred.kex.iter().any(|k| k.as_ref() == "kex-strict-c-v00@openssh.com"), "strict key exchange is offered");
        assert_eq!(config.inactivity_timeout, None);
        assert!(config.keepalive_interval.is_some());
    }
}
