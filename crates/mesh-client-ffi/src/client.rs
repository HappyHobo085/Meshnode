//! The exported `MeshClient` object and the runtime it runs on.
//!
//! Every async export runs its work on this library's own Tokio runtime (a UniFFI async
//! call is polled by the foreign executor, so the work is spawned there and its handle
//! awaited), so the SDK's client, and its node, live on that runtime from start to
//! teardown. Panics are caught and returned as `Internal`.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use futures_util::FutureExt;
use mesh_client as sdk;
use mesh_client::{ClientError, ClientEvent, NodeId};
use tokio::sync::{RwLock, broadcast, watch};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::types::{
    MeshClientError, MeshClientOptions, MeshEvent, MeshMember, MeshRequest, MeshResponse, MeshRole,
    MeshService, MeshStatus, bad_request, from_http, to_http,
};

/// The runtime's worker threads.
const WORKER_THREADS: usize = 2;

/// Receives a client's events. `on_event` is called on a library thread, one event at a
/// time; it should return quickly and must not block on this client's calls.
#[uniffi::export(callback_interface)]
pub trait MeshClientListener: Send + Sync {
    fn on_event(&self, event: MeshEvent);
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("mesh-client")
            .enable_all()
            .build()
            .expect("cannot build the mesh-client runtime")
    })
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    let msg = if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    };
    format!("panicked: {msg}")
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `fut` on the library's runtime; a panic becomes `Internal`.
async fn on_rt<T, F>(fut: F) -> Result<T, MeshClientError>
where
    F: Future<Output = Result<T, MeshClientError>> + Send + 'static,
    T: Send + 'static,
{
    let task = catch_unwind(AssertUnwindSafe(|| {
        runtime().spawn(AssertUnwindSafe(fut).catch_unwind())
    }))
    .map_err(|p| internal(panic_message(&*p)))?;
    match task.await {
        Ok(Ok(r)) => r,
        Ok(Err(p)) => Err(internal(panic_message(&*p))),
        Err(e) => Err(internal(e.to_string())),
    }
}

fn internal(message: String) -> MeshClientError {
    MeshClientError::Internal { message }
}

/// A call on the SDK client, borrowed for the call's duration.
type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, ClientError>> + Send + 'a>>;

/// A client-role member of a mesh (see the `mesh-client` crate). Create it with `join`
/// (first run) or `open` (later runs, same directory); call `shutdown` when done.
///
/// `shutdown` releases the directory before it returns, ends calls in flight with
/// `Closed`, and every later call fails with `Closed`. An object dropped (collected)
/// without `shutdown` is stopped in the background; reopening its directory meanwhile
/// fails with `AlreadyOpen`.
#[derive(uniffi::Object)]
pub struct MeshClient {
    /// `None` once shut down. Calls hold a read guard while they run; `shutdown` takes it
    /// for writing (after `closing` has ended the calls in flight).
    inner: RwLock<Option<sdk::MeshClient>>,
    /// Set by `shutdown`: calls in flight end with `Closed`.
    closing: watch::Sender<bool>,
    /// The task delivering events to the listener.
    listener: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for MeshClient {
    fn drop(&mut self) {
        // Never shut down: the SDK client's own drop tears it down on the runtime it
        // started on (this one), whatever thread this runs on (ruling R16).
        if let Some(t) = lock(&self.listener).take() {
            t.abort();
        }
    }
}

impl MeshClient {
    fn wrap(c: sdk::MeshClient) -> Arc<MeshClient> {
        Arc::new(MeshClient {
            inner: RwLock::new(Some(c)),
            closing: watch::Sender::new(false),
            listener: Mutex::new(None),
        })
    }

    /// Starts a client on the library's runtime.
    async fn start<F>(start: F) -> Result<Arc<MeshClient>, MeshClientError>
    where
        F: Future<Output = Result<sdk::MeshClient, ClientError>> + Send + 'static,
    {
        #[cfg(target_os = "android")]
        if let Err(message) = crate::android::check_initialised() {
            return Err(MeshClientError::NotInitialised { message });
        }
        on_rt(async move { start.await.map(Self::wrap).map_err(Into::into) }).await
    }

    /// Runs `f` on the SDK client, on the library's runtime; `Closed` once shut down or
    /// when a shutdown begins while it runs.
    async fn call<T, F>(self: Arc<Self>, f: F) -> Result<T, MeshClientError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a sdk::MeshClient) -> Call<'a, T> + Send + 'static,
    {
        on_rt(async move {
            let mut closing = self.closing.subscribe();
            let guard = self.inner.read().await;
            let c = guard.as_ref().ok_or(MeshClientError::Closed)?;
            tokio::select! {
                r = f(c) => r.map_err(Into::into),
                _ = closing.wait_for(|c| *c) => Err(MeshClientError::Closed),
            }
        })
        .await
    }

    /// Runs a synchronous `f` on the SDK client; `Closed` once shut down (or shutting
    /// down).
    fn sync<T>(&self, f: impl FnOnce(&sdk::MeshClient) -> T) -> Result<T, MeshClientError> {
        let _rt = runtime().enter();
        let guard = self.inner.try_read().map_err(|_| MeshClientError::Closed)?;
        let c = guard.as_ref().ok_or(MeshClientError::Closed)?;
        catch_unwind(AssertUnwindSafe(|| f(c))).map_err(|p| internal(panic_message(&*p)))
    }
}

#[uniffi::export]
impl MeshClient {
    /// Joins the mesh of a client invite `ticket`, keeping the new identity in `dir`
    /// (created if needed). See `mesh_client::MeshClient::join`.
    #[uniffi::constructor]
    pub async fn join(
        dir: String,
        ticket: String,
        options: MeshClientOptions,
    ) -> Result<Arc<MeshClient>, MeshClientError> {
        options.check()?;
        Self::start(
            async move { sdk::MeshClient::join(Path::new(&dir), &ticket, options.into()).await },
        )
        .await
    }

    /// Opens the client joined earlier in `dir`. See `mesh_client::MeshClient::open`.
    #[uniffi::constructor]
    pub async fn open(
        dir: String,
        options: MeshClientOptions,
    ) -> Result<Arc<MeshClient>, MeshClientError> {
        options.check()?;
        Self::start(async move { sdk::MeshClient::open(Path::new(&dir), options.into()).await })
            .await
    }

    /// Sends `req` to `service` on the nearest online host that runs it (trying the next
    /// on failure). The host's response is returned as it is, `4xx`/`5xx` included; a
    /// request that cannot be built (bad method, header or path) gets a local `400`.
    pub async fn request(
        self: Arc<Self>,
        service: String,
        req: MeshRequest,
    ) -> Result<MeshResponse, MeshClientError> {
        let Some(req) = to_http(req) else {
            return Ok(bad_request());
        };
        self.call(move |c| Box::pin(async move { c.request(&service, req).await.map(from_http) }))
            .await
    }

    /// A single-use invite ticket for `role`, valid for `ttl_secs` seconds.
    pub async fn invite(
        self: Arc<Self>,
        role: MeshRole,
        ttl_secs: u64,
    ) -> Result<String, MeshClientError> {
        self.call(move |c| Box::pin(c.invite(role.into(), Duration::from_secs(ttl_secs))))
            .await
    }

    /// Removes member `id` (hex, as `members()` lists it) from the mesh.
    pub async fn remove(self: Arc<Self>, id: String) -> Result<(), MeshClientError> {
        let id = parse_id(&id)?;
        self.call(move |c| Box::pin(c.remove(id))).await
    }

    /// Stops the client and releases its directory, which can be opened again once this
    /// returns. Calls in flight end with `Closed`. Idempotent.
    pub async fn shutdown(self: Arc<Self>) {
        let r = on_rt(async move {
            self.closing.send_replace(true);
            if let Some(t) = lock(&self.listener).take() {
                t.abort();
            }
            let client = self.inner.write().await.take();
            if let Some(c) = client {
                c.shutdown().await;
            }
            Ok(())
        })
        .await;
        if let Err(e) = r {
            warn!("mesh-client shutdown: {e}");
        }
    }

    pub fn status(&self) -> Result<MeshStatus, MeshClientError> {
        self.sync(|c| c.status().into())
    }

    /// Every member, this client included.
    pub fn members(&self) -> Result<Vec<MeshMember>, MeshClientError> {
        self.sync(|c| c.members().into_iter().map(Into::into).collect())
    }

    /// Every deployed service, with the hosts its placement selects.
    pub fn services(&self) -> Result<Vec<MeshService>, MeshClientError> {
        self.sync(|c| c.services().into_iter().map(Into::into).collect())
    }

    /// Delivers this client's events to `listener` from now on, replacing any earlier
    /// listener. Delivery ends at `shutdown` (an `on_event` already running then is not
    /// interrupted and may outlast it).
    pub fn set_listener(
        &self,
        listener: Box<dyn MeshClientListener>,
    ) -> Result<(), MeshClientError> {
        let rx = self.sync(|c| c.subscribe())?;
        let task = runtime().spawn(deliver(rx, listener));
        if let Some(old) = lock(&self.listener).replace(task) {
            old.abort();
        }
        Ok(())
    }
}

/// Test-only constructors (Rust, not exported): loopback only, no relay or discovery,
/// fast timings, for in-process hosts.
#[cfg(feature = "test-hooks")]
impl MeshClient {
    pub async fn join_for_test(
        dir: String,
        ticket: String,
        options: MeshClientOptions,
    ) -> Result<Arc<MeshClient>, MeshClientError> {
        options.check()?;
        Self::start(async move {
            sdk::MeshClient::join_for_test(Path::new(&dir), &ticket, options.into()).await
        })
        .await
    }

    pub async fn open_for_test(
        dir: String,
        options: MeshClientOptions,
    ) -> Result<Arc<MeshClient>, MeshClientError> {
        options.check()?;
        Self::start(
            async move { sdk::MeshClient::open_for_test(Path::new(&dir), options.into()).await },
        )
        .await
    }
}

fn parse_id(id: &str) -> Result<NodeId, MeshClientError> {
    let bad = || MeshClientError::InvalidArgument {
        message: format!("not a node id (64 hex digits): {id:?}"),
    };
    let bytes = hex::decode(id).map_err(|_| bad())?;
    Ok(NodeId(bytes.try_into().map_err(|_| bad())?))
}

/// Hands the client's events to `listener` until the client stops. Events dropped for
/// lag are replaced by `MembersChanged` and `ServicesChanged` (read everything again).
async fn deliver(mut rx: broadcast::Receiver<ClientEvent>, listener: Box<dyn MeshClientListener>) {
    loop {
        let events = match rx.recv().await {
            Ok(e) => vec![MeshEvent::from(e)],
            Err(broadcast::error::RecvError::Lagged(_)) => {
                vec![MeshEvent::MembersChanged, MeshEvent::ServicesChanged]
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        for e in events {
            if catch_unwind(AssertUnwindSafe(|| listener.on_event(e))).is_err() {
                warn!("mesh-client listener failed on {e:?}");
            }
        }
    }
}
