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
