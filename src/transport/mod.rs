//! The seam between the plugin and the network: the [`HttpTransport`] trait, the
//! [`HttpTransportRes`] resource, the [`FakeHttpTransport`](crate::FakeHttpTransport) (always compiled) and, with feature
//! `http`, the real `UreqTransport`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use bevy_ecs::resource::Resource;

use crate::request::{PreparedRequest, RequestId};
use crate::response::{BackendError, RawResponse};

pub(crate) mod fake;
#[cfg(feature = "http")]
pub(crate) mod http_pool;

/// What a transport reports for one request: the server's answer (any status; the plugin turns
/// a non-2xx status into [`BackendError::Status`]) or the error that stopped it.
pub type HttpTransportResult = Result<RawResponse, BackendError>;

/// Moves prepared requests to a server and results back. The plugin owns the bookkeeping
/// (pending requests, deadlines, cancel, exit); a transport only has to deliver.
///
/// Rules for an implementation:
/// - [`submit`](Self::submit) and [`poll`](Self::poll) run on the main thread in the schedule:
///   they must never block.
/// - Report each submitted request at most once. A result for an id the plugin already answered
///   (cancelled, timed out) is discarded, so reporting late is harmless.
/// - Never panic.
///
/// **Compatibility promise:** methods added to this trait in later versions always come with a
/// default implementation.
pub trait HttpTransport: Send + Sync + 'static {
    /// Start `request`. Called in `PostUpdate` ([`BackendSystems::Send`](crate::BackendSystems::Send)).
    fn submit(&mut self, id: RequestId, request: PreparedRequest);

    /// Every result that arrived since the last call. Called once per frame in `First`
    /// ([`BackendSystems::Receive`](crate::BackendSystems::Receive)).
    fn poll(&mut self) -> Vec<(RequestId, HttpTransportResult)>;

    /// The plugin answered `id` without the transport (cancelled or timed out); drop it if it has
    /// not started. Default: nothing.
    fn cancel(&mut self, id: RequestId) {
        let _ = id;
    }

    /// The app is exiting: the plugin has answered everything. Release threads and connections;
    /// never wait for work in progress. Default: nothing.
    fn shutdown(&mut self) {}
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The installed transport. The plugin inserts the HTTP transport (feature `http`) when the app
/// has none; insert your own (for example a [`FakeHttpTransport`](crate::FakeHttpTransport)) to replace it.
///
/// Without this resource every request is answered with [`BackendError::NoTransport`]. Requests
/// still waiting on a transport that is removed or replaced are answered the same way.
#[derive(Resource)]
pub struct HttpTransportRes {
    inner: Box<dyn HttpTransport>,
    generation: u64,
}

impl fmt::Debug for HttpTransportRes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransportRes").field("generation", &self.generation).finish_non_exhaustive()
    }
}

impl HttpTransportRes {
    /// Wrap a transport.
    pub fn new(transport: impl HttpTransport) -> Self {
        Self { inner: Box::new(transport), generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed) }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn get_mut(&mut self) -> &mut dyn HttpTransport {
        self.inner.as_mut()
    }
}
