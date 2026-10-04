//! (feature `http`) ureq's connection chain with the crate's own rustls config, for
//! [`TlsSettings`](crate::TlsSettings) other than the default (extra root certificates, the
//! operating system's store).
//!
//! ureq 3.4's `TlsConfig` takes a crypto provider and a root list, but not a whole rustls
//! `ClientConfig` (nor a verifier with extra roots on top of Mozilla's or the operating system's).
//! So the agent gets ureq's own chain (`ConnectProxyConnector` → `TcpConnector`) followed by
//! [`ConfiguredTls`] instead of ureq's `RustlsConnector`. [`ConfiguredTls`] does what ureq's
//! connector does (`ureq-3.4.2/src/tls/rustls.rs`), with the config built once from the settings.
//! The default settings keep ureq's stock chain.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport, TransportAdapter};

use crate::response::BackendError;

/// A TLS setup that could not be built: every `https://` connection fails with it (the HTTP
/// transport turns it back into the [`BackendError`]).
#[derive(Debug)]
pub(crate) struct TlsSetupError(pub(crate) BackendError);

impl fmt::Display for TlsSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for TlsSetupError {}

/// The last link of the chain: wraps the connection in TLS with the crate's config when the URL is
/// `https://` and the connection is not TLS already (the same rule as ureq's `RustlsConnector`).
pub(crate) struct ConfiguredTls {
    config: Result<Arc<rustls::ClientConfig>, BackendError>,
}

impl ConfiguredTls {
    pub(crate) fn new(config: Result<Arc<rustls::ClientConfig>, BackendError>) -> Self {
        Self { config }
    }
}

impl fmt::Debug for ConfiguredTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredTls").field("ready", &self.config.is_ok()).finish()
    }
}

impl<In: Transport> Connector<In> for ConfiguredTls {
    type Out = Either<In, ConfiguredTlsTransport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let config = match &self.config {
            Ok(config) => Arc::clone(config),
            Err(error) => return Err(ureq::Error::Io(io::Error::other(TlsSetupError(error.clone())))),
        };
        // `Uri::host` keeps an IPv6 address in brackets; the TLS name has none.
        let host = details.uri.host().unwrap_or_default();
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        let name = ServerName::try_from(host.to_string()).map_err(|_| ureq::Error::Tls("Rustls invalid dns name error"))?;
        let mut conn = ClientConnection::new(config, name)?;
        let mut sock = TransportAdapter::new(transport.boxed());
        sock.set_timeout(details.timeout);
        conn.complete_io(&mut sock)?;
        let buffers = LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size());
        Ok(Some(Either::B(ConfiguredTlsTransport { buffers, stream: StreamOwned { conn, sock } })))
    }
}

/// A TLS connection made by [`ConfiguredTls`] (ureq's `RustlsTransport`, with our config).
pub(crate) struct ConfiguredTlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}

impl fmt::Debug for ConfiguredTlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredTlsTransport").field("chained", &self.stream.sock.inner()).finish()
    }
}

impl Transport for ConfiguredTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = self.buffers.output().get(..amount).ok_or_else(|| ureq::Error::Io(io::Error::other("output amount beyond the buffer")))?;
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}
