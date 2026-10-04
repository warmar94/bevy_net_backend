//! The desktop sign-in (feature `oauth`) against a mock OpenID Connect provider this test starts on
//! 127.0.0.1: an authorization endpoint that sends the "browser" back to the loopback redirect
//! (or declines), and a token endpoint that checks the PKCE verifier, the redirect address, the
//! client and that each code is used once. The test plays the browser. Never another host;
//! every wait is bounded.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_headless_test::prelude::*;
use bevy_net_backend::*;

#[allow(dead_code)]
#[path = "support/oauth_provider.rs"]
mod oauth_provider;

use oauth_provider::*;

fn app() -> TestApp {
    let mut app = TestApp::builder().frame_duration(Duration::from_millis(1)).real_pause(Duration::from_millis(2)).build();
    app.add_plugins(BackendPlugin::default());
    app.watch::<OAuthSignInUrl>().watch::<OAuthSignedIn>();
    app
}

fn oauth(app: &TestApp) -> &OAuthClient {
    app.world().resource::<OAuthClient>()
}

fn url_of(app: &mut TestApp, id: RequestId) -> String {
    for _ in 0..3000 {
        app.step();
        if let Some(page) = app.all_messages::<OAuthSignInUrl>().into_iter().find(|p| p.id == id) {
            return page.url;
        }
    }
    panic!("no sign-in URL for {id}")
}

fn answer_of(app: &mut TestApp, id: RequestId) -> Result<OAuthTokens, BackendError> {
    for _ in 0..4000 {
        app.step();
        if let Some(answer) = app.all_messages::<OAuthSignedIn>().into_iter().find(|a| a.id == id) {
            return answer.result;
        }
    }
    panic!("{id} was not answered")
}

fn listener_closed(redirect: &str) -> bool {
    // Bounded: the thread drops the listener right after the page.
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if browser_get(redirect).is_none() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_full_sign_in_with_pkce_state_and_nonce() {
    let provider = Provider::start();
    let mut app = app();
    let id = oauth(&app).sign_in(&provider.flow().with_param("prompt", "select_account"));
    let url = url_of(&mut app, id);
    let q = query_of(&url);
    assert_eq!(q.get("scope").map(String::as_str), Some("openid email"));
    assert_eq!(q.get("prompt").map(String::as_str), Some("select_account"));
    assert_eq!(q.get("state").map(String::len), Some(22), "128 random bits");
    assert_eq!(q.get("code_challenge").map(String::len), Some(43), "SHA-256, base64url");
    assert_eq!(app.world().resource::<InFlight>().describe(id).map(|i| i.kind), Some(RequestKind::OAuth));
    let redirect = browse(&url);
    let tokens = answer_of(&mut app, id).unwrap_or_else(|e| panic!("{e}"));
    let nonce = q.get("nonce").cloned().unwrap_or_default();
    assert_eq!(tokens.id_token.expose(), format!("fake-id-token.{nonce}.sig"));
    assert_eq!(tokens.nonce.expose(), nonce);
    assert_eq!(tokens.access_token.as_ref().map(Secret::expose), Some("fake-access-fake-code-1"));
    assert_eq!(tokens.refresh_token.as_ref().map(Secret::expose), Some("fake-refresh-fake-code-1"));
    assert_eq!(
        (tokens.token_type.as_deref(), tokens.expires_in, tokens.scope.as_deref()),
        (Some("Bearer"), Some(Duration::from_secs(3599)), Some("openid email"))
    );
    assert!(!format!("{tokens:?}").contains("fake-"), "Debug redacts every token");
    // The verifier matched the challenge (the mock refuses otherwise) and is 256 random bits.
    assert_eq!(provider.with(|s| s.verifiers.clone()).first().map(String::len), Some(43));
    // Single use: the listener is closed, a replay of the same redirect reaches nobody.
    assert!(listener_closed(&redirect), "the listener still answers after the sign-in");
    assert!(app.world().resource::<InFlight>().is_empty());

    // A second sign-in has its own port, state, nonce and verifier.
    let second = oauth(&app).sign_in(&provider.flow());
    let url2 = url_of(&mut app, second);
    let q2 = query_of(&url2);
    for key in ["state", "nonce", "code_challenge", "redirect_uri"] {
        assert_ne!(q.get(key), q2.get(key), "{key} is new");
    }
    browse(&url2);
    assert!(answer_of(&mut app, second).is_ok());
}

#[test]
fn a_wrong_state_and_other_requests_are_ignored_until_the_right_redirect() {
    let provider = Provider::start();
    let mut app = app();
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    let (_, location) = browser_get(&url).unwrap_or_else(|| panic!("provider down"));
    let location = location.unwrap_or_default();
    let base = location.split('?').next().unwrap_or("").to_string();
    let port_root = base.trim_end_matches("/callback").to_string();
    // A forged redirect (wrong state), a code without state, another path: refused pages.
    assert_eq!(browser_get(&format!("{base}?code=forged&state=forged")).map(|a| a.0), Some(400));
    assert_eq!(browser_get(&format!("{base}?code=forged")).map(|a| a.0), Some(400));
    assert_eq!(browser_get(&format!("{port_root}/favicon.ico")).map(|a| a.0), Some(404));
    // A connection that sends nothing does not block the real redirect.
    let idle = TcpStream::connect(base.trim_start_matches("http://").split('/').next().unwrap_or(""));
    assert!(idle.is_ok());
    for _ in 0..20 {
        app.step();
    }
    assert!(app.all_messages::<OAuthSignedIn>().is_empty(), "still waiting");
    assert_eq!(browser_get(&location).map(|a| a.0), Some(200));
    assert!(answer_of(&mut app, id).is_ok());
    drop(idle);
    assert_eq!(provider.token_hits.load(Ordering::SeqCst), 1, "only the right code was exchanged");
}

#[test]
fn declined_refused_and_incomplete_answers() {
    let provider = Provider::start();
    let mut app = app();

    // The player declines: the provider's error, nothing exchanged.
    provider.with(|s| s.deny = true);
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    let (_, location) = browser_get(&url).unwrap_or_else(|| panic!("provider down"));
    assert_eq!(browser_get(&location.unwrap_or_default()).map(|a| a.0), Some(200));
    let error = answer_of(&mut app, id).err();
    assert!(matches!(&error, Some(BackendError::OAuth(why)) if why.contains("access_denied")), "{error:?}");
    assert_eq!(provider.token_hits.load(Ordering::SeqCst), 0);
    provider.with(|s| s.deny = false);

    // The token endpoint refuses a code that was already used.
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    let (_, location) = browser_get(&url).unwrap_or_else(|| panic!("provider down"));
    let code = query_of(&location.clone().unwrap_or_default()).get("code").cloned().unwrap_or_default();
    provider.with(|s| s.used.push(code));
    assert_eq!(browser_get(&location.unwrap_or_default()).map(|a| a.0), Some(200));
    let error = answer_of(&mut app, id).err();
    assert!(matches!(&error, Some(BackendError::OAuth(why)) if why.contains("invalid_grant") && why.contains("400")), "{error:?}");

    // An answer without an ID token.
    provider.with(|s| s.no_id_token = true);
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    browse(&url);
    let error = answer_of(&mut app, id).err();
    assert!(matches!(&error, Some(BackendError::OAuth(why)) if why.contains("id_token")), "{error:?}");
    provider.with(|s| s.no_id_token = false);

    // A token endpoint that is not running.
    let dead = TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(1);
    let flow = OAuthFlow::new(format!("http://127.0.0.1:{}/authorize", provider.port), format!("http://127.0.0.1:{dead}/token"), CLIENT_ID);
    let id = oauth(&app).sign_in(&flow);
    browse(&url_of(&mut app, id));
    assert!(matches!(answer_of(&mut app, id), Err(BackendError::Network(_))));
}

#[test]
fn timeout_cancel_exit_and_bad_settings() {
    let provider = Provider::start();
    let mut app = app();

    // Nobody comes back: the time limit, and the listener is closed.
    let id = oauth(&app).sign_in(&provider.flow().with_timeout(Duration::from_secs(1)));
    let url = url_of(&mut app, id);
    let redirect = query_of(&url).get("redirect_uri").cloned().unwrap_or_default();
    let started = Instant::now();
    let error = answer_of(&mut app, id).err();
    assert!(matches!(&error, Some(BackendError::Timeout(why)) if why.contains("browser")), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(listener_closed(&redirect));

    // Cancelled while waiting: answered, listener closed, a late redirect is not exchanged.
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    let (_, location) = browser_get(&url).unwrap_or_else(|| panic!("provider down"));
    oauth(&app).cancel(id);
    assert_eq!(answer_of(&mut app, id).err(), Some(BackendError::Cancelled));
    assert!(listener_closed(&location.unwrap_or_default()));
    assert_eq!(provider.token_hits.load(Ordering::SeqCst), 0);

    // Bad settings: refused at once.
    for flow in [
        OAuthFlow::new("http://login.example.com/authorize", "https://login.example.com/token", CLIENT_ID),
        OAuthFlow::new("https://login.example.com/authorize", "http://login.example.com/token", CLIENT_ID),
        OAuthFlow::new("https://login.example.com/authorize", "https://login.example.com/token", ""),
    ] {
        let id = oauth(&app).sign_in(&flow);
        let error = answer_of(&mut app, id).err();
        assert!(matches!(&error, Some(BackendError::InvalidRequest(_))), "{error:?}");
        assert!(app.all_messages::<OAuthSignInUrl>().iter().all(|p| p.id != id), "no URL for refused settings");
    }

    // App exit while waiting: Shutdown, the listener closes.
    let id = oauth(&app).sign_in(&provider.flow());
    let url = url_of(&mut app, id);
    let redirect = query_of(&url).get("redirect_uri").cloned().unwrap_or_default();
    app.world_mut().write_message(AppExit::Success);
    app.step();
    assert_eq!(app.all_messages::<OAuthSignedIn>().into_iter().find(|a| a.id == id).and_then(|a| a.result.err()), Some(BackendError::Shutdown));
    assert!(listener_closed(&redirect));
}
