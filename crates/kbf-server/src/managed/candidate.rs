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

/// How long [`Candidate::retire`] waits for open connections to finish (the listeners
/// sent GOAWAY, so none opens a stream after that).
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
    /// Stopped by `retire`: an open health `Watch` ends.
    readiness: Arc<Readiness>,
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
        let readiness = Arc::new(Readiness::default());
        let checks = Arc::new(crate::frontend::checks(
            Arc::clone(&readiness),
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
        // A listener that fails shows as probes that are refused.
        tasks.0.push(tokio::spawn(async move {
            let _ = served.await;
        }));
        if let Some(socket) = &api {
            let listener = socket.try_clone()?.http()?;
            let served = serve_api(
                Some(listener),
                crate::health::router_of(checks),
                either(&stop, global),
            );
            tasks.0.push(tokio::spawn(async move {
                let _ = served.await;
            }));
        }
        Ok(Self {
            store,
            reapi: reapi.local,
            api: api.as_ref().map(|socket| socket.local),
            sockets: Sockets { reapi, api },
            stop,
            readiness,
            tasks,
        })
    }

    /// Stops the stand-ins and returns the sockets they served, free of them, and the
    /// store handle. The stand-ins go not ready first, which ends an open health `Watch`
    /// (UNAVAILABLE), then their listeners are told to stop (GOAWAY) and are waited for up
    /// to [`GRACE`]. A stand-in must be gone before the real server accepts, so that no
    /// connection is answered by it.
    pub async fn retire(self) -> (Sockets, state_store::Candidate) {
        let Self {
            store,
            sockets,
            stop,
            readiness,
            mut tasks,
            ..
        } = self;
        readiness.stop();
        stop.fire();
        for task in std::mem::take(&mut tasks.0) {
            let _ = tokio::time::timeout(GRACE, task).await;
        }
        (sockets, store)
    }
}
