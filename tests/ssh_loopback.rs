//! The real `RusshTransport` against the mock from `examples/mock_ssh_server.rs` (and a few
//! hostile raw TCP peers), all on 127.0.0.1 in this process. Throwaway keys and known_hosts files
//! are generated at runtime under `target/tmp`; nothing touches `~/.ssh` or an SSH agent.
//! Bounded: every wait has a frame limit.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

#[allow(dead_code)]
#[path = "../examples/mock_ssh_server.rs"]
mod mock_ssh_server;

use mock_ssh_server::{random_key, write_key, MockOptions, MockSshServer};

const USER: &str = "tester";

/// A fresh directory under `target/tmp` for one test's throwaway files.
fn scratch() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ssh-loopback-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    dir
}

/// A mock server, a client key file (encrypted with `passphrase` if given) and a known_hosts file
/// that lists the mock.
struct Setup {
    mock: MockSshServer,
    dir: PathBuf,
    key: PathBuf,
    known_hosts: PathBuf,
}

impl Setup {
    fn new(passphrase: Option<&str>) -> Self {
        Self::with(passphrase, MockOptions::default())
    }

    fn with(passphrase: Option<&str>, options: MockOptions) -> Self {
        let dir = scratch();
        let client = random_key();
        let key = dir.join("id_test");
        write_key(&client, &key, passphrase).unwrap_or_else(|e| panic!("{e}"));
        let mock = MockSshServer::start_with("127.0.0.1:0", USER, client.public_key().clone(), options).unwrap_or_else(|e| panic!("{e}"));
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, format!("{}\n", mock.known_hosts_line("127.0.0.1"))).unwrap_or_else(|e| panic!("{e}"));
        Self { mock, dir, key, known_hosts }
    }

    fn target(&self) -> SshTarget {
        SshTarget::new("127.0.0.1", USER).with_port(self.mock.port()).with_auth(SshAuth::key_file(&self.key)).with_known_hosts_file(&self.known_hosts)
    }
}

fn app() -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default());
    app.watch::<SshStateChanged>().watch::<SshOutput>().watch::<SshFinished>();
    #[cfg(feature = "sftp")]
    app.watch::<SftpFinished>().watch::<SftpProgress>();
    app
}

fn ssh(app: &TestApp) -> &SshClient {
    app.world().resource::<SshClient>()
}

/// Connect `target` as "main" and wait until it is connected or refused; the final info.
fn connect(app: &mut TestApp, target: SshTarget) -> SshConnectionInfo {
    ssh(app).connect("main", target);
    app.step();
    app.run_until(|world| world.resource::<SshConnections>().state("main").is_some_and(|s| s != SshState::Connecting), 5000);
    app.world().resource::<SshConnections>().get("main").cloned().unwrap_or_else(|| panic!("no connection info"))
}

fn connected(app: &mut TestApp, target: SshTarget) {
    let info = connect(app, target);
    assert_eq!(info.state, SshState::Connected, "{:?}", info.last_error);
}

/// Run and wait for the one answer; also the stdout and stderr text.
fn run(app: &mut TestApp, command: impl Into<SshCommand>) -> (SshFinished, String, String) {
    let id = ssh(app).run("main", command);
    wait(app, id)
}

fn wait(app: &mut TestApp, id: RequestId) -> (SshFinished, String, String) {
    app.step();
    app.run_until(|world| world.resource::<InFlight>().describe(id).is_none(), 5000);
    app.step();
    let mut answers: Vec<SshFinished> = app.all_messages::<SshFinished>().into_iter().filter(|f| f.id == id).collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
    let text = |stream| app.all_messages::<SshOutput>().into_iter().filter(|o| o.id == id && o.stream == stream).map(|o| o.text()).collect::<String>();
    let (stdout, stderr) = (text(SshStream::Stdout), text(SshStream::Stderr));
    (answers.remove(0), stdout, stderr)
}

fn wait_until(mut done: impl FnMut() -> bool, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[test]
fn commands_run_and_report_output_and_exit() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let info = app.world().resource::<SshConnections>().get("main").cloned().unwrap_or_else(|| panic!("no info"));
    assert_eq!(info.fingerprint.as_deref(), Some(setup.mock.fingerprint().as_str()));

    let (answer, out, _) = run(&mut app, "echo hello world");
    assert_eq!((answer.started, out.as_str()), (Some(true), "hello world\n"));
    assert!(answer.result.as_ref().is_ok_and(SshExit::success), "{answer:?}");
    let (answer, out, _) = run(&mut app, "whoami");
    assert_eq!(out, "tester\n");
    assert_eq!(answer.result.map(|e| e.stdout_bytes).ok(), Some(7));
    let (answer, out, err) = run(&mut app, "fail");
    assert_eq!((out.as_str(), err.as_str()), ("", "mock: failed as asked\n"));
    assert_eq!(answer.result.map(|e| e.status).ok(), Some(Some(3)));
    let (answer, _, _) = run(&mut app, "signal");
    assert_eq!(answer.result.map(|e| (e.status, e.signal)).ok(), Some((None, Some("KILL".to_string()))));
    let (answer, _, _) = run(&mut app, "noexit");
    assert_eq!(answer.result.map(|e| (e.status, e.signal)).ok(), Some((None, None)));
    let (answer, out, _) = run(&mut app, SshCommand::new("cat").with_stdin(b"piped in".to_vec()));
    assert_eq!(out, "piped in");
    assert!(answer.result.is_ok_and(|e| e.success()));
    assert!(setup.mock.stats().execs.load(Ordering::SeqCst) >= 6);
}

#[test]
fn a_refused_exec_says_it_never_started() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let (answer, _, _) = run(&mut app, "refuse");
    assert!(matches!(&answer.result, Err(BackendError::Ssh(why)) if why.contains("refused")), "{answer:?}");
    assert_eq!(answer.started, Some(false));
}

#[test]
fn a_timeout_stops_the_command_and_says_it_started() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let started = Instant::now();
    let (answer, _, _) = run(&mut app, SshCommand::new("sleep 30000").with_timeout(Duration::from_millis(300)));
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert!(matches!(&answer.result, Err(BackendError::Timeout(why)) if why.contains("TERM")), "{answer:?}");
    assert_eq!(answer.started, Some(true));
    // The remote side got the TERM signal and the channel close.
    assert!(wait_until(|| setup.mock.stats().signals.load(Ordering::SeqCst) >= 1, Duration::from_secs(3)));
    // The connection is still good.
    let (answer, out, _) = run(&mut app, "echo still here");
    assert!(answer.result.is_ok() && out == "still here\n");
}

#[test]
fn cancel_stops_a_running_command() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let id = ssh(&app).run("main", "hang");
    app.step();
    assert!(wait_until(|| setup.mock.stats().execs.load(Ordering::SeqCst) >= 1, Duration::from_secs(3)));
    app.step_n(20);
    ssh(&app).cancel(id);
    let (answer, _, _) = wait(&mut app, id);
    assert_eq!((answer.result.err(), answer.started), (Some(BackendError::Cancelled), Some(true)));
    assert!(wait_until(|| setup.mock.stats().closes.load(Ordering::SeqCst) >= 1, Duration::from_secs(3)), "the channel was not closed");
}

#[test]
fn output_over_the_limit_stops_the_command() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let (answer, out, _) = run(&mut app, SshCommand::new("flood 1000000").with_max_output_bytes(64 * 1024));
    assert!(matches!(answer.result, Err(BackendError::BodyTooLarge { limit: 65_536, .. })), "{answer:?}");
    assert!(out.len() <= 64 * 1024, "{} bytes delivered", out.len());
    // Within the limit it all arrives.
    let (answer, out, _) = run(&mut app, "flood 200000");
    assert!(answer.result.is_ok());
    assert_eq!(out.len(), 200_000);
}

#[test]
fn more_commands_than_channels_wait_for_a_free_one() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target().with_max_channels(3));
    let ids: Vec<RequestId> = (0..9).map(|i| ssh(&app).run("main", format!("echo {i}"))).collect();
    app.step();
    app.run_until(|world| world.resource::<InFlight>().is_empty(), 5000);
    app.step();
    for id in ids {
        let answers: Vec<SshFinished> = app.all_messages::<SshFinished>().into_iter().filter(|f| f.id == id).collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].result.is_ok(), "{:?}", answers[0].result);
    }
}

#[test]
fn host_keys_are_checked_strictly() {
    let setup = Setup::new(None);
    // Empty known_hosts: unknown, never trusted on first use, and the file is never written.
    let empty = setup.dir.join("empty_known_hosts");
    std::fs::write(&empty, "").unwrap_or_else(|e| panic!("{e}"));
    let mut app = app();
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key)).with_known_hosts_file(&empty);
    let info = connect(&mut app, target);
    assert!(
        matches!(&info.last_error, Some(BackendError::HostKey { problem: HostKeyProblem::Unknown, fingerprint, .. }) if *fingerprint == setup.mock.fingerprint()),
        "{info:?}"
    );
    assert_eq!(std::fs::read_to_string(&empty).unwrap_or_default(), "");
    // Another key for the host: changed.
    let other = random_key().public_key().to_openssh().unwrap_or_default();
    let changed = setup.dir.join("changed_known_hosts");
    std::fs::write(&changed, format!("[127.0.0.1]:{} {other}\n", setup.mock.port())).unwrap_or_else(|e| panic!("{e}"));
    let mut app2 = self::app();
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key)).with_known_hosts_file(&changed);
    let info = connect(&mut app2, target);
    assert!(matches!(info.last_error, Some(BackendError::HostKey { problem: HostKeyProblem::Changed, .. })), "{info:?}");
    // Revoked, even with the right line present.
    let revoked = setup.dir.join("revoked_known_hosts");
    let mock_key = setup.mock.known_hosts_line("127.0.0.1");
    let key_part = mock_key.split_once(' ').map(|(_, k)| k.to_string()).unwrap_or_default();
    std::fs::write(&revoked, format!("{mock_key}\n@revoked * {key_part}\n")).unwrap_or_else(|e| panic!("{e}"));
    let mut app3 = self::app();
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key)).with_known_hosts_file(&revoked);
    let info = connect(&mut app3, target);
    assert!(matches!(info.last_error, Some(BackendError::HostKey { problem: HostKeyProblem::Revoked, .. })), "{info:?}");
    // None of them got as far as logging in.
    assert_eq!(setup.mock.stats().logins.load(Ordering::SeqCst), 0);
}

#[test]
fn a_pinned_fingerprint_is_enough_and_a_wrong_one_is_refused() {
    let setup = Setup::new(None);
    let mut app = app();
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key));
    connected(&mut app, target.clone().trust_host_key_fingerprint(setup.mock.fingerprint()));
    let mut app2 = self::app();
    let wrong = random_key().public_key().fingerprint(Default::default()).to_string();
    let info = connect(&mut app2, target.trust_host_key_fingerprint(wrong));
    assert!(matches!(info.last_error, Some(BackendError::HostKey { problem: HostKeyProblem::Unknown, .. })), "{info:?}");
}

#[test]
fn authentication_failures_are_clear_and_never_show_secrets() {
    let setup = Setup::new(Some("fake-passphrase-1"));
    // The encrypted key with its passphrase works.
    let mut app = app();
    let target = |auth: SshAuth| SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(auth).with_known_hosts_file(&setup.known_hosts);
    connected(&mut app, target(SshAuth::key_file_with_passphrase(&setup.key, "fake-passphrase-1")));
    // A wrong passphrase, no passphrase, a missing file, another key: `AuthFailed`, and the
    // passphrase is in no text.
    let other = setup.dir.join("id_other");
    write_key(&random_key(), &other, None).unwrap_or_else(|e| panic!("{e}"));
    let cases = [
        SshAuth::key_file_with_passphrase(&setup.key, "fake-wrong-passphrase"),
        SshAuth::key_file(&setup.key),
        SshAuth::key_file(setup.dir.join("no-such-key")),
        SshAuth::key_file(&other),
    ];
    for auth in cases {
        let mut app = self::app();
        let info = connect(&mut app, target(auth));
        let error = info.last_error.unwrap_or_else(|| panic!("connected"));
        assert!(matches!(error, BackendError::AuthFailed(_)), "{error:?}");
        let text = format!("{error} {error:?}");
        assert!(!text.contains("fake-wrong-passphrase") && !text.contains("fake-passphrase-1"), "{text}");
        assert!(!text.contains(&*setup.dir.to_string_lossy()), "a full path leaked: {text}");
    }
}

#[test]
fn an_ssh_config_alias_is_resolved() {
    let setup = Setup::new(None);
    let config = setup.dir.join("config");
    let text = format!(
        "Host mock-box\n  HostName 127.0.0.1\n  Port {}\n  User {USER}\n  IdentityFile {}\n",
        setup.mock.port(),
        setup.key.to_string_lossy().replace('\\', "/")
    );
    std::fs::write(&config, text).unwrap_or_else(|e| panic!("{e}"));
    let mut app = app();
    connected(&mut app, SshTarget::from_ssh_config_file(&config, "mock-box").with_known_hosts_file(&setup.known_hosts));
    let (answer, out, _) = run(&mut app, "whoami");
    assert!(answer.result.is_ok() && out == "tester\n");
}

#[test]
fn a_refused_port_fails_fast() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        listener.local_addr().map(|a| a.port()).unwrap_or(1)
    };
    let mut app = app();
    let dir = scratch();
    let started = Instant::now();
    let info = connect(
        &mut app,
        SshTarget::new("127.0.0.1", USER).with_port(port).with_auth(SshAuth::key_file(dir.join("none"))).with_known_hosts_file(dir.join("kh")),
    );
    assert!(matches!(info.last_error, Some(BackendError::Network(_)) | Some(BackendError::InvalidRequest(_))), "{info:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_server_without_strict_kex_connects_through_aes_gcm() {
    // The reviewer's scenario: no strict key exchange, but AES-GCM (and ChaCha20) offered.
    let setup = Setup::with(None, MockOptions { no_strict_kex: true, ..MockOptions::default() });
    let mut app = app();
    connected(&mut app, setup.target());
    let (answer, out, _) = run(&mut app, "echo terrapin-safe");
    assert!(answer.result.is_ok() && out == "terrapin-safe\n");
}

#[test]
fn chacha20_without_strict_kex_is_refused_unless_explicitly_allowed() {
    let only_chacha = MockOptions { no_strict_kex: true, ciphers: Some(vec!["chacha20-poly1305@openssh.com"]), ..MockOptions::default() };
    let setup = Setup::with(None, only_chacha);
    let mut app = app();
    let info = connect(&mut app, setup.target());
    assert!(matches!(&info.last_error, Some(BackendError::Ssh(why)) if why.contains("Terrapin") && why.contains("allow_terrapin_vulnerable")), "{info:?}");
    assert_eq!(setup.mock.stats().logins.load(Ordering::SeqCst), 0, "refused before authentication");
    let mut app2 = self::app();
    connected(&mut app2, setup.target().allow_terrapin_vulnerable(true));
    // With strict key exchange, ChaCha20 alone is fine.
    let strict = Setup::with(None, MockOptions { ciphers: Some(vec!["chacha20-poly1305@openssh.com"]), ..MockOptions::default() });
    let mut app3 = self::app();
    connected(&mut app3, strict.target());
}

#[test]
fn the_key_type_in_known_hosts_is_preferred_and_another_type_is_unknown_not_changed() {
    // The server has ed25519 AND ECDSA keys; known_hosts lists only its ECDSA key: that is used.
    let setup = Setup::with(None, MockOptions { ecdsa_host_key: true, ..MockOptions::default() });
    let ecdsa = setup.mock.host_keys().get(1).cloned().unwrap_or_else(|| panic!("no ecdsa key"));
    let known = setup.dir.join("ecdsa_known_hosts");
    std::fs::write(&known, format!("{}\n", setup.mock.known_hosts_line_for("127.0.0.1", &ecdsa))).unwrap_or_else(|e| panic!("{e}"));
    let target = SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(SshAuth::key_file(&setup.key));
    let mut app = app();
    connected(&mut app, target.clone().with_known_hosts_file(&known));
    let fingerprint = app.world().resource::<SshConnections>().get("main").and_then(|c| c.fingerprint.clone());
    assert_eq!(fingerprint, Some(ecdsa.fingerprint(Default::default()).to_string()));
    // A server offering only ECDSA while known_hosts has only an ed25519 key for it: unknown.
    let only = Setup::with(None, MockOptions { only_ecdsa_host_key: true, ..MockOptions::default() });
    let old = only.dir.join("old_type_known_hosts");
    let other = random_key().public_key().to_openssh().unwrap_or_default();
    std::fs::write(&old, format!("[127.0.0.1]:{} {other}\n", only.mock.port())).unwrap_or_else(|e| panic!("{e}"));
    let mut app2 = self::app();
    let info =
        connect(&mut app2, SshTarget::new("127.0.0.1", USER).with_port(only.mock.port()).with_auth(SshAuth::key_file(&only.key)).with_known_hosts_file(&old));
    assert!(matches!(info.last_error, Some(BackendError::HostKey { problem: HostKeyProblem::Unknown, .. })), "{info:?}");
}

#[test]
fn password_and_keyboard_interactive_logins_work_and_never_leak() {
    let options = MockOptions {
        password: Some("fake-password-7".into()),
        keyboard_interactive: Some(("fake-password-7".into(), "424242".into())),
        ..MockOptions::default()
    };
    let setup = Setup::with(None, options);
    let target = |auth: SshAuth| SshTarget::new("127.0.0.1", USER).with_port(setup.mock.port()).with_auth(auth).with_known_hosts_file(&setup.known_hosts);
    let mut app = app();
    connected(&mut app, target(SshAuth::password("fake-password-7")));
    let (answer, out, _) = run(&mut app, "whoami");
    assert!(answer.result.is_ok() && out == "tester\n");
    let mut app2 = self::app();
    connected(
        &mut app2,
        target(SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("password", "fake-password-7").answer_containing("code", "424242"))),
    );
    for (auth, what) in [
        (SshAuth::password("fake-wrong-9"), "password"),
        (
            SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("password", "fake-password-7").answer_containing("code", "000000")),
            "keyboard-interactive",
        ),
        (SshAuth::keyboard_interactive(SshPromptAnswers::new().answer_containing("password", "fake-password-7")), "keyboard-interactive"),
    ] {
        let mut app = self::app();
        let info = connect(&mut app, target(auth));
        let error = info.last_error.unwrap_or_else(|| panic!("connected"));
        let text = format!("{error} {error:?}");
        assert!(matches!(error, BackendError::AuthFailed(_)) && text.contains(what), "{text}");
        assert!(!text.contains("fake-password-7") && !text.contains("fake-wrong-9") && !text.contains("424242"), "{text}");
    }
}

#[test]
fn an_ssh_config_that_includes_itself_is_an_error_not_a_crash() {
    let setup = Setup::new(None);
    let config = setup.dir.join("config");
    let path = config.to_string_lossy().replace('\\', "/");
    std::fs::write(&config, format!("Include {path}\nHost mock-box\n  HostName 127.0.0.1\n")).unwrap_or_else(|e| panic!("{e}"));
    let mut app = app();
    let info = connect(&mut app, SshTarget::from_ssh_config_file(&config, "mock-box").with_known_hosts_file(&setup.known_hosts));
    assert!(matches!(&info.last_error, Some(BackendError::InvalidRequest(why)) if why.contains("deeper than 16")), "{info:?}");
}

/// A hostile peer on 127.0.0.1: accepts one connection and plays `behave` on it (bounded by the
/// test's own lifetime: the thread ends once the client is gone or after 20 s).
fn hostile(behave: fn(&mut std::net::TcpStream)) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
            behave(&mut stream);
        }
    });
    port
}

fn connect_hostile(port: u16) -> (SshConnectionInfo, Duration) {
    let dir = scratch();
    let known = dir.join("known_hosts");
    std::fs::write(&known, "").unwrap_or_else(|e| panic!("{e}"));
    let mut app = app();
    let started = Instant::now();
    let target = SshTarget::new("127.0.0.1", USER)
        .with_port(port)
        .with_auth(SshAuth::key_file(dir.join("none")))
        .with_known_hosts_file(known)
        .with_connect_timeout(Duration::from_secs(1));
    let info = connect(&mut app, target);
    (info, started.elapsed())
}

fn trickle(stream: &mut std::net::TcpStream, bytes: &[u8], every: Duration, limit: Duration) {
    let start = Instant::now();
    for byte in bytes.iter().cycle() {
        if start.elapsed() > limit || stream.write_all(&[*byte]).is_err() {
            return;
        }
        std::thread::sleep(every);
        let mut sink = [0u8; 256];
        let _ = stream.read(&mut sink);
    }
}

#[test]
fn a_silent_server_is_given_up_at_the_connect_deadline() {
    let port = hostile(|stream| {
        let start = Instant::now();
        let mut sink = [0u8; 256];
        while start.elapsed() < Duration::from_secs(20) {
            match stream.read(&mut sink) {
                Ok(0) => return,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    let (info, took) = connect_hostile(port);
    assert!(matches!(&info.last_error, Some(BackendError::Timeout(why)) if why.contains("1s")), "{info:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

#[test]
fn a_trickled_banner_cannot_stretch_the_connect_deadline() {
    // A valid-looking banner, one byte every 150 ms, forever.
    let port =
        hostile(|stream| trickle(stream, b"SSH-2.0-OpenSSH_9.9 trickle trickle trickle trickle\r\n", Duration::from_millis(150), Duration::from_secs(20)));
    let (info, took) = connect_hostile(port);
    assert!(matches!(info.last_error, Some(BackendError::Timeout(_))), "{info:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

#[test]
fn a_banner_then_silence_during_key_exchange_hits_the_deadline() {
    let port = hostile(|stream| {
        let _ = stream.write_all(b"SSH-2.0-OpenSSH_9.9\r\n");
        let start = Instant::now();
        let mut sink = [0u8; 4096];
        while start.elapsed() < Duration::from_secs(20) {
            match stream.read(&mut sink) {
                Ok(0) => return,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    let (info, took) = connect_hostile(port);
    assert!(matches!(info.last_error, Some(BackendError::Timeout(_))), "{info:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

#[test]
fn a_huge_banner_is_an_error_not_a_hang() {
    let port = hostile(|stream| {
        let line = vec![b'A'; 64 * 1024];
        for _ in 0..30 {
            if stream.write_all(&line).is_err() {
                return;
            }
        }
    });
    let (info, took) = connect_hostile(port);
    assert!(info.last_error.is_some(), "{info:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

#[test]
fn a_server_that_goes_away_is_reported_lost() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target().with_keepalive(Duration::from_secs(1), 2));
    let id = ssh(&app).run("main", "hang");
    app.step_n(20);
    drop(setup.mock);
    app.run_until(|world| world.resource::<SshConnections>().state("main") == Some(SshState::Disconnected), 5000);
    assert_eq!(app.world().resource::<SshConnections>().state("main"), Some(SshState::Disconnected));
    let (answer, _, _) = wait(&mut app, id);
    assert!(matches!(answer.result, Err(BackendError::Disconnected { .. })), "{answer:?}");
    let _ = (setup.dir, setup.key, setup.known_hosts);
}

#[test]
fn app_exit_answers_at_once_and_does_not_wait_for_the_server() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    let id = ssh(&app).run("main", "hang");
    assert!(wait_until(
        || {
            app.step();
            setup.mock.stats().execs.load(Ordering::SeqCst) >= 1
        },
        Duration::from_secs(3)
    ));
    let late = ssh(&app).run("main", "echo never");
    let execs = setup.mock.stats().execs.load(Ordering::SeqCst);
    app.world_mut().write_message(AppExit::Success);
    let started = Instant::now();
    app.step();
    assert!(started.elapsed() < Duration::from_millis(500), "exit took {:?}", started.elapsed());
    let answers = app.all_messages::<SshFinished>();
    let find = |id| answers.iter().find(|f| f.id == id).map(|f| (f.result.clone().err(), f.started));
    assert_eq!(find(id), Some((Some(BackendError::Shutdown), Some(true))));
    assert_eq!(find(late), Some((Some(BackendError::Shutdown), Some(false))));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(setup.mock.stats().execs.load(Ordering::SeqCst), execs, "a command of the exit frame reached the server");
}

#[test]
fn disconnect_closes_the_connection() {
    let setup = Setup::new(None);
    let mut app = app();
    connected(&mut app, setup.target());
    ssh(&app).disconnect("main");
    app.step_n(3);
    assert_eq!(app.world().resource::<SshConnections>().state("main"), Some(SshState::Disconnected));
    // The server sees the connection end (its session ends, so it accepts a new one later).
    let mut app2 = self::app();
    connected(&mut app2, setup.target());
}

#[cfg(feature = "sftp")]
mod sftp {
    use std::path::Path;

    use super::*;

    fn op(app: &mut TestApp, id: RequestId) -> SftpFinished {
        app.step();
        app.run_until(|world| world.resource::<InFlight>().describe(id).is_none(), 5000);
        app.step();
        let mut answers: Vec<SftpFinished> = app.all_messages::<SftpFinished>().into_iter().filter(|f| f.id == id).collect();
        assert_eq!(answers.len(), 1, "{answers:?}");
        answers.remove(0)
    }

    #[test]
    fn upload_list_download_rename_remove() {
        let setup = Setup::new(None);
        let mut app = app();
        connected(&mut app, setup.target());
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let id = ssh(&app).create_dir("main", "work");
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Done));
        let id = ssh(&app).upload("main", "work/data.bin", data.clone());
        let answer = op(&mut app, id);
        assert_eq!((answer.result, answer.started), (Ok(SftpOutcome::Uploaded { bytes: 300_000 }), Some(true)));
        assert_eq!(setup.mock.file("work/data.bin"), Some(data.clone()));
        assert!(app.all_messages::<SftpProgress>().iter().any(|p| p.id == id && p.done == 300_000));
        let id = ssh(&app).list_dir("main", "work");
        match op(&mut app, id).result {
            Ok(SftpOutcome::Listing(entries)) => {
                assert_eq!(entries.len(), 1);
                assert_eq!((entries[0].name.as_str(), entries[0].kind, entries[0].size), ("data.bin", SftpEntryKind::File, Some(300_000)));
            }
            other => panic!("{other:?}"),
        }
        let id = ssh(&app).download("main", "work/data.bin");
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Data(data.clone())));
        let local = setup.dir.join("downloaded.bin");
        let id = ssh(&app).download_file("main", "work/data.bin", &local);
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Downloaded { bytes: 300_000 }));
        assert_eq!(std::fs::read(&local).ok(), Some(data.clone()));
        let id = ssh(&app).upload_file("main", &local, "work/copy.bin");
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Uploaded { bytes: 300_000 }));
        let id = ssh(&app).rename("main", "work/copy.bin", "work/moved.bin");
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Done));
        assert_eq!(setup.mock.file("work/moved.bin"), Some(data));
        for id in [ssh(&app).remove_file("main", "work/data.bin"), ssh(&app).remove_file("main", "work/moved.bin")] {
            assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Done));
        }
        let id = ssh(&app).remove_dir("main", "work");
        assert_eq!(op(&mut app, id).result, Ok(SftpOutcome::Done));
    }

    #[test]
    fn errors_carry_the_servers_words_and_limits_hold() {
        let setup = Setup::new(None);
        let mut app = app();
        connected(&mut app, setup.target().with_max_transfer_bytes(64 * 1024));
        let id = ssh(&app).download("main", "missing.txt");
        assert!(matches!(op(&mut app, id).result, Err(BackendError::Ssh(why)) if why.starts_with("SFTP:")));
        setup.mock.put_file("big.bin", &vec![7u8; 200_000]);
        let id = ssh(&app).download("main", "big.bin");
        assert!(matches!(op(&mut app, id).result, Err(BackendError::BodyTooLarge { limit: 65_536, .. })));
        // A failed download to a file leaves neither the file nor its part file.
        let local = setup.dir.join("big.bin");
        let id = ssh(&app).download_file("main", "big.bin", &local);
        assert!(matches!(op(&mut app, id).result, Err(BackendError::BodyTooLarge { .. })));
        assert!(!local.exists() && !Path::new(&format!("{}.part", local.display())).exists());
        // Uploads over the limit, from memory and from a file, get the same answer and are
        // refused before anything is sent.
        let id = ssh(&app).upload("main", "too-big.bin", vec![0u8; 100_000]);
        let answer = op(&mut app, id);
        assert!(matches!(answer.result, Err(BackendError::RequestTooLarge { limit: 65_536, size: 100_000, .. })), "{:?}", answer.result);
        assert_eq!(answer.started, Some(false));
        let big_local = setup.dir.join("too-big-local.bin");
        std::fs::write(&big_local, vec![0u8; 100_000]).unwrap_or_else(|e| panic!("{e}"));
        let id = ssh(&app).upload_file("main", &big_local, "too-big-file.bin");
        let answer = op(&mut app, id);
        assert!(matches!(answer.result, Err(BackendError::RequestTooLarge { limit: 65_536, size: 100_000, .. })), "{:?}", answer.result);
        assert_eq!(answer.started, Some(false));
        assert_eq!(setup.mock.file("too-big.bin"), None);
        assert_eq!(setup.mock.file("too-big-file.bin"), None);
    }
}
