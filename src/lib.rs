//! Call your game's own HTTPS JSON API from Bevy.
//!
//! A system fires a request through [`HttpClient`] and gets a [`RequestId`]; a few frames
//! later exactly one answer arrives as a Bevy message: `JsonResponse<T>` for typed JSON
//! (feature `json`) or [`HttpResponse`] for raw bytes. The answer is the decoded value or a
//! [`BackendError`]: network, TLS, timeout, an HTTP status with the server's body, a decode
//! error, cancelled, or shutdown on `AppExit`. Nothing is ever dropped silently.
//!
//! Features: `http` (the real transport: ureq on a few worker threads, rustls with ring, and
//! `multipart/form-data` uploads with `Multipart`), `json`
//! (typed requests), `gzip`, `ws` (named WebSocket connections: `WsClient` and friends), `ssh`
//! (named SSH connections that run commands, ADMIN / DEV builds only: `SshClient`), `sftp` (file
//! operations on them), `ssh-rsa` (RSA keys for SSH). Default: `http`, `json`. No tokio unless you
//! enable `ssh`. Without `http` the crate still builds, and the [`FakeHttpTransport`] drives
//! everything in tests. The README is the full manual.
#![cfg_attr(
    feature = "json",
    doc = r##"
A quick start:

```no_run
use bevy::prelude::*;
use bevy_net_backend::prelude::*;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
struct Profile {
    name: String,
    level: u32,
}

fn main() {
    App::new()
        .add_plugins((MinimalPlugins, BackendPlugin::new(HttpConfig::new("https://api.example.com"))))
        .add_json_response::<Profile>()
        .add_systems(Startup, |backend: Res<HttpClient>| {
            backend.get_json::<Profile>("/me");
        })
        .add_systems(Update, |mut answers: MessageReader<JsonResponse<Profile>>| {
            for answer in answers.read() {
                match &answer.result {
                    Ok(profile) => info!("{} is level {}", profile.name, profile.level),
                    Err(error) => warn!("could not load the profile: {error}"),
                }
            }
        })
        .run();
}
```
"##
)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod client;
mod config;
mod credentials;
mod inflight;
#[cfg(feature = "http")]
mod multipart;
mod request;
mod response;
#[cfg(feature = "ssh")]
mod ssh;
#[cfg(any(feature = "http", feature = "ws"))]
mod tls;
mod transport;
#[cfg(feature = "ws")]
mod ws;

#[cfg(test)]
mod tests;

/// Every Rust example in the README compiles (checked by `cargo test --all-features`).
#[cfg(all(doctest, feature = "http", feature = "json", feature = "ws", feature = "ssh", feature = "sftp"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub use client::{BackendAppExt, HttpClient};
pub use config::{ConfigError, HttpConfig, DEFAULT_MAX_BODY_BYTES, DEFAULT_TIMEOUT, DEFAULT_WORKERS, MAX_TIMEOUT, MAX_WORKERS};
#[cfg(feature = "json")]
pub use credentials::JsonBodyField;
pub use credentials::{ApiKeyHeader, ApiKeyQuery, BackendCredentials, BearerToken, Credentials, Secret};
/// The `http` crate (1.x), whose types this crate's API uses: `Method`, `StatusCode`,
/// `HeaderMap`, `HeaderName`, `HeaderValue`, `Uri`.
pub use http;
pub use inflight::{InFlight, RequestInfo, RequestKind, DEADLINE_GRACE};
#[cfg(feature = "http")]
pub use multipart::{Multipart, DEFAULT_MULTIPART_MAX_BYTES, DEFAULT_MULTIPART_MAX_PARTS};
pub use request::{OutgoingRequest, PreparedRequest, RequestId, RequestPurpose};
#[cfg(feature = "json")]
pub use response::JsonResponse;
pub use response::{BackendError, HostKeyProblem, HttpResponse, RawResponse, Rejection};
#[cfg(feature = "ssh")]
pub use ssh::{
    FakeSshTransport, RusshTransport, SshAuth, SshClient, SshCommand, SshConnId, SshConnectionInfo, SshConnections, SshEvent, SshExit, SshFinished, SshName,
    SshOutput, SshPrompt, SshPromptAnswers, SshPromptRequest, SshPromptResponder, SshReconnect, SshSettings, SshState, SshStateChanged, SshStream, SshTarget,
    SshTransport, SshTransportRes, DEFAULT_SFTP_MAX_BYTES, DEFAULT_SFTP_TIMEOUT, DEFAULT_SSH_COMMAND_TIMEOUT, DEFAULT_SSH_CONNECT_TIMEOUT,
    DEFAULT_SSH_MAX_OUTPUT_BYTES, MAX_SSH_COMMAND_BYTES,
};
#[cfg(feature = "sftp")]
pub use ssh::{SftpEntry, SftpEntryKind, SftpFinished, SftpOp, SftpOutcome, SftpProgress};
pub use transport::fake::FakeHttpTransport;
#[cfg(feature = "http")]
pub use transport::http_pool::UreqTransport;
pub use transport::{HttpTransport, HttpTransportRes, HttpTransportResult};
#[cfg(feature = "ws")]
pub use ws::{
    FakeWsTransport, TungsteniteTransport, WsClient, WsConnectionInfo, WsConnections, WsFrame, WsHandshake, WsIncoming, WsLinkEvent, WsLinkId, WsMessage,
    WsName, WsOutgoing, WsProtocol, WsRawResponse, WsReconnect, WsSettings, WsState, WsStateChanged, WsTransport, WsTransportRes, DEFAULT_WS_MAX_MESSAGE_BYTES,
    DEFAULT_WS_READ_TIMEOUT,
};
#[cfg(all(feature = "ws", feature = "json"))]
pub use ws::{JsonEnvelope, WsPush, WsPushMessage, WsRequest, WsResponse};

/// Everything a game usually needs: `use bevy_net_backend::prelude::*;`.
pub mod prelude {
    #[cfg(feature = "json")]
    pub use crate::JsonResponse;
    #[cfg(feature = "http")]
    pub use crate::Multipart;
    pub use crate::{
        BackendAppExt, BackendCredentials, BackendError, BackendPlugin, BackendSystems, BearerToken, HttpClient, HttpConfig, HttpResponse, InFlight,
        OutgoingRequest, RequestId,
    };
    #[cfg(feature = "sftp")]
    pub use crate::{SftpFinished, SftpOutcome, SftpProgress};
    #[cfg(feature = "ssh")]
    pub use crate::{SshClient, SshConnections, SshFinished, SshOutput, SshState, SshStateChanged};
    #[cfg(feature = "ws")]
    pub use crate::{WsClient, WsConnections, WsFrame, WsMessage, WsSettings, WsState, WsStateChanged};
    #[cfg(all(feature = "ws", feature = "json"))]
    pub use crate::{WsPush, WsPushMessage, WsRequest, WsResponse};
}

use bevy_app::{App, AppExit, First, Last, Plugin, PostUpdate};
use bevy_ecs::message::MessageUpdateSystems;
use bevy_ecs::schedule::common_conditions::on_message;
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_time::TimeSystems;

/// The plugin. Add it once; it adds no other plugin and works under `MinimalPlugins` (and even
/// without Bevy's `TimePlugin`).
///
/// It inserts the [`HttpConfig`] it was given, [`HttpClient`], [`InFlight`], an empty
/// [`BackendCredentials`] (unless one exists), the [`HttpResponse`] message, and with feature
/// `http` a `UreqTransport` as the [`HttpTransportRes`] (unless one exists; replacing it
/// later is fine, its threads only start on the first request). With feature `ws` it also adds
/// the WebSocket side (`WsClient`, `WsConnections`, the `Ws*` messages and a
/// `TungsteniteTransport` unless a `WsTransportRes` exists). With feature `ssh` it adds the SSH side
/// (`SshClient`, `SshConnections`, the `Ssh*` / `Sftp*` messages and a `RusshTransport` unless an
/// `SshTransportRes` exists; its thread only starts on the first connect).
///
/// ```
/// use std::time::Duration;
/// use bevy_app::App;
/// use bevy_net_backend::{HttpConfig, BackendPlugin};
///
/// let mut app = App::new();
/// app.add_plugins(BackendPlugin::new(HttpConfig::new("https://api.example.com").with_timeout(Duration::from_secs(10))));
/// ```
#[derive(Clone, Debug, Default)]
pub struct BackendPlugin {
    config: HttpConfig,
    #[cfg(feature = "ssh")]
    ssh: SshSettings,
}

impl BackendPlugin {
    /// A plugin with this config.
    pub fn new(config: HttpConfig) -> Self {
        Self {
            config,
            #[cfg(feature = "ssh")]
            ssh: SshSettings::default(),
        }
    }

    /// Replace the config (builder style).
    pub fn with_config(mut self, config: HttpConfig) -> Self {
        self.config = config;
        self
    }

    /// The SSH settings (feature `ssh`), e.g. to allow SSH in a release build of an admin tool.
    #[cfg(feature = "ssh")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ssh")))]
    pub fn with_ssh(mut self, settings: SshSettings) -> Self {
        self.ssh = settings;
        self
    }
}

/// The plugin's system sets, named after phases: HTTP, WebSocket (feature `ws`) and SSH (feature
/// `ssh`) systems all run in them (in that order within a set). `#[non_exhaustive]`: a later
/// version may add a set.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendSystems {
    /// `First`, after Bevy's `TimeSystems` and before `MessageUpdateSystems`: collect what the
    /// transport reports, enforce deadlines, write every answer ([`HttpResponse`],
    /// `JsonResponse<T>`). `PreUpdate` and `Update` read them in the same frame.
    Receive,
    /// `PostUpdate`: apply defaults and credentials, check the URL, hand every request made so far
    /// (e.g. in `Update`) to the transport; apply cancels. In a frame with an `AppExit` message
    /// nothing is sent (those requests are answered `Shutdown` by [`Exit`](Self::Exit)).
    Send,
    /// `Last`, only in a frame with an `AppExit` message: answer everything still open with
    /// [`BackendError::Shutdown`] and stop the transport. Readable by systems ordered after this
    /// set in `Last`.
    Exit,
}

impl Plugin for BackendPlugin {
    fn build(&self, app: &mut App) {
        match self.config.validate() {
            Ok(()) => tracing::info!(">>> NET-BACKEND: base URL {}", self.config.base_url()),
            Err(ConfigError::NoBaseUrl) if cfg!(any(feature = "ws", feature = "ssh")) => {
                tracing::debug!(">>> NET-BACKEND: no HTTP base URL set; HTTP requests are answered with an error until one is")
            }
            Err(ConfigError::NoBaseUrl) => tracing::info!(">>> NET-BACKEND: no HTTP base URL set; HTTP requests are answered with an error until one is"),
            // The error text never quotes the URL (it may hold a secret by mistake).
            Err(e) => tracing::warn!(">>> NET-BACKEND: {e}; every request is answered with an error until the config is fixed"),
        }
        app.insert_resource(self.config.clone()).init_resource::<HttpClient>().init_resource::<InFlight>();
        // One cancel list for every client.
        let cancels = app.world().resource::<InFlight>().cancel_list();
        if let Some(mut client) = app.world_mut().get_resource_mut::<HttpClient>() {
            client.share_cancels(cancels);
        }
        app.init_resource::<BackendCredentials>()
            .add_message::<HttpResponse>()
            .configure_sets(First, BackendSystems::Receive.after(TimeSystems).before(MessageUpdateSystems))
            .configure_sets(PostUpdate, BackendSystems::Send)
            .configure_sets(Last, BackendSystems::Exit)
            .add_systems(First, inflight::receive_answers.in_set(BackendSystems::Receive))
            .add_systems(PostUpdate, inflight::send_requests.in_set(BackendSystems::Send))
            .add_systems(Last, inflight::shutdown_on_exit.in_set(BackendSystems::Exit).run_if(on_message::<AppExit>));

        #[cfg(feature = "http")]
        if !app.world().contains_resource::<HttpTransportRes>() {
            let transport = UreqTransport::new(&self.config);
            tracing::info!(">>> NET-BACKEND: HTTP transport, up to {} worker threads", transport.workers());
            app.insert_resource(HttpTransportRes::new(transport));
        }
        #[cfg(feature = "ws")]
        ws::build(app);
        #[cfg(feature = "ssh")]
        ssh::build(app, &self.ssh);
        #[cfg(not(feature = "http"))]
        if !app.world().contains_resource::<HttpTransportRes>() {
            if cfg!(any(feature = "ws", feature = "ssh")) {
                tracing::debug!(">>> NET-BACKEND: no HTTP transport compiled in (feature `http` is off)");
            } else {
                tracing::info!(">>> NET-BACKEND: no transport compiled in (feature `http` is off); insert an `HttpTransportRes`");
            }
        }
    }
}
