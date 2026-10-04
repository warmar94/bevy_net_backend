//! The TLS setup, in one place for every transport that needs rustls (HTTP and the WebSocket).
//!
//! ring is the only provider. It is ALWAYS handed to the TLS client explicitly: never
//! `CryptoProvider::install_default()` (process-wide: it would change other crates' TLS) and never a
//! path that falls back to the process default or to rustls' crate-feature default (rustls
//! `expect`s there when a game also links another provider, e.g. aws-lc-rs; ureq `panic!`s when it
//! finds none).
//!
//! Which server certificates are trusted ([`TlsSettings`]):
//! - by default Mozilla's root certificates (webpki-roots), checked by rustls (webpki);
//! - with [`TlsSettings::with_os_certificates`] (feature `os-certificates`) the operating system's
//!   certificate store and its own checks, through `rustls-platform-verifier` (still ring), INSTEAD
//!   of webpki-roots;
//! - plus, in both cases, the extra root certificates of
//!   [`TlsSettings::with_root_certificates_pem`] / [`TlsSettings::with_root_certificates_file`].

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

use crate::config::ConfigError;
use crate::response::BackendError;

/// The provider every TLS connection of this crate uses: ring's.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Fill `bytes` from the operating system's secure random source, through the same ring provider
/// (no extra dependency): for multipart boundaries and WebSocket frame masks.
#[cfg(any(feature = "http", feature = "ws"))]
pub(crate) fn random_bytes(bytes: &mut [u8]) -> Result<(), ()> {
    rustls::crypto::ring::default_provider().secure_random.fill(bytes).map_err(|_| ())
}

/// The rustls client config for `wss://` (feature `ws`) with the default trust: ring, TLS 1.2 +
/// 1.3, Mozilla's roots (webpki-roots, the same list ureq uses for `https://`).
#[cfg(feature = "ws")]
pub(crate) fn client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Where extra root certificates come from (read and parsed when a transport is created).
#[derive(Clone)]
enum PemSource {
    /// PEM text given by the game.
    Bytes(Vec<u8>),
    /// A PEM file.
    File(PathBuf),
}

/// Which server certificates `https://` (feature `http`) and `wss://` (feature `ws`) connections
/// trust. Hand it to [`BackendPlugin::with_tls`](crate::BackendPlugin::with_tls) (both
/// transports), or to `UreqTransport::with_tls` / `TungsteniteTransport::with_tls` for a transport
/// you create yourself.
///
/// - **Default:** Mozilla's root certificates (webpki-roots), checked by rustls.
/// - [`with_root_certificates_pem`](Self::with_root_certificates_pem) /
///   [`with_root_certificates_file`](Self::with_root_certificates_file): also trust these root
///   certificates, e.g. the certificate of a self-signed development server or a company CA.
/// - `with_os_certificates(true)` (feature `os-certificates`): the operating system's certificate
///   store and checks instead of Mozilla's list; extra roots are added on top.
///
/// The PEM text and files are read and checked when a transport is created (at plugin build);
/// [`validate`](Self::validate) runs the same check earlier. With a problem (a file that cannot be
/// read, PEM without a certificate, a certificate that cannot be a root) every `https://` /
/// `wss://` request and connection is answered with [`BackendError::InvalidRequest`] (never
/// sent) and a warning is logged when the transport is created; `http://` and `ws://` are not
/// affected. A certificate is public, so nothing here is secret; `Debug` shows counts.
///
/// ```
/// use bevy_net_backend::{BackendPlugin, HttpConfig, TlsSettings};
///
/// # let dev_ca_pem = "";
/// // A development server with its own CA (the PEM text of its certificate).
/// let tls = TlsSettings::new().with_root_certificates_pem(dev_ca_pem);
/// let plugin = BackendPlugin::new(HttpConfig::new("https://localhost:8443")).with_tls(tls);
/// ```
#[derive(Clone, Default)]
pub struct TlsSettings {
    #[cfg(feature = "os-certificates")]
    os_store: bool,
    extra: Vec<PemSource>,
}

impl fmt::Debug for TlsSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("TlsSettings");
        #[cfg(feature = "os-certificates")]
        s.field("os_store", &self.os_store);
        s.field("extra_pem_sources", &self.extra.len()).finish()
    }
}

impl TlsSettings {
    /// The default: Mozilla's root certificates (webpki-roots).
    pub fn new() -> Self {
        Self::default()
    }

    /// Also trust the root certificates in this PEM text (every `CERTIFICATE` block; other blocks,
    /// e.g. a key, are skipped). Can be called more than once.
    pub fn with_root_certificates_pem(mut self, pem: impl AsRef<[u8]>) -> Self {
        self.extra.push(PemSource::Bytes(pem.as_ref().to_vec()));
        self
    }

    /// Like [`with_root_certificates_pem`](Self::with_root_certificates_pem), with the PEM read
    /// from this file when a transport is created.
    pub fn with_root_certificates_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.extra.push(PemSource::File(path.into()));
        self
    }

    /// Trust the operating system's certificate store and certificate checks (Windows, macOS /
    /// iOS, the system CA files on Linux / BSD, Android) instead of Mozilla's root certificates,
    /// e.g. for a company CA installed on the machine (default `false`). Extra root certificates
    /// are added on top (not on Android). Uses `rustls-platform-verifier` with ring; on Android
    /// that crate needs its JNI initialization first (see its documentation).
    #[cfg(feature = "os-certificates")]
    #[cfg_attr(docsrs, doc(cfg(feature = "os-certificates")))]
    pub fn with_os_certificates(mut self, on: bool) -> Self {
        self.os_store = on;
        self
    }

    /// Whether the operating system's store is used (always `false` without the feature
    /// `os-certificates`).
    pub fn os_certificates(&self) -> bool {
        #[cfg(feature = "os-certificates")]
        return self.os_store;
        #[cfg(not(feature = "os-certificates"))]
        false
    }

    /// How many PEM sources (texts and files) were added.
    pub fn root_certificate_sources(&self) -> usize {
        self.extra.len()
    }

    /// Read and check everything now, as creating a transport does: the files, the PEM, every
    /// certificate as a root, and the operating system's verifier.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.build() {
            Some(Err(error)) => Err(ConfigError::Tls(error.to_string())),
            _ => Ok(()),
        }
    }

    /// Whether these are the default settings (the transports then keep their built-in setup).
    pub(crate) fn is_default(&self) -> bool {
        self.extra.is_empty() && !self.os_certificates()
    }

    /// The rustls client config for these settings, built once per transport. `None` for the
    /// default settings. A PEM source that cannot be read or holds no usable certificate is
    /// `InvalidRequest`; an operating-system verifier that cannot be set up is `Tls`.
    pub(crate) fn build(&self) -> Option<Result<Arc<rustls::ClientConfig>, BackendError>> {
        if self.is_default() {
            return None;
        }
        Some(self.config().map(Arc::new))
    }

    fn config(&self) -> Result<rustls::ClientConfig, BackendError> {
        let extra = extra_roots(&self.extra)?;
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| BackendError::Tls(format!("the TLS configuration could not be built: {e}")))?;
        #[cfg(feature = "os-certificates")]
        if self.os_store {
            let verifier = os_verifier(extra)?;
            return Ok(builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth());
        }
        let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        for (index, cert) in extra.into_iter().enumerate() {
            roots.add(cert).map_err(|e| BackendError::InvalidRequest(format!("extra root certificate #{} is not usable as a root: {e}", index + 1)))?;
        }
        Ok(builder.with_root_certificates(roots).with_no_client_auth())
    }
}

/// The operating system's verifier (ring), with the extra roots on top.
#[cfg(feature = "os-certificates")]
fn os_verifier(extra: Vec<CertificateDer<'static>>) -> Result<rustls_platform_verifier::Verifier, BackendError> {
    let failed = |e: rustls::Error| BackendError::Tls(format!("the operating system's certificate verifier could not be set up: {e}"));
    if extra.is_empty() {
        return rustls_platform_verifier::Verifier::new(provider()).map_err(failed);
    }
    #[cfg(not(target_os = "android"))]
    {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider()).map_err(failed)
    }
    #[cfg(target_os = "android")]
    {
        Err(BackendError::InvalidRequest("extra root certificates together with the operating system's certificate store are not supported on Android".into()))
    }
}

/// Every certificate of every PEM source, in order. A source without any `CERTIFICATE` block is
/// refused (a wrong file must not pass silently); other blocks (e.g. a key) are skipped.
fn extra_roots(sources: &[PemSource]) -> Result<Vec<CertificateDer<'static>>, BackendError> {
    let mut roots = Vec::new();
    for source in sources {
        let (name, read);
        let bytes: &[u8] = match source {
            PemSource::Bytes(bytes) => {
                name = "the root certificate PEM".to_string();
                bytes
            }
            PemSource::File(path) => {
                name = format!("the root certificate file `{}`", path.display());
                read = std::fs::read(path).map_err(|e| BackendError::InvalidRequest(format!("{name} could not be read: {e}")))?;
                &read
            }
        };
        let before = roots.len();
        for cert in CertificateDer::pem_slice_iter(bytes) {
            roots.push(cert.map_err(|e| BackendError::InvalidRequest(format!("{name} is not valid PEM: {e}")))?);
        }
        if roots.len() == before {
            return Err(BackendError::InvalidRequest(format!("{name} holds no CERTIFICATE block")));
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_pem() -> String {
        let key = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}"));
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|e| panic!("{e}"));
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap_or_else(|e| panic!("{e}")).pem()
    }

    #[test]
    fn the_default_builds_nothing_and_has_no_alpn() {
        let tls = TlsSettings::new();
        assert!(tls.is_default() && tls.build().is_none() && tls.validate().is_ok());
        let config = TlsSettings::new().with_root_certificates_pem(ca_pem()).build();
        assert!(config.is_some_and(|c| c.is_ok_and(|c| c.alpn_protocols.is_empty())));
    }

    #[test]
    fn pem_sources_add_every_certificate_and_refuse_sources_without_one() {
        let two = format!("{}\n{}", ca_pem(), ca_pem());
        assert_eq!(extra_roots(&[PemSource::Bytes(two.clone().into_bytes())]).map(|r| r.len()).ok(), Some(2));
        assert!(TlsSettings::new().with_root_certificates_pem(&two).validate().is_ok());
        // A key block is skipped; no certificate at all is refused.
        let key_only = rcgen::KeyPair::generate().unwrap_or_else(|e| panic!("{e}")).serialize_pem();
        let with_key = format!("{key_only}\n{}", ca_pem());
        assert_eq!(extra_roots(&[PemSource::Bytes(with_key.into_bytes())]).map(|r| r.len()).ok(), Some(1));
        for bad in [key_only.as_bytes(), b"", b"not pem at all"] {
            let error = TlsSettings::new().with_root_certificates_pem(bad).validate().err();
            assert!(matches!(error, Some(ConfigError::Tls(ref why)) if why.contains("no CERTIFICATE")), "{error:?}");
        }
        let broken = TlsSettings::new().with_root_certificates_pem("-----BEGIN CERTIFICATE-----\nAAAA\n").build();
        assert!(matches!(broken, Some(Err(BackendError::InvalidRequest(_)))), "{broken:?}");
        let missing = TlsSettings::new().with_root_certificates_file("this-file-does-not-exist.pem").validate().err();
        assert!(matches!(missing, Some(ConfigError::Tls(ref why)) if why.contains("this-file-does-not-exist.pem")), "{missing:?}");
    }

    #[test]
    fn debug_shows_counts_only() {
        let tls = TlsSettings::new().with_root_certificates_file("a.pem").with_root_certificates_pem("x");
        assert_eq!(tls.root_certificate_sources(), 2);
        assert!(!format!("{tls:?}").contains("a.pem"), "{tls:?}");
    }

    #[test]
    #[cfg(feature = "os-certificates")]
    fn the_os_store_builds_with_and_without_extra_roots() {
        let os = TlsSettings::new().with_os_certificates(true);
        assert!(os.os_certificates() && !os.is_default());
        assert!(os.build().is_some_and(|c| c.is_ok_and(|c| c.alpn_protocols.is_empty())));
        let os = os.with_root_certificates_pem(ca_pem());
        assert!(os.validate().is_ok());
        assert!(format!("{os:?}").contains("os_store: true"));
    }
}
