//! A headless avatar upload: a `multipart/form-data` form with a text field and an image file,
//! sent with `post_multipart_json`, and the server's typed answer.
//!
//! ```text
//! cargo run --example upload                                                # against the built-in mock
//! BACKEND_URL=https://api.example.com cargo run --example upload            # your server: POST /upload
//! ```
//!
//! Without `BACKEND_URL` it starts the mock from `examples/mock_server.rs` on 127.0.0.1 in this
//! process; the mock parses the form and answers what it received. Your own `POST /upload` should
//! answer JSON too (the example prints whatever comes back). Exits when answered, or after 10 s.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[allow(dead_code)]
#[path = "mock_server.rs"]
mod mock_server;

/// What the mock answers (`fields` and `files` it parsed).
#[derive(Deserialize, Clone, Debug)]
struct Received {
    fields: Vec<serde_json::Value>,
    files: Vec<serde_json::Value>,
}

fn main() -> AppExit {
    let (url, _mock) = match std::env::var("BACKEND_URL") {
        Ok(url) => (url, None),
        Err(_) => match mock_server::MockServer::start() {
            Ok(mock) => (mock.url(), Some(mock)),
            Err(e) => {
                eprintln!("could not start the mock server: {e}");
                return AppExit::error();
            }
        },
    };
    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(10))),
            LogPlugin::default(),
            BackendPlugin::new(HttpConfig::new(url)),
        ))
        .add_json_response::<Received>()
        .add_systems(Startup, upload)
        .add_systems(Update, (on_answer, give_up))
        .run()
}

/// A tiny "PNG" (a real game would load the bytes from disk or a screenshot first).
fn avatar_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    bytes.extend((0..4096u32).map(|i| (i * 31 % 251) as u8));
    bytes
}

fn upload(backend: Res<HttpClient>) {
    let form = Multipart::new().text("display_name", "Ayla").file("avatar", "avatar.png", "image/png", avatar_bytes());
    info!("uploading {} parts, {} bytes", form.len(), form.encoded_len());
    backend.post_multipart_json::<Received>("/upload", &form);
}

fn on_answer(mut answers: MessageReader<JsonResponse<Received>>, mut exit: MessageWriter<AppExit>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(received) => {
                info!("the server read fields {:?}", received.fields);
                info!("and files {:?}", received.files);
                exit.write(AppExit::Success);
            }
            Err(error) => {
                error!("upload failed: {error}");
                exit.write(AppExit::error());
            }
        }
    }
}

fn give_up(time: Res<Time<Real>>, mut exit: MessageWriter<AppExit>) {
    if time.elapsed() > Duration::from_secs(10) {
        error!("not done after 10 s");
        exit.write(AppExit::error());
    }
}
