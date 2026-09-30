//! Log in, keep the token, and make authenticated JSON calls: the usual game ↔ API flow.
//!
//! ```text
//! cargo run --example post_with_token          # against the built-in mock
//! ```
//!
//! Steps (each waits for the previous answer):
//!
//! 1. `POST /saves` before logging in → `401`, shown with the server's own message.
//! 2. `POST /login` (sent `without_credentials`) → `{"token": "…"}`; the game stores it with
//!    `BackendCredentials::set(BearerToken::new(token))`: from now on every request carries
//!    `Authorization: Bearer …` (Laravel Sanctum, Passport, most Node / Go APIs).
//! 3. `POST /saves` with a body the server refuses → `422` with Laravel-style validation errors,
//!    decoded from the error body.
//! 4. `POST /saves` with a proper save → `201 {"id":42,…}`, then exit.
//!
//! With `BACKEND_URL` set it runs against your API instead (routes as above; the login reads
//! `BACKEND_USERNAME` / `BACKEND_PASSWORD`). Headless; exits when done or after 10 s.

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_net_backend::http::StatusCode;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[path = "mock_server.rs"]
mod mock_server;

#[derive(Serialize)]
struct Login {
    username: String,
    password: String,
}

#[derive(Deserialize, Clone, Debug)]
struct LoginAnswer {
    token: String,
}

#[derive(Serialize)]
struct SaveGame {
    slot: u8,
    level: u32,
    gold: u64,
}

#[derive(Deserialize, Clone, Debug)]
struct SaveAck {
    id: u64,
}

/// The error body most frameworks send (Laravel: `message` + `errors`).
#[derive(Deserialize, Debug)]
struct ApiError {
    message: String,
    #[serde(default)]
    errors: BTreeMap<String, Vec<String>>,
}

/// The current step, and the id being waited for.
#[derive(Resource)]
enum Step {
    SaveWithoutLogin(RequestId),
    Login(RequestId),
    BadSave(RequestId),
    GoodSave(RequestId),
}

fn main() -> AppExit {
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
        .add_json_response::<LoginAnswer>()
        .add_json_response::<SaveAck>()
        .add_systems(Startup, first_save)
        .add_systems(Update, (on_login, on_save, give_up))
        .run()
}

fn save() -> SaveGame {
    SaveGame { slot: 1, level: 7, gold: 1250 }
}

fn first_save(backend: Res<HttpClient>, mut commands: Commands) {
    commands.insert_resource(Step::SaveWithoutLogin(backend.post_json::<SaveAck>("/saves", &save())));
}

fn on_save(mut answers: MessageReader<JsonResponse<SaveAck>>, mut step: ResMut<Step>, backend: Res<HttpClient>, mut exit: MessageWriter<AppExit>) {
    for answer in answers.read() {
        match (&*step, &answer.result) {
            (Step::SaveWithoutLogin(id), Err(error)) if *id == answer.id && error.status() == Some(StatusCode::UNAUTHORIZED) => {
                let message = error.response().and_then(|r| r.json::<ApiError>().ok()).map(|e| e.message).unwrap_or_default();
                info!("1. saving without a token: {error} ({message:?}) - logging in");
                let username = std::env::var("BACKEND_USERNAME").unwrap_or_else(|_| "demo-player".into());
                let password = std::env::var("BACKEND_PASSWORD").unwrap_or_else(|_| mock_server::PASSWORD.into());
                // The login call itself must not carry an old token.
                let request = OutgoingRequest::post("/login").with_json(&Login { username, password }).without_credentials();
                *step = Step::Login(backend.send_json::<LoginAnswer>(request));
            }
            (Step::BadSave(id), Err(error)) if *id == answer.id && error.status() == Some(StatusCode::UNPROCESSABLE_ENTITY) => {
                if let Some(Ok(details)) = error.response().map(|r| r.json::<ApiError>()) {
                    info!("3. the server refused the save: {} {:?}", details.message, details.errors);
                }
                *step = Step::GoodSave(backend.post_json::<SaveAck>("/saves", &save()));
            }
            (Step::GoodSave(id), Ok(ack)) if *id == answer.id => {
                info!("4. saved as #{}", ack.id);
                exit.write(AppExit::Success);
            }
            (_, Err(error)) => {
                error!("unexpected answer {}: {error}", answer.id);
                exit.write(AppExit::error());
            }
            (_, Ok(ack)) => warn!("unexpected save answer {} (#{})", answer.id, ack.id),
        }
    }
}

fn on_login(
    mut answers: MessageReader<JsonResponse<LoginAnswer>>,
    mut step: ResMut<Step>,
    mut credentials: ResMut<BackendCredentials>,
    backend: Res<HttpClient>,
    mut exit: MessageWriter<AppExit>,
) {
    for answer in answers.read() {
        let Step::Login(id) = *step else { continue };
        if answer.id != id {
            continue;
        }
        match &answer.result {
            Ok(login) => {
                // Never log the token itself. BearerToken keeps it out of Debug output and logs.
                credentials.set(BearerToken::new(login.token.clone()));
                info!("2. logged in; the token is stored in BackendCredentials");
                // A body the server refuses (not a JSON object): shows the 422 path.
                *step = Step::BadSave(backend.post_json::<SaveAck>("/saves", &[1, 2, 3]));
            }
            Err(error) => {
                error!("login failed: {error}");
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
