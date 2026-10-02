//! Named SSH connections that run commands and (feature `sftp`) move files (feature `ssh`).
//!
//! **ADMIN / DEV BUILDS ONLY.** An SSH key (or agent access) inside a build you give to players
//! is shell access for anyone who extracts it. The plugin refuses every SSH request in a release
//! build (`cfg(not(debug_assertions))`) unless the game opts in with
//! [`SshSettings::allow_in_release`].
//!
//! A game opens a connection by name with [`SshClient::connect`] (host, user, how to
//! authenticate, where the known host keys are), then runs commands on it by name with
//! [`SshClient::run`]. Each command streams [`SshOutput`] messages (stdout / stderr chunks) and
//! gets exactly one [`SshFinished`] (the exit status, or the error). [`SshConnections`] holds each
//! connection's state; [`SshStateChanged`] reports every change.
//!
//! The work runs on ONE private thread with a tokio current-thread runtime (russh needs tokio),
//! started on the first connect and shut down on `AppExit`; nothing runs on the game's threads or
//! on Bevy's task pools. The server's host key is always checked against known_hosts (or a
//! fingerprint pinned in code): an unknown or changed key is an error, never accepted silently.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bevy_app::{App, First, Last, PostUpdate};
use bevy_ecs::message::Message;
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::common_conditions::on_message;
use bevy_ecs::schedule::IntoScheduleConfigs;

use crate::credentials::Secret;
use crate::request::RequestId;
use crate::response::BackendError;
use crate::BackendSystems;

mod known_hosts;
mod russh_client;
#[cfg(feature = "sftp")]
mod sftp;
mod ssh_config;
mod systems;
mod transport;

pub use russh_client::RusshTransport;
#[cfg(feature = "sftp")]
pub use sftp::{SftpEntry, SftpEntryKind, SftpFinished, SftpOp, SftpOutcome, SftpProgress};
pub use transport::{FakeSshTransport, SshConnId, SshEvent, SshTransport, SshTransportRes};

/// Default limit for connect + key exchange + host key check + authentication together.
pub const DEFAULT_SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default time a command may run.
pub const DEFAULT_SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Default limit of a command's output (stdout + stderr): 8 MiB.
pub const DEFAULT_SSH_MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Default time an SFTP operation (a whole transfer) may take.
pub const DEFAULT_SFTP_TIMEOUT: Duration = Duration::from_secs(300);
/// Default limit of one SFTP transfer (upload or download): 256 MiB.
pub const DEFAULT_SFTP_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The longest command line accepted: 64 KiB.
pub const MAX_SSH_COMMAND_BYTES: usize = 64 * 1024;

/// The name of an SSH connection (`"main"`, `"build-box"`, …). Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SshName(Arc<str>);

impl SshName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SshName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SshName({:?})", &*self.0)
    }
}

impl fmt::Display for SshName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SshName {
    fn from(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl From<String> for SshName {
    fn from(name: String) -> Self {
        Self(Arc::from(name))
    }
}

impl From<&String> for SshName {
    fn from(name: &String) -> Self {
        Self(Arc::from(name.as_str()))
    }
}

impl From<&SshName> for SshName {
    fn from(name: &SshName) -> Self {
        name.clone()
    }
}

impl PartialEq<str> for SshName {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for SshName {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

/// App-wide SSH settings, given to the plugin with
/// [`BackendPlugin::with_ssh`](crate::BackendPlugin::with_ssh). Private fields + builder.
///
/// ```
/// use bevy_net_backend::{BackendPlugin, SshSettings};
///
/// // An internal admin tool that is also built in release mode.
/// let plugin = BackendPlugin::default().with_ssh(SshSettings::default().allow_in_release(true));
/// # let _ = plugin;
/// ```
#[derive(Clone, Debug)]
pub struct SshSettings {
    pub(crate) allow_in_release: bool,
    pub(crate) max_connections: usize,
    pub(crate) max_requests: usize,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self { allow_in_release: false, max_connections: 16, max_requests: 256 }
    }
}

impl SshSettings {
    /// Allow SSH in release builds (default `false`: every SSH request of a release build is
    /// answered with `InvalidRequest` and nothing connects). Only for internal admin / dev tools:
    /// an SSH key in a build that reaches players is shell access for anyone who extracts it.
    pub fn allow_in_release(mut self, allow: bool) -> Self {
        self.allow_in_release = allow;
        self
    }

    /// How many connections may be open (connecting, connected or reconnecting) at once (default
    /// 16, at least 1). `Disconnected` names do not count. A `connect` beyond it is refused: the
    /// name's entry is `Disconnected` with the error.
    pub fn with_max_connections(mut self, connections: usize) -> Self {
        self.max_connections = connections.max(1);
        self
    }

    /// How many requests (commands and SFTP operations) may wait or run per connection at once
    /// (default 256, at least 1); more are answered `InvalidRequest` at once, never sent.
    pub fn with_max_requests_per_connection(mut self, requests: usize) -> Self {
        self.max_requests = requests.max(1);
        self
    }

    /// Whether SSH may run in this build: always in debug builds, in release builds only with
    /// [`allow_in_release`](Self::allow_in_release).
    pub fn is_allowed(&self) -> bool {
        allowed(cfg!(debug_assertions), self.allow_in_release)
    }
}

/// The release-build guard.
pub(crate) fn allowed(debug_build: bool, allow_in_release: bool) -> bool {
    debug_build || allow_in_release
}

/// How to log in. Several can be given ([`SshTarget::with_auth`]); they are tried in order.
/// **Keys are the recommendation** (a key file or the agent); a password and keyboard-interactive
/// (e.g. password + a 2FA code) are opt-in for servers that need them, with values typed by the
/// admin at runtime. There is deliberately no way to pass key bytes: keys are loaded at runtime
/// from the admin's machine, never baked into a binary. `Debug` shows the key file's name, never
/// its path, contents, passphrase or password.
#[derive(Clone)]
pub struct SshAuth(pub(crate) AuthKind);

#[derive(Clone)]
pub(crate) enum AuthKind {
    KeyFile { path: PathBuf, passphrase: Option<Secret> },
    Agent,
    Password(Secret),
    KeyboardInteractive(Arc<dyn SshPromptResponder>),
}

impl SshAuth {
    /// A private key file without a passphrase (OpenSSH, PKCS#8 or PuTTY format; ed25519 or
    /// ECDSA, RSA with feature `ssh-rsa`). Read when connecting (at most 256 KiB).
    pub fn key_file(path: impl Into<PathBuf>) -> Self {
        Self(AuthKind::KeyFile { path: path.into(), passphrase: None })
    }

    /// An encrypted private key file and its passphrase (typed by the admin at runtime; kept in
    /// a redacted [`Secret`]).
    pub fn key_file_with_passphrase(path: impl Into<PathBuf>, passphrase: impl Into<Secret>) -> Self {
        Self(AuthKind::KeyFile { path: path.into(), passphrase: Some(passphrase.into()) })
    }

    /// The running SSH agent: `SSH_AUTH_SOCK` on Unix; on Windows the OpenSSH agent's named pipe
    /// (`\\.\pipe\openssh-ssh-agent`), then Pageant. Each key the agent offers is tried (RSA keys
    /// only with feature `ssh-rsa`; certificates are skipped).
    pub fn agent() -> Self {
        Self(AuthKind::Agent)
    }

    /// Opt-in: a password (SSH `password` method), typed by the admin at runtime and held in a
    /// redacted [`Secret`]. Prefer a key: a password can be guessed and is sent to the server
    /// (encrypted, after the host key was verified).
    pub fn password(password: impl Into<Secret>) -> Self {
        Self(AuthKind::Password(password.into()))
    }

    /// Opt-in: keyboard-interactive (the server asks questions, e.g. `Password:` then
    /// `Verification code:` for 2FA). `responder` answers each round of prompts; it runs on the
    /// SSH thread and must not block (collect the answers from the admin first, e.g. with
    /// [`SshPromptAnswers`]). At most 8 rounds.
    pub fn keyboard_interactive(responder: impl SshPromptResponder) -> Self {
        Self(AuthKind::KeyboardInteractive(Arc::new(responder)))
    }
}

/// One prompt of a keyboard-interactive round. The texts come from the server (untrusted; control
/// characters removed). `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshPrompt {
    /// The question, e.g. `Password:` or `Verification code:`.
    pub text: String,
    /// Whether the answer may be shown while typed (`false` for secrets).
    pub echo: bool,
}

/// One keyboard-interactive round: the server's name and instructions and its prompts (all
/// server-supplied, untrusted). `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshPromptRequest {
    /// The round's name (often empty).
    pub name: String,
    /// Instructions (often empty).
    pub instructions: String,
    /// The questions; answer each, in order.
    pub prompts: Vec<SshPrompt>,
}

impl SshPromptRequest {
    /// A request (for tests of a responder).
    pub fn new(prompts: Vec<SshPrompt>) -> Self {
        Self { name: String::new(), instructions: String::new(), prompts }
    }
}

impl SshPrompt {
    /// A prompt (for tests of a responder).
    pub fn new(text: impl Into<String>, echo: bool) -> Self {
        Self { text: text.into(), echo }
    }
}

/// Answers keyboard-interactive prompts ([`SshAuth::keyboard_interactive`]). Called on the SSH
/// thread: never block. Return one answer per prompt, or `None` to give up (the method then
/// fails). Methods are only ever added to it with a default implementation.
pub trait SshPromptResponder: Send + Sync + 'static {
    /// Answer one round.
    fn respond(&self, request: &SshPromptRequest) -> Option<Vec<Secret>>;
}

/// A ready-made [`SshPromptResponder`]: answers each prompt whose text contains a given word
/// (case-insensitive), e.g. `password` and `code`. A round with a prompt nothing matches gives up.
/// `Debug` shows the words, never the answers.
///
/// ```
/// use bevy_net_backend::{SshAuth, SshPromptAnswers};
///
/// // Both typed by the admin in the tool's UI before connecting.
/// let (password, code) = ("typed-password", "123456");
/// let auth = SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("password", password).answer_containing("code", code));
/// # let _ = auth;
/// ```
#[derive(Clone, Default)]
pub struct SshPromptAnswers {
    answers: Vec<(String, Secret)>,
}

impl fmt::Debug for SshPromptAnswers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshPromptAnswers").field("words", &self.answers.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>()).finish()
    }
}

impl SshPromptAnswers {
    /// An empty set of answers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer prompts containing `word` (case-insensitive) with `answer`. The first matching word
    /// wins.
    pub fn answer_containing(mut self, word: impl Into<String>, answer: impl Into<Secret>) -> Self {
        self.answers.push((word.into().to_lowercase(), answer.into()));
        self
    }
}

impl SshPromptResponder for SshPromptAnswers {
    fn respond(&self, request: &SshPromptRequest) -> Option<Vec<Secret>> {
        request
            .prompts
            .iter()
            .map(|prompt| {
                let text = prompt.text.to_lowercase();
                self.answers.iter().find(|(word, _)| text.contains(word.as_str())).map(|(_, answer)| answer.clone())
            })
            .collect()
    }
}

/// Automatic reconnect of an SSH connection ([`SshTarget::with_reconnect`]; OFF unless set):
/// exponential backoff with full jitter, as for WebSocket. A reconnect **never re-runs a
/// command**: commands that were running when the connection was lost are answered
/// `Disconnected` (with their honest `started`); commands that were never sent wait for the
/// new connection. Not retried: host key, authentication, protocol (`Ssh`) and invalid-settings
/// errors. The delay before attempt `n` is a random value in `0..=min(cap, base · 2^(n-1))`;
/// the counter resets after the connection stayed up for `stable_after`.
#[derive(Clone, Debug)]
pub struct SshReconnect {
    pub(crate) base: Duration,
    pub(crate) cap: Duration,
    pub(crate) max_attempts: Option<u32>,
    pub(crate) stable_after: Duration,
    pub(crate) jitter: bool,
}

impl Default for SshReconnect {
    fn default() -> Self {
        Self { base: Duration::from_secs(1), cap: Duration::from_secs(30), max_attempts: None, stable_after: Duration::from_secs(10), jitter: true }
    }
}

impl SshReconnect {
    /// The first delay bound (default 1 s, at least 1 ms, at most 1 h).
    pub fn with_base(mut self, base: Duration) -> Self {
        self.base = base.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// The largest delay (default 30 s, at most 1 h).
    pub fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap.min(crate::config::MAX_TIMEOUT);
        self
    }

    /// Give up after this many attempts in a row (`None` = never give up).
    pub fn with_max_attempts(mut self, max: Option<u32>) -> Self {
        self.max_attempts = max;
        self
    }

    /// How long a connection must stay up before the attempt counter resets (default 10 s).
    pub fn with_stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after.min(crate::config::MAX_TIMEOUT);
        self
    }

    /// Random jitter on (default) or off (exact delays).
    pub fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }

    /// The upper bound of the delay before attempt `attempt` (1-based).
    pub fn delay_bound(&self, attempt: u32) -> Duration {
        let factor = 2u32.checked_pow(attempt.saturating_sub(1).min(30)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap.max(self.base))
    }

    pub(crate) fn delay(&self, attempt: u32, random: u64) -> Duration {
        let bound = self.delay_bound(attempt);
        if !self.jitter {
            return bound;
        }
        let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(random % nanos.saturating_add(1))
    }

    pub(crate) fn may_retry(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

/// The last component of a path, for logs and errors (a full path can hold a user name).
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| "<no file name>".to_string(), |n| n.to_string_lossy().into_owned())
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            AuthKind::KeyFile { path, passphrase } => {
                f.debug_struct("KeyFile").field("file", &file_name(path)).field("passphrase", &passphrase.as_ref().map(|_| "<redacted>")).finish()
            }
            AuthKind::Agent => f.write_str("Agent"),
            AuthKind::Password(_) => f.write_str("Password(<redacted>)"),
            AuthKind::KeyboardInteractive(_) => f.write_str("KeyboardInteractive"),
        }
    }
}

/// Where the settings of a connection come from.
#[derive(Clone)]
pub(crate) enum TargetSource {
    Direct,
    /// A `Host` alias of an ssh_config file (`None` = `~/.ssh/config`), resolved on the SSH thread.
    Config(Option<PathBuf>),
}

/// One SSH server and how to reach it, given to [`SshClient::connect`]. Private fields + builder.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::{SshAuth, SshTarget};
///
/// let target = SshTarget::new("build.example.com", "deploy")
///     .with_port(2222)
///     .with_auth(SshAuth::agent())
///     .with_auth(SshAuth::key_file("/home/admin/.ssh/id_ed25519"))
///     .with_known_hosts_file("/home/admin/.ssh/known_hosts")
///     .with_command_timeout(Duration::from_secs(30));
/// assert!(target.validate().is_ok());
/// ```
///
/// Host keys: the server's key must be in a known_hosts file ([`with_known_hosts_file`](Self::with_known_hosts_file);
/// default `~/.ssh/known_hosts` when no file and no pinned fingerprint is given) or match a
/// fingerprint pinned with [`trust_host_key_fingerprint`](Self::trust_host_key_fingerprint).
/// Nothing is ever written to a known_hosts file.
#[derive(Clone)]
pub struct SshTarget {
    pub(crate) host: String,
    pub(crate) source: TargetSource,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) auth: Vec<SshAuth>,
    pub(crate) known_hosts: Vec<PathBuf>,
    pub(crate) pinned: Vec<String>,
    pub(crate) connect_timeout: Duration,
    pub(crate) keepalive_interval: Duration,
    pub(crate) keepalive_max: u32,
    pub(crate) command_timeout: Duration,
    pub(crate) max_output_bytes: u64,
    pub(crate) sftp_timeout: Duration,
    pub(crate) max_transfer_bytes: u64,
    pub(crate) max_channels: usize,
    pub(crate) reconnect: Option<SshReconnect>,
    pub(crate) allow_terrapin_vulnerable: bool,
}

impl fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("from_ssh_config", &matches!(self.source, TargetSource::Config(_)))
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("known_hosts_files", &self.known_hosts.len())
            .field("pinned_fingerprints", &self.pinned.len())
            .field("connect_timeout", &self.connect_timeout)
            .field("keepalive", &(self.keepalive_interval, self.keepalive_max))
            .field("command_timeout", &self.command_timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("sftp_timeout", &self.sftp_timeout)
            .field("max_transfer_bytes", &self.max_transfer_bytes)
            .field("max_channels", &self.max_channels)
            .field("reconnect", &self.reconnect)
            .field("allow_terrapin_vulnerable", &self.allow_terrapin_vulnerable)
            .finish()
    }
}

impl SshTarget {
    /// A server by host name (or IP address), port 22, logging in as `user`. Add at least one
    /// [`SshAuth`].
    pub fn new(host: impl Into<String>, user: impl Into<String>) -> Self {
        Self::blank(host.into(), TargetSource::Direct, Some(user.into()))
    }

    /// A `Host` alias of the user's `~/.ssh/config`: `HostName`, `Port`, `User`, `IdentityFile`
    /// (no passphrase) and `ConnectTimeout` are taken from it when connecting (on the SSH thread).
    /// Settings given here win over the file. `Match` blocks, `%` tokens and `ProxyJump` /
    /// `ProxyCommand` are ignored (the connection goes straight to the host). Without
    /// an `IdentityFile` and without [`with_auth`](Self::with_auth) the connection fails.
    pub fn from_ssh_config(alias: impl Into<String>) -> Self {
        Self::blank(alias.into(), TargetSource::Config(None), None)
    }

    /// Like [`from_ssh_config`](Self::from_ssh_config) with another config file.
    pub fn from_ssh_config_file(path: impl Into<PathBuf>, alias: impl Into<String>) -> Self {
        Self::blank(alias.into(), TargetSource::Config(Some(path.into())), None)
    }

    fn blank(host: String, source: TargetSource, user: Option<String>) -> Self {
        Self {
            host,
            source,
            port: None,
            user,
            auth: Vec::new(),
            known_hosts: Vec::new(),
            pinned: Vec::new(),
            connect_timeout: DEFAULT_SSH_CONNECT_TIMEOUT,
            keepalive_interval: Duration::from_secs(15),
            keepalive_max: 3,
            command_timeout: DEFAULT_SSH_COMMAND_TIMEOUT,
            max_output_bytes: DEFAULT_SSH_MAX_OUTPUT_BYTES,
            sftp_timeout: DEFAULT_SFTP_TIMEOUT,
            max_transfer_bytes: DEFAULT_SFTP_MAX_BYTES,
            max_channels: 8,
            reconnect: None,
            allow_terrapin_vulnerable: false,
        }
    }

    /// The port (default 22, or the config's `Port`).
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// The user to log in as (wins over the config's `User`).
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Add an authentication method; methods are tried in the order they were added (before any
    /// `IdentityFile` of the config).
    pub fn with_auth(mut self, auth: SshAuth) -> Self {
        self.auth.push(auth);
        self
    }

    /// Read host keys from this known_hosts file (call again for more files). Once a file or a
    /// pinned fingerprint is given, `~/.ssh/known_hosts` is no longer read. Read-only: nothing is
    /// ever added to it. OpenSSH format: host patterns (`*`, `?`, `!`), hashed hosts (`|1|…`),
    /// `[host]:port`, `@revoked` (`@cert-authority` lines are ignored: host certificates are not
    /// supported).
    pub fn with_known_hosts_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts.push(path.into());
        self
    }

    /// **Trust the server key with this fingerprint** (`SHA256:…` as `ssh-keygen -lf` prints it)
    /// without a known_hosts entry. Only pin a fingerprint you checked on the server itself (for
    /// example `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`): a wrong pin trusts an
    /// attacker. `@revoked` lines of the known_hosts files that are read still apply.
    pub fn trust_host_key_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.pinned.push(fingerprint.into().trim().to_string());
        self
    }

    /// One deadline for TCP connect + key exchange + host key check + authentication (default 15
    /// s, clamped to 1 s..=1 h). A server that trickles bytes cannot stretch it.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.clamp(Duration::from_secs(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// Keepalive: when nothing arrived for `interval` (default 15 s, 1 s..=1 h), ask the server
    /// for a sign of life; after `max_missed` (default 3, 1..=100) unanswered asks the connection
    /// counts as lost.
    pub fn with_keepalive(mut self, interval: Duration, max_missed: u32) -> Self {
        self.keepalive_interval = interval.clamp(Duration::from_secs(1), crate::config::MAX_TIMEOUT);
        self.keepalive_max = max_missed.clamp(1, 100);
        self
    }

    /// The default time a command may run (default 60 s, clamped to 1 ms..=1 h); a command can
    /// override it ([`SshCommand::with_timeout`]).
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// The default limit of a command's output, stdout + stderr (default 8 MiB, at least 1 KiB);
    /// at the limit the command is stopped and answered `BodyTooLarge`.
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes.max(1024);
        self
    }

    /// The time one SFTP operation (a whole transfer) may take (default 5 min, 1 ms..=1 h).
    pub fn with_sftp_timeout(mut self, timeout: Duration) -> Self {
        self.sftp_timeout = timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// The largest SFTP upload or download (default 256 MiB, at least 1 KiB).
    pub fn with_max_transfer_bytes(mut self, bytes: u64) -> Self {
        self.max_transfer_bytes = bytes.max(1024);
        self
    }

    /// How many commands may run on this connection at once (default 8, 1..=64); more wait for a
    /// free channel (their timeout keeps counting). OpenSSH allows 10 channels per connection by
    /// default (`MaxSessions`), and SFTP uses one more.
    pub fn with_max_channels(mut self, channels: usize) -> Self {
        self.max_channels = channels.clamp(1, 64);
        self
    }

    /// Reconnect automatically after a lost connection or a failed attempt (OFF by default; see
    /// [`SshReconnect`]: a command is never re-run).
    pub fn with_reconnect(mut self, reconnect: SshReconnect) -> Self {
        self.reconnect = Some(reconnect);
        self
    }

    /// **INSECURE, for old servers only: accept a Terrapin-vulnerable connection** (default
    /// `false`). Without it, a server that does not support strict key exchange (OpenSSH before
    /// 9.6 without the distribution's backport) and ends up with ChaCha20-Poly1305 or a CBC +
    /// encrypt-then-MAC cipher is refused (CVE-2023-48795: an attacker on the network path can
    /// silently drop messages at the start of the connection). AES-GCM is preferred and is not
    /// affected, so most such servers connect anyway; update the server instead of setting this.
    pub fn allow_terrapin_vulnerable(mut self, allow: bool) -> Self {
        self.allow_terrapin_vulnerable = allow;
        self
    }

    /// The host name (or the ssh_config alias).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port given here (the config's `Port`, else 22, is used when `None`).
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The user given here.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The connect timeout.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The default command timeout.
    pub fn command_timeout(&self) -> Duration {
        self.command_timeout
    }

    /// The default output limit of a command.
    pub fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes
    }

    /// The SFTP timeout.
    pub fn sftp_timeout(&self) -> Duration {
        self.sftp_timeout
    }

    /// The SFTP transfer limit.
    pub fn max_transfer_bytes(&self) -> u64 {
        self.max_transfer_bytes
    }

    /// Check what can be checked before connecting: host, user and port syntax, the pinned
    /// fingerprints' format, and that there is a way to authenticate (for an ssh_config alias the
    /// file is only read when connecting).
    pub fn validate(&self) -> Result<(), BackendError> {
        check_host(&self.host)?;
        if let Some(user) = &self.user {
            check_user(user)?;
        }
        if self.port == Some(0) {
            return Err(BackendError::InvalidRequest("the SSH port must not be 0".into()));
        }
        for pin in &self.pinned {
            check_fingerprint(pin)?;
        }
        match self.source {
            TargetSource::Direct if self.user.is_none() => Err(BackendError::InvalidRequest("no SSH user given".into())),
            TargetSource::Direct if self.auth.is_empty() => {
                Err(BackendError::InvalidRequest("no SSH authentication method given (SshAuth::key_file or SshAuth::agent)".into()))
            }
            _ => Ok(()),
        }
    }
}

/// A host name or address: no whitespace, control characters or leading `-`, 1..=255 bytes.
pub(crate) fn check_host(host: &str) -> Result<(), BackendError> {
    if host.is_empty() || host.len() > 255 || host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BackendError::InvalidRequest("the SSH host is not a valid host name or address".into()));
    }
    Ok(())
}

/// A user name: 1..=255 bytes, no whitespace or control characters.
pub(crate) fn check_user(user: &str) -> Result<(), BackendError> {
    if user.is_empty() || user.len() > 255 || user.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BackendError::InvalidRequest("the SSH user name is empty or has whitespace / control characters".into()));
    }
    Ok(())
}

/// `SHA256:` followed by the unpadded base64 of 32 bytes (43 characters).
pub(crate) fn check_fingerprint(pin: &str) -> Result<(), BackendError> {
    let ok = pin.strip_prefix("SHA256:").is_some_and(|b64| b64.len() == 43 && b64.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'));
    if ok {
        Ok(())
    } else {
        Err(BackendError::InvalidRequest(
            "a pinned host key fingerprint must look like `SHA256:` + 43 base64 characters (as `ssh-keygen -lf` prints it)".into(),
        ))
    }
}

/// A command to run ([`SshClient::run`]). The command line goes to the server's shell as it is:
/// quote arguments yourself. It may hold secrets (a token in an argument): its `Debug` shows only
/// its length, and the crate never logs it. Prefer passing secrets through
/// [`with_stdin`](Self::with_stdin); a command line is visible to other users of the server in
/// its process list.
#[derive(Clone)]
pub struct SshCommand {
    pub(crate) command: String,
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_output_bytes: Option<u64>,
    pub(crate) stdin: Option<Vec<u8>>,
}

impl fmt::Debug for SshCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshCommand")
            .field("command_bytes", &self.command.len())
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("stdin_bytes", &self.stdin.as_ref().map(Vec::len))
            .finish()
    }
}

impl SshCommand {
    /// A command line (non-empty, no NUL, at most [`MAX_SSH_COMMAND_BYTES`]).
    pub fn new(command: impl Into<String>) -> Self {
        Self { command: command.into(), timeout: None, max_output_bytes: None, stdin: None }
    }

    /// This command's time limit (instead of the connection's; clamped to 1 ms..=1 h). It counts
    /// from the moment the command is handed to the SSH thread, waiting for a free channel
    /// included.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.clamp(Duration::from_millis(1), crate::config::MAX_TIMEOUT));
        self
    }

    /// This command's output limit, stdout + stderr (instead of the connection's; at least 1 KiB).
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = Some(bytes.max(1024));
        self
    }

    /// Bytes written to the command's stdin, followed by end-of-file (without it, stdin is closed
    /// at once).
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// The command line. Do not log it.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The time limit (always set when a transport receives the command).
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The output limit (always set when a transport receives the command).
    pub fn max_output_bytes(&self) -> Option<u64> {
        self.max_output_bytes
    }

    /// The stdin bytes, if any.
    pub fn stdin(&self) -> Option<&[u8]> {
        self.stdin.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), BackendError> {
        if self.command.trim().is_empty() {
            return Err(BackendError::InvalidRequest("the SSH command is empty".into()));
        }
        if self.command.contains('\0') {
            return Err(BackendError::InvalidRequest("the SSH command contains a NUL byte".into()));
        }
        if self.command.len() > MAX_SSH_COMMAND_BYTES {
            return Err(BackendError::RequestTooLarge { limit: MAX_SSH_COMMAND_BYTES as u64, size: u64::try_from(self.command.len()).unwrap_or(u64::MAX) });
        }
        Ok(())
    }
}

impl From<&str> for SshCommand {
    fn from(command: &str) -> Self {
        Self::new(command)
    }
}

impl From<String> for SshCommand {
    fn from(command: String) -> Self {
        Self::new(command)
    }
}

/// How a command ended. A non-zero status is still `Ok`: the command ran (check
/// [`success`](Self::success)). `#[non_exhaustive]`: build one with the constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshExit {
    /// The exit status, if the server sent one.
    pub status: Option<u32>,
    /// The signal that ended it (`"TERM"`, `"KILL"`, …), if the server sent one.
    pub signal: Option<String>,
    /// Bytes of stdout received.
    pub stdout_bytes: u64,
    /// Bytes of stderr received.
    pub stderr_bytes: u64,
}

impl SshExit {
    /// Ended with this exit status (for custom transports and tests).
    pub fn with_status(status: u32) -> Self {
        Self { status: Some(status), signal: None, stdout_bytes: 0, stderr_bytes: 0 }
    }

    /// Ended by this signal (for custom transports and tests).
    pub fn with_signal(signal: impl Into<String>) -> Self {
        Self { status: None, signal: Some(signal.into()), stdout_bytes: 0, stderr_bytes: 0 }
    }

    /// Set the byte counts (builder style).
    pub fn with_output_bytes(mut self, stdout: u64, stderr: u64) -> Self {
        self.stdout_bytes = stdout;
        self.stderr_bytes = stderr;
        self
    }

    /// Whether the exit status is 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// Which output stream a chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SshStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// The state of one named SSH connection. `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SshState {
    /// Connecting: TCP, key exchange, host key check, authentication.
    Connecting,
    /// Connected and authenticated: commands run.
    Connected,
    /// The connection was lost (or an attempt failed) and [`SshTarget::with_reconnect`] is set:
    /// the next attempt starts after `retry_in`. New commands wait for it; none is re-run.
    Reconnecting {
        /// The next attempt (1 for the first retry).
        attempt: u32,
        /// How long until it starts.
        retry_in: Duration,
    },
    /// Not connected: never connected, closed by the game, failed or lost (without reconnect, or
    /// out of attempts, or a permanent error), refused in a release build, or the app is exiting.
    Disconnected,
}

/// A connection's state changed. Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct SshStateChanged {
    /// The connection.
    pub name: SshName,
    /// The new state.
    pub state: SshState,
    /// Why, when it failed or was lost (`None` when it connected or was closed by the game).
    pub error: Option<BackendError>,
}

/// A chunk of a command's output, as it arrived (a chunk may end in the middle of a line or of a
/// UTF-8 character: join chunks before splitting lines). 0..n per command, all written before or
/// in the same frame as its [`SshFinished`]. `Debug` shows the length only (output may hold
/// secrets); the crate never logs output.
#[derive(Message, Clone)]
#[non_exhaustive]
pub struct SshOutput {
    /// The command.
    pub id: RequestId,
    /// The connection.
    pub name: SshName,
    /// stdout or stderr.
    pub stream: SshStream,
    /// The bytes.
    pub data: Vec<u8>,
}

impl SshOutput {
    /// The bytes as text (invalid UTF-8 replaced by `U+FFFD`).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

impl fmt::Debug for SshOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshOutput").field("id", &self.id).field("name", &self.name).field("stream", &self.stream).field("bytes", &self.data.len()).finish()
    }
}

/// The one answer to [`SshClient::run`]: how the command ended, or why it did not run to its end.
/// Written in `First`.
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct SshFinished {
    /// The command.
    pub id: RequestId,
    /// The connection.
    pub name: SshName,
    /// Whether the server started the command: `Some(true)` it accepted the exec request (the
    /// command ran, or is still running after a timeout / cancel: closing the channel does not
    /// always stop it), `Some(false)` it was never sent, `None` it was sent but the answer never
    /// came (it may have started).
    pub started: Option<bool>,
    /// The exit status, or the error: `Timeout`, `Cancelled`, `Shutdown`, `Disconnected`,
    /// `BodyTooLarge` (output limit), `RequestTooLarge` (a command line over 64 KiB),
    /// `InvalidRequest`, `Ssh`, …
    pub result: Result<SshExit, BackendError>,
}

/// What one connection is doing, in [`SshConnections`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SshConnectionInfo {
    /// The state.
    pub state: SshState,
    /// The server's host key fingerprint (`SHA256:…`) once connected.
    pub fingerprint: Option<String>,
    /// The last error, if it failed or was lost.
    pub last_error: Option<BackendError>,
    /// Commands and SFTP operations waiting or running.
    pub pending_requests: usize,
    /// Failed reconnect attempts in a row (with [`SshTarget::with_reconnect`]; 0 otherwise).
    pub attempt: u32,
}

/// The state of every named SSH connection (read-only for the game). Every name passed to
/// `connect` gets an entry, a refused one too (`Disconnected` with the error). Only connections
/// that are not `Disconnected` count against [`SshSettings::with_max_connections`]; beyond 256
/// remembered names the oldest `Disconnected` ones are forgotten.
#[derive(Resource, Default, Debug)]
pub struct SshConnections {
    pub(crate) map: HashMap<SshName, SshConnectionInfo>,
}

impl SshConnections {
    /// The connection named `name`, once `connect` was applied for it (it stays after a
    /// disconnect, with state `Disconnected`).
    pub fn get(&self, name: &str) -> Option<&SshConnectionInfo> {
        self.map.get(&SshName::from(name))
    }

    /// Its state.
    pub fn state(&self, name: &str) -> Option<SshState> {
        self.get(name).map(|c| c.state)
    }

    /// Whether it is connected.
    pub fn is_connected(&self, name: &str) -> bool {
        self.state(name) == Some(SshState::Connected)
    }

    /// Every connection.
    pub fn iter(&self) -> impl Iterator<Item = (&SshName, &SshConnectionInfo)> {
        self.map.iter()
    }
}

/// Something game systems asked for.
pub(crate) enum SshQueued {
    Connect(SshName, Box<SshTarget>),
    Disconnect(SshName),
    Run {
        name: SshName,
        id: RequestId,
        command: SshCommand,
    },
    #[cfg(feature = "sftp")]
    Sftp {
        name: SshName,
        id: RequestId,
        op: SftpOp,
    },
}

impl SshQueued {
    pub(crate) fn request_id(&self) -> Option<RequestId> {
        match self {
            SshQueued::Run { id, .. } => Some(*id),
            #[cfg(feature = "sftp")]
            SshQueued::Sftp { id, .. } => Some(*id),
            SshQueued::Connect(..) | SshQueued::Disconnect(_) => None,
        }
    }
}

/// Opens, closes and uses named SSH connections (feature `ssh`). Like
/// [`HttpClient`](crate::HttpClient) a resource with shared access only (`Res<SshClient>`), so
/// any number of systems can use it without ordering. Everything is applied in `PostUpdate`
/// ([`BackendSystems::Send`]).
///
/// ```no_run
/// use bevy::prelude::*;
/// use bevy_net_backend::prelude::*;
/// use bevy_net_backend::{SshAuth, SshTarget};
///
/// fn open(ssh: Res<SshClient>) {
///     ssh.connect(
///         "build",
///         SshTarget::new("build.example.com", "deploy")
///             .with_auth(SshAuth::agent())
///             .with_known_hosts_file("/home/admin/.ssh/known_hosts"),
///     );
///     ssh.run("build", "uname -a"); // waits for the connection
/// }
///
/// fn show(mut output: MessageReader<SshOutput>, mut done: MessageReader<SshFinished>) {
///     for chunk in output.read() {
///         info!("{}", chunk.text());
///     }
///     for finished in done.read() {
///         info!("{} ended: {:?}", finished.id, finished.result);
///     }
/// }
/// # let _ = (open, show);
/// ```
#[derive(Resource, Default)]
pub struct SshClient {
    queue: Mutex<Vec<SshQueued>>,
    cancels: crate::inflight::CancelList,
}

impl fmt::Debug for SshClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshClient").field("queued", &self.lock().len()).finish_non_exhaustive()
    }
}

impl SshClient {
    fn lock(&self) -> MutexGuard<'_, Vec<SshQueued>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn drain(&self) -> Vec<SshQueued> {
        std::mem::take(&mut *self.lock())
    }

    #[cfg(feature = "sftp")]
    pub(crate) fn push_sftp(&self, name: SshName, op: SftpOp) -> RequestId {
        let id = RequestId::next();
        self.lock().push(SshQueued::Sftp { name, id, op });
        id
    }

    /// Open (or re-open with new settings) the connection `name`: connect, check the host key,
    /// authenticate. A connection of that name that is open is closed first; its requests are
    /// answered `Disconnected`.
    pub fn connect(&self, name: impl Into<SshName>, target: SshTarget) {
        self.lock().push(SshQueued::Connect(name.into(), Box::new(target)));
    }

    /// Close the connection `name`. Its waiting and running requests are answered
    /// `Disconnected` (a running command's channel is closed; the remote process may keep
    /// running).
    pub fn disconnect(&self, name: impl Into<SshName>) {
        self.lock().push(SshQueued::Disconnect(name.into()));
    }

    /// Run a command on the connection `name` (it waits while the connection is still
    /// connecting). Output arrives as [`SshOutput`], then exactly one [`SshFinished`]: the exit
    /// status (a non-zero status is still `Ok`), or `Timeout`, `Cancelled`, `Shutdown`,
    /// `Disconnected`, `BodyTooLarge` (output limit), `RequestTooLarge` (a command line over
    /// 64 KiB), `InvalidRequest` (unknown connection, bad command, SSH disabled in a release
    /// build), …
    pub fn run(&self, name: impl Into<SshName>, command: impl Into<SshCommand>) -> RequestId {
        let id = RequestId::next();
        self.lock().push(SshQueued::Run { name: name.into(), id, command: command.into() });
        id
    }

    /// Cancel a command or SFTP operation: the same shared path as
    /// [`HttpClient::cancel`](crate::HttpClient::cancel), for every protocol. A request still
    /// waiting or running is answered `Cancelled` in the next frame's `First`; a running command's
    /// channel is closed after a `TERM` signal (best effort: the remote process may keep running;
    /// see [`SshFinished::started`]).
    pub fn cancel(&self, id: RequestId) {
        self.cancels.push(id);
    }

    pub(crate) fn share_cancels(&mut self, cancels: crate::inflight::CancelList) {
        self.cancels = cancels;
    }
}

/// Plugin part of `ssh`: resources, messages, the three systems, and the real transport unless one
/// is installed.
pub(crate) fn build(app: &mut App, settings: &SshSettings) {
    if settings.is_allowed() {
        if cfg!(debug_assertions) {
            tracing::info!(">>> NET-BACKEND: SSH available (feature `ssh`; admin / dev builds only)");
        } else {
            tracing::warn!(">>> NET-BACKEND: SSH ENABLED IN A RELEASE BUILD (allow_in_release): never ship SSH keys to players");
        }
    } else {
        tracing::info!(">>> NET-BACKEND: SSH is compiled in but disabled in this release build; every SSH request is refused");
    }
    app.init_resource::<SshClient>();
    let cancels = app.world().resource::<crate::InFlight>().cancel_list();
    if let Some(mut client) = app.world_mut().get_resource_mut::<SshClient>() {
        client.share_cancels(cancels);
    }
    app.insert_resource(systems::SshRuntime::new(settings.clone()))
        .init_resource::<SshConnections>()
        .add_message::<SshStateChanged>()
        .add_message::<SshOutput>()
        .add_message::<SshFinished>();
    #[cfg(feature = "sftp")]
    app.add_message::<SftpProgress>().add_message::<SftpFinished>();
    // After the HTTP (and WebSocket) systems of the same set: they all write `InFlight`.
    #[cfg(feature = "ws")]
    app.add_systems(First, systems::ssh_receive.in_set(BackendSystems::Receive).after(crate::ws::ws_receive))
        .add_systems(PostUpdate, systems::ssh_send.in_set(BackendSystems::Send).after(crate::ws::ws_send))
        .add_systems(Last, systems::ssh_exit.in_set(BackendSystems::Exit).after(crate::ws::ws_exit).run_if(on_message::<bevy_app::AppExit>));
    #[cfg(not(feature = "ws"))]
    app.add_systems(First, systems::ssh_receive.in_set(BackendSystems::Receive).after(crate::inflight::receive_answers))
        .add_systems(PostUpdate, systems::ssh_send.in_set(BackendSystems::Send).after(crate::inflight::send_requests))
        .add_systems(Last, systems::ssh_exit.in_set(BackendSystems::Exit).after(crate::inflight::shutdown_on_exit).run_if(on_message::<bevy_app::AppExit>));
    if !app.world().contains_resource::<SshTransportRes>() {
        app.insert_resource(SshTransportRes::new(RusshTransport::new().with_release_allowed(settings.allow_in_release)));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{allowed, SshAuth, SshCommand, SshPrompt, SshPromptAnswers, SshPromptRequest, SshPromptResponder, SshReconnect, SshSettings, SshTarget};
    use crate::BackendError;

    #[test]
    fn prompt_answers_match_words_and_give_up_on_unknown_prompts() {
        let answers = SshPromptAnswers::new().answer_containing("Password", "fake-pw-1").answer_containing("code", "123456");
        let request = SshPromptRequest::new(vec![SshPrompt::new("Password: ", false), SshPrompt::new("Verification code: ", true)]);
        let got: Option<Vec<String>> = answers.respond(&request).map(|a| a.iter().map(|s| s.expose().to_string()).collect());
        assert_eq!(got, Some(vec!["fake-pw-1".to_string(), "123456".to_string()]));
        assert!(answers.respond(&SshPromptRequest::new(vec![SshPrompt::new("Favourite colour?", true)])).is_none());
        assert_eq!(answers.respond(&SshPromptRequest::new(Vec::new())).map(|a| a.len()), Some(0));
        let debug = format!("{answers:?} {:?} {:?}", SshAuth::password("fake-pw-1"), SshAuth::keyboard_interactive(answers.clone()));
        assert!(!debug.contains("fake-pw-1") && !debug.contains("123456"), "{debug}");
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        let policy = SshReconnect::default().with_base(Duration::from_millis(100)).with_cap(Duration::from_secs(2)).with_jitter(false);
        assert_eq!(policy.delay_bound(1), Duration::from_millis(100));
        assert_eq!(policy.delay_bound(3), Duration::from_millis(400));
        assert_eq!(policy.delay_bound(40), Duration::from_secs(2));
        assert_eq!(policy.delay(u32::MAX, u64::MAX), Duration::from_secs(2));
        let jitter = SshReconnect::default().with_base(Duration::MAX).with_cap(Duration::MAX);
        assert!(jitter.delay(5, u64::MAX) <= crate::MAX_TIMEOUT);
        assert!(SshReconnect::default().with_max_attempts(Some(2)).may_retry(2));
        assert!(!SshReconnect::default().with_max_attempts(Some(2)).may_retry(3));
    }

    #[test]
    fn the_release_guard() {
        assert!(allowed(true, false), "debug builds always");
        assert!(!allowed(false, false), "release builds refuse by default");
        assert!(allowed(false, true), "release builds with allow_in_release");
        assert_eq!(SshSettings::default().is_allowed(), cfg!(debug_assertions));
        assert!(SshSettings::default().allow_in_release(true).is_allowed());
    }

    #[test]
    fn targets_are_validated() {
        let ok = SshTarget::new("host.example.com", "deploy").with_auth(SshAuth::agent());
        assert!(ok.validate().is_ok());
        assert!(ok.clone().with_port(0).validate().is_err());
        assert!(SshTarget::new("host.example.com", "deploy").validate().is_err(), "no auth");
        assert!(SshTarget::new("", "deploy").with_auth(SshAuth::agent()).validate().is_err());
        assert!(SshTarget::new("-oProxyCommand=x", "deploy").with_auth(SshAuth::agent()).validate().is_err());
        assert!(SshTarget::new("host", "de ploy").with_auth(SshAuth::agent()).validate().is_err());
        assert!(SshTarget::new(
            "host", "deploy
"
        )
        .with_auth(SshAuth::agent())
        .validate()
        .is_err());
        let pin = format!("SHA256:{}", "A".repeat(43));
        assert!(ok.clone().trust_host_key_fingerprint(pin).validate().is_ok());
        assert!(ok.clone().trust_host_key_fingerprint("SHA256:short").validate().is_err());
        assert!(ok.clone().trust_host_key_fingerprint("MD5:aa:bb").validate().is_err());
        // An ssh_config alias is resolved when connecting: no user or auth needed here.
        assert!(SshTarget::from_ssh_config("build").validate().is_ok());
    }

    #[test]
    fn durations_and_limits_are_clamped() {
        let target = SshTarget::new("h", "u")
            .with_connect_timeout(Duration::MAX)
            .with_command_timeout(Duration::MAX)
            .with_sftp_timeout(Duration::ZERO)
            .with_keepalive(Duration::MAX, 0)
            .with_max_output_bytes(0)
            .with_max_transfer_bytes(0)
            .with_max_channels(10_000);
        assert_eq!(target.connect_timeout(), crate::MAX_TIMEOUT);
        assert_eq!(target.command_timeout(), crate::MAX_TIMEOUT);
        assert_eq!(target.sftp_timeout(), Duration::from_millis(1));
        assert_eq!((target.keepalive_interval, target.keepalive_max), (crate::MAX_TIMEOUT, 1));
        assert_eq!((target.max_output_bytes(), target.max_transfer_bytes(), target.max_channels), (1024, 1024, 64));
        let command = SshCommand::new("x").with_timeout(Duration::MAX).with_max_output_bytes(1);
        assert_eq!((command.timeout(), command.max_output_bytes()), (Some(crate::MAX_TIMEOUT), Some(1024)));
    }

    #[test]
    fn debug_output_never_shows_secrets_or_full_paths() {
        let target = SshTarget::new("host", "deploy")
            .with_auth(SshAuth::key_file_with_passphrase("/home/someone/.ssh/id_ed25519", "fake-pass-1234"))
            .with_known_hosts_file("/home/someone/.ssh/known_hosts");
        let debug = format!("{target:?}");
        assert!(!debug.contains("fake-pass") && !debug.contains("someone"), "{debug}");
        assert!(debug.contains("id_ed25519"));
        let error = BackendError::AuthFailed("the server accepted none of: key file `id_ed25519`".into());
        assert!(error.to_string().starts_with("SSH authentication failed"));
        assert_eq!(error.was_sent(), Some(false));
    }
}
