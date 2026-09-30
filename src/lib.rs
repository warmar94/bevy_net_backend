//! Call your game's own HTTPS JSON API from Bevy.
//!
//! A system fires a request through [`HttpClient`] and gets a [`RequestId`]; a few frames
//! later exactly one answer arrives as a Bevy message: `JsonResponse<T>` for typed JSON
//! (feature `json`) or [`HttpResponse`] for raw bytes. The answer is the decoded value or a
//! [`BackendError`]: network, TLS, timeout, an HTTP status with the server's body, a decode
//! error, cancelled, or shutdown on `AppExit`. Nothing is ever dropped silently.
//!
//! Features: `http` (the real transport: ureq on a few worker threads, rustls with ring), `json`
//! (typed requests), `gzip`. Default: `http`, `json`. Without `http` the crate still builds, and the
//! [`FakeHttpTransport`] drives everything in tests. The README is the full manual.
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
mod request;
mod response;
#[cfg(feature = "http")]
mod tls;
mod transport;

#[cfg(test)]
mod tests;

/// Every Rust example in the README compiles (checked by `cargo test` with the default features).
#[cfg(all(doctest, feature = "http", feature = "json"))]
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
pub use request::{OutgoingRequest, PreparedRequest, RequestId, RequestPurpose};
#[cfg(feature = "json")]
pub use response::JsonResponse;
pub use response::{BackendError, HttpResponse, RawResponse};
pub use transport::fake::FakeHttpTransport;
#[cfg(feature = "http")]
pub use transport::http_pool::UreqTransport;
pub use transport::{HttpTransport, HttpTransportRes, HttpTransportResult};

/// Everything a game usually needs: `use bevy_net_backend::prelude::*;`.
pub mod prelude {
    #[cfg(feature = "json")]
    pub use crate::JsonResponse;
    pub use crate::{
        BackendAppExt, BackendCredentials, BackendError, BackendPlugin, BackendSystems, BearerToken, HttpClient, HttpConfig, HttpResponse, InFlight,
        OutgoingRequest, RequestId,
    };
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
/// later is fine, its threads only start on the first request).
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
}

impl BackendPlugin {
    /// A plugin with this config.
    pub fn new(config: HttpConfig) -> Self {
        Self { config }
    }

    /// Replace the config (builder style).
    pub fn with_config(mut self, config: HttpConfig) -> Self {
        self.config = config;
        self
    }
}

/// The plugin's system sets, named after phases so every kind of connection a later version
/// adds uses the same three. `#[non_exhaustive]`: a later version may add a set.
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
            // The error text never quotes the URL (it may hold a secret by mistake).
            Err(e) => tracing::warn!(">>> NET-BACKEND: {e}; every request is answered with an error until the config is fixed"),
        }
        app.insert_resource(self.config.clone())
            .init_resource::<HttpClient>()
            .init_resource::<InFlight>()
            .init_resource::<BackendCredentials>()
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
        #[cfg(not(feature = "http"))]
        if !app.world().contains_resource::<HttpTransportRes>() {
            tracing::info!(">>> NET-BACKEND: no transport compiled in (feature `http` is off); insert an `HttpTransportRes`");
        }
    }
}
