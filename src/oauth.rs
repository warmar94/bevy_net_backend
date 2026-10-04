//! Desktop sign-in at an OAuth 2.0 / OpenID Connect provider (feature `oauth`): the authorization
//! code flow with PKCE and a loopback redirect (RFC 8252, RFC 7636).
//!
//! 1. [`OAuthClient::sign_in`] starts a sign-in on its own thread (`net-backend-oauth`): a
//!    one-time listener on `127.0.0.1` (a free port) is the redirect address
//!    (`http://127.0.0.1:{port}/callback`); a fresh PKCE verifier (S256), `state` and nonce come
//!    from the operating system's random source.
//! 2. The provider's sign-in page URL arrives as an [`OAuthSignInUrl`] message: the game opens it
//!    in the system browser (or shows it). The crate never starts a browser.
//! 3. The browser comes back to the listener with `code` and `state`: a wrong `state` gets an
//!    error page and is ignored (the sign-in keeps waiting); the provider's `error` ends the
//!    sign-in; anything else gets 404. After the one redirect that carries this sign-in's `state`
//!    the listener is closed.
//! 4. The code (with the verifier) is exchanged at the provider's token endpoint (`https://`, or
//!    `http://` only for a loopback address) through the crate's HTTP stack (ureq, the plugin's
//!    [`TlsSettings`](crate::TlsSettings) and [`ProxySettings`](crate::ProxySettings)).
//! 5. [`OAuthSignedIn`] carries the tokens as [`Secret`]s ([`OAuthTokens`]). Sending the ID token
//!    to your own server (and that server checking it) is the game's request, like any other.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bevy_app::{App, AppExit, First, Last, PostUpdate};
use bevy_ecs::message::{Message, MessageReader, MessageWriter};
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::common_conditions::on_message;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::{Res, ResMut};
use http::Uri;
use zeroize::Zeroizing;

use crate::config::HttpConfig;
use crate::credentials::Secret;
use crate::inflight::{CancelList, InFlight, Protocol, RequestInfo, RequestKind};
use crate::request::{is_loopback_host, RequestId};
use crate::response::BackendError;
use crate::tls::TlsSettings;
use crate::transport::http_pool::HttpAgent;
use crate::BackendSystems;

/// Google's authorization endpoint (from its discovery document).
pub const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint (from its discovery document).
pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// How long a sign-in waits for the browser to come back by default: 5 minutes.
pub const DEFAULT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest request the loopback listener reads (the browser's redirect).
const MAX_REDIRECT_BYTES: usize = 16 * 1024;

/// The bytes read from the listener at once.
const READ_CHUNK: usize = 2048;

/// A redirect head buffer's capacity: reading stops once the head reaches `MAX_REDIRECT_BYTES`,
/// so it never holds more than that plus one chunk.
const HEAD_CAPACITY: usize = MAX_REDIRECT_BYTES + READ_CHUNK;
/// The largest token endpoint answer read.
const MAX_TOKEN_ANSWER_BYTES: u64 = 256 * 1024;
/// Connections to the listener read at the same time; more wait in the backlog.
const MAX_OPEN_CONNECTIONS: usize = 16;
/// A connection to the listener that sends no complete request in this time is closed.
const CONNECTION_IDLE: Duration = Duration::from_secs(10);
/// How often the listener thread looks for connections, a cancel and the time limit.
const POLL: Duration = Duration::from_millis(10);
/// How long a page is written to the browser at most.
const PAGE_WRITE: Duration = Duration::from_secs(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One provider's desktop sign-in settings: its endpoints, the game's client id there, scopes,
/// the time limit. Build it once, start a sign-in with [`OAuthClient::sign_in`] each time.
///
/// ```
/// use std::time::Duration;
/// use bevy_net_backend::OAuthFlow;
///
/// // Google: a "Desktop app" OAuth client in the Google Cloud console gives the id and the secret.
/// let google = OAuthFlow::google("1234-abc.apps.googleusercontent.com")
///     .with_client_secret("GOCSPX-desktop-secret")
///     .with_scopes(["openid", "email"])
///     .with_timeout(Duration::from_secs(120));
/// // Any OpenID Connect provider: its endpoints from its discovery document.
/// let other = OAuthFlow::new("https://login.example.com/authorize", "https://login.example.com/token", "my-game");
/// # let _ = (google, other);
/// ```
///
/// `Debug` never shows the client secret.
#[derive(Clone)]
pub struct OAuthFlow {
    authorization_endpoint: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<Secret>,
    scopes: Vec<String>,
    timeout: Duration,
    extra: Vec<(String, String)>,
}

impl fmt::Debug for OAuthFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let extra: Vec<&str> = self.extra.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("OAuthFlow")
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .field("scopes", &self.scopes)
            .field("timeout", &self.timeout)
            .field("param_names", &extra)
            .finish()
    }
}

impl OAuthFlow {
    /// A provider's authorization and token endpoints (from its discovery document,
    /// `/.well-known/openid-configuration`) and the game's client id there. Scope `openid`, time
    /// limit [`DEFAULT_SIGN_IN_TIMEOUT`]. Both endpoints must be `https://` (`http://` only for a
    /// loopback address); others are answered `InvalidRequest`.
    pub fn new(authorization_endpoint: impl Into<String>, token_endpoint: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            authorization_endpoint: authorization_endpoint.into(),
            token_endpoint: token_endpoint.into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: vec!["openid".into()],
            timeout: DEFAULT_SIGN_IN_TIMEOUT,
            extra: Vec::new(),
        }
    }

    /// Google's endpoints ([`GOOGLE_AUTHORIZATION_ENDPOINT`], [`GOOGLE_TOKEN_ENDPOINT`]) with this
    /// client id.
    pub fn google(client_id: impl Into<String>) -> Self {
        Self::new(GOOGLE_AUTHORIZATION_ENDPOINT, GOOGLE_TOKEN_ENDPOINT, client_id)
    }

    /// The client secret, for providers that give installed apps one (Google's "Desktop app"
    /// clients; it is not confidential inside a game). Sent only to the token endpoint.
    pub fn with_client_secret(mut self, secret: impl Into<Secret>) -> Self {
        self.client_secret = Some(secret.into());
        self
    }

    /// The scopes (default `openid`; `openid` is always sent), e.g. `["openid", "email"]`.
    pub fn with_scopes<I: IntoIterator<Item = S>, S: Into<String>>(mut self, scopes: I) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        if !self.scopes.iter().any(|s| s == "openid") {
            self.scopes.insert(0, "openid".into());
        }
        self
    }

    /// How long a sign-in waits for the browser to come back (default 5 minutes; at least 1
    /// second, at most [`MAX_TIMEOUT`](crate::MAX_TIMEOUT)). The code exchange afterwards has the
    /// [`HttpConfig`] timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.clamp(Duration::from_secs(1), crate::config::MAX_TIMEOUT);
        self
    }

    /// An extra query parameter for the sign-in page (e.g. `prompt=select_account`,
    /// `login_hint`). The flow's own parameters (`state`, `redirect_uri`, …) cannot be replaced.
    pub fn with_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.push((name.into(), value.into()));
        self
    }

    /// The authorization endpoint.
    pub fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }

    /// The token endpoint.
    pub fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }

    /// The client id.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The scopes sent.
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// How long a sign-in waits for the browser.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The settings' own problems (answered `InvalidRequest` before a listener opens).
    fn check(&self) -> Result<(), BackendError> {
        endpoint(&self.authorization_endpoint, "authorization")?;
        endpoint(&self.token_endpoint, "token")?;
        if self.client_id.is_empty() {
            return Err(BackendError::InvalidRequest("sign-in: the client id is empty".into()));
        }
        Ok(())
    }

    /// The sign-in page URL.
    fn authorization_url(&self, redirect_uri: &str, challenge: &str, state: &str, nonce: &str) -> String {
        let mut params: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", &self.client_id),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ];
        let scope = self.scopes.join(" ");
        params.push(("scope", &scope));
        for (name, value) in &self.extra {
            if !params.iter().any(|(n, _)| n == name) {
                params.push((name, value));
            }
        }
        let mut url = self.authorization_endpoint.clone();
        let mut joiner = if url.contains('?') { '&' } else { '?' };
        for (name, value) in params {
            url.push(joiner);
            crate::request::encode_component(name, &mut url);
            url.push('=');
            crate::request::encode_component(value, &mut url);
            joiner = '&';
        }
        url
    }
}

/// An endpoint must be an absolute `https://` URL (`http://` only for a loopback host).
fn endpoint(url: &str, which: &str) -> Result<Uri, BackendError> {
    let refuse = || BackendError::InvalidRequest(format!("sign-in: the {which} endpoint must be an https:// URL (http:// only for a loopback address)"));
    let uri = Uri::try_from(url).map_err(|_| refuse())?;
    let host = uri.host().unwrap_or("");
    match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("https") if !host.is_empty() => Ok(uri),
        Some("http") if is_loopback_host(host) => Ok(uri),
        _ => Err(refuse()),
    }
}

/// What a finished sign-in gives: the provider's tokens and the nonce of this sign-in. Every token
/// is a [`Secret`] (redacted in `Debug`, wiped on drop).
///
/// The ID token is a signed JWT the crate does not check: send it (with the nonce) to your own
/// server, which checks the signature, issuer, audience, expiry and nonce before it trusts it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OAuthTokens {
    /// The ID token (OpenID Connect; a compact JWT, not checked here).
    pub id_token: Secret,
    /// The access token, if the provider sent one.
    pub access_token: Option<Secret>,
    /// The refresh token, if the provider sent one.
    pub refresh_token: Option<Secret>,
    /// The token type (`Bearer`), if sent.
    pub token_type: Option<String>,
    /// How long the access token is valid, if sent.
    pub expires_in: Option<Duration>,
    /// The scopes granted, if sent.
    pub scope: Option<String>,
    /// The nonce this sign-in put into the authorization request (the ID token carries it back).
    pub nonce: Secret,
}

/// The provider's sign-in page for a sign-in ([`OAuthClient::sign_in`]): open `url` in the
/// system browser (e.g. with the `open` or `webbrowser` crate), or show it to the player. Written
/// once per sign-in, in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)).
///
/// `Debug` shows the endpoint without the query.
#[derive(Message, Clone)]
#[non_exhaustive]
pub struct OAuthSignInUrl {
    /// The sign-in.
    pub id: RequestId,
    /// The full sign-in page URL (it carries this sign-in's `state`, nonce and PKCE challenge).
    pub url: String,
}

impl fmt::Debug for OAuthSignInUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let endpoint = self.url.split('?').next().unwrap_or("");
        f.debug_struct("OAuthSignInUrl").field("id", &self.id).field("endpoint", &endpoint).finish_non_exhaustive()
    }
}

/// The answer to a sign-in ([`OAuthClient::sign_in`]): the tokens, or why not. Every sign-in gets
/// exactly one. Errors: [`BackendError::OAuth`] (the player declined, the code was refused, no ID
/// token), [`Timeout`](BackendError::Timeout) (the browser did not come back in time),
/// [`Cancelled`](BackendError::Cancelled), [`Shutdown`](BackendError::Shutdown),
/// `InvalidRequest` (bad settings), and the HTTP errors of the code exchange (`Network`, `Tls`, …).
///
/// Written in `First` ([`BackendSystems::Receive`](crate::BackendSystems::Receive)).
#[derive(Message, Clone, Debug)]
#[non_exhaustive]
pub struct OAuthSignedIn {
    /// The sign-in this answers.
    pub id: RequestId,
    /// The tokens, or the error.
    pub result: Result<OAuthTokens, BackendError>,
}

/// Starts desktop sign-ins (feature `oauth`). A resource with shared access only
/// (`Res<OAuthClient>`), like [`HttpClient`](crate::HttpClient).
///
/// ```no_run
/// use bevy::prelude::*;
/// use bevy_net_backend::prelude::*;
/// use bevy_net_backend::{OAuthClient, OAuthFlow, OAuthSignInUrl, OAuthSignedIn};
///
/// fn sign_in(oauth: Res<OAuthClient>) {
///     oauth.sign_in(&OAuthFlow::google("1234-abc.apps.googleusercontent.com").with_client_secret("GOCSPX-desktop-secret"));
/// }
///
/// fn open_browser(mut urls: MessageReader<OAuthSignInUrl>) {
///     for page in urls.read() {
///         // e.g. `webbrowser::open(&page.url)`; here: show it.
///         info!("open {} in your browser", page.url);
///     }
/// }
///
/// fn signed_in(mut answers: MessageReader<OAuthSignedIn>, backend: Res<HttpClient>) {
///     for answer in answers.read() {
///         match &answer.result {
///             // Your own server's login route checks the ID token and the nonce.
///             Ok(tokens) => {
///                 let body = serde_json::json!({ "id_token": tokens.id_token.expose(), "nonce": tokens.nonce.expose() });
///                 backend.send(OutgoingRequest::post("/auth/oauth/google").with_json(&body).without_credentials());
///             }
///             Err(error) => warn!("sign-in failed: {error}"),
///         }
///     }
/// }
/// ```
#[derive(Resource, Default)]
pub struct OAuthClient {
    queue: Mutex<Vec<(RequestId, OAuthFlow)>>,
    cancels: CancelList,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient").field("queued", &lock(&self.queue).len()).finish()
    }
}

impl OAuthClient {
    /// Start a sign-in with `flow`; its page URL arrives as an [`OAuthSignInUrl`], its answer as
    /// an [`OAuthSignedIn`]. Each sign-in has its own listener, verifier, `state` and nonce. It
    /// starts in `PostUpdate` ([`BackendSystems::Send`](crate::BackendSystems::Send)).
    pub fn sign_in(&self, flow: &OAuthFlow) -> RequestId {
        let id = RequestId::next();
        lock(&self.queue).push((id, flow.clone()));
        id
    }

    /// Cancel a sign-in (the same as [`HttpClient::cancel`](crate::HttpClient::cancel)): it is
    /// answered [`BackendError::Cancelled`], its listener closes, and a code that already arrived
    /// is not exchanged if the exchange has not started.
    pub fn cancel(&self, id: RequestId) {
        self.cancels.push(id);
    }

    fn drain(&self) -> Vec<(RequestId, OAuthFlow)> {
        std::mem::take(&mut *lock(&self.queue))
    }
}

enum Event {
    Url(RequestId, String),
    Done(RequestId, Result<OAuthTokens, BackendError>),
}

struct Running {
    stop: Arc<AtomicBool>,
    deadline: Instant,
    target: String,
}

/// The sign-ins in progress (private).
#[derive(Resource)]
pub(crate) struct OAuthRuntime {
    /// Built once with the plugin (the TLS settings are read then); each exchange sets its own
    /// timeout.
    agent: HttpAgent,
    running: HashMap<RequestId, Running>,
    ready: Vec<(RequestId, Result<OAuthTokens, BackendError>)>,
    events_tx: Sender<Event>,
    events_rx: Mutex<Receiver<Event>>,
}

impl OAuthRuntime {
    fn new(tls: &TlsSettings, proxy: &crate::ProxySettings, timeout: Duration) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        // The HTTP transport warns about TLS settings that cannot be used; this agent answers the
        // exchange with the same error.
        let agent = crate::transport::http_pool::build_agent(timeout, tls, Arc::new(proxy.resolve()), false);
        Self { agent, running: HashMap::new(), ready: Vec::new(), events_tx, events_rx: Mutex::new(events_rx) }
    }

    fn rows(&self, inflight: &mut InFlight) {
        inflight.set_rows(
            Protocol::OAuth,
            self.running.iter().map(|(id, run)| (*id, RequestInfo { kind: RequestKind::OAuth, method: None, target: run.target.clone() })),
        );
    }
}

pub(crate) fn build(app: &mut App, tls: &TlsSettings, proxy: &crate::ProxySettings) {
    app.init_resource::<OAuthClient>();
    let cancels = app.world().resource::<InFlight>().cancel_list();
    if let Some(mut client) = app.world_mut().get_resource_mut::<OAuthClient>() {
        client.cancels = cancels;
    }
    let timeout = app.world().get_resource::<HttpConfig>().map_or(crate::config::DEFAULT_TIMEOUT, HttpConfig::timeout);
    app.insert_resource(OAuthRuntime::new(tls, proxy, timeout)).add_message::<OAuthSignInUrl>().add_message::<OAuthSignedIn>();
    // After the other protocols' systems of the same set: they all write `InFlight`.
    #[cfg(feature = "ssh")]
    app.add_systems(First, oauth_receive.in_set(BackendSystems::Receive).after(crate::ssh::ssh_receive))
        .add_systems(PostUpdate, oauth_send.in_set(BackendSystems::Send).after(crate::ssh::ssh_send))
        .add_systems(Last, oauth_exit.in_set(BackendSystems::Exit).after(crate::ssh::ssh_exit).run_if(on_message::<AppExit>));
    #[cfg(all(feature = "ws", not(feature = "ssh")))]
    app.add_systems(First, oauth_receive.in_set(BackendSystems::Receive).after(crate::ws::ws_receive))
        .add_systems(PostUpdate, oauth_send.in_set(BackendSystems::Send).after(crate::ws::ws_send))
        .add_systems(Last, oauth_exit.in_set(BackendSystems::Exit).after(crate::ws::ws_exit).run_if(on_message::<AppExit>));
    #[cfg(not(any(feature = "ws", feature = "ssh")))]
    app.add_systems(First, oauth_receive.in_set(BackendSystems::Receive).after(crate::inflight::receive_answers))
        .add_systems(PostUpdate, oauth_send.in_set(BackendSystems::Send).after(crate::inflight::send_requests))
        .add_systems(Last, oauth_exit.in_set(BackendSystems::Exit).after(crate::inflight::shutdown_on_exit).run_if(on_message::<AppExit>));
}

/// `PostUpdate` ([`BackendSystems::Send`]): start the queued sign-ins, apply cancels. Nothing
/// starts in a frame with an `AppExit` message.
fn oauth_send(
    client: Res<OAuthClient>,
    mut runtime: ResMut<OAuthRuntime>,
    mut inflight: ResMut<InFlight>,
    config: Res<HttpConfig>,
    mut exit: MessageReader<AppExit>,
) {
    if exit.read().count() > 0 {
        return;
    }
    let runtime = &mut *runtime;
    let queued = client.drain();
    let cancels: Vec<RequestId> = inflight.claim_cancels(|id| runtime.running.contains_key(&id) || queued.iter().any(|(q, _)| *q == id));
    if queued.is_empty() && cancels.is_empty() {
        return;
    }
    for (id, flow) in queued {
        if cancels.contains(&id) {
            runtime.ready.push((id, Err(BackendError::Cancelled)));
            continue;
        }
        if let Err(error) = flow.check() {
            runtime.ready.push((id, Err(error)));
            continue;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (agent, exchange_timeout) = (runtime.agent.clone(), config.timeout());
        let (thread_stop, events) = (Arc::clone(&stop), runtime.events_tx.clone());
        let target = flow.authorization_endpoint.split('?').next().unwrap_or("").to_string();
        // The thread's own limits end it: the browser wait, then the exchange's timeout. The ECS
        // backstop below adds a margin.
        let allowed = flow.timeout.saturating_add(config.timeout()).saturating_add(crate::DEADLINE_GRACE);
        match thread::Builder::new().name("net-backend-oauth".into()).spawn(move || run_flow(id, &flow, &agent, exchange_timeout, &thread_stop, &events)) {
            Ok(_) => {
                tracing::debug!(">>> NET-BACKEND: sign-in {id} started");
                runtime.running.insert(id, Running { stop, deadline: Instant::now().checked_add(allowed).unwrap_or_else(Instant::now), target });
            }
            Err(e) => runtime.ready.push((id, Err(BackendError::Network(format!("sign-in: could not start its thread: {e}"))))),
        }
    }
    for id in cancels {
        if let Some(run) = runtime.running.remove(&id) {
            run.stop.store(true, Ordering::SeqCst);
            runtime.ready.push((id, Err(BackendError::Cancelled)));
        }
    }
    runtime.rows(&mut inflight);
}

/// `First` ([`BackendSystems::Receive`]): page URLs and answers from the sign-in threads, the
/// backstop deadline, and the answers decided in `PostUpdate`.
fn oauth_receive(
    mut runtime: ResMut<OAuthRuntime>,
    mut inflight: ResMut<InFlight>,
    mut urls: MessageWriter<OAuthSignInUrl>,
    mut answers: MessageWriter<OAuthSignedIn>,
) {
    let runtime = &mut *runtime;
    let mut done = std::mem::take(&mut runtime.ready);
    let events: Vec<Event> = {
        let rx = lock(&runtime.events_rx);
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    };
    for event in events {
        match event {
            Event::Url(id, url) if runtime.running.contains_key(&id) => {
                urls.write(OAuthSignInUrl { id, url });
            }
            Event::Done(id, result) if runtime.running.remove(&id).is_some() => done.push((id, result)),
            // A sign-in already answered (cancelled, timed out): its late report is dropped.
            _ => {}
        }
    }
    let now = Instant::now();
    let late: Vec<RequestId> = runtime.running.iter().filter(|(_, run)| run.deadline <= now).map(|(id, _)| *id).collect();
    for id in late {
        if let Some(run) = runtime.running.remove(&id) {
            run.stop.store(true, Ordering::SeqCst);
            done.push((id, Err(BackendError::Timeout("the sign-in thread gave no answer in time".into()))));
        }
    }
    if done.is_empty() {
        return;
    }
    runtime.rows(&mut inflight);
    deliver(done, &mut answers);
}

/// `Last` on `AppExit` ([`BackendSystems::Exit`]): stop every sign-in and answer it `Shutdown`
/// (`Cancelled` when cancelled in that frame).
fn oauth_exit(client: Res<OAuthClient>, mut runtime: ResMut<OAuthRuntime>, mut inflight: ResMut<InFlight>, mut answers: MessageWriter<OAuthSignedIn>) {
    let runtime = &mut *runtime;
    let queued = client.drain();
    let cancels: Vec<RequestId> = inflight.claim_cancels(|id| runtime.running.contains_key(&id) || queued.iter().any(|(q, _)| *q == id));
    let mut done = std::mem::take(&mut runtime.ready);
    let error = |id: RequestId| if cancels.contains(&id) { BackendError::Cancelled } else { BackendError::Shutdown };
    for (id, _) in queued {
        done.push((id, Err(error(id))));
    }
    for (id, run) in runtime.running.drain() {
        run.stop.store(true, Ordering::SeqCst);
        done.push((id, Err(error(id))));
    }
    runtime.rows(&mut inflight);
    deliver(done, &mut answers);
}

fn deliver(mut done: Vec<(RequestId, Result<OAuthTokens, BackendError>)>, answers: &mut MessageWriter<OAuthSignedIn>) {
    done.sort_by_key(|(id, _)| *id);
    for (id, result) in done {
        match &result {
            Ok(_) => tracing::info!(">>> NET-BACKEND: sign-in {id} -> tokens received"),
            Err(error) => tracing::info!(">>> NET-BACKEND: sign-in {id} -> {error}"),
        }
        answers.write(OAuthSignedIn { id, result });
    }
}

// ---- the sign-in thread ------------------------------------------------------------------------

fn run_flow(id: RequestId, flow: &OAuthFlow, agent: &HttpAgent, timeout: Duration, stop: &AtomicBool, events: &Sender<Event>) {
    let result = sign_in(id, flow, agent, timeout, stop, events);
    // The plugin may be gone (exit): then nobody waits for the answer.
    let _ = events.send(Event::Done(id, result));
}

fn sign_in(
    id: RequestId,
    flow: &OAuthFlow,
    agent: &HttpAgent,
    timeout: Duration,
    stop: &AtomicBool,
    events: &Sender<Event>,
) -> Result<OAuthTokens, BackendError> {
    let deadline = Instant::now().checked_add(flow.timeout).unwrap_or_else(Instant::now);
    let listen_failed = |e: std::io::Error| BackendError::Network(format!("sign-in: could not listen on 127.0.0.1: {e}"));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(listen_failed)?;
    listener.set_nonblocking(true).map_err(listen_failed)?;
    let port = listener.local_addr().map_err(listen_failed)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_text(32)?;
    let challenge = base64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
    let state = random_text(16)?;
    let nonce = random_text(16)?;
    let url = flow.authorization_url(&redirect_uri, &challenge, &state, &nonce);
    if events.send(Event::Url(id, url)).is_err() {
        return Err(BackendError::Shutdown);
    }
    tracing::debug!(">>> NET-BACKEND: sign-in {id}: waiting for the browser on 127.0.0.1:{port}");
    let code = wait_for_code(&listener, &state, deadline, flow.timeout, stop)?;
    // One redirect with this sign-in's state: the listener is closed now.
    drop(listener);
    if stop.load(Ordering::SeqCst) {
        return Err(BackendError::Cancelled);
    }
    tracing::debug!(">>> NET-BACKEND: sign-in {id}: the browser came back, exchanging the code");
    let mut tokens = exchange(flow, agent, timeout, &code, &redirect_uri, &verifier)?;
    let mut nonce = nonce;
    tokens.nonce = Secret::new(std::mem::take(&mut *nonce));
    Ok(tokens)
}

/// `bytes` random bytes from the operating system, base64url (no padding).
fn random_text(bytes: usize) -> Result<Zeroizing<String>, BackendError> {
    let mut buffer = Zeroizing::new(vec![0u8; bytes]);
    crate::tls::random_bytes(&mut buffer).map_err(|()| BackendError::Network("sign-in: the operating system's random source failed".into()))?;
    Ok(Zeroizing::new(base64url(&buffer)))
}

/// base64url without padding (RFC 4648 §5), for PKCE and the random values.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let sym = |n: u32| ALPHABET.get(usize::try_from(n & 63).unwrap_or(0)).copied().map_or('A', char::from);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        if chunk.len() > 1 {
            out.push(sym(n >> 6));
        }
        if chunk.len() > 2 {
            out.push(sym(n));
        }
    }
    out
}

/// Percent-decode a query value (`+` is a space); invalid UTF-8 replaced.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        let hex = |at: usize| bytes.get(at).and_then(|b| char::from(*b).to_digit(16));
        match (byte, hex(i + 1), hex(i + 2)) {
            (b'+', _, _) => out.push(b' '),
            (b'%', Some(hi), Some(lo)) => {
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                i += 2;
            }
            (b, _, _) => out.push(b),
        }
        i += 1;
    }
    // Valid UTF-8 moves into the String (no copy left behind).
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// A provider's text for a message: short and printable.
fn short(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// Constant-time comparison of the returned `state` with this sign-in's.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What one request to the listener asked for.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum Redirect {
    Code(Zeroizing<String>),
    Refused(String),
    WrongState,
    Other,
}

fn parse_redirect(head: &str, state: &str) -> Redirect {
    let Some(target) = head.lines().next().and_then(|line| line.strip_prefix("GET ")).and_then(|rest| rest.split(' ').next()) else {
        return Redirect::Other;
    };
    let Some(query) = target.strip_prefix("/callback?") else { return Redirect::Other };
    let mut code = None;
    let mut error = None;
    let mut got_state = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "code" => code = Some(Zeroizing::new(decode(value))),
            "error" => error = Some(decode(value)),
            "state" => got_state = Some(decode(value)),
            _ => {}
        }
    }
    if !got_state.as_deref().is_some_and(|got| same(got, state)) {
        return Redirect::WrongState;
    }
    match (code, error) {
        (_, Some(error)) => Redirect::Refused(error),
        (Some(code), None) if !code.is_empty() => Redirect::Code(code),
        _ => Redirect::Other,
    }
}

fn page(status: &str, text: &str) -> String {
    let body = format!("<!doctype html><meta charset=\"utf-8\"><title>Sign-in</title><p>{text}</p>");
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

struct Pending {
    stream: TcpStream,
    head: Zeroizing<Vec<u8>>,
    since: Instant,
}

/// Accept connections on the listener until the browser brings the code for `state`, the
/// provider's error, the time limit or a stop.
fn wait_for_code(listener: &TcpListener, state: &str, deadline: Instant, timeout: Duration, stop: &AtomicBool) -> Result<Zeroizing<String>, BackendError> {
    let mut open: Vec<Pending> = Vec::new();
    let mut chunk = Zeroizing::new([0u8; READ_CHUNK]);
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(BackendError::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(BackendError::Timeout(format!("the browser did not come back within {timeout:?}")));
        }
        loop {
            match listener.accept() {
                Ok((stream, _)) if open.len() < MAX_OPEN_CONNECTIONS && stream.set_nonblocking(true).is_ok() => {
                    // Sized for the largest head read (the cap plus one more chunk) up front: the
                    // buffer never reallocates, so no unwiped copy of the code is left behind.
                    open.push(Pending { stream, head: Zeroizing::new(Vec::with_capacity(HEAD_CAPACITY)), since: now });
                }
                // Over the limit (or unusable): closed at once.
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => break,
                Err(e) => return Err(BackendError::Network(format!("sign-in: the loopback listener failed: {e}"))),
            }
        }
        let mut finished: Option<Result<Zeroizing<String>, BackendError>> = None;
        open.retain_mut(|pending| {
            if finished.is_some() {
                return true;
            }
            let complete = loop {
                match pending.stream.read(&mut chunk[..]) {
                    // Closed before a whole request: forget it.
                    Ok(0) => return false,
                    Ok(n) => {
                        pending.head.extend_from_slice(chunk.get(..n).unwrap_or_default());
                        if pending.head.windows(4).any(|w| w == b"\r\n\r\n") || pending.head.len() >= MAX_REDIRECT_BYTES {
                            break true;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            };
            if !complete {
                return now.saturating_duration_since(pending.since) < CONNECTION_IDLE;
            }
            // Read in place (no copy of the code): a head that is not UTF-8 is no redirect.
            let head = std::str::from_utf8(&pending.head).unwrap_or("");
            let (answer, outcome) = match parse_redirect(head, state) {
                // Written before the code is exchanged: no claim of success here.
                Redirect::Code(code) => (page("200 OK", "You can close this tab and return to the game."), Some(Ok(code))),
                Redirect::Refused(error) => (
                    page("200 OK", "The sign-in was not completed. You can close this tab and return to the game."),
                    Some(Err(BackendError::OAuth(format!("the provider answered `{}`", short(&error))))),
                ),
                Redirect::WrongState => (page("400 Bad Request", "This sign-in link is not the current one."), None),
                Redirect::Other => (page("404 Not Found", "Not found."), None),
            };
            let _ = pending.stream.set_nonblocking(false);
            let _ = pending.stream.set_write_timeout(Some(PAGE_WRITE));
            let _ = pending.stream.write_all(answer.as_bytes());
            let _ = pending.stream.flush();
            let _ = pending.stream.shutdown(std::net::Shutdown::Both);
            finished = outcome;
            false
        });
        if let Some(outcome) = finished {
            return outcome;
        }
        thread::sleep(POLL);
    }
}

/// `application/x-www-form-urlencoded` of `pairs` (percent-encoding everything but the unreserved
/// characters), in a buffer that is wiped on drop.
fn form(pairs: &[(&str, &str)]) -> Zeroizing<String> {
    let mut body = Zeroizing::new(String::with_capacity(512));
    for (n, (name, value)) in pairs.iter().enumerate() {
        if n > 0 {
            body.push('&');
        }
        crate::request::encode_component(name, &mut body);
        body.push('=');
        crate::request::encode_component(value, &mut body);
    }
    body
}

/// Exchange the code at the token endpoint (the whole call within `timeout`).
fn exchange(flow: &OAuthFlow, agent: &HttpAgent, timeout: Duration, code: &str, redirect_uri: &str, verifier: &str) -> Result<OAuthTokens, BackendError> {
    let uri = endpoint(&flow.token_endpoint, "token")?;
    let mut pairs: Vec<(&str, &str)> =
        vec![("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri), ("client_id", &flow.client_id), ("code_verifier", verifier)];
    if let Some(secret) = &flow.client_secret {
        pairs.push(("client_secret", secret.expose()));
    }
    let body = form(&pairs);
    // Before anything is sent: a proxy the client cannot use refuses the exchange.
    let bypass = agent.bypass(&uri)?;
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(http::header::ACCEPT, "application/json")
        .body(body.as_bytes())
        .map_err(|e| BackendError::InvalidRequest(format!("sign-in: the token request cannot be built ({e})")))?;
    let request = agent.agent.configure_request(request).timeout_global(Some(timeout));
    let request = if bypass { request.proxy(None).build() } else { request.build() };
    let response = agent.agent.run(request).map_err(|e| crate::transport::http_pool::map_error(e, MAX_TOKEN_ANSWER_BYTES))?;
    let status = response.status();
    let bytes = Zeroizing::new(
        response
            .into_body()
            .into_with_config()
            .limit(MAX_TOKEN_ANSWER_BYTES)
            .read_to_vec()
            .map_err(|e| crate::transport::http_pool::map_error(e, MAX_TOKEN_ANSWER_BYTES))?,
    );
    let value: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
    if !status.is_success() {
        let code = value.as_ref().and_then(|v| v.get("error")).and_then(serde_json::Value::as_str).map_or_else(|| "no error code".to_string(), short);
        return Err(BackendError::OAuth(format!("the token endpoint refused the code (HTTP {status}, {code})")));
    }
    let Some(serde_json::Value::Object(mut map)) = value else {
        return Err(BackendError::OAuth(format!("the token endpoint's answer (HTTP {status}) is not a JSON object")));
    };
    let mut take = |key: &str| match map.remove(key) {
        Some(serde_json::Value::String(text)) => Some(text),
        _ => None,
    };
    let id_token =
        take("id_token").map(Secret::new).ok_or_else(|| BackendError::OAuth("the token answer has no id_token (is `openid` among the scopes?)".into()))?;
    let access_token = take("access_token").map(Secret::new);
    let refresh_token = take("refresh_token").map(Secret::new);
    let token_type = take("token_type");
    let scope = take("scope");
    let expires_in = match map.get("expires_in") {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
    .map(Duration::from_secs);
    Ok(OAuthTokens { id_token, access_token, refresh_token, token_type, expires_in, scope, nonce: Secret::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_redirects_and_encodings() {
        let flow = OAuthFlow::google("cid").with_scopes(["email"]).with_param("prompt", "select_account").with_param("state", "evil");
        let url = flow.authorization_url("http://127.0.0.1:5/callback", "chal", "st", "no");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=cid"), "{url}");
        for part in [
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback",
            "scope=openid%20email",
            "&state=st&",
            "nonce=no",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "prompt=select_account",
        ] {
            assert!(url.contains(part), "{part}: {url}");
        }
        assert!(!url.contains("evil"), "the flow's own parameters are not replaced");
        assert!(OAuthFlow::new("https://a.example.com/auth?x=1", "https://t.example.com", "c")
            .authorization_url("r", "c", "s", "n")
            .contains("auth?x=1&response_type"));
        assert!(OAuthFlow::new("http://login.example.com/auth", "https://t.example.com/token", "c").check().is_err());
        assert!(OAuthFlow::new("https://login.example.com/auth", "http://t.example.com/token", "c").check().is_err());
        assert!(OAuthFlow::new("https://login.example.com/auth", "/token", "c").check().is_err());
        assert!(OAuthFlow::new("https://login.example.com/auth", "https://t.example.com/token", "").check().is_err());
        assert!(OAuthFlow::new("http://127.0.0.1:9/auth", "http://localhost:9/token", "c").check().is_ok());
        let debug = format!("{:?}", OAuthFlow::google("c").with_client_secret("very-secret").with_param("login_hint", "hint-value"));
        assert!(!debug.contains("very-secret") && !debug.contains("hint-value"), "{debug}");

        let code = |c: &str| Redirect::Code(Zeroizing::new(c.to_string()));
        assert_eq!(parse_redirect("GET /callback?code=a%2Fb&state=st HTTP/1.1\r\n", "st"), code("a/b"));
        assert_eq!(parse_redirect("GET /callback?state=st&code=x+y HTTP/1.1\r\n", "st"), code("x y"));
        assert_eq!(parse_redirect("GET /callback?error=access_denied&state=st HTTP/1.1\r\n", "st"), Redirect::Refused("access_denied".into()));
        assert_eq!(parse_redirect("GET /callback?code=a&state=other HTTP/1.1\r\n", "st"), Redirect::WrongState);
        assert_eq!(parse_redirect("GET /callback?code=a&state=s HTTP/1.1\r\n", "st"), Redirect::WrongState);
        assert_eq!(parse_redirect("GET /callback?code=a HTTP/1.1\r\n", "st"), Redirect::WrongState);
        assert_eq!(parse_redirect("GET /callback?error=x&state=bad HTTP/1.1\r\n", "st"), Redirect::WrongState, "an error with a wrong state ends nothing");
        assert_eq!(parse_redirect("GET /callback?state=st HTTP/1.1\r\n", "st"), Redirect::Other);
        assert_eq!(parse_redirect("GET /favicon.ico HTTP/1.1\r\n", "st"), Redirect::Other);
        assert_eq!(parse_redirect("POST /callback?code=a&state=st HTTP/1.1\r\n", "st"), Redirect::Other);
        assert_eq!(decode("a%20b+c%zz%4"), "a b c%zz%4");

        // RFC 4648 test vectors (base64url, no padding) and RFC 7636 appendix B.
        for (input, expected) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64url(input.as_bytes()), expected);
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(base64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref()), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(random_text(32).map(|t| t.len()).unwrap_or(0), 43);
        assert_ne!(random_text(16).map(|t| t.to_string()).ok(), random_text(16).map(|t| t.to_string()).ok());
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert_eq!(form(&[("a b", "c&d"), ("e", "")]).as_str(), "a%20b=c%26d&e=");
    }
}
