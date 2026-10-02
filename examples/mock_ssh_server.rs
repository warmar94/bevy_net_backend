//! A mock SSH server on 127.0.0.1 built on russh's server side (feature `ssh`; SFTP with `sftp`).
//! It NEVER executes anything: every command gets canned output, and SFTP works on an in-memory
//! file system. The `ssh_console` example and the tests start it in their own process. Run it
//! alone:
//!
//! ```text
//! cargo run --example mock_ssh_server --features ssh,sftp                                    # 60 s
//! cargo run --example mock_ssh_server --features ssh,sftp -- --seconds 600 --bind 127.0.0.1:2222
//! ```
//!
//! Alone it generates a throwaway host key and a throwaway client key and writes `client_key`,
//! `client_key.pub` and `known_hosts` into `--out-dir` (default `target/mock-ssh`); only that
//! client key is accepted, for the user `--user` (default `tester`). `--bind` defaults to
//! `127.0.0.1:0` (loopback, a free port); `--seconds` (default 60, at most 86400) is the maximum
//! runtime. `--host NAME` is the host name clients use to reach it (for the known_hosts line; with
//! `--bind 0.0.0.0:P` and no `--host` the line uses `CHANGE-ME`: replace it, or pin the printed
//! fingerprint with `SshTarget::trust_host_key_fingerprint`). Never bind it to a public address
//! without a firewall: it is a test server.
//!
//! `--password PW` also accepts that password, `--kbd PW:CODE` a keyboard-interactive login
//! (`Password:` then `Verification code:`): throwaway test values only (they are visible in the
//! process list). In tests it can also skip strict key exchange, restrict its ciphers, offer
//! several host key types, and answer SFTP reads short, slowly or with an error from an offset on
//! ([`MockOptions`]).
//!
//! Limits: 16 connections at once (more are dropped), 10 minutes per connection, 60 s without any
//! traffic, 10 channels per connection; `flood` at most 64 MiB, `sleep` at most 60 s, stdin at most
//! 16 MiB; SFTP at most 1024 files, 16 MiB per file, 64 MiB in total.
//!
//! | Command | Output |
//! |---|---|
//! | `echo <text>` | `<text>` + newline, exit 0 |
//! | `uname` / `uname -a` | `MockOS 1.0 mock-ssh-server`, exit 0 |
//! | `whoami` | the user name, exit 0 |
//! | `stderr <text>` | `<text>` on stderr, exit 0 |
//! | `fail` | `mock: failed as asked` on stderr, exit 3 |
//! | `sleep <ms>` | waits, then `slept`, exit 0; a `TERM` signal ends it with exit signal `TERM` |
//! | `hang` | never answers (until the channel closes or a `TERM` signal) |
//! | `flood <bytes>` | that many bytes of `x` on stdout, exit 0 |
//! | `cat` | echoes stdin once it ends, exit 0 |
//! | `signal` | ends with exit signal `KILL` |
//! | `noexit` | closes the channel without an exit status |
//! | `refuse` | refuses the exec request |
//! | anything else | `mock: unknown command` on stderr, exit 127 |

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(feature = "sftp")]
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::{Cipher, HashAlg, Kdf, LineEnding};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, Sig};

const MAX_CONNECTIONS: usize = 16;
const CONNECTION_LIFETIME: Duration = Duration::from_secs(600);
const MAX_FLOOD: u64 = 64 * 1024 * 1024;
const MAX_SLEEP_MS: u64 = 60_000;
const MAX_STDIN: usize = 16 * 1024 * 1024;

/// A fresh random ed25519 key (never written anywhere unless the caller does).
pub fn random_key() -> PrivateKey {
    let mut seed = [0u8; 32];
    if getrandom::fill(&mut seed).is_err() {
        // No OS randomness: a test-only fallback that is still unique per call.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for (i, byte) in nanos.to_le_bytes().iter().cycle().take(32).enumerate() {
            if let Some(slot) = seed.get_mut(i) {
                *slot = *byte ^ u8::try_from(i).unwrap_or(0);
            }
        }
    }
    PrivateKey::from(Ed25519Keypair::from_seed(&seed))
}

/// Write `key` as an OpenSSH private key file (encrypted with `passphrase` when given) plus its
/// `.pub` next to it. Throwaway test keys only.
pub fn write_key(key: &PrivateKey, path: &Path, passphrase: Option<&str>) -> std::io::Result<()> {
    let key = match passphrase {
        Some(passphrase) => {
            let mut salt = vec![0u8; 16];
            let _ = getrandom::fill(&mut salt);
            key.encrypt_with(Cipher::Aes256Ctr, Kdf::Bcrypt { salt, rounds: 4 }, 0x5eed_5eed, passphrase).map_err(std::io::Error::other)?
        }
        None => key.clone(),
    };
    let text = key.to_openssh(LineEnding::LF).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text.as_bytes())?;
    let public = key.public_key().to_openssh().map_err(std::io::Error::other)?;
    std::fs::write(path.with_extension("pub"), format!("{public}\n"))
}

/// What the server saw (for tests).
#[derive(Default)]
pub struct Stats {
    /// Connections accepted.
    pub connections: AtomicUsize,
    /// Successful logins.
    pub logins: AtomicUsize,
    /// Exec requests received.
    pub execs: AtomicUsize,
    /// Signals received.
    pub signals: AtomicUsize,
    /// Channels closed by the client.
    pub closes: AtomicUsize,
    /// SFTP file / directory handles open right now (opened and not closed by the client).
    pub sftp_handles: AtomicUsize,
}

/// How a test mock behaves (all off by default: key login only, strict key exchange, the default
/// ciphers, one ed25519 host key).
#[derive(Clone, Default)]
pub struct MockOptions {
    /// Also accept this password (`password` method).
    pub password: Option<String>,
    /// Also accept keyboard-interactive with these answers to `Password:` and `Verification code:`.
    pub keyboard_interactive: Option<(String, String)>,
    /// Do NOT offer strict key exchange (like OpenSSH before 9.6 without a backport).
    pub no_strict_kex: bool,
    /// Only these ciphers (names as in SSH, e.g. `chacha20-poly1305@openssh.com`).
    pub ciphers: Option<Vec<&'static str>>,
    /// Extra host keys besides the ed25519 one (e.g. an ECDSA P-256 key).
    pub ecdsa_host_key: bool,
    /// Offer ONLY the ECDSA host key.
    pub only_ecdsa_host_key: bool,
    /// SFTP reads answer fewer bytes than asked (a varying length), as servers may.
    pub sftp_short_reads: bool,
    /// SFTP reads at or after this offset fail with `Failure` (a server error in the middle of a
    /// download).
    pub sftp_fail_reads_at: Option<u64>,
    /// Every SFTP read waits this long first (a slow server).
    pub sftp_read_delay: Option<Duration>,
}

/// A running mock SSH server; stops when dropped.
pub struct MockSshServer {
    addr: SocketAddr,
    host_key: PublicKey,
    host_keys: Vec<PublicKey>,
    stats: Arc<Stats>,
    #[cfg(feature = "sftp")]
    files: Arc<Mutex<sftp::Fs>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MockSshServer {
    /// Start on 127.0.0.1 with a free port, accepting `client` for `user`.
    pub fn start(user: &str, client: PublicKey) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", user, client)
    }

    /// Start on `addr`.
    pub fn start_on(addr: &str, user: &str, client: PublicKey) -> std::io::Result<Self> {
        Self::start_with(addr, user, client, MockOptions::default())
    }

    /// Start on `addr` with test options.
    pub fn start_with(addr: &str, user: &str, client: PublicKey, options: MockOptions) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let mut keys = Vec::new();
        if !options.only_ecdsa_host_key {
            keys.push(random_key());
        }
        if options.ecdsa_host_key || options.only_ecdsa_host_key {
            use russh::keys::ssh_key::rand_core::UnwrapErr;
            let ecdsa = PrivateKey::random(&mut UnwrapErr(getrandom::SysRng), russh::keys::Algorithm::Ecdsa { curve: russh::keys::EcdsaCurve::NistP256 })
                .map_err(std::io::Error::other)?;
            keys.push(ecdsa);
        }
        let host_keys: Vec<PublicKey> = keys.iter().map(|k| k.public_key().clone()).collect();
        let host_key = host_keys.first().cloned().ok_or_else(|| std::io::Error::other("no host key"))?;
        let stats = Arc::new(Stats::default());
        #[cfg(feature = "sftp")]
        let files = Arc::new(Mutex::new(sftp::Fs::new(user)));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let shared = Shared {
            user: user.to_string(),
            client,
            password: options.password.clone(),
            keyboard_interactive: options.keyboard_interactive.clone(),
            stats: Arc::clone(&stats),
            #[cfg(feature = "sftp")]
            files: Arc::clone(&files),
            #[cfg(feature = "sftp")]
            sftp: sftp::Behaviour { short_reads: options.sftp_short_reads, fail_reads_at: options.sftp_fail_reads_at, read_delay: options.sftp_read_delay },
        };
        let mut preferred = russh::Preferred::default();
        if options.no_strict_kex {
            let kex: Vec<russh::kex::Name> = preferred.kex.iter().filter(|k| !k.as_ref().starts_with("kex-strict-")).cloned().collect();
            preferred.kex = std::borrow::Cow::Owned(kex);
        }
        if let Some(ciphers) = &options.ciphers {
            let names: Vec<russh::cipher::Name> = ciphers.iter().filter_map(|c| russh::cipher::Name::try_from(*c).ok()).collect();
            preferred.cipher = std::borrow::Cow::Owned(names);
        }
        let config = russh::server::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            max_auth_attempts: 6,
            keys,
            preferred,
            ..Default::default()
        };
        let thread = std::thread::Builder::new().name("mock-ssh-server".into()).spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            runtime.block_on(accept_loop(listener, Arc::new(config), shared, stopped));
        })?;
        Ok(Self {
            addr,
            host_key,
            host_keys,
            stats,
            #[cfg(feature = "sftp")]
            files,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// The address it listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The host key's fingerprint (`SHA256:…`).
    pub fn fingerprint(&self) -> String {
        self.host_key.fingerprint(HashAlg::Sha256).to_string()
    }

    /// A known_hosts line for this server's (first) host key as `host` (e.g. `127.0.0.1`).
    pub fn known_hosts_line(&self, host: &str) -> String {
        self.known_hosts_line_for(host, &self.host_key)
    }

    /// Every host key the server offers.
    pub fn host_keys(&self) -> &[PublicKey] {
        &self.host_keys
    }

    /// A known_hosts line for `key` as `host`.
    pub fn known_hosts_line_for(&self, host: &str, key: &PublicKey) -> String {
        let key = key.to_openssh().unwrap_or_default();
        if self.port() == 22 {
            format!("{host} {key}")
        } else {
            format!("[{host}]:{} {key}", self.port())
        }
    }

    /// What it saw.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The content of an in-memory SFTP file (feature `sftp`).
    #[cfg(feature = "sftp")]
    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner).file(path)
    }

    /// Put a file into the in-memory SFTP file system (feature `sftp`).
    #[cfg(feature = "sftp")]
    pub fn put_file(&self, path: &str, data: &[u8]) {
        self.files.lock().unwrap_or_else(PoisonError::into_inner).put(path, data);
    }
}

impl Drop for MockSshServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct Shared {
    user: String,
    client: PublicKey,
    password: Option<String>,
    keyboard_interactive: Option<(String, String)>,
    stats: Arc<Stats>,
    #[cfg(feature = "sftp")]
    files: Arc<Mutex<sftp::Fs>>,
    #[cfg(feature = "sftp")]
    sftp: sftp::Behaviour,
}

async fn accept_loop(listener: std::net::TcpListener, config: Arc<russh::server::Config>, shared: Shared, mut stopped: tokio::sync::oneshot::Receiver<()>) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = tokio::select! {
            _ = &mut stopped => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else { continue };
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            continue; // dropped: over the connection limit
        }
        active.fetch_add(1, Ordering::SeqCst);
        shared.stats.connections.fetch_add(1, Ordering::SeqCst);
        let (config, handler, slot) =
            (Arc::clone(&config), Handler { shared: shared.clone(), channels: HashMap::new(), stdin: HashMap::new() }, Arc::clone(&active));
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            let _ = tokio::time::timeout(CONNECTION_LIFETIME, async move {
                if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                    let _ = session.await;
                }
            })
            .await;
            slot.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

struct Handler {
    shared: Shared,
    /// Open channels (kept for the SFTP subsystem).
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// stdin of `cat` channels, and stop signals of running commands.
    stdin: HashMap<ChannelId, Running>,
}

#[derive(Default)]
struct Running {
    cat: Option<Vec<u8>>,
    stop: Option<Arc<tokio::sync::Notify>>,
}

impl russh::server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if user == self.shared.user && key.key_data() == self.shared.client.key_data() {
            self.shared.stats.logins.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject { proceed_with_methods: None, partial_success: false })
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == self.shared.user && self.shared.password.as_deref() == Some(password) {
            self.shared.stats.logins.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some((password, code)) = self.shared.keyboard_interactive.clone() else { return Ok(Auth::reject()) };
        match response {
            None => Ok(Auth::Partial {
                name: "mock login".into(),
                instructions: "two questions".into(),
                prompts: vec![("Password: ".into(), false), ("Verification code: ".into(), true)].into(),
            }),
            Some(answers) => {
                let answers: Vec<String> = answers.take(4).map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
                if user == self.shared.user && answers == [password, code] {
                    self.shared.stats.logins.fetch_add(1, Ordering::SeqCst);
                    Ok(Auth::Accept)
                } else {
                    Ok(Auth::reject())
                }
            }
        }
    }

    async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: ChannelOpenHandle, _session: &mut Session) -> Result<(), Self::Error> {
        if self.channels.len() >= 10 {
            return Ok(()); // dropping `reply` refuses the channel
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.shared.stats.closes.fetch_add(1, Ordering::SeqCst);
        self.channels.remove(&channel);
        if let Some(stop) = self.stdin.remove(&channel).and_then(|r| r.stop) {
            stop.notify_one();
        }
        Ok(())
    }

    async fn signal(&mut self, channel: ChannelId, _signal: Sig, _session: &mut Session) -> Result<(), Self::Error> {
        self.shared.stats.signals.fetch_add(1, Ordering::SeqCst);
        if let Some(stop) = self.stdin.get(&channel).and_then(|r| r.stop.clone()) {
            stop.notify_one();
        }
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(buffer) = self.stdin.get_mut(&channel).and_then(|r| r.cat.as_mut()) {
            if buffer.len().saturating_add(data.len()) <= MAX_STDIN {
                buffer.extend_from_slice(data);
            }
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        if let Some(input) = self.stdin.get_mut(&channel).and_then(|r| r.cat.take()) {
            let handle = session.handle();
            tokio::spawn(async move {
                let _ = handle.data(channel, input).await;
                finish(&handle, channel, 0).await;
            });
        }
        Ok(())
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        self.shared.stats.execs.fetch_add(1, Ordering::SeqCst);
        let command = String::from_utf8_lossy(data).into_owned();
        let (word, rest) = command.split_once(' ').unwrap_or((command.as_str(), ""));
        if word == "refuse" {
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        let handle = session.handle();
        let stop = Arc::new(tokio::sync::Notify::new());
        self.stdin.insert(channel, Running { cat: (word == "cat").then(Vec::new), stop: Some(Arc::clone(&stop)) });
        let user = self.shared.user.clone();
        let (word, rest) = (word.to_string(), rest.to_string());
        tokio::spawn(async move {
            match word.as_str() {
                "echo" => {
                    let _ = handle.data(channel, format!("{rest}\n").into_bytes()).await;
                    finish(&handle, channel, 0).await;
                }
                "uname" => {
                    let _ = handle.data(channel, b"MockOS 1.0 mock-ssh-server\n".to_vec()).await;
                    finish(&handle, channel, 0).await;
                }
                "whoami" => {
                    let _ = handle.data(channel, format!("{user}\n").into_bytes()).await;
                    finish(&handle, channel, 0).await;
                }
                "stderr" => {
                    let _ = handle.extended_data(channel, 1, format!("{rest}\n").into_bytes()).await;
                    finish(&handle, channel, 0).await;
                }
                "fail" => {
                    let _ = handle.extended_data(channel, 1, b"mock: failed as asked\n".to_vec()).await;
                    finish(&handle, channel, 3).await;
                }
                "sleep" => {
                    let ms = rest.trim().parse::<u64>().unwrap_or(1000).min(MAX_SLEEP_MS);
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(ms)) => {
                            let _ = handle.data(channel, b"slept\n".to_vec()).await;
                            finish(&handle, channel, 0).await;
                        }
                        () = stop.notified() => {
                            let _ = handle.exit_signal_request(channel, Sig::TERM, false, "stopped".into(), "en".into()).await;
                            let _ = handle.eof(channel).await;
                            let _ = handle.close(channel).await;
                        }
                    }
                }
                "hang" => {
                    stop.notified().await;
                    let _ = handle.close(channel).await;
                }
                "flood" => {
                    let mut left = rest.trim().parse::<u64>().unwrap_or(1024).min(MAX_FLOOD);
                    let chunk = vec![b'x'; 32 * 1024];
                    while left > 0 {
                        let n = usize::try_from(left.min(32 * 1024)).unwrap_or(32 * 1024);
                        if handle.data(channel, chunk.get(..n).unwrap_or_default().to_vec()).await.is_err() {
                            return;
                        }
                        left = left.saturating_sub(n as u64);
                    }
                    finish(&handle, channel, 0).await;
                }
                "cat" => {} // answered at end of stdin
                "signal" => {
                    let _ = handle.exit_signal_request(channel, Sig::KILL, false, String::new(), "en".into()).await;
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                }
                "noexit" => {
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                }
                _ => {
                    let _ = handle.extended_data(channel, 1, b"mock: unknown command\n".to_vec()).await;
                    finish(&handle, channel, 127).await;
                }
            }
        });
        Ok(())
    }

    #[allow(unused_variables)]
    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        #[cfg(feature = "sftp")]
        if name == "sftp" {
            if let Some(open) = self.channels.remove(&channel) {
                session.channel_success(channel)?;
                russh_sftp::server::run(
                    open.into_stream(),
                    sftp::Session::new(Arc::clone(&self.shared.files), self.shared.sftp, Arc::clone(&self.shared.stats)),
                )
                .await;
                return Ok(());
            }
        }
        session.channel_failure(channel)?;
        Ok(())
    }
}

async fn finish(handle: &russh::server::Handle, channel: ChannelId, status: u32) {
    let _ = handle.exit_status_request(channel, status).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// The in-memory SFTP file system (feature `sftp`).
#[cfg(feature = "sftp")]
mod sftp {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};

    const MAX_FILES: usize = 1024;
    const MAX_FILE: usize = 16 * 1024 * 1024;
    const MAX_TOTAL: usize = 64 * 1024 * 1024;

    pub struct Fs {
        home: String,
        files: BTreeMap<String, Vec<u8>>,
        dirs: BTreeSet<String>,
    }

    impl Fs {
        pub fn new(user: &str) -> Self {
            let home = format!("/home/{user}");
            let dirs = ["/".to_string(), "/home".to_string(), home.clone()].into_iter().collect();
            Self { home, files: BTreeMap::new(), dirs }
        }

        /// An absolute, normalised path (`None` for `..` tricks).
        fn path(&self, path: &str) -> Option<String> {
            let full = if path.starts_with('/') { path.to_string() } else { format!("{}/{path}", self.home) };
            let mut parts: Vec<&str> = Vec::new();
            for part in full.split('/') {
                match part {
                    "" | "." => {}
                    ".." => return None,
                    other => parts.push(other),
                }
            }
            Some(format!("/{}", parts.join("/")))
        }

        fn parent_exists(&self, path: &str) -> bool {
            let parent = path.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap_or("/");
            self.dirs.contains(parent)
        }

        fn total(&self) -> usize {
            self.files.values().map(Vec::len).sum()
        }

        pub fn file(&self, path: &str) -> Option<Vec<u8>> {
            self.path(path).and_then(|p| self.files.get(&p).cloned())
        }

        pub fn put(&mut self, path: &str, data: &[u8]) {
            if let Some(p) = self.path(path) {
                self.files.insert(p, data.to_vec());
            }
        }
    }

    enum Open {
        File(String),
        Dir { entries: Vec<File>, sent: bool },
    }

    /// Test behaviour of the SFTP side (see `MockOptions`).
    #[derive(Clone, Copy, Default)]
    pub struct Behaviour {
        pub short_reads: bool,
        pub fail_reads_at: Option<u64>,
        pub read_delay: Option<std::time::Duration>,
    }

    pub struct Session {
        fs: Arc<Mutex<Fs>>,
        handles: HashMap<String, Open>,
        next: u64,
        behaviour: Behaviour,
        stats: Arc<super::Stats>,
    }

    impl Drop for Session {
        /// Handles still open when the SFTP channel ends are gone with it.
        fn drop(&mut self) {
            self.stats.sftp_handles.fetch_sub(self.handles.len(), std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Session {
        pub fn new(fs: Arc<Mutex<Fs>>, behaviour: Behaviour, stats: Arc<super::Stats>) -> Self {
            Self { fs, handles: HashMap::new(), next: 1, behaviour, stats }
        }

        fn handle(&mut self, open: Open) -> Result<String, StatusCode> {
            if self.handles.len() >= 64 {
                return Err(StatusCode::Failure);
            }
            let handle = format!("h{}", self.next);
            self.next = self.next.saturating_add(1);
            self.handles.insert(handle.clone(), open);
            self.stats.sftp_handles.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(handle)
        }
    }

    fn ok(id: u32) -> Status {
        Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
    }

    fn file_attrs(size: usize) -> FileAttributes {
        FileAttributes { size: Some(size as u64), permissions: Some(0o100_644), ..FileAttributes::empty() }
    }

    fn dir_attrs() -> FileAttributes {
        FileAttributes { permissions: Some(0o040_755), ..FileAttributes::empty() }
    }

    impl russh_sftp::server::Handler for Session {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, _attrs: FileAttributes) -> Result<Handle, Self::Error> {
            let path = {
                let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
                let path = fs.path(&filename).ok_or(StatusCode::PermissionDenied)?;
                if pflags.contains(OpenFlags::WRITE) {
                    if !fs.parent_exists(&path) || fs.dirs.contains(&path) {
                        return Err(StatusCode::NoSuchFile);
                    }
                    if !fs.files.contains_key(&path) && fs.files.len() >= MAX_FILES {
                        return Err(StatusCode::Failure);
                    }
                    if pflags.contains(OpenFlags::TRUNCATE) || !fs.files.contains_key(&path) {
                        fs.files.insert(path.clone(), Vec::new());
                    }
                } else if !fs.files.contains_key(&path) {
                    return Err(StatusCode::NoSuchFile);
                }
                path
            };
            Ok(Handle { id, handle: self.handle(Open::File(path))? })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            let closed = self.handles.remove(&handle).map(|_| ok(id)).ok_or(StatusCode::Failure);
            if closed.is_ok() {
                self.stats.sftp_handles.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            closed
        }

        async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
            if let Some(delay) = self.behaviour.read_delay {
                tokio::time::sleep(delay).await;
            }
            if self.behaviour.fail_reads_at.is_some_and(|at| offset >= at) {
                return Err(StatusCode::Failure);
            }
            let Some(Open::File(path)) = self.handles.get(&handle) else { return Err(StatusCode::Failure) };
            let fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let data = fs.files.get(path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= data.len() {
                return Err(StatusCode::Eof);
            }
            let mut len = len.min(64 * 1024);
            if self.behaviour.short_reads && len > 1 {
                // A varying shorter answer (1..len bytes), the same for the same offset.
                let mixed = offset.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23);
                len = 1 + u32::try_from(mixed % u64::from(len - 1)).unwrap_or(0);
            }
            let end = start.saturating_add(usize::try_from(len).unwrap_or(0)).min(data.len());
            Ok(Data { id, data: data.get(start..end).unwrap_or_default().to_vec() })
        }

        async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
            let Some(Open::File(path)) = self.handles.get(&handle) else { return Err(StatusCode::Failure) };
            let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let total = fs.total();
            let file = fs.files.get_mut(path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let end = start.checked_add(data.len()).ok_or(StatusCode::Failure)?;
            let growth = end.saturating_sub(file.len());
            if end > MAX_FILE || total.saturating_add(growth) > MAX_TOTAL {
                return Err(StatusCode::Failure);
            }
            if file.len() < end {
                file.resize(end, 0);
            }
            if let Some(target) = file.get_mut(start..end) {
                target.copy_from_slice(&data);
            }
            Ok(ok(id))
        }

        async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
            let Some(Open::File(path)) = self.handles.get(&handle) else { return Err(StatusCode::Failure) };
            let fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let size = fs.files.get(path).map(Vec::len).ok_or(StatusCode::NoSuchFile)?;
            Ok(Attrs { id, attrs: file_attrs(size) })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let path = fs.path(&path).ok_or(StatusCode::NoSuchFile)?;
            if fs.dirs.contains(&path) {
                return Ok(Attrs { id, attrs: dir_attrs() });
            }
            fs.files.get(&path).map(|f| Attrs { id, attrs: file_attrs(f.len()) }).ok_or(StatusCode::NoSuchFile)
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            let entries = {
                let fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
                let dir = fs.path(&path).ok_or(StatusCode::NoSuchFile)?;
                if !fs.dirs.contains(&dir) {
                    return Err(StatusCode::NoSuchFile);
                }
                let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
                let direct = |p: &String| p.strip_prefix(&prefix).filter(|rest| !rest.is_empty() && !rest.contains('/')).map(str::to_string);
                let mut entries: Vec<File> = vec![File { filename: ".".into(), longname: String::new(), attrs: dir_attrs() }];
                entries.extend(fs.dirs.iter().filter_map(|d| direct(d).map(|name| File { filename: name, longname: String::new(), attrs: dir_attrs() })));
                entries.extend(
                    fs.files
                        .iter()
                        .filter_map(|(p, data)| direct(p).map(|name| File { filename: name, longname: String::new(), attrs: file_attrs(data.len()) })),
                );
                entries
            };
            Ok(Handle { id, handle: self.handle(Open::Dir { entries, sent: false })? })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            match self.handles.get_mut(&handle) {
                Some(Open::Dir { entries, sent }) if !*sent => {
                    *sent = true;
                    Ok(Name { id, files: std::mem::take(entries) })
                }
                Some(Open::Dir { .. }) => Err(StatusCode::Eof),
                _ => Err(StatusCode::Failure),
            }
        }

        async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
            let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let path = fs.path(&path).ok_or(StatusCode::PermissionDenied)?;
            if fs.dirs.contains(&path) || fs.files.contains_key(&path) || !fs.parent_exists(&path) || fs.dirs.len() >= MAX_FILES {
                return Err(StatusCode::Failure);
            }
            fs.dirs.insert(path);
            Ok(ok(id))
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let path = fs.path(&path).ok_or(StatusCode::PermissionDenied)?;
            let prefix = format!("{path}/");
            if !fs.dirs.contains(&path) {
                return Err(StatusCode::NoSuchFile);
            }
            if fs.files.keys().chain(fs.dirs.iter()).any(|p| p.starts_with(&prefix)) {
                return Err(StatusCode::Failure);
            }
            fs.dirs.remove(&path);
            Ok(ok(id))
        }

        async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
            let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let path = fs.path(&filename).ok_or(StatusCode::PermissionDenied)?;
            fs.files.remove(&path).map(|_| ok(id)).ok_or(StatusCode::NoSuchFile)
        }

        async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
            let mut fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let (from, to) = (fs.path(&oldpath).ok_or(StatusCode::PermissionDenied)?, fs.path(&newpath).ok_or(StatusCode::PermissionDenied)?);
            if fs.files.contains_key(&to) || fs.dirs.contains(&to) || !fs.parent_exists(&to) {
                return Err(StatusCode::Failure);
            }
            let data = fs.files.remove(&from).ok_or(StatusCode::NoSuchFile)?;
            fs.files.insert(to, data);
            Ok(ok(id))
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            let fs = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
            let path = fs.path(&path).ok_or(StatusCode::NoSuchFile)?;
            Ok(Name { id, files: vec![File { filename: path, longname: String::new(), attrs: FileAttributes::empty() }] })
        }
    }
}

/// `--seconds N` (maximum runtime, default 60), `--bind ADDR` (default `127.0.0.1:0`),
/// `--out-dir DIR` (default `target/mock-ssh`), `--user NAME` (default `tester`), `--host NAME`
/// (the name clients reach it by, for the known_hosts line).
fn main() -> std::io::Result<()> {
    let (mut seconds, mut addr, mut out, mut user) = (60u64, "127.0.0.1:0".to_string(), PathBuf::from("target").join("mock-ssh"), "tester".to_string());
    let mut host_name: Option<String> = None;
    let mut options = MockOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--seconds", Some(value)) if value.parse::<u64>().is_ok() => seconds = value.parse::<u64>().unwrap_or(60).min(86_400),
            ("--bind", Some(value)) => addr = value,
            ("--out-dir", Some(value)) => out = PathBuf::from(value),
            ("--user", Some(value)) if !value.is_empty() => user = value,
            ("--host", Some(value)) if !value.is_empty() => host_name = Some(value),
            ("--password", Some(value)) if !value.is_empty() => options.password = Some(value),
            ("--kbd", Some(value)) if value.contains(':') => {
                let (password, code) = value.split_once(':').unwrap_or_default();
                options.keyboard_interactive = Some((password.to_string(), code.to_string()));
            }
            _ => {
                eprintln!(
                    "usage: mock_ssh_server [--seconds N] [--bind ADDR] [--out-dir DIR] [--user NAME] [--host NAME] [--password PW] [--kbd PW:CODE]   (defaults: 60, 127.0.0.1:0, target/mock-ssh, tester, the bind address)"
                );
                return Err(std::io::Error::other("bad arguments"));
            }
        }
    }
    let client = random_key();
    let key_path = out.join("client_key");
    write_key(&client, &key_path, None)?;
    let server = MockSshServer::start_with(&addr, &user, client.public_key().clone(), options)?;
    let ip = server.addr().ip();
    let host = match host_name {
        Some(name) => name,
        None if ip.is_unspecified() => {
            println!("NOTE: bound to {ip}; the known_hosts line says `CHANGE-ME`: replace it with the name or address clients use,");
            println!("      or skip known_hosts and pin the fingerprint below with SshTarget::trust_host_key_fingerprint(..)");
            "CHANGE-ME".to_string()
        }
        None => ip.to_string(),
    };
    let known_hosts = out.join("known_hosts");
    std::fs::write(&known_hosts, format!("{}\n", server.known_hosts_line(&host)))?;
    println!("mock SSH server on {} for {seconds} s (user `{user}`, host key {})", server.addr(), server.fingerprint());
    println!("throwaway client key: {}", key_path.display());
    println!("known_hosts:          {}", known_hosts.display());
    std::thread::sleep(Duration::from_secs(seconds));
    drop(server);
    println!("mock SSH server stopped");
    Ok(())
}
