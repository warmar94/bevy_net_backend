//! The HTTP `CONNECT` tunnel WebSocket connections take through an `http://` proxy. Which proxy
//! (and whether one at all) comes from the transport's [`ProxySettings`](crate::ProxySettings),
//! resolved in `crate::proxy`.

use std::io::{self, Read, Write};
use std::time::Instant;

use tungstenite::handshake::machine::TryParse;
use zeroize::Zeroizing;

use crate::proxy::ProxyTarget;
use crate::response::BackendError;

/// The longest `CONNECT` answer header accepted.
const MAX_CONNECT_ANSWER: usize = 16 * 1024;

/// Ask the proxy on `stream` for a tunnel to `target` (`host:port`, IPv6 in brackets), within
/// `deadline`. Reads exactly the proxy's answer header, nothing of the tunnel.
pub(crate) fn connect_tunnel<S: Read + Write>(stream: &mut S, proxy: &ProxyTarget, target: &str, deadline: Instant) -> Result<(), BackendError> {
    let mut request = Zeroizing::new(Vec::with_capacity(64 + 2 * target.len() + proxy.authorization.as_ref().map_or(0, |a| a.len() + 24)));
    request.extend_from_slice(b"CONNECT ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b"\r\n");
    if let Some(authorization) = &proxy.authorization {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(authorization.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    let failed = |e: io::Error| {
        if Instant::now() >= deadline {
            BackendError::Timeout("connect limit".into())
        } else {
            BackendError::Network(format!("could not connect through the proxy: {e}"))
        }
    };
    stream.write_all(&request).map_err(failed)?;
    stream.flush().map_err(failed)?;
    // One byte at a time up to the blank line: the bytes after it belong to the tunnel.
    let mut answer = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\r\n\r\n") {
        if answer.len() >= MAX_CONNECT_ANSWER {
            return Err(BackendError::Network(format!("could not connect through the proxy: its answer is longer than {MAX_CONNECT_ANSWER} bytes")));
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err(BackendError::Network("could not connect through the proxy: it closed the connection".into())),
            Ok(_) => answer.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(BackendError::Timeout("connect limit".into()));
                }
            }
            Err(e) => return Err(failed(e)),
        }
    }
    let parsed = tungstenite::handshake::client::Response::try_parse(&answer)
        .map_err(|e| BackendError::Network(format!("could not connect through the proxy: its answer is not HTTP ({e})")))?;
    match parsed {
        Some((_, response)) if response.status().is_success() => Ok(()),
        Some((_, response)) => Err(BackendError::Network(format!("could not connect through the proxy: it answered {}", response.status()))),
        None => Err(BackendError::Network("could not connect through the proxy: its answer is incomplete".into())),
    }
}
