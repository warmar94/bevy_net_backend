//! [`HttpClient`]: the resource game systems make requests with, and [`BackendAppExt`].

#[cfg(feature = "json")]
use std::any::type_name;
use std::any::TypeId;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bevy_app::App;
use bevy_ecs::resource::Resource;
#[cfg(feature = "json")]
use bevy_ecs::system::Commands;
use http::Method;

use crate::request::{OutgoingRequest, RequestId};
#[cfg(feature = "json")]
use crate::response::{BackendError, RawResponse};

/// Where an answer goes.
pub(crate) enum Route {
    /// [`HttpResponse`](crate::HttpResponse).
    Raw,
    /// [`JsonResponse<T>`](crate::JsonResponse), decoded on delivery.
    #[cfg(feature = "json")]
    Json(Box<dyn JsonRoute>),
}

/// Decodes and delivers one typed JSON answer (type-erased `T`).
#[cfg(feature = "json")]
pub(crate) trait JsonRoute: Send + Sync + 'static {
    fn deliver(&self, id: RequestId, result: Result<RawResponse, BackendError>, commands: &mut Commands);
}

#[cfg(feature = "json")]
struct JsonRouteFor<T>(std::marker::PhantomData<fn() -> T>);

#[cfg(feature = "json")]
impl<T: serde::de::DeserializeOwned + Send + Sync + 'static> JsonRoute for JsonRouteFor<T> {
    fn deliver(&self, id: RequestId, result: Result<RawResponse, BackendError>, commands: &mut Commands) {
        let result =
            result.and_then(|response| response.json::<T>().map_err(|e| BackendError::Decode { message: e.to_string(), response: Box::new(response) }));
        commands.queue(move |world: &mut bevy_ecs::world::World| {
            // serde_json's message can quote the body (a token, say): it is not logged.
            if let Err(BackendError::Decode { response, .. }) = &result {
                tracing::debug!(">>> NET-BACKEND: {id} -> HTTP {} but the body is not a `{}`", response.status, type_name::<T>());
            }
            if world.write_message(crate::JsonResponse::<T> { id, result }).is_none() {
                tracing::error!(">>> NET-BACKEND: {id}: `JsonResponse<{}>` is not registered; the answer is lost", type_name::<T>());
            }
        });
    }
}

/// Something game systems asked for, drained by the plugin's systems.
pub(crate) enum Queued {
    Send { id: RequestId, request: Box<OutgoingRequest>, route: Route },
}

/// Makes requests. A resource with shared access only (`Res<HttpClient>`), so any number of
/// systems in any schedule can use it without ordering against each other; it also works from
/// `&World`.
///
/// Every call returns the [`RequestId`] its answer will carry, and every request gets exactly
/// one answer: [`HttpResponse`](crate::HttpResponse) for the raw calls,
/// `JsonResponse<T>` for the typed ones (feature `json`).
///
/// Requests are handed to the transport in `PostUpdate`
/// ([`BackendSystems::Send`](crate::BackendSystems::Send)); one made later in a frame goes out in
/// the next frame's `PostUpdate`.
#[derive(Resource, Default)]
pub struct HttpClient {
    queue: Mutex<Vec<Queued>>,
    cancels: crate::inflight::CancelList,
    json_types: HashSet<TypeId>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient").field("queued", &self.lock().len()).field("json_types", &self.json_types.len()).finish()
    }
}

impl HttpClient {
    fn lock(&self) -> MutexGuard<'_, Vec<Queued>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, request: OutgoingRequest, route: Route) -> RequestId {
        let id = RequestId::next();
        self.lock().push(Queued::Send { id, request: Box::new(request), route });
        id
    }

    pub(crate) fn drain(&self) -> Vec<Queued> {
        std::mem::take(&mut *self.lock())
    }

    /// Send a request you built; the answer is a [`HttpResponse`](crate::HttpResponse).
    pub fn send(&self, request: OutgoingRequest) -> RequestId {
        self.push(request, Route::Raw)
    }

    /// `method` on `path` with an optional body; the answer is a
    /// [`HttpResponse`](crate::HttpResponse).
    pub fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> RequestId {
        let mut request = OutgoingRequest::new(method, path);
        request.set_body(body);
        self.send(request)
    }

    /// `GET path`; the answer is a [`HttpResponse`](crate::HttpResponse).
    pub fn get(&self, path: &str) -> RequestId {
        self.send(OutgoingRequest::get(path))
    }

    /// Cancel a request, HTTP or WebSocket alike (one shared path; `WsClient::cancel` is the same
    /// call). If it is still waiting, it is answered with [`BackendError::Cancelled`](crate::BackendError::Cancelled) in the next
    /// frame's `First` and nothing else is delivered for it (a request already on the wire may
    /// still reach the server; its result is discarded). Cancelling an id that was already
    /// answered does nothing.
    pub fn cancel(&self, id: RequestId) {
        self.cancels.push(id);
    }

    pub(crate) fn share_cancels(&mut self, cancels: crate::inflight::CancelList) {
        self.cancels = cancels;
    }

    /// Send a request you built and decode the 2xx body as JSON `T` (register `T` first with
    /// [`add_json_response::<T>()`](BackendAppExt::add_json_response)); the answer is a
    /// [`JsonResponse<T>`](crate::JsonResponse). Adds `Accept: application/json` unless set.
    ///
    /// An unregistered `T` is a programming error: the request is not sent, it is answered on
    /// [`HttpResponse`](crate::HttpResponse) (not `JsonResponse<T>`) with
    /// [`BackendError::InvalidRequest`], and an error is logged.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn send_json<T: serde::de::DeserializeOwned + Send + Sync + 'static>(&self, mut request: OutgoingRequest) -> RequestId {
        if !self.json_types.contains(&TypeId::of::<T>()) {
            let name = type_name::<T>();
            tracing::error!(">>> NET-BACKEND: `{name}` is not registered: call `app.add_json_response::<{name}>()`; answered on HttpResponse");
            request.reject(format!("response type `{name}` is not registered; call `app.add_json_response::<{name}>()`"));
            return self.push(request, Route::Raw);
        }
        if !request.headers().contains_key(http::header::ACCEPT) {
            request.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static("application/json"));
        }
        self.push(request, Route::Json(Box::new(JsonRouteFor::<T>(std::marker::PhantomData))))
    }

    /// `GET path` and decode the answer as JSON `T` (see [`send_json`](Self::send_json)).
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn get_json<T: serde::de::DeserializeOwned + Send + Sync + 'static>(&self, path: &str) -> RequestId {
        self.send_json::<T>(OutgoingRequest::get(path))
    }

    /// `POST path` with `body` as JSON and decode the answer as JSON `T` (see
    /// [`send_json`](Self::send_json)). A body that cannot be serialized is answered with
    /// [`BackendError::Encode`] and never sent.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn post_json<T: serde::de::DeserializeOwned + Send + Sync + 'static>(&self, path: &str, body: &(impl serde::Serialize + ?Sized)) -> RequestId {
        self.send_json::<T>(OutgoingRequest::post(path).with_json(body))
    }

    /// Whether `JsonResponse<T>` is registered.
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    pub fn is_json_registered<T: 'static>(&self) -> bool {
        self.json_types.contains(&TypeId::of::<T>())
    }
}

mod sealed {
    /// Only this crate implements [`BackendAppExt`](super::BackendAppExt).
    pub trait Sealed {}
    impl Sealed for bevy_app::App {}
}

/// `App` extension of this crate (implemented for `App` only; sealed, so methods behind features
/// never break an outside implementation).
pub trait BackendAppExt: sealed::Sealed {
    /// Register [`JsonResponse<T>`](crate::JsonResponse) (the message and the decoder), so
    /// `get_json::<T>` / `post_json::<T>` / `send_json::<T>` can answer with it. Call once per
    /// response type, before or after adding [`BackendPlugin`](crate::BackendPlugin).
    #[cfg(feature = "json")]
    #[cfg_attr(docsrs, doc(cfg(feature = "json")))]
    fn add_json_response<T: serde::de::DeserializeOwned + Send + Sync + 'static>(&mut self) -> &mut Self;

    /// Register a WebSocket request type (features `ws` + `json`): its answers arrive as
    /// [`WsResponse<R::Response>`](crate::WsResponse). Call once per type, before or after adding
    /// the plugin.
    #[cfg(all(feature = "ws", feature = "json"))]
    #[cfg_attr(docsrs, doc(cfg(all(feature = "ws", feature = "json"))))]
    fn add_ws_request<R: crate::WsRequest>(&mut self) -> &mut Self;

    /// Register a WebSocket server-push type (features `ws` + `json`): pushes of kind `P::KIND`
    /// arrive as [`WsPush<P>`](crate::WsPush).
    #[cfg(all(feature = "ws", feature = "json"))]
    #[cfg_attr(docsrs, doc(cfg(all(feature = "ws", feature = "json"))))]
    fn add_ws_push<P: crate::WsPushMessage>(&mut self) -> &mut Self;
}

impl BackendAppExt for App {
    #[cfg(feature = "json")]
    fn add_json_response<T: serde::de::DeserializeOwned + Send + Sync + 'static>(&mut self) -> &mut Self {
        self.add_message::<crate::JsonResponse<T>>();
        self.init_resource::<HttpClient>();
        if let Some(mut client) = self.world_mut().get_resource_mut::<HttpClient>() {
            client.json_types.insert(TypeId::of::<T>());
        }
        self
    }

    #[cfg(all(feature = "ws", feature = "json"))]
    fn add_ws_request<R: crate::WsRequest>(&mut self) -> &mut Self {
        crate::ws::register_request::<R>(self);
        self
    }

    #[cfg(all(feature = "ws", feature = "json"))]
    fn add_ws_push<P: crate::WsPushMessage>(&mut self) -> &mut Self {
        crate::ws::register_push::<P>(self);
        self
    }
}
