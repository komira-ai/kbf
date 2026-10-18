//! What a process serves while it waits to take over: the REAPI listener with the health
//! service and `GetCapabilities`, and the operator API's `/readyz`. No worker listener is
//! bound, no scheduler or reconciler runs and nothing is written to the state store.
//!
//! The two listeners are bound once. The stand-ins serve duplicates of the sockets, so
//! that [`Candidate::retire`] can hand the original sockets to the real servers: a
//! connection that arrives between the two waits in the socket's queue and is accepted
//! by the real server.

use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_auth::AuthenticateLayer;
use kbf_front::{CapabilitiesService, MAX_MESSAGE_BYTES};
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::reapi::capabilities_server::CapabilitiesServer;
use kbf_storage::service::RemoteStorage;
use tokio::task::JoinHandle;
use tonic::service::Routes;
use tonic::transport::Server;

use crate::frontend::FrontendFlags;
use crate::grpc_health::{HealthService, WATCH_INTERVAL};
use crate::handover::Stop;
use crate::health::Readiness;
use crate::listen::{Binder, Socket};
use crate::serve::serve_api;
use crate::state_store;

/// How long [`Candidate::retire`] lets open connections finish before it cuts them. A
/// health `Watch` stream never finishes by itself.
const GRACE: Duration = Duration::from_secs(2);

/// The bound listeners of a candidate, handed to the real servers.
pub(crate) struct Sockets {
    pub reapi: Socket,
    pub api: Option<Socket>,
}

/// Ends its tasks when dropped.
#[derive(Default)]
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        self.0.iter().for_each(JoinHandle::abort);
    }
}

/// A process waiting for a takeover.
pub(crate) struct Candidate {
    pub store: state_store::Candidate,
    sockets: Sockets,
    pub reapi: SocketAddr,
    pub api: Option<SocketAddr>,
    stop: Stop,
    tasks: Tasks,
}

/// A future that completes when either signal does.
fn either(a: &Stop, b: &Stop) -> impl Future<Output = ()> + Send + 'static {
    let (a, b) = (a.fired(), b.fired());
    async move {
        tokio::select! {
            () = a => {},
            () = b => {},
        }
    }
}

impl Candidate {
    /// Binds the listeners and serves the stand-ins until `retire` or until `global`
    /// fires. Must run inside a Tokio runtime.
    ///
    /// # Errors
    /// A flag or file the listeners need is refused, or an address cannot be bound.
    pub fn start(
        flags: &FrontendFlags,
        store: state_store::Candidate,
        global: &Stop,
    ) -> Result<Self, Box<dyn Error>> {
        let args = &flags.server;
        let listeners = args.listeners()?;
        let policy = args.reapi_auth_policy()?;
        let api = args.api()?;
        let binder = Binder::new();
        let reapi = Socket::bind(listeners.reapi, &binder)?;
        let api = api
            .as_ref()
            .map(|api| Socket::bind(api.listen, &binder))
            .transpose()?;
        let storage = Arc::new(RemoteStorage::new(
            flags.storage_endpoint()?.connect_lazy(),
            0,
        ));
        let checks = Arc::new(crate::frontend::checks(
            Arc::new(Readiness::default()),
            storage,
            None,
            Some(store.reader()),
            listeners.store_probe_timeout,
        ));
        let stop = Stop::new();
        let mut tasks = Tasks::default();

        let (incoming, _latch) = reapi.try_clone()?.grpc(listeners.reapi_tls)?;
        let capabilities = CapabilitiesService::new(true, Arc::new(policy.authorizers));
        let routes = Routes::from(
            Routes::new(
                CapabilitiesServer::new(capabilities)
                    .max_decoding_message_size(MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(MAX_MESSAGE_BYTES),
            )
            .into_axum_router()
            .layer(AuthenticateLayer::new(policy.authenticator)),
        )
        .add_service(HealthServer::new(HealthService::of(
            Arc::clone(&checks),
            WATCH_INTERVAL,
        )));
        let served = Server::builder()
            .add_routes(routes)
            .serve_with_incoming_shutdown(incoming, either(&stop, global));
        tasks.0.push(tokio::spawn(async move {
            if let Err(error) = served.await {
                tracing::warn!(%error, "the candidate's REAPI listener failed");
            }
        }));
        if let Some(socket) = &api {
            let listener = socket.try_clone()?.http()?;
            let served = serve_api(
                Some(listener),
                crate::health::router_of(checks),
                either(&stop, global),
            );
            tasks.0.push(tokio::spawn(async move {
                if let Err(error) = served.await {
                    tracing::warn!(%error, "the candidate's operator API listener failed");
                }
            }));
        }
        Ok(Self {
            store,
            reapi: reapi.local,
            api: api.as_ref().map(|socket| socket.local),
            sockets: Sockets { reapi, api },
            stop,
            tasks,
        })
    }

    /// Stops the stand-ins (open connections finish for [`GRACE`], then are cut) and
    /// returns the sockets they served, free of them, and the store handle. A stand-in
    /// must be gone before the real server accepts, so that no connection is answered
    /// by it.
    pub async fn retire(self) -> (Sockets, state_store::Candidate) {
        let Self {
            store,
            sockets,
            stop,
            mut tasks,
            ..
        } = self;
        stop.fire();
        for mut task in std::mem::take(&mut tasks.0) {
            if tokio::time::timeout(GRACE, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        (sockets, store)
    }
}
