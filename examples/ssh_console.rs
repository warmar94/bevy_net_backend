//! A headless admin console on a named SSH connection: connect (host key checked against a
//! known_hosts file), run a few commands, print their output and exit status, (feature `sftp`)
//! upload, list and download a file, then disconnect and exit.
//!
//! ```text
//! cargo run --example ssh_console --features ssh,sftp          # against the built-in mock
//! SSH_HOST=build.example.com SSH_USER=deploy SSH_KEY=/path/to/id_ed25519 SSH_KNOWN_HOSTS=/path/to/known_hosts \
//!   cargo run --example ssh_console --features ssh,sftp        # your server (harmless commands only)
//! ```
//!
//! Without those variables it starts the mock from `examples/mock_ssh_server.rs` on 127.0.0.1 in
//! this process, with a throwaway client key and known_hosts file written to
//! `target/ssh-example/`. The mock never executes anything. Exits when done, or after 10 s.

use std::path::PathBuf;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use bevy_net_backend::{SshAuth, SshCommand, SshTarget};

#[allow(dead_code)]
#[path = "mock_ssh_server.rs"]
mod mock_ssh_server;

/// Where to connect (a resource so a system can connect with it).
#[derive(Resource)]
struct Target(SshTarget);

/// Requests still waiting for their answer; the example exits when none is left.
#[derive(Resource, Default)]
struct Open(Vec<RequestId>);

fn main() -> AppExit {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (target, _mock) = match (var("SSH_HOST"), var("SSH_USER"), var("SSH_KEY"), var("SSH_KNOWN_HOSTS")) {
        (Some(host), Some(user), Some(key), Some(known_hosts)) => {
            (SshTarget::new(host, user).with_auth(SshAuth::key_file(key)).with_known_hosts_file(known_hosts), None)
        }
        _ => match start_mock() {
            Ok(started) => started,
            Err(e) => {
                eprintln!("could not start the mock SSH server: {e}");
                return AppExit::error();
            }
        },
    };

    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(10))),
            LogPlugin::default(),
            // No HTTP API here; SSH is allowed because this is a debug (dev) build.
            BackendPlugin::default(),
        ))
        .insert_resource(Target(target))
        .init_resource::<Open>()
        .add_systems(Startup, connect)
        .add_systems(Update, (on_state, on_output, on_finished).chain())
        .add_systems(Update, give_up)
        .add_systems(Update, on_sftp.run_if(|| cfg!(feature = "sftp")))
        .run()
}

/// The mock, a throwaway client key and a known_hosts file with the mock's host key.
fn start_mock() -> std::io::Result<(SshTarget, Option<mock_ssh_server::MockSshServer>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("ssh-example");
    let key = mock_ssh_server::random_key();
    let key_path = dir.join("id_example");
    mock_ssh_server::write_key(&key, &key_path, None)?;
    let mock = mock_ssh_server::MockSshServer::start("tester", key.public_key().clone())?;
    let known_hosts = dir.join("known_hosts");
    std::fs::write(&known_hosts, format!("{}\n", mock.known_hosts_line("127.0.0.1")))?;
    let target = SshTarget::new("127.0.0.1", "tester").with_port(mock.port()).with_auth(SshAuth::key_file(key_path)).with_known_hosts_file(known_hosts);
    Ok((target, Some(mock)))
}

/// One connection, named "main".
fn connect(ssh: Res<SshClient>, target: Res<Target>) {
    ssh.connect("main", target.0.clone());
}

fn on_state(mut changes: MessageReader<SshStateChanged>, ssh: Res<SshClient>, mut open: ResMut<Open>, mut exit: MessageWriter<AppExit>) {
    for change in changes.read() {
        match (&change.state, &change.error) {
            (SshState::Connected, _) => {
                info!("`{}` connected", change.name);
                open.0.push(ssh.run(&change.name, "uname -a"));
                open.0.push(ssh.run(&change.name, "echo hello from Bevy"));
                open.0.push(ssh.run(&change.name, SshCommand::new("fail").with_timeout(Duration::from_secs(5))));
                // Requests on one connection run at the same time: SFTP steps that depend on each
                // other go one after the other (the next one starts when the previous answer arrives).
                #[cfg(feature = "sftp")]
                open.0.extend(sftp_step(&ssh, 0));
            }
            (state, Some(error)) => {
                error!("`{}` is {state:?}: {error}", change.name);
                exit.write(AppExit::error());
            }
            (state, None) => info!("`{}` is {state:?}", change.name),
        }
    }
}

fn on_output(mut output: MessageReader<SshOutput>) {
    for chunk in output.read() {
        info!("{} {:?}: {}", chunk.id, chunk.stream, chunk.text().trim_end());
    }
}

fn done(open: &mut Open, id: RequestId, ssh: &SshClient, exit: &mut MessageWriter<AppExit>) {
    open.0.retain(|o| *o != id);
    if open.0.is_empty() {
        info!("all done");
        ssh.disconnect("main");
        exit.write(AppExit::Success);
    }
}

fn on_finished(mut finished: MessageReader<SshFinished>, mut open: ResMut<Open>, ssh: Res<SshClient>, mut exit: MessageWriter<AppExit>) {
    for answer in finished.read() {
        match &answer.result {
            Ok(end) => info!("{} ended with status {:?} (signal {:?})", answer.id, end.status, end.signal),
            Err(error) => warn!("{} failed: {error} (started: {:?})", answer.id, answer.started),
        }
        done(&mut open, answer.id, &ssh, &mut exit);
    }
}

/// The SFTP steps, in order: upload, list, download, remove.
#[cfg(feature = "sftp")]
fn sftp_step(ssh: &SshClient, step: usize) -> Option<RequestId> {
    let file = "bnb-example.txt";
    match step {
        0 => Some(ssh.upload("main", file, b"written by the ssh_console example".to_vec())),
        1 => Some(ssh.list_dir("main", ".")),
        2 => Some(ssh.download("main", file)),
        3 => Some(ssh.remove_file("main", file)),
        _ => None,
    }
}

#[cfg(feature = "sftp")]
fn on_sftp(mut finished: MessageReader<SftpFinished>, mut open: ResMut<Open>, mut step: Local<usize>, ssh: Res<SshClient>, mut exit: MessageWriter<AppExit>) {
    for answer in finished.read() {
        *step += 1;
        open.0.extend(sftp_step(&ssh, *step));
        match &answer.result {
            Ok(SftpOutcome::Data(data)) => info!("{} downloaded: {}", answer.id, String::from_utf8_lossy(data)),
            Ok(SftpOutcome::Listing(entries)) => info!("{} listing: {:?}", answer.id, entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()),
            Ok(outcome) => info!("{} done: {outcome:?}", answer.id),
            Err(error) => warn!("{} failed: {error}", answer.id),
        }
        done(&mut open, answer.id, &ssh, &mut exit);
    }
}

#[cfg(not(feature = "sftp"))]
fn on_sftp() {}

fn give_up(time: Res<Time<Real>>, mut exit: MessageWriter<AppExit>) {
    if time.elapsed() > Duration::from_secs(10) {
        error!("not done after 10 s");
        exit.write(AppExit::error());
    }
}
