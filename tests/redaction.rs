//! Secrets never reach the logs: every `tracing` event of this crate (at every level, TRACE
//! included) is captured while requests with every kind of credential run, succeed, fail and are
//! cancelled; none contains a secret. (Debug / Display redaction is unit-tested in the crate.)
//! Own test binary: it installs a process-wide subscriber.

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

    let logs = capture.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert!(logs.contains(">>> NET-BACKEND"), "nothing captured:\n{logs}");
    assert!(logs.contains("not sent"), "the refused request is logged:\n{logs}");
    for secret in SECRETS {
        assert!(!logs.contains(secret), "`{secret}` was logged:\n{logs}");
    }
}
