//! The TLS crypto provider, in one place for every transport that needs rustls.
//!
//! ring is the only provider. It is ALWAYS handed to the TLS client explicitly: never
//! `CryptoProvider::install_default()` (process-wide: it would change other crates' TLS) and never a
//! path that falls back to the process default or to rustls' crate-feature default (rustls
//! `expect`s there when a game also links another provider, e.g. aws-lc-rs; ureq `panic!`s when it
//! finds none).

use std::sync::Arc;

use rustls::crypto::CryptoProvider;

/// The provider every TLS connection of this crate uses: ring's.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The rustls client config for `wss://` (feature `ws`): ring, TLS 1.2 + 1.3, Mozilla's roots
/// (webpki-roots, the same list ureq uses for `https://`).
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
