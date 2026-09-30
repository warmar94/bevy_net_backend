//! Unit tests: URL rules, encoding, redaction, credentials, config, errors. No network.

use std::time::Duration;

use http::header::{HeaderValue, AUTHORIZATION, USER_AGENT};
use http::{Method, StatusCode, Uri};

use crate::inflight::prepare;
use crate::request::{build_uri, check_scheme, is_loopback_host, parse_base, redacted_url};
use crate::*;

const SECRET: &str = "s3cr3t-fake-token";

fn cfg(base: &str) -> HttpConfig {
    HttpConfig::new(base)
}

// --- URL building and the plain-http rule -------------------------------------------------------

#[test]
fn base_and_path_are_joined() {
    let uri = build_uri("https://api.example.com/v1/", "/players/me", &[], false).ok();
    assert_eq!(uri.map(|u| u.to_string()), Some("https://api.example.com/v1/players/me".to_string()));
    let uri = build_uri("https://api.example.com", "/", &[], false).ok();
    assert_eq!(uri.map(|u| u.to_string()), Some("https://api.example.com/".to_string()));
}

#[test]
fn query_is_percent_encoded() {
    let query = vec![("name".to_string(), "Ayla & Bo".to_string()), ("x/y".to_string(), "ü=1".to_string())];
    let uri = build_uri("https://api.example.com", "/search", &query, false).ok();
    assert_eq!(uri.map(|u| u.to_string()), Some("https://api.example.com/search?name=Ayla%20%26%20Bo&x%2Fy=%C3%BC%3D1".to_string()));
}

#[test]
fn bad_paths_are_invalid_requests() {
    for path in ["players", "https://evil.example.com/steal", "/a?b=c", "/a#frag", "/has space", ""] {
        let result = build_uri("https://api.example.com", path, &[], false);
        assert!(matches!(result, Err(BackendError::InvalidRequest(_))), "{path:?} -> {result:?}");
    }
}

#[test]
fn bad_base_urls_are_rejected() {
    assert_eq!(parse_base("").err(), Some(ConfigError::NoBaseUrl));
    for base in ["api.example.com", "ftp://api.example.com", "https://user:pw@api.example.com", "https://api.example.com/?q=1", "https://api.example.com/#x"] {
        assert!(matches!(parse_base(base), Err(ConfigError::BadBaseUrl(_))), "{base}");
    }
    assert!(parse_base("http://localhost:8000/api").is_ok());
}

#[test]
fn config_errors_never_quote_the_url() {
    let err = cfg("https://player:hunter2@api.example.com").validate().err().map(|e| e.to_string()).unwrap_or_default();
    assert!(!err.contains("hunter2"), "{err}");
}

#[test]
fn dot_segments_and_host_tricks_stay_on_the_base() {
    for path in [
        "/../admin",
        "/v1/./x",
        "/a/%2e%2E/b",
        "/%2E",
        // The bypasses of review r1b: encoded separators, backslash, `;` parameters, double encoding.
        "/..%2fadmin",
        "/%2e%2e%2fadmin",
        "/..%5cadmin",
        "/%2e%2e%5cadmin",
        "/..;/admin",
        "/%252e%252e/admin",
        "/%25252e%25252e/admin",
        "/players/..%2Fadmin",
        "/a%252fb",
        "/a\\b",
        "/..\\admin",
        "/.;x/y",
        "/a%00b",
        "/a%2500b",
        "/a%0d%0aX-Injected:1",
        "/%2525252525252525252e",
    ] {
        assert!(matches!(build_uri("https://api.example.com/v1", path, &[], false), Err(BackendError::InvalidRequest(_))), "{path}");
    }
    let uri = build_uri("https://api.example.com", "//evil.example.com/x", &[], false).ok();
    assert_eq!(uri.as_ref().and_then(Uri::host), Some("api.example.com"));
    for path in ["/a.b/..c/...", "/names/Ann%20Lee", "/matrix;v=1/x", "/100%", "/caf%C3%A9", "/a/.well-known/x"] {
        assert!(build_uri("https://api.example.com", path, &[], false).is_ok(), "{path}");
    }
    let mut config = HttpConfig::default();
    config.set_base_url("http://localhost@evil.example.com");
    assert!(config.validate().is_err());
    assert!(!is_loopback_host("127.0.0.1.nip.io"));
}

#[test]
fn a_fixed_or_removed_default_header_clears_its_error() {
    let broken = cfg("https://a.example").with_header("X-A", "bad\n");
    assert!(matches!(broken.validate(), Err(ConfigError::BadHeader(_))));
    assert!(broken.clone().with_header("x-a", "fine").validate().is_ok());
    assert!(broken.without_header("X-A").validate().is_ok());
    assert!(cfg("https://a.example").with_header("bad name", "x").with_header("X-B", "\r").without_header("bad name").validate().is_err());
}

#[test]
fn decode_errors_do_not_display_the_body() {
    let response = RawResponse::new(StatusCode::OK, r#"{"token":"s3cr3t-fake-token"}"#);
    let message = format!("invalid type: string \"{SECRET}\", expected u32");
    let decode = BackendError::Decode { message, response: Box::new(response) };
    assert!(!decode.to_string().contains(SECRET), "{decode}");
}

#[test]
fn loopback_hosts() {
    for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "[::1]", "::1", "[::ffff:127.0.0.1]"] {
        assert!(is_loopback_host(host), "{host}");
    }
    for host in ["example.com", "10.0.0.1", "192.168.1.2", "[::2]", "localhost.example.com", "0.0.0.0"] {
        assert!(!is_loopback_host(host), "{host}");
    }
}

#[test]
fn plain_http_only_for_loopback_unless_allowed() {
    let ok = |url: &'static str, allow: bool| check_scheme(&Uri::from_static(url), allow);
    assert!(ok("https://example.com/", false).is_ok());
    assert!(ok("http://127.0.0.1:8000/", false).is_ok());
    assert!(ok("http://localhost/", false).is_ok());
    assert!(ok("http://[::1]:9000/", false).is_ok());
    assert!(ok("ws://localhost/", false).is_ok());
    assert!(ok("wss://example.com/", false).is_ok());
    assert_eq!(ok("http://192.168.1.20:8000/", false), Err(BackendError::InsecureHttp { host: "192.168.1.20".into() }));
    assert!(matches!(ok("ws://example.com/", false), Err(BackendError::InsecureHttp { .. })));
    assert!(ok("http://192.168.1.20:8000/", true).is_ok());
    assert!(matches!(ok("ftp://example.com/", true), Err(BackendError::InvalidRequest(_))));
}

#[test]
fn redacted_url_hides_the_query() {
    let uri = Uri::from_static("https://api.example.com/x?api_key=abc");
    assert_eq!(redacted_url(&uri), "https://api.example.com/x?…");
}

// --- Request preparation ------------------------------------------------------------------------

#[test]
fn defaults_then_own_headers_then_credentials() {
    let config = cfg("https://api.example.com").with_header("X-Game", "demo").with_header("Authorization", "from-config");
    let credentials = BackendCredentials::new(BearerToken::new(SECRET));
    let request = OutgoingRequest::get("/me").with_header("X-Game", "own");
    let prepared = prepare(request, &config, Some(&credentials)).ok();
    let headers = prepared.map(|p| p.headers).unwrap_or_default();
    assert_eq!(headers.get("x-game"), Some(&HeaderValue::from_static("own")));
    assert_eq!(headers.get(AUTHORIZATION).map(|v| v.to_str().ok()), Some(Some(format!("Bearer {SECRET}").as_str())));
    assert!(headers.get(AUTHORIZATION).is_some_and(HeaderValue::is_sensitive));
    assert!(headers.contains_key(USER_AGENT));
}

#[test]
fn without_credentials_skips_them() {
    let config = cfg("https://api.example.com");
    let credentials = BackendCredentials::new(BearerToken::new(SECRET));
    let prepared = prepare(OutgoingRequest::post("/login").without_credentials(), &config, Some(&credentials)).ok();
    assert!(prepared.is_some_and(|p| !p.headers.contains_key(AUTHORIZATION)));
}

#[test]
fn request_timeout_overrides_config() {
    let config = cfg("https://api.example.com").with_timeout(Duration::from_secs(3)).with_max_body_bytes(99);
    let a = prepare(OutgoingRequest::get("/a"), &config, None).ok();
    let b = prepare(OutgoingRequest::get("/b").with_timeout(Duration::from_millis(250)), &config, None).ok();
    assert_eq!(a.as_ref().map(|p| p.timeout), Some(Duration::from_secs(3)));
    assert_eq!(a.map(|p| p.max_body_bytes), Some(99));
    assert_eq!(b.map(|p| p.timeout), Some(Duration::from_millis(250)));
}

#[test]
fn invalid_builders_are_answered_not_panicked() {
    let config = cfg("https://api.example.com");
    let bad_name = prepare(OutgoingRequest::get("/a").with_header("bad name", "x"), &config, None);
    assert!(matches!(bad_name, Err(BackendError::InvalidRequest(_))));
    let bad_value = prepare(OutgoingRequest::get("/a").with_header("X-A", "line\nbreak"), &config, None);
    assert!(matches!(bad_value, Err(BackendError::InvalidRequest(_))));
    let bad_config = prepare(OutgoingRequest::get("/a"), &cfg("https://api.example.com").with_header("X-B", "\r\n"), None);
    assert!(matches!(bad_config, Err(BackendError::InvalidRequest(_))));
    let no_base = prepare(OutgoingRequest::get("/a"), &HttpConfig::default(), None);
    assert!(matches!(no_base, Err(BackendError::InvalidRequest(_))));
}

#[test]
fn request_purpose_is_http() {
    assert_eq!(OutgoingRequest::get("/a").purpose(), RequestPurpose::Http);
}

// --- Credentials --------------------------------------------------------------------------------

#[test]
fn api_key_header_and_query() {
    let mut request = OutgoingRequest::get("/a");
    ApiKeyHeader::new("X-Api-Key", SECRET).apply(&mut request);
    assert_eq!(request.headers().get("x-api-key").and_then(|v| v.to_str().ok()), Some(SECRET));
    assert!(request.headers().get("x-api-key").is_some_and(HeaderValue::is_sensitive));

    let mut request = OutgoingRequest::get("/a").with_query("api_key", "old");
    ApiKeyQuery::new("api_key", SECRET).apply(&mut request);
    assert_eq!(request.query(), &[("api_key".to_string(), SECRET.to_string())]);
}

#[test]
fn invalid_credentials_reject_without_the_secret() {
    let mut request = OutgoingRequest::get("/a");
    BearerToken::new(format!("{SECRET}\nX-Injected: 1")).apply(&mut request);
    let error = request.error().map(ToString::to_string).unwrap_or_default();
    assert!(error.starts_with("invalid request"), "{error}");
    assert!(!error.contains(SECRET));

    let mut request = OutgoingRequest::get("/a");
    ApiKeyHeader::new("bad name", SECRET).apply(&mut request);
    assert!(matches!(request.error(), Some(BackendError::InvalidRequest(_))));
}

#[cfg(feature = "json")]
#[test]
fn json_body_field_only_touches_json_objects() {
    let field = JsonBodyField::new("token", SECRET);
    let mut request = OutgoingRequest::post("/a").with_json(&serde_json::json!({"score": 10}));
    field.apply(&mut request);
    let body: serde_json::Value = serde_json::from_slice(request.body().unwrap_or_default()).unwrap_or_default();
    assert_eq!(body, serde_json::json!({"score": 10, "token": SECRET}));

    for body in [None, Some(b"[1,2]".to_vec()), Some(b"not json".to_vec())] {
        let mut request = OutgoingRequest::post("/a");
        request.set_body(body.clone());
        field.apply(&mut request);
        assert_eq!(request.body().map(<[u8]>::to_vec), body);
        assert!(request.error().is_none());
    }
}

// --- Redaction ----------------------------------------------------------------------------------

#[test]
fn secrets_never_show_in_debug_or_display() {
    let secret = Secret::new(SECRET);
    assert_eq!(format!("{secret}"), "<redacted>");
    assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    assert_eq!(secret.expose(), SECRET);

    let mut outputs = vec![
        format!("{:?}", BearerToken::new(SECRET)),
        format!("{:?}", ApiKeyHeader::new("X-Api-Key", SECRET)),
        format!("{:?}", ApiKeyQuery::new("key", SECRET)),
        format!("{:?}", BackendCredentials::new(BearerToken::new(SECRET))),
        format!("{:?}", cfg("https://api.example.com").with_header("X-Api-Key", SECRET)),
    ];
    #[cfg(feature = "json")]
    outputs.push(format!("{:?}", JsonBodyField::new("token", SECRET)));

    let config = cfg("https://api.example.com");
    let mut request = OutgoingRequest::post("/a").with_query("q", SECRET).with_header("X-Other", SECRET).with_body(SECRET.as_bytes().to_vec());
    BearerToken::new(SECRET).apply(&mut request);
    outputs.push(format!("{request:?}"));
    let prepared = prepare(request, &config, Some(&BackendCredentials::new(ApiKeyQuery::new("k", SECRET))));
    outputs.push(format!("{prepared:?}"));
    let response = RawResponse::new(StatusCode::OK, format!(r#"{{"token":"{SECRET}"}}"#));
    outputs.push(format!("{response:?}"));
    outputs.push(format!("{:?}", BackendError::Status(Box::new(response.clone()))));
    outputs.push(format!("{}", BackendError::Status(Box::new(response))));

    for output in outputs {
        assert!(!output.contains(SECRET), "leaked: {output}");
    }
}

// --- Config -------------------------------------------------------------------------------------

#[test]
fn config_values_are_clamped() {
    let config = cfg("https://a.example").with_workers(0).with_timeout(Duration::ZERO).with_max_body_bytes(0);
    assert_eq!((config.workers(), config.timeout(), config.max_body_bytes()), (1, Duration::from_millis(1), 1));
    let config = config.with_workers(1000).with_timeout(Duration::from_secs(100_000));
    assert_eq!((config.workers(), config.timeout()), (MAX_WORKERS, MAX_TIMEOUT));
    let defaults = HttpConfig::default();
    assert_eq!((defaults.workers(), defaults.timeout(), defaults.max_body_bytes()), (DEFAULT_WORKERS, DEFAULT_TIMEOUT, DEFAULT_MAX_BODY_BYTES));
    assert!(!defaults.insecure_http_allowed());
    assert_eq!(defaults.validate(), Err(ConfigError::NoBaseUrl));
}

#[test]
fn set_base_url_at_runtime() {
    let mut config = HttpConfig::default();
    config.set_base_url("https://api.example.com");
    assert!(config.validate().is_ok());
    assert!(cfg("https://a.example").without_header("user-agent").headers().is_empty());
}

// --- Errors and responses -----------------------------------------------------------------------

#[test]
fn error_helpers() {
    let response = RawResponse::new(StatusCode::UNPROCESSABLE_ENTITY, "{}");
    let status = BackendError::Status(Box::new(response.clone()));
    assert_eq!(status.status(), Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert_eq!(status.response(), Some(&response));
    assert_eq!(BackendError::Cancelled.status(), None);
    assert!(BackendError::Encode("x".into()).is_invalid_request());
    assert!(!BackendError::Timeout("x".into()).is_invalid_request());
    assert_eq!(status.to_string(), "HTTP status 422 Unprocessable Entity");
}

#[cfg(feature = "json")]
#[test]
fn raw_response_json_and_empty_body() {
    let response = RawResponse::new(StatusCode::OK, r#"{"a":1}"#);
    assert_eq!(response.json::<serde_json::Value>().ok(), Some(serde_json::json!({"a": 1})));
    let empty = RawResponse::new(StatusCode::NO_CONTENT, "");
    assert!(empty.json::<()>().is_ok());
    assert_eq!(empty.json::<Option<u32>>().ok(), Some(None));
    assert!(RawResponse::new(StatusCode::OK, "nope").json::<u32>().is_err());
}

#[test]
fn request_ids_are_unique_and_ordered() {
    let a = RequestId::next();
    let b = RequestId::next();
    assert!(a < b);
    assert_ne!(a.to_string(), b.to_string());
}

#[test]
fn raw_request_helpers_build_the_right_requests() {
    let method = Method::DELETE;
    let request = OutgoingRequest::new(method.clone(), "/x").with_body(vec![1, 2]);
    assert_eq!((request.method(), request.path(), request.body()), (&method, "/x", Some(&[1u8, 2][..])));
    assert!(request.uses_credentials());
}

// --- TLS provider -------------------------------------------------------------------------------

#[cfg(feature = "http")]
#[test]
fn the_tls_provider_is_ring_with_tls13_and_tls12_suites() {
    let provider = crate::tls::provider();
    assert!(provider.cipher_suites.iter().any(|s| s.version() == &rustls::version::TLS13));
    assert!(provider.cipher_suites.iter().any(|s| s.version() == &rustls::version::TLS12));
}

#[test]
fn the_shared_cancel_list_is_claimed_per_protocol_and_forgets_unowned_ids() {
    use crate::inflight::CancelList;
    let list = CancelList::default();
    let (a, b, c) = (RequestId::next(), RequestId::next(), RequestId::next());
    for id in [a, b, c] {
        list.push(id);
    }
    // One protocol takes only its own id; the others stay for the next protocol.
    assert_eq!(list.claim(|id| id == b), vec![b]);
    assert_eq!(list.claim(|id| id == c), vec![c]);
    assert_eq!(list.len(), 1);
    // An id nobody claims survives one full phase (it may belong to a request queued late) ...
    list.age();
    list.age();
    assert_eq!(list.len(), 1);
    // ... and is dropped at the start of the third.
    list.age();
    assert_eq!(list.len(), 0);
    // A late push restarts the count for that id only.
    list.push(a);
    list.age();
    assert_eq!(list.claim(|id| id == a), vec![a]);
}

#[cfg(feature = "ws")]
#[test]
fn each_protocol_replaces_only_its_own_in_flight_rows() {
    use crate::inflight::{Protocol, RequestInfo, RequestKind};
    let mut inflight = InFlight::default();
    let (a, b) = (RequestId::next(), RequestId::next());
    let row = |kind| RequestInfo { kind, method: None, target: "main".into() };
    inflight.set_rows(Protocol::WebSocket, [(a, row(RequestKind::WebSocket)), (b, row(RequestKind::WebSocket))]);
    assert_eq!(inflight.ids(), vec![a, b]);
    inflight.set_rows(Protocol::WebSocket, [(b, row(RequestKind::WebSocket))]);
    assert_eq!(inflight.ids(), vec![b]);
    assert_eq!(inflight.len(), 1);
    assert_eq!(inflight.describe(b).map(|r| r.kind), Some(RequestKind::WebSocket));
    inflight.set_rows(Protocol::WebSocket, []);
    assert!(inflight.is_empty());
}

#[test]
fn request_too_large_and_body_too_large_say_different_things_about_sending() {
    let request = BackendError::request_too_large(1024, 5000);
    assert_eq!(request.was_sent(), Some(false), "a refused request never left the machine");
    assert!(request.is_invalid_request());
    assert_eq!(request.to_string(), "the request (5000 bytes) is larger than the limit of 1024 bytes; not sent");
    assert_eq!(format!("{request:?}"), "RequestTooLarge { limit: 1024, size: 5000 }");
    let answer = BackendError::BodyTooLarge { limit: 1024 };
    assert_eq!(answer.was_sent(), Some(true), "the server answered: the request went out");
    assert!(!answer.is_invalid_request());
}
