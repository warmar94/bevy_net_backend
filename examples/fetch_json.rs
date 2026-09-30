//! Load typed JSON from your API: one `GET`, one typed answer, then exit.
//!
//! ```text
//! cargo run --example fetch_json                                   # against the built-in mock
//! BACKEND_URL=http://127.0.0.1:8000/api cargo run --example fetch_json   # against your dev server
//! ```
//!
//! With `BACKEND_URL` set, it asks `GET {BACKEND_URL}/characters/1` and expects
//! `{"id":1,"name":"…","class":"…","level":7}` (any extra fields are ignored). Without it, it
//! starts the mock from `examples/mock_server.rs` on 127.0.0.1 in this process. Headless; exits
//! on the answer or after 10 s.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[allow(dead_code)]
#[path = "mock_server.rs"]
mod mock_server;

/// The answer's shape: a plain serde type. In Laravel this is what `return $character;` (or a
/// JsonResource) sends; in Express `res.json(character)`; in Go `json.NewEncoder(w).Encode(c)`.
#[derive(Deserialize, Clone, Debug)]
struct Character {
    id: u32,
    name: String,
    class: String,
    level: u32,
}

/// The id of the request being waited for.
#[derive(Resource)]
struct Waiting(RequestId);

fn main() -> AppExit {
    // The game's config: where the API lives (from a settings file in a real game).
    let (base_url, _mock) = match std::env::var("BACKEND_URL") {
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
            BackendPlugin::new(HttpConfig::new(base_url).with_timeout(Duration::from_secs(5))),
        ))
        // Every typed answer is registered once.
        .add_json_response::<Character>()
        .add_systems(Startup, ask)
        .add_systems(Update, (show, give_up))
        .run()
}

/// Fire the request; the id comes back at once, the answer a few frames later.
fn ask(backend: Res<HttpClient>, mut commands: Commands) {
    let id = backend.get_json::<Character>("/characters/1");
    commands.insert_resource(Waiting(id));
}

/// Read the answer in `Update`: it is written in `First` of the frame it arrives in.
fn show(mut answers: MessageReader<JsonResponse<Character>>, waiting: Option<Res<Waiting>>, mut exit: MessageWriter<AppExit>) {
    let Some(waiting) = waiting else { return };
    for answer in answers.read().filter(|a| a.id == waiting.0) {
        match &answer.result {
            Ok(character) => {
                info!("character #{}: {} the {} (level {})", character.id, character.name, character.class, character.level);
                exit.write(AppExit::Success);
            }
            Err(error) => {
                // Status errors keep the server's answer: show its body.
                let body = error.response().map(|r| r.text()).unwrap_or_default();
                error!("the request failed: {error} {body}");
                exit.write(AppExit::error());
            }
        }
    }
}

fn give_up(time: Res<Time<Real>>, mut exit: MessageWriter<AppExit>) {
    if time.elapsed() > Duration::from_secs(10) {
        error!("no answer after 10 s");
        exit.write(AppExit::error());
    }
}
