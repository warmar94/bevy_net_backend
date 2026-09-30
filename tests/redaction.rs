//! Secrets never reach the logs: every `tracing` event of this crate (at every level, TRACE
//! included) is captured while requests with every kind of credential run, succeed, fail and are
//! cancelled; none contains a secret. With feature `ssh` also a real SSH connection to the mock
//! (encrypted key + passphrase, a wrong passphrase, a command line, stdin and output holding
//! secrets). (Debug / Display redaction is unit-tested in the crate.) Own test binary: it
//! installs a process-wide subscriber.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::http::{Method, StatusCode};
use bevy_net_backend::*;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

const SECRETS: [&str; 4] = ["fake-bearer-7f3a", "fake-header-9c2e", "fake-query-41bd", "fake-field-d00d"];
#[cfg(feature = "ssh")]
const SSH_SECRETS: [&str; 4] = ["fake-ssh-pass-5e1f", "fake-ssh-cmd-77aa", "fake-ssh-stdin-9b9b", "fake-ssh-wrong-0a0a"];

#[cfg(feature = "ssh")]
#[allow(dead_code)]
#[path = "../examples/mock_ssh_server.rs"]
mod mock_ssh_server;

/// Puts one kind of credentials into the resource.
type SetCredentials = dyn Fn(&mut BackendCredentials);

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        all.push_str(event.metadata().target());
        all.push_str(": ");
        all.push_str(&line);
        all.push('\n');
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn no_secret_is_ever_logged() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap_or_else(|e| panic!("{e}"));

    let fake = FakeHttpTransport::new();
    fake.route(Method::GET, "/ok", Ok(RawResponse::new(StatusCode::OK, format!(r#"{{"token":"{}"}}"#, SECRETS[0]))));
    fake.route(Method::POST, "/ok", Ok(RawResponse::new(StatusCode::UNAUTHORIZED, SECRETS[1])));
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(fake.clone())).add_plugins(BackendPlugin::new(
        HttpConfig::new("https://api.example.com").with_header("X-Api-Key", SECRETS[1]).with_timeout(Duration::from_millis(1)),
    ));

    let credentials: Vec<Box<SetCredentials>> = vec![
        Box::new(|c| c.set(BearerToken::new(SECRETS[0]))),
        Box::new(|c| c.set(ApiKeyHeader::new("X-Key", SECRETS[1]))),
        Box::new(|c| c.set(ApiKeyQuery::new("key", SECRETS[2]))),
        // Invalid as a header value: the request is refused, and the reason must not quote it.
        Box::new(|c| c.set(BearerToken::new(format!("{}\n", SECRETS[0])))),
        #[cfg(feature = "json")]
        Box::new(|c| c.set(JsonBodyField::new("token", SECRETS[3]))),
    ];
    for set in credentials {
        set(&mut app.world_mut().resource_mut::<BackendCredentials>());
        let backend = app.world().resource::<HttpClient>();
        backend.get("/ok");
        backend.send(OutgoingRequest::post("/ok").with_body(br#"{"a":1}"#.to_vec()).with_query("q", SECRETS[2]));
        let never = backend.get("/never");
        backend.get("/timeout");
        app.step();
        app.world().resource::<HttpClient>().cancel(never);
        app.step_n(400);
    }
    app.world_mut().write_message(AppExit::Success);
    app.step();
    #[cfg(feature = "ssh")]
    ssh_part();

    let logs = capture.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(logs.contains(">>> NET-BACKEND"), "nothing captured:\n{logs}");
    assert!(logs.contains("not sent"), "the refused request is logged:\n{logs}");
    for secret in SECRETS {
        assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
    }
    #[cfg(feature = "ssh")]
    {
        assert!(logs.contains("ssh `main` connected"), "the SSH part logged nothing:\n{logs}");
        for secret in SSH_SECRETS {
            assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
        }
    }
}

/// A real SSH connection (to the mock) whose passphrase, command line, stdin and output are
/// secrets, plus a failed login with a wrong passphrase.
#[cfg(feature = "ssh")]
fn ssh_part() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("redaction-ssh-{}", std::process::id()));
    let client = mock_ssh_server::random_key();
    let key = dir.join("id_test");
    mock_ssh_server::write_key(&client, &key, Some(SSH_SECRETS[0])).unwrap_or_else(|e| panic!("{e}"));
    let mock = mock_ssh_server::MockSshServer::start("tester", client.public_key().clone()).unwrap_or_else(|e| panic!("{e}"));
    let target = |passphrase: &str| {
        SshTarget::new("127.0.0.1", "tester")
            .with_port(mock.port())
            .with_auth(SshAuth::key_file_with_passphrase(&key, passphrase))
            .trust_host_key_fingerprint(mock.fingerprint())
    };
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default());
    fn ssh(app: &TestApp) -> &SshClient {
        app.world().resource::<SshClient>()
    }
    ssh(&app).connect("main", target(SSH_SECRETS[0]));
    ssh(&app).connect("wrong", target(SSH_SECRETS[3]));
    ssh(&app).run("main", format!("echo {}", SSH_SECRETS[1]));
    ssh(&app).run("main", SshCommand::new("cat").with_stdin(SSH_SECRETS[2].as_bytes().to_vec()));
    ssh(&app).run("main", format!("stderr {}", SSH_SECRETS[1]));
    app.step();
    app.run_until(|world| world.resource::<InFlight>().is_empty(), 3000);
    app.step_n(5);
    app.world_mut().write_message(AppExit::Success);
    app.step();
}
