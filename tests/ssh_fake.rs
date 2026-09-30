//! Named SSH connections driven by the `FakeSshTransport`: states, exactly-one answers (exit,
//! error, cancel, timeout, connect failure, loss, exit), honest `started`, output before the
//! answer, limits, the shared cancel path with HTTP (and WebSocket), and SFTP (feature `sftp`).
//! Strict `TestApp` (simulated time), no sockets, no files.

use std::time::Duration;

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

const FINGERPRINT: &str = FakeSshTransport::FINGERPRINT;

fn app(fake: &FakeSshTransport) -> TestApp {
    let mut app = TestApp::new();
    app.insert_resource(SshTransportRes::new(fake.clone())).add_plugins(BackendPlugin::default());
    app.watch::<SshStateChanged>().watch::<SshOutput>().watch::<SshFinished>();
    app
}

fn ssh(app: &TestApp) -> &SshClient {
    app.world().resource::<SshClient>()
}

fn target() -> SshTarget {
    SshTarget::new("build.example.com", "deploy").with_auth(SshAuth::agent()).with_known_hosts_file("/nowhere/known_hosts")
}

fn state(app: &TestApp, name: &str) -> Option<SshState> {
    app.world().resource::<SshConnections>().state(name)
}

fn finished(app: &TestApp, id: RequestId) -> Vec<SshFinished> {
    app.all_messages::<SshFinished>().into_iter().filter(|f| f.id == id).collect()
}

fn one(app: &TestApp, id: RequestId) -> SshFinished {
    let mut all = finished(app, id);
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

fn connected(fake: &FakeSshTransport) -> TestApp {
    let mut app = app(fake);
    ssh(&app).connect("main", target());
    app.step_n(2);
    assert_eq!(state(&app, "main"), Some(SshState::Connected));
    app
}

#[test]
fn connect_reports_connecting_then_connected_with_the_fingerprint() {
    let fake = FakeSshTransport::new();
    let mut app = app(&fake);
    ssh(&app).connect("main", target().with_port(2222));
    app.step_n(2);
    let states: Vec<SshState> = app.all_messages::<SshStateChanged>().into_iter().map(|c| c.state).collect();
    assert_eq!(states, vec![SshState::Connecting, SshState::Connected]);
    let (_, seen) = fake.connects().remove(0);
    assert_eq!((seen.host(), seen.port(), seen.user()), ("build.example.com", Some(2222), Some("deploy")));
    let info = app.world().resource::<SshConnections>().get("main").cloned().unwrap_or_else(|| panic!("no info"));
    assert_eq!(info.fingerprint.as_deref(), Some(FINGERPRINT));
}

#[test]
fn a_command_streams_output_then_one_answer_with_the_exit() {
    let fake = FakeSshTransport::new();
    fake.on_command("uname -a", &[(SshStream::Stdout, "MockOS\n"), (SshStream::Stderr, "warning\n")], Ok(SshExit::with_status(0).with_output_bytes(7, 8)));
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "uname -a");
    app.step_n(2);
    let output: Vec<(SshStream, String)> = app.all_messages::<SshOutput>().into_iter().filter(|o| o.id == id).map(|o| (o.stream, o.text())).collect();
    assert_eq!(output, vec![(SshStream::Stdout, "MockOS\n".to_string()), (SshStream::Stderr, "warning\n".to_string())]);
    let answer = one(&app, id);
    assert_eq!(answer.started, Some(true));
    assert_eq!(answer.name, "main");
    let exit = answer.result.unwrap_or_else(|e| panic!("{e}"));
    assert!(exit.success());
    // The transport got the connection's defaults filled in.
    let (_, _, command) = fake.commands().remove(0);
    assert_eq!((command.timeout(), command.max_output_bytes()), (Some(DEFAULT_SSH_COMMAND_TIMEOUT), Some(DEFAULT_SSH_MAX_OUTPUT_BYTES)));
    assert!(app.world().resource::<InFlight>().is_empty());
}

#[test]
fn a_non_zero_exit_is_still_ok() {
    let fake = FakeSshTransport::new();
    fake.on_command("false", &[], Ok(SshExit::with_status(1)));
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "false");
    app.step_n(2);
    let exit = one(&app, id).result.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((exit.status, exit.success()), (Some(1), false));
}

#[test]
fn commands_made_while_connecting_wait_and_go_out_in_order() {
    let fake = FakeSshTransport::new();
    fake.manual_connect(true);
    let mut app = app(&fake);
    ssh(&app).connect("main", target());
    let first = ssh(&app).run("main", "echo 1");
    let second = ssh(&app).run("main", "echo 2");
    app.step_n(3);
    assert!(fake.commands().is_empty(), "sent before the connection was up");
    assert_eq!(app.world().resource::<InFlight>().describe(first).map(|i| i.kind), Some(RequestKind::Ssh));
    assert_eq!(app.world().resource::<SshConnections>().get("main").map(|c| c.pending_requests), Some(2));
    let conn = fake.last_conn().unwrap_or_else(|| panic!("no conn"));
    fake.accept(conn);
    app.step();
    let ids: Vec<RequestId> = fake.commands().into_iter().map(|(_, id, _)| id).collect();
    assert_eq!(ids, vec![first, second]);
}

#[test]
fn a_failed_connect_answers_the_waiting_commands_as_never_sent() {
    let fake = FakeSshTransport::new();
    fake.reject_next(BackendError::host_key("build.example.com", "SHA256:x", HostKeyProblem::Changed));
    let mut app = app(&fake);
    ssh(&app).connect("main", target());
    let id = ssh(&app).run("main", "uname");
    app.step_n(2);
    let change = app.messages::<SshStateChanged>().pop().unwrap_or_else(|| panic!("no change"));
    assert_eq!(change.state, SshState::Disconnected);
    assert!(matches!(change.error, Some(BackendError::HostKey { problem: HostKeyProblem::Changed, .. })));
    let answer = one(&app, id);
    assert_eq!(answer.started, Some(false));
    assert!(matches!(&answer.result, Err(e) if e.was_sent() == Some(false)), "{:?}", answer.result);
    assert!(fake.commands().is_empty());
    // A command on the disconnected connection is refused at once, never sent.
    let later = ssh(&app).run("main", "uname");
    app.step_n(2);
    assert!(matches!(one(&app, later).result, Err(BackendError::Disconnected { sent: Some(false), .. })));
}

#[test]
fn unknown_connections_and_bad_commands_are_invalid_and_never_sent() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let unknown = ssh(&app).run("nope", "uname");
    let empty = ssh(&app).run("main", "   ");
    let nul = ssh(&app).run("main", "echo \0");
    let long = ssh(&app).run("main", "x".repeat(MAX_SSH_COMMAND_BYTES + 1));
    app.step_n(2);
    for id in [unknown, empty, nul] {
        let answer = one(&app, id);
        assert!(matches!(answer.result, Err(BackendError::InvalidRequest(_))), "{:?}", answer.result);
        assert_eq!(answer.started, Some(false));
    }
    let answer = one(&app, long);
    assert!(matches!(answer.result, Err(BackendError::RequestTooLarge { limit: 65_536, .. })), "{:?}", answer.result);
    assert_eq!(answer.started, Some(false));
    assert!(fake.commands().is_empty());
}

#[test]
fn invalid_targets_never_reach_the_transport() {
    let fake = FakeSshTransport::new();
    let mut app = app(&fake);
    ssh(&app).connect("no-auth", SshTarget::new("host.example.com", "deploy"));
    ssh(&app).connect("valid", target());
    ssh(&app).connect("bad-pin", target().trust_host_key_fingerprint("MD5:aa:bb"));
    ssh(&app).connect("space", SshTarget::new("host name", "deploy").with_auth(SshAuth::agent()));
    app.step_n(2);
    assert_eq!(fake.connects().len(), 1, "only the valid target connects");
    for name in ["no-auth", "bad-pin", "space"] {
        assert_eq!(state(&app, name), Some(SshState::Disconnected), "{name}");
        let error = app.world().resource::<SshConnections>().get(name).and_then(|c| c.last_error.clone());
        assert!(matches!(error, Some(BackendError::InvalidRequest(_))), "{name}: {error:?}");
    }
}

#[test]
fn cancel_while_waiting_is_never_sent() {
    let fake = FakeSshTransport::new();
    fake.manual_connect(true);
    let mut app = app(&fake);
    ssh(&app).connect("main", target());
    let id = ssh(&app).run("main", "sleep 1000");
    app.step();
    ssh(&app).cancel(id);
    app.step_n(2);
    let answer = one(&app, id);
    assert_eq!((answer.result.err(), answer.started), (Some(BackendError::Cancelled), Some(false)));
    fake.accept(fake.last_conn().unwrap_or_else(|| panic!("no conn")));
    app.step_n(2);
    assert!(fake.commands().is_empty());
}

#[test]
fn cancel_in_the_same_frame_is_never_sent() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "sleep 1000");
    ssh(&app).cancel(id);
    app.step_n(2);
    assert_eq!(one(&app, id).result.err(), Some(BackendError::Cancelled));
    assert!(fake.commands().is_empty());
}

#[test]
fn cancel_while_running_answers_once_says_it_started_and_drops_late_results() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    assert_eq!(fake.running(), vec![id]);
    // Through the HTTP client: one shared cancel path.
    app.world().resource::<HttpClient>().cancel(id);
    app.step_n(2);
    let answer = one(&app, id);
    assert_eq!((answer.result.err(), answer.started), (Some(BackendError::Cancelled), Some(true)));
    assert_eq!(fake.cancelled(), vec![id]);
    fake.output(id, SshStream::Stdout, b"late");
    fake.finish(id, Ok(SshExit::with_status(0)));
    app.step_n(2);
    assert_eq!(finished(&app, id).len(), 1);
    assert!(app.all_messages::<SshOutput>().is_empty(), "output after the answer was delivered");
}

#[test]
fn the_backstop_answers_a_command_the_transport_forgets() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", SshCommand::new("hang").with_timeout(Duration::from_millis(100)));
    app.step();
    // 100 ms + DEADLINE_GRACE (5 s) at 1/64 s per frame.
    let frames = app.run_until(|world| world.resource::<InFlight>().is_empty(), 1000);
    assert!(frames > 300, "answered after {frames} frames");
    let answer = one(&app, id);
    assert!(matches!(&answer.result, Err(BackendError::Timeout(why)) if !why.starts_with("not sent")), "{:?}", answer.result);
    assert_eq!(answer.started, Some(true));
    assert_eq!(fake.cancelled(), vec![id]);
}

#[test]
fn a_command_waiting_for_a_connection_that_never_opens_times_out_as_not_sent() {
    let fake = FakeSshTransport::new();
    fake.manual_connect(true);
    let mut app = app(&fake);
    ssh(&app).connect("main", target().with_connect_timeout(Duration::from_secs(1)));
    let id = ssh(&app).run("main", "uname");
    app.step();
    let frames = app.run_until(|world| world.resource::<InFlight>().is_empty(), 1000);
    assert!(frames > 300, "answered after {frames} frames");
    let answer = one(&app, id);
    assert_eq!(answer.started, Some(false));
    // Either the connect backstop (the transport never reported) or the waiting deadline: never sent.
    assert!(matches!(&answer.result, Err(e) if e.was_sent() == Some(false)), "{:?}", answer.result);
    // The connection itself is given up too.
    app.step_n(2);
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
    assert_eq!(fake.closed().len(), 1);
}

#[test]
fn a_lost_connection_answers_every_request_honestly() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let running = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    let conn = fake.last_conn().unwrap_or_else(|| panic!("no conn"));
    fake.drop_conn(conn, BackendError::Network("connection reset by peer".into()));
    app.step();
    let answer = one(&app, running);
    assert_eq!(answer.started, Some(true));
    assert!(matches!(answer.result, Err(BackendError::Disconnected { sent: Some(true), .. })), "{:?}", answer.result);
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
    // No automatic reconnect.
    app.step_n(5);
    assert_eq!(fake.connects().len(), 1);
}

#[test]
fn disconnect_by_the_game_answers_and_closes() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    ssh(&app).disconnect("main");
    app.step_n(2);
    assert!(matches!(one(&app, id).result, Err(BackendError::Disconnected { sent: Some(true), .. })));
    assert_eq!(fake.closed().len(), 1);
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
    let change = app.all_messages::<SshStateChanged>().pop().unwrap_or_else(|| panic!("no change"));
    assert_eq!((change.state, change.error), (SshState::Disconnected, None));
}

#[test]
fn reconnect_with_the_same_name_replaces_the_connection() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let old = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    ssh(&app).connect("main", target().with_port(2200));
    app.step_n(2);
    assert!(matches!(one(&app, old).result, Err(BackendError::Disconnected { .. })));
    assert_eq!(fake.connects().len(), 2);
    assert_eq!(state(&app, "main"), Some(SshState::Connected));
}

#[test]
fn app_exit_answers_everything_shutdown_and_sends_nothing_of_the_exit_frame() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let running = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    let late = ssh(&app).run("main", "uname");
    app.world_mut().write_message(AppExit::Success);
    app.step();
    let answer = one(&app, running);
    assert_eq!((answer.result.err(), answer.started), (Some(BackendError::Shutdown), Some(true)));
    let answer = one(&app, late);
    assert_eq!((answer.result.err(), answer.started), (Some(BackendError::Shutdown), Some(false)));
    assert_eq!(fake.commands().len(), 1, "the exit frame's command was sent");
    assert_eq!(fake.shutdown_count(), 1);
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
}

#[test]
fn too_many_requests_on_one_connection_are_refused() {
    let fake = FakeSshTransport::new();
    let mut app = TestApp::new();
    app.insert_resource(SshTransportRes::new(fake.clone()))
        .add_plugins(BackendPlugin::default().with_ssh(SshSettings::default().with_max_requests_per_connection(2)));
    app.watch::<SshFinished>();
    ssh(&app).connect("main", target());
    app.step_n(2);
    let ids: Vec<RequestId> = (0..3).map(|_| ssh(&app).run("main", "sleep 1000")).collect();
    app.step_n(2);
    assert_eq!(fake.commands().len(), 2);
    assert!(matches!(one(&app, ids[2]).result, Err(BackendError::InvalidRequest(_))));
}

#[test]
fn a_replaced_transport_loses_its_connections() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    app.insert_resource(SshTransportRes::new(FakeSshTransport::new()));
    app.step_n(2);
    assert!(matches!(one(&app, id).result, Err(BackendError::Disconnected { .. })));
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
}

#[test]
fn output_debug_never_shows_the_bytes_and_command_debug_never_shows_the_command() {
    let fake = FakeSshTransport::new();
    fake.on_command("print-token", &[(SshStream::Stdout, "token=fake-secret-123")], Ok(SshExit::with_status(0)));
    let mut app = connected(&fake);
    ssh(&app).run("main", "print-token");
    app.step_n(2);
    let output = app.all_messages::<SshOutput>().pop().unwrap_or_else(|| panic!("no output"));
    assert!(!format!("{output:?}").contains("fake-secret"));
    let command = SshCommand::new("deploy --token fake-secret-123").with_stdin(b"fake-secret-456".to_vec());
    let debug = format!("{command:?}");
    assert!(!debug.contains("fake-secret"), "{debug}");
    let auth = SshAuth::key_file_with_passphrase("/home/someone/.ssh/id_ed25519", "fake-passphrase");
    let debug = format!("{auth:?}");
    assert!(!debug.contains("fake-passphrase") && !debug.contains("someone"), "{debug}");
}

#[test]
fn only_open_connections_count_against_the_limit_and_a_refused_name_has_an_entry() {
    let fake = FakeSshTransport::new();
    let mut app = app(&fake);
    for i in 0..16 {
        ssh(&app).connect(format!("host-{i}"), target());
    }
    app.step_n(2);
    // The 17th name is refused, and says so in `SshConnections`.
    ssh(&app).connect("host-16", target());
    app.step_n(2);
    let info = app.world().resource::<SshConnections>().get("host-16").cloned().unwrap_or_else(|| panic!("no entry for the refused name"));
    assert_eq!(info.state, SshState::Disconnected);
    assert!(matches!(&info.last_error, Some(BackendError::InvalidRequest(why)) if why.contains("limit 16")), "{info:?}");
    assert_eq!(fake.connects().len(), 16);
    // A disconnected name no longer counts: the 17th connects now.
    ssh(&app).disconnect("host-0");
    ssh(&app).connect("host-16", target());
    app.step_n(2);
    assert_eq!(state(&app, "host-16"), Some(SshState::Connected));
    assert_eq!(state(&app, "host-0"), Some(SshState::Disconnected));
    // Many different names over time never exhaust the limit.
    for i in 17..60 {
        ssh(&app).disconnect(format!("host-{}", i - 16));
        ssh(&app).connect(format!("host-{i}"), target());
        app.step_n(2);
        assert_eq!(state(&app, &format!("host-{i}")), Some(SshState::Connected), "host-{i}");
    }
}

fn reconnecting_target() -> SshTarget {
    target().with_reconnect(SshReconnect::default().with_base(Duration::from_millis(100)).with_jitter(false))
}

#[test]
fn reconnect_never_re_runs_a_command_and_new_commands_use_the_new_connection() {
    let fake = FakeSshTransport::new();
    let mut app = app(&fake);
    ssh(&app).connect("main", reconnecting_target());
    app.step_n(2);
    let running = ssh(&app).run("main", "deploy");
    app.step_n(2);
    let first = fake.last_conn().unwrap_or_else(|| panic!("no conn"));
    fake.drop_conn(first, BackendError::Network("connection reset by peer".into()));
    app.step();
    // The running command is answered honestly, not kept for the new connection.
    let answer = one(&app, running);
    assert_eq!(answer.started, Some(true));
    assert!(matches!(answer.result, Err(BackendError::Disconnected { sent: Some(true), .. })), "{:?}", answer.result);
    assert!(matches!(state(&app, "main"), Some(SshState::Reconnecting { attempt: 1, .. })), "{:?}", state(&app, "main"));
    // A command made while reconnecting waits for the new connection.
    let later = ssh(&app).run("main", "uname");
    app.run_until(|world| world.resource::<SshConnections>().is_connected("main"), 200);
    app.step_n(2);
    assert_eq!(fake.connects().len(), 2);
    let second = fake.last_conn().unwrap_or_else(|| panic!("no second conn"));
    let handed: Vec<(SshConnId, RequestId)> = fake.commands().into_iter().map(|(c, id, _)| (c, id)).collect();
    assert_eq!(handed, vec![(first, running), (second, later)], "the old command was re-run or the new one went elsewhere");
    assert_eq!(app.world().resource::<SshConnections>().get("main").map(|c| c.attempt), Some(1));
}

#[test]
fn permanent_errors_are_not_retried_and_reconnect_is_off_by_default() {
    let fake = FakeSshTransport::new();
    fake.reject_next(BackendError::host_key("build.example.com", "SHA256:x", HostKeyProblem::Changed));
    let mut app = app(&fake);
    ssh(&app).connect("main", reconnecting_target());
    app.step_n(30);
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
    assert_eq!(fake.connects().len(), 1, "a host key error was retried");
    // Without `with_reconnect` a loss is final.
    ssh(&app).connect("plain", target());
    app.step_n(2);
    let conn = fake.last_conn().unwrap_or_else(|| panic!("no conn"));
    fake.drop_conn(conn, BackendError::Network("reset".into()));
    app.step_n(30);
    assert_eq!(state(&app, "plain"), Some(SshState::Disconnected));
    assert_eq!(fake.connects().len(), 2);
}

#[test]
fn reconnect_gives_up_after_max_attempts() {
    let fake = FakeSshTransport::new();
    let mut app = app(&fake);
    for _ in 0..5 {
        fake.reject_next(BackendError::Network("connection refused".into()));
    }
    ssh(&app)
        .connect("main", target().with_reconnect(SshReconnect::default().with_base(Duration::from_millis(20)).with_jitter(false).with_max_attempts(Some(2))));
    app.step_n(120);
    assert_eq!(fake.connects().len(), 3, "the first attempt plus 2 retries");
    assert_eq!(state(&app, "main"), Some(SshState::Disconnected));
}

#[test]
fn output_over_the_limit_is_stopped_even_if_the_transport_does_not() {
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", SshCommand::new("flood").with_max_output_bytes(1024));
    app.step_n(2);
    fake.output(id, SshStream::Stdout, &[b'x'; 1000]);
    fake.output(id, SshStream::Stdout, &[b'x'; 1000]);
    app.step();
    let answer = one(&app, id);
    assert!(matches!(answer.result, Err(BackendError::BodyTooLarge { limit: 1024, .. })), "{:?}", answer.result);
    assert_eq!(app.all_messages::<SshOutput>().len(), 1, "output past the limit was delivered");
    assert_eq!(fake.cancelled(), vec![id]);
}

#[test]
fn a_result_that_arrives_in_the_exit_frame_is_delivered_not_shutdown() {
    #[derive(Resource)]
    struct FinishOnExit(FakeSshTransport, Option<RequestId>);
    let fake = FakeSshTransport::new();
    let mut app = connected(&fake);
    let id = ssh(&app).run("main", "sleep 1000");
    app.step_n(2);
    // In the exit frame the result arrives after `First` (as from a real thread) and before `Last`.
    app.insert_resource(FinishOnExit(fake.clone(), Some(id))).add_systems(Update, |mut finish: ResMut<FinishOnExit>, exit: MessageReader<AppExit>| {
        if !exit.is_empty() {
            if let Some(id) = finish.1.take() {
                finish.0.finish(id, Ok(SshExit::with_status(0)));
            }
        }
    });
    app.world_mut().write_message(AppExit::Success);
    app.step();
    let answer = one(&app, id);
    assert!(answer.result.is_ok_and(|e| e.success()), "the arrived result was answered Shutdown");
}

/// HTTP, WebSocket and SSH in one app (fakes): rows and cancels of the three protocols never
/// interfere, whichever client cancels.
#[cfg(all(feature = "ws", feature = "json"))]
#[test]
fn http_websocket_and_ssh_rows_and_cancels_never_interfere() {
    let http = FakeHttpTransport::new();
    let ws = FakeWsTransport::new();
    let fake = FakeSshTransport::new();
    let mut app = TestApp::new();
    app.insert_resource(HttpTransportRes::new(http.clone()))
        .insert_resource(WsTransportRes::new(ws.clone()))
        .insert_resource(SshTransportRes::new(fake.clone()))
        .add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com")));
    app.watch::<HttpResponse>().watch::<WsRawResponse>().watch::<SshFinished>();
    app.world().resource::<WsClient>().connect("main", WsSettings::new("wss://game.example.com/ws"));
    ssh(&app).connect("main", target());
    app.step_n(2);
    let h = app.world().resource::<HttpClient>().get("/slow");
    let w = app.world().resource::<WsClient>().request_raw("main", WsOutgoing::new("echo", b"1".to_vec()));
    let s1 = ssh(&app).run("main", "sleep 1000");
    let s2 = ssh(&app).run("main", "sleep 2000");
    app.step();
    let inflight = app.world().resource::<InFlight>();
    assert_eq!(inflight.ids(), vec![h, w, s1, s2]);
    assert_eq!(inflight.describe(s1).map(|i| (i.kind, i.target.clone())), Some((RequestKind::Ssh, "main".to_string())));
    // Every client cancels a request of another protocol.
    app.world().resource::<WsClient>().cancel(s1);
    ssh(&app).cancel(h);
    app.world().resource::<HttpClient>().cancel(w);
    app.step_n(2);
    assert_eq!(one(&app, s1).result.err(), Some(BackendError::Cancelled));
    assert_eq!(app.all_messages::<HttpResponse>().into_iter().filter(|a| a.id == h).count(), 1);
    assert_eq!(app.all_messages::<WsRawResponse>().into_iter().filter(|a| a.id == w).count(), 1);
    assert_eq!(fake.cancelled(), vec![s1]);
    assert_eq!(http.cancelled(), vec![h]);
    // Only the second SSH command is left, and a WebSocket change does not erase it.
    assert_eq!(app.world().resource::<InFlight>().ids(), vec![s2]);
    let w2 = app.world().resource::<WsClient>().request_raw("main", WsOutgoing::new("echo", b"2".to_vec()));
    app.step_n(2);
    assert_eq!(app.world().resource::<InFlight>().ids(), vec![s2, w2]);
    fake.finish(s2, Ok(SshExit::with_status(0)));
    app.step();
    assert_eq!(app.world().resource::<InFlight>().ids(), vec![w2]);
}

#[cfg(feature = "sftp")]
mod sftp {
    use super::*;

    fn sftp_app(fake: &FakeSshTransport) -> TestApp {
        let mut app = connected(fake);
        app.watch::<SftpProgress>().watch::<SftpFinished>();
        app
    }

    fn sftp_answer(app: &TestApp, id: RequestId) -> SftpFinished {
        let mut all: Vec<SftpFinished> = app.all_messages::<SftpFinished>().into_iter().filter(|f| f.id == id).collect();
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    #[test]
    fn operations_reach_the_transport_and_are_answered_once() {
        let fake = FakeSshTransport::new();
        let mut app = sftp_app(&fake);
        fake.on_next_sftp(Ok(SftpOutcome::Uploaded { bytes: 5 }));
        let upload = ssh(&app).upload("main", "remote.txt", b"hello".to_vec());
        app.step_n(2);
        let answer = sftp_answer(&app, upload);
        assert_eq!((answer.result, answer.started), (Ok(SftpOutcome::Uploaded { bytes: 5 }), Some(true)));
        assert_eq!(app.world().resource::<InFlight>().describe(upload), None);
        let list = ssh(&app).list_dir("main", ".");
        app.step();
        assert_eq!(app.world().resource::<InFlight>().describe(list).map(|i| i.kind), Some(RequestKind::Sftp));
        fake.sftp_progress(list, 1, None);
        fake.sftp_finish(list, Ok(SftpOutcome::Listing(vec![SftpEntry::new("a", SftpEntryKind::File).with_size(3)])));
        app.step();
        assert_eq!(app.all_messages::<SftpProgress>().len(), 1);
        assert!(matches!(sftp_answer(&app, list).result, Ok(SftpOutcome::Listing(ref entries)) if entries.len() == 1));
        let ops: Vec<String> = fake.sftp_ops().into_iter().map(|(_, _, op)| format!("{op:?}")).collect();
        assert!(ops[0].starts_with("UploadBytes") && ops[0].contains("bytes: 5"), "{ops:?}");
    }

    #[test]
    fn bad_paths_and_oversized_uploads_are_never_sent() {
        let fake = FakeSshTransport::new();
        let mut app = TestApp::new();
        app.insert_resource(SshTransportRes::new(fake.clone())).add_plugins(BackendPlugin::default());
        app.watch::<SftpFinished>();
        ssh(&app).connect("main", target().with_max_transfer_bytes(1024));
        app.step_n(2);
        let big = ssh(&app).upload("main", "big.bin", vec![0u8; 2048]);
        let empty = ssh(&app).remove_file("main", "");
        let nul = ssh(&app).create_dir("main", "a\0b");
        app.step_n(2);
        let answer = sftp_answer(&app, big);
        assert!(matches!(answer.result, Err(BackendError::RequestTooLarge { limit: 1024, size: 2048, .. })), "{:?}", answer.result);
        assert_eq!(answer.result.as_ref().err().and_then(BackendError::was_sent), Some(false));
        assert_eq!(answer.started, Some(false));
        for id in [empty, nul] {
            let answer = sftp_answer(&app, id);
            assert!(matches!(answer.result, Err(BackendError::InvalidRequest(_))), "{:?}", answer.result);
            assert_eq!(answer.started, Some(false));
        }
        assert!(fake.sftp_ops().is_empty());
    }

    #[test]
    fn cancel_and_exit_answer_sftp_operations() {
        let fake = FakeSshTransport::new();
        let mut app = sftp_app(&fake);
        let a = ssh(&app).download("main", "a.bin");
        let b = ssh(&app).download("main", "b.bin");
        app.step_n(2);
        ssh(&app).cancel(a);
        app.step_n(2);
        assert_eq!(sftp_answer(&app, a).result.err(), Some(BackendError::Cancelled));
        app.world_mut().write_message(AppExit::Success);
        app.step();
        assert_eq!(sftp_answer(&app, b).result.err(), Some(BackendError::Shutdown));
    }
}
