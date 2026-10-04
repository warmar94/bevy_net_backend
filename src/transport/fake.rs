//! [`FakeHttpTransport`]: an in-memory transport for tests. No network, no threads.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use http::Method;

use super::{HttpTransport, HttpTransportResult};
use crate::request::{PreparedRequest, RequestId};

#[derive(Default)]
struct FakeState {
    routes: Vec<(Method, String, HttpTransportResult)>,
    submitted: Vec<(RequestId, PreparedRequest)>,
    answered: HashSet<RequestId>,
    outbox: Vec<(RequestId, HttpTransportResult)>,
    progress: Vec<(RequestId, u64, Option<u64>)>,
    download_progress: Vec<(RequestId, u64, Option<u64>)>,
    cancelled: Vec<RequestId>,
    shutdowns: usize,
}

/// An in-memory [`HttpTransport`] for tests: it records every request and answers from scripted
/// routes or by hand. Clones share one state, so keep a clone to script and inspect it after
/// inserting it:
///
/// ```
/// use bevy_net_backend::http::{Method, StatusCode};
/// use bevy_net_backend::{HttpTransportRes, FakeHttpTransport, RawResponse};
///
/// let fake = FakeHttpTransport::new();
/// fake.route(Method::GET, "/me", Ok(RawResponse::new(StatusCode::OK, r#"{"name":"Ayla"}"#)));
/// let resource = HttpTransportRes::new(fake.clone());
/// # let _ = resource;
/// ```
///
/// - A request that matches a route (method + path, the newest route first) is answered with a
///   copy of the route's result on the next poll (the next frame's `First`).
/// - Any other request waits until [`reply`](Self::reply) answers it, or forever (the plugin's
///   deadline then answers it with a timeout).
/// - A download ([`HttpClient::download`](crate::HttpClient::download), feature `http`): a 2xx
///   result's body is written to the download's file with
///   [`HttpDownload::receive`](crate::HttpDownload) when the route or reply answers it (its
///   progress included), and the result carries the file instead of the body.
#[derive(Clone, Default)]
pub struct FakeHttpTransport {
    state: Arc<Mutex<FakeState>>,
}

impl fmt::Debug for FakeHttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeHttpTransport")
            .field("routes", &state.routes.len())
            .field("submitted", &state.submitted.len())
            .field("queued_answers", &state.outbox.len())
            .finish()
    }
}

impl FakeHttpTransport {
    /// An empty fake: no routes, nothing recorded.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answer every request with this method and path (no query) with `result` from now on.
    /// A newer route for the same method and path wins.
    pub fn route(&self, method: Method, path: &str, result: HttpTransportResult) -> &Self {
        self.lock().routes.push((method, path.to_string(), result));
        self
    }

    /// Remove every route.
    pub fn clear_routes(&self) {
        self.lock().routes.clear();
    }

    /// Report `result` for `id` on the next poll, whether or not the plugin still waits for it
    /// (a late or duplicate report is how tests prove the plugin discards them).
    pub fn reply(&self, id: RequestId, result: HttpTransportResult) {
        let mut state = self.lock();
        let request = state.submitted.iter().find(|(submitted, _)| *submitted == id).map(|(_, request)| request.clone());
        let result = match request {
            Some(request) => written(&mut state, id, &request, result),
            None => result,
        };
        state.answered.insert(id);
        state.outbox.push((id, result));
    }

    /// Report upload progress for `id` on the next poll (an `HttpProgress` message while the
    /// request waits for its answer).
    pub fn progress(&self, id: RequestId, sent: u64, total: Option<u64>) {
        self.lock().progress.push((id, sent, total));
    }

    /// Every request submitted so far, in order. A form with files from disk is in
    /// `PreparedRequest::streaming_body` (`read_all` gives its bytes).
    pub fn requests(&self) -> Vec<(RequestId, PreparedRequest)> {
        self.lock().submitted.clone()
    }

    /// The last request submitted.
    pub fn last_request(&self) -> Option<(RequestId, PreparedRequest)> {
        self.lock().submitted.last().cloned()
    }

    /// Submitted requests that no route or [`reply`](Self::reply) has answered so far.
    pub fn waiting(&self) -> Vec<RequestId> {
        let state = self.lock();
        state.submitted.iter().map(|(id, _)| *id).filter(|id| !state.answered.contains(id)).collect()
    }

    /// Every id the plugin cancelled on this transport (cancel or deadline).
    pub fn cancelled(&self) -> Vec<RequestId> {
        self.lock().cancelled.clone()
    }

    /// How many times the plugin shut this transport down.
    pub fn shutdown_count(&self) -> usize {
        self.lock().shutdowns
    }
}

impl HttpTransport for FakeHttpTransport {
    fn submit(&mut self, id: RequestId, request: PreparedRequest) {
        let mut state = self.lock();
        let route = state.routes.iter().rev().find(|(method, path, _)| *method == request.method && path == request.path()).map(|(_, _, r)| r.clone());
        if let Some(result) = route {
            let result = written(&mut state, id, &request, result);
            state.answered.insert(id);
            state.outbox.push((id, result));
        }
        state.submitted.push((id, request));
    }

    fn poll(&mut self) -> Vec<(RequestId, HttpTransportResult)> {
        std::mem::take(&mut self.lock().outbox)
    }

    fn cancel(&mut self, id: RequestId) {
        self.lock().cancelled.push(id);
    }

    fn shutdown(&mut self) {
        self.lock().shutdowns += 1;
    }

    /// It records streamed bodies like any other (it never reads them).
    fn streams_bodies(&self) -> bool {
        true
    }

    fn poll_progress(&mut self) -> Vec<(RequestId, u64, Option<u64>)> {
        std::mem::take(&mut self.lock().progress)
    }

    /// With feature `http`: a 2xx answer to a download is written to its file.
    fn downloads_to_files(&self) -> bool {
        cfg!(feature = "http")
    }

    fn poll_download_progress(&mut self) -> Vec<(RequestId, u64, Option<u64>)> {
        std::mem::take(&mut self.lock().download_progress)
    }
}

/// A download's 2xx result: its body written to the download's file (feature `http`).
#[cfg(feature = "http")]
fn written(state: &mut FakeState, id: RequestId, request: &PreparedRequest, result: HttpTransportResult) -> HttpTransportResult {
    let Some(download) = &request.download else { return result };
    match result {
        Ok(response) if response.is_success() && response.file.is_none() => {
            let length = u64::try_from(response.body.len()).ok();
            let progress = &mut state.download_progress;
            let file = download.receive(id, &mut response.body.as_slice(), length, &mut |received, total| progress.push((id, received, total)))?;
            Ok(crate::RawResponse { body: Vec::new(), file: Some(file), ..response })
        }
        other => other,
    }
}

#[cfg(not(feature = "http"))]
fn written(_: &mut FakeState, _: RequestId, _: &PreparedRequest, result: HttpTransportResult) -> HttpTransportResult {
    result
}
