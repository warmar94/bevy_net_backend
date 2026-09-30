//! Live SSH checks against a real OpenSSH server. `#[ignore]`d: they only run on request, and the
//! server comes from environment variables (never from a file in this crate):
//!
//! ```text
//! BNB_TEST_HOST=<host> BNB_TEST_SSH_USER=<user> BNB_TEST_SSH_KEY=<private key file> \
//! BNB_TEST_SSH_KNOWN_HOSTS=<known_hosts file> \
//!   cargo test --features ssh,sftp --test live_ssh -- --ignored --test-threads 1
//! ```
//!
//! Optional: `BNB_TEST_SSH_PORT` (default 22), `BNB_TEST_SSH_PASSPHRASE` (for an encrypted key).
//! They run harmless commands only (`echo`, `uname`, `whoami`, `sleep`), and SFTP only inside a
//! new temporary directory in the user's home that they remove again.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

struct Live {
    host: String,
    user: String,
    key: String,
    known_hosts: String,
    port: u16,
    passphrase: Option<String>,
}

fn live() -> Option<Live> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    Some(Live {
        host: var("BNB_TEST_HOST")?,
        user: var("BNB_TEST_SSH_USER")?,
        key: var("BNB_TEST_SSH_KEY")?,
        known_hosts: var("BNB_TEST_SSH_KNOWN_HOSTS")?,
        port: var("BNB_TEST_SSH_PORT").and_then(|p| p.parse().ok()).unwrap_or(22),
        passphrase: var("BNB_TEST_SSH_PASSPHRASE"),
    })
}

fn target(live: &Live) -> SshTarget {
    let auth = match &live.passphrase {
        Some(passphrase) => SshAuth::key_file_with_passphrase(&live.key, passphrase.as_str()),
        None => SshAuth::key_file(&live.key),
    };
    SshTarget::new(&live.host, &live.user).with_port(live.port).with_auth(auth).with_known_hosts_file(&live.known_hosts)
}

fn app() -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(5)).build();
    app.add_plugins(BackendPlugin::default());
    app.watch::<SshStateChanged>().watch::<SshOutput>().watch::<SshFinished>();
    #[cfg(feature = "sftp")]
    app.watch::<SftpFinished>();
    app
}

fn connect(app: &mut TestApp, live: &Live) {
    app.world().resource::<SshClient>().connect("live", target(live));
    app.run_until(|world| world.resource::<SshConnections>().state("live").is_some_and(|s| s != SshState::Connecting), 6000);
    let info = app.world().resource::<SshConnections>().get("live").cloned();
    assert_eq!(info.as_ref().map(|i| i.state), Some(SshState::Connected), "{info:?}");
}

fn run(app: &mut TestApp, command: impl Into<SshCommand>) -> (SshFinished, String) {
    let id = app.world().resource::<SshClient>().run("live", command);
    app.step();
    app.run_until(|world| world.resource::<InFlight>().describe(id).is_none(), 10_000);
    app.step();
    let output: String = app.all_messages::<SshOutput>().into_iter().filter(|o| o.id == id).map(|o| o.text()).collect();
    let answer = app.all_messages::<SshFinished>().into_iter().find(|f| f.id == id).unwrap_or_else(|| panic!("no answer"));
    (answer, output)
}

#[test]
#[ignore = "live: needs BNB_TEST_HOST, BNB_TEST_SSH_USER, BNB_TEST_SSH_KEY, BNB_TEST_SSH_KNOWN_HOSTS"]
fn live_harmless_commands() {
    let Some(live) = live() else { panic!("set BNB_TEST_HOST, BNB_TEST_SSH_USER, BNB_TEST_SSH_KEY and BNB_TEST_SSH_KNOWN_HOSTS") };
    let mut app = app();
    connect(&mut app, &live);
    let (answer, output) = run(&mut app, "echo bevy_net_backend-live");
    assert!(answer.result.as_ref().is_ok_and(SshExit::success), "{answer:?}");
    assert_eq!(output.trim(), "bevy_net_backend-live");
    let (answer, output) = run(&mut app, "whoami");
    assert!(answer.result.is_ok());
    assert_eq!(output.trim(), live.user);
    let (answer, output) = run(&mut app, "uname -s");
    assert!(answer.result.is_ok() && !output.trim().is_empty());
    // A timeout stops a running command and says it had started.
    let (answer, _) = run(&mut app, SshCommand::new("sleep 30").with_timeout(Duration::from_millis(800)));
    assert!(matches!(&answer.result, Err(BackendError::Timeout(_))), "{answer:?}");
    assert_eq!(answer.started, Some(true));
    app.world().resource::<SshClient>().disconnect("live");
    app.step_n(5);
}

#[test]
#[ignore = "live: needs BNB_TEST_HOST, BNB_TEST_SSH_USER, BNB_TEST_SSH_KEY, BNB_TEST_SSH_KNOWN_HOSTS"]
fn live_unknown_host_key_is_refused() {
    let Some(live) = live() else { panic!("set the BNB_TEST_* variables") };
    let mut app = app();
    // An empty known_hosts file: the real server's key must be refused, not learned.
    let empty = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("live-empty-known-hosts");
    std::fs::write(&empty, "").unwrap_or_else(|e| panic!("{e}"));
    let target = SshTarget::new(&live.host, &live.user).with_port(live.port).with_auth(SshAuth::key_file(&live.key)).with_known_hosts_file(&empty);
    app.world().resource::<SshClient>().connect("live", target);
    app.run_until(|world| world.resource::<SshConnections>().state("live").is_some_and(|s| s != SshState::Connecting), 6000);
    let error = app.world().resource::<SshConnections>().get("live").and_then(|c| c.last_error.clone());
    assert!(matches!(error, Some(BackendError::HostKey { problem: HostKeyProblem::Unknown, .. })), "{error:?}");
}

#[cfg(feature = "sftp")]
#[test]
#[ignore = "live: needs BNB_TEST_HOST, BNB_TEST_SSH_USER, BNB_TEST_SSH_KEY, BNB_TEST_SSH_KNOWN_HOSTS"]
fn live_sftp_in_a_temporary_directory() {
    let Some(live) = live() else { panic!("set the BNB_TEST_* variables") };
    let mut app = app();
    connect(&mut app, &live);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = format!("bnb-live-sftp-{stamp}");
    let file = format!("{dir}/hello.txt");
    fn client(app: &TestApp) -> &SshClient {
        app.world().resource::<SshClient>()
    }
    let wait = |app: &mut TestApp, id: RequestId| -> SftpFinished {
        app.step();
        app.run_until(|world| world.resource::<InFlight>().describe(id).is_none(), 10_000);
        app.step();
        app.all_messages::<SftpFinished>().into_iter().find(|f| f.id == id).unwrap_or_else(|| panic!("no answer"))
    };
    let id = client(&app).create_dir("live", &dir);
    assert!(wait(&mut app, id).result.is_ok());
    let id = client(&app).upload("live", &file, b"hello from bevy_net_backend".to_vec());
    assert_eq!(wait(&mut app, id).result, Ok(SftpOutcome::Uploaded { bytes: 27 }));
    let id = client(&app).download("live", &file);
    assert_eq!(wait(&mut app, id).result, Ok(SftpOutcome::Data(b"hello from bevy_net_backend".to_vec())));
    let id = client(&app).list_dir("live", &dir);
    assert!(matches!(wait(&mut app, id).result, Ok(SftpOutcome::Listing(ref entries)) if entries.len() == 1));
    let id = client(&app).remove_file("live", &file);
    assert!(wait(&mut app, id).result.is_ok());
    let id = client(&app).remove_dir("live", &dir);
    assert!(wait(&mut app, id).result.is_ok());
}
