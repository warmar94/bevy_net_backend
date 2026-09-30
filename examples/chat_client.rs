//! A headless chat client on a named WebSocket connection: connect, send a typed request, read
//! its typed answer and the server's pushes, then disconnect and exit.
//!
//! ```text
//! cargo run --example chat_client --features ws,json                                     # against the built-in mock
//! BACKEND_WS_URL=wss://game.example.com/ws cargo run --example chat_client --features ws,json   # your server
//! ```
//!
//! Without `BACKEND_WS_URL` it starts the mock from `examples/mock_ws_server.rs` on 127.0.0.1 in
//! this process. The wire format is the crate's default JSON envelope
//! (`{"id":1,"type":"chat.send","data":{…}}` → `{"id":1,"ok":true,"data":{…}}`, pushes
//! `{"type":"chat.message","data":{…}}`). Exits after three `server.tick` pushes, or after 10 s.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[path = "mock_ws_server.rs"]
mod mock_ws_server;

/// A request: `{"type":"chat.send","data":{"text":…}}`, answered with a `ChatAck`.
#[derive(Serialize)]
struct ChatSend {
    text: String,
}

#[derive(Deserialize, Clone, Debug)]
struct ChatAck {
    accepted: bool,
}

impl WsRequest for ChatSend {
    type Response = ChatAck;
    const KIND: &'static str = "chat.send";
}

/// A push: `{"type":"chat.message","data":{"from":…,"text":…}}`.
#[derive(Deserialize, Clone, Debug)]
struct ChatMessage {
    from: String,
    text: String,
}

impl WsPushMessage for ChatMessage {
    const KIND: &'static str = "chat.message";
}

/// A push: `{"type":"server.tick","data":{"n":…}}`.
#[derive(Deserialize, Clone, Debug)]
struct ServerTick {
    n: u64,
}

impl WsPushMessage for ServerTick {
    const KIND: &'static str = "server.tick";
}

/// The connection's URL (a resource so a system can connect with it).
#[derive(Resource)]
struct ChatUrl(String);

fn main() -> AppExit {
    let (url, _mock) = match std::env::var("BACKEND_WS_URL") {
        Ok(url) => (url, None),
        Err(_) => match mock_ws_server::MockWsServer::start(200) {
            Ok(mock) => (format!("{}/chat", mock.url()), Some(mock)),
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
            // No HTTP API here: the plugin still sets up both sides; the base URL is not used.
            BackendPlugin::default(),
        ))
        .add_ws_request::<ChatSend>()
        .add_ws_push::<ChatMessage>()
        .add_ws_push::<ServerTick>()
        .insert_resource(ChatUrl(url))
        .add_systems(Startup, connect)
        .add_systems(Update, (on_state, on_ack, on_chat, on_tick, give_up))
        .run()
}

/// One connection, named "main". A game needing a second one just connects another name.
fn connect(ws: Res<WsClient>, url: Res<ChatUrl>) {
    ws.connect("main", WsSettings::new(url.0.clone()).with_request_timeout(Duration::from_secs(5)));
}

fn on_state(mut changes: MessageReader<WsStateChanged>, ws: Res<WsClient>) {
    for change in changes.read() {
        match (&change.state, &change.error) {
            (WsState::Connected, _) => {
                info!("`{}` connected; saying hello", change.name);
                ws.request(&change.name, &ChatSend { text: "hello from Bevy".into() });
            }
            (WsState::Reconnecting { attempt, retry_in }, Some(error)) => warn!("`{}` lost ({error}); attempt {attempt} in {retry_in:?}", change.name),
            (state, Some(error)) => warn!("`{}` is {state:?}: {error}", change.name),
            (state, None) => info!("`{}` is {state:?}", change.name),
        }
    }
}

fn on_ack(mut answers: MessageReader<WsResponse<ChatAck>>) {
    for answer in answers.read() {
        match &answer.result {
            Ok(ack) => info!("the server took the message: {}", ack.accepted),
            Err(error) => warn!("chat.send failed: {error}"),
        }
    }
}

fn on_chat(mut pushes: MessageReader<WsPush<ChatMessage>>) {
    for push in pushes.read() {
        info!("[{}] {}: {}", push.name, push.data.from, push.data.text);
    }
}

fn on_tick(mut pushes: MessageReader<WsPush<ServerTick>>, ws: Res<WsClient>, mut exit: MessageWriter<AppExit>) {
    for push in pushes.read() {
        info!("server tick {}", push.data.n);
        if push.data.n >= 3 {
            ws.disconnect("main");
            exit.write(AppExit::Success);
        }
    }
}

fn give_up(time: Res<Time<Real>>, mut exit: MessageWriter<AppExit>) {
    if time.elapsed() > Duration::from_secs(10) {
        error!("not done after 10 s");
        exit.write(AppExit::error());
    }
}
