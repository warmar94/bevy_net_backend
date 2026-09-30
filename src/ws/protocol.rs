//! The message protocol on top of WebSocket frames: [`WsProtocol`] and the default
//! `JsonEnvelope` (feature `json`).

use super::WsFrame;

/// What the protocol made of an incoming frame.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsIncoming {
    /// The answer to the request with this wire id: its payload, or the server's error payload.
    Response {
        /// The id the request was encoded with.
        wire_id: u64,
        /// The payload (`Ok`) or the error payload (`Err`, answered as `BackendError::Rejected`).
        result: Result<Vec<u8>, Vec<u8>>,
    },
    /// A server push of `kind`.
    Push {
        /// The push kind.
        kind: String,
        /// Its payload.
        data: Vec<u8>,
    },
    /// The server accepted the first-message authentication (releases requests held by
    /// `WsSettings::with_auth_ack`).
    AuthOk,
    /// The server refused the first-message authentication: the connection is closed and not
    /// retried. The text is the server's (not logged by this crate).
    AuthFailed(String),
    /// Nothing for the protocol (the frame still arrives as a `WsMessage`).
    Ignore,
}

/// How requests and pushes are laid out in frames. Every frame also arrives raw as a
/// [`WsMessage`](super::WsMessage), whatever the protocol does with it.
///
/// **Compatibility promise:** methods added later always come with a default implementation.
pub trait WsProtocol: Send + Sync + 'static {
    /// Encode a request. `wire_id` is unique per process and must come back in the answer.
    fn encode_request(&self, wire_id: u64, kind: &str, payload: &[u8]) -> Result<WsFrame, String>;

    /// Decode a frame the server sent.
    fn decode(&self, frame: &WsFrame) -> WsIncoming;

    /// Whether to reconnect after the server closed with `code` (default: yes, except the
    /// application policy range 4000–4099).
    fn retry_after_close(&self, code: u16) -> bool {
        !(4000..4100).contains(&code)
    }
}

/// The default protocol (feature `json`): JSON objects in text frames.
///
/// - request: `{"id":7,"type":"chat.send","data":{…}}`
/// - response: `{"id":7,"ok":true,"data":…}` or `{"id":7,"ok":false,"error":…}`: an object with
///   a numeric `id` AND either an `ok` field or no `type` (`ok` defaults to `true`)
/// - push: `{"type":"chat.message","data":…}`; a push may carry its own `id` (a message id) as
///   long as it has no `ok` field
/// - first-message auth: `{"type":"auth.ok"}` accepted, `{"type":"auth.failed","error":…}` refused
///
/// Binary frames and anything else are left to the game (`WsMessage`).
#[cfg(feature = "json")]
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonEnvelope;

#[cfg(feature = "json")]
impl WsProtocol for JsonEnvelope {
    fn encode_request(&self, wire_id: u64, kind: &str, payload: &[u8]) -> Result<WsFrame, String> {
        let data: serde_json::Value = if payload.iter().all(u8::is_ascii_whitespace) {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(payload).map_err(|e| format!("the payload is not JSON: {e}"))?
        };
        let envelope = serde_json::json!({ "id": wire_id, "type": kind, "data": data });
        Ok(WsFrame::Text(envelope.to_string()))
    }

    fn decode(&self, frame: &WsFrame) -> WsIncoming {
        let WsFrame::Text(text) = frame else { return WsIncoming::Ignore };
        let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(text) else {
            return WsIncoming::Ignore;
        };
        let bytes = |key: &str| object.get(key).map(|v| v.to_string().into_bytes()).unwrap_or_else(|| b"null".to_vec());
        let kind = object.get("type").and_then(serde_json::Value::as_str);
        if let Some(wire_id) = object.get("id").and_then(serde_json::Value::as_u64) {
            if object.contains_key("ok") || kind.is_none() {
                let ok = object.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(true);
                let result = if ok { Ok(bytes("data")) } else { Err(bytes("error")) };
                return WsIncoming::Response { wire_id, result };
            }
        }
        match kind {
            Some("auth.ok") => WsIncoming::AuthOk,
            Some("auth.failed") => WsIncoming::AuthFailed(object.get("error").map(ToString::to_string).unwrap_or_default()),
            Some(kind) => WsIncoming::Push { kind: kind.to_string(), data: bytes("data") },
            None => WsIncoming::Ignore,
        }
    }
}

#[cfg(all(test, feature = "json"))]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let frame = JsonEnvelope.encode_request(7, "chat.send", br#"{"text":"hi"}"#).unwrap_or_else(|e| panic!("{e}"));
        let text = frame.as_text().unwrap_or_default().to_string();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(value, serde_json::json!({"id": 7, "type": "chat.send", "data": {"text": "hi"}}));
        assert!(JsonEnvelope.encode_request(1, "x", b"not json").is_err());
        assert_eq!(
            JsonEnvelope.decode(&WsFrame::Text(r#"{"id":7,"ok":true,"data":{"n":1}}"#.into())),
            WsIncoming::Response { wire_id: 7, result: Ok(br#"{"n":1}"#.to_vec()) }
        );
        assert_eq!(
            JsonEnvelope.decode(&WsFrame::Text(r#"{"id":8,"ok":false,"error":{"code":"nope"}}"#.into())),
            WsIncoming::Response { wire_id: 8, result: Err(br#"{"code":"nope"}"#.to_vec()) }
        );
        assert_eq!(JsonEnvelope.decode(&WsFrame::Text(r#"{"type":"tick","data":3}"#.into())), WsIncoming::Push { kind: "tick".into(), data: b"3".to_vec() });
        assert!(matches!(JsonEnvelope.decode(&WsFrame::Text(r#"{"type":"auth.failed","error":"bad"}"#.into())), WsIncoming::AuthFailed(_)));
        assert_eq!(JsonEnvelope.decode(&WsFrame::Text("plain".into())), WsIncoming::Ignore);
        // A push with its own message id is still a push; `{"type":"auth.ok"}` releases held requests.
        assert_eq!(
            JsonEnvelope.decode(&WsFrame::Text(r#"{"type":"chat.message","id":123,"data":1}"#.into())),
            WsIncoming::Push { kind: "chat.message".into(), data: b"1".to_vec() }
        );
        assert_eq!(JsonEnvelope.decode(&WsFrame::Text(r#"{"type":"auth.ok"}"#.into())), WsIncoming::AuthOk);
        assert_eq!(JsonEnvelope.decode(&WsFrame::Binary(vec![1])), WsIncoming::Ignore);
        assert!(JsonEnvelope.retry_after_close(1006) && !JsonEnvelope.retry_after_close(4001));
    }
}
