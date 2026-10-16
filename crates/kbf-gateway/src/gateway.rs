//! The gateway process: the registration listener (mutual TLS), the xDS listener (loopback)
//! and the timer that expires registrations.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use envoy_types::pb::envoy::service::discovery::v3::aggregated_discovery_service_server::AggregatedDiscoveryServiceServer;
use kbf_proto::gateway::registration_server::RegistrationServer;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Server, ServerTlsConfig};

use crate::ads::AdsService;
use crate::registration::RegistrationService;
use crate::registry::{Clock, Registry};

/// What the gateway listens on and how long registrations live.
#[derive(Clone, Debug)]
pub struct Config {
    /// The registration listener, for servers anywhere on the farm network.
    pub register_listen: SocketAddr,
    /// Its identity and the CA callers' certificates must chain to (mutual TLS).
    pub tls: ServerTlsConfig,
    /// The xDS listener, for the Envoy on this box. Plain text and unauthenticated, so
    /// it must be a loopback address.
    pub xds_listen: SocketAddr,
    /// How long a registration lives without another call.
    pub ttl: Duration,
}

/// Why the gateway could not start or stopped.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The xDS listener was asked for off loopback.
    #[error("--xds-listen {0} is not a loopback address: xDS is unauthenticated plain text")]
    XdsNotLoopback(SocketAddr),
    /// A listener could not be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    /// The transport failed, or a TLS configuration was refused.
    #[error(transparent)]
    Transport(#[from] tonic::transport::Error),
    /// A file named by a flag could not be read.
    #[error("read {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
    /// The async runtime or the signal handlers could not be set up.
    #[error("runtime: {0}")]
    Runtime(#[source] io::Error),
}

/// A gateway that is listening: the addresses it bound, its registry, and the future
/// that serves until shutdown.
pub struct Bound {
    /// Where registrations are accepted.
    pub registration: SocketAddr,
    /// Where xDS is served.
    pub xds: SocketAddr,
    /// The registrations, which a test reads and moves with its clock.
    pub registry: Arc<Registry>,
    serving: Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>,
}

impl Bound {
    /// Serves until the shutdown future given to [`bind`] completes or a listener fails.
    ///
    /// # Errors
    /// A listener fails while serving.
    pub async fn serve(self) -> Result<(), Error> {
        self.serving.await
    }
}

fn listen(addr: SocketAddr) -> Result<(TcpIncoming, SocketAddr), Error> {
    let bound = TcpIncoming::bind(addr).and_then(|incoming| {
        let local = incoming.local_addr()?;
        Ok((incoming.with_nodelay(Some(true)), local))
    });
    bound.map_err(|source| Error::Bind { addr, source })
}

/// Binds both listeners. Nothing is served until [`Bound::serve`] runs; it serves until
/// `shutdown` completes. The registry reads time through `clock`.
///
/// # Errors
/// The xDS address is not loopback, a listener cannot be bound, or the TLS configuration
/// is refused.
pub fn bind(
    config: Config,
    clock: Clock,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound, Error> {
    if !config.xds_listen.ip().is_loopback() {
        return Err(Error::XdsNotLoopback(config.xds_listen));
    }
    let (registration_in, registration) = listen(config.register_listen)?;
    let (xds_in, xds) = listen(config.xds_listen)?;
    let registry = Arc::new(Registry::new(config.ttl, clock));
    let registrations = Server::builder()
        .tls_config(config.tls)?
        .add_service(RegistrationServer::new(RegistrationService::new(
            Arc::clone(&registry),
        )))
        .serve_with_incoming(registration_in);
    let discovery = Server::builder()
        .add_service(AggregatedDiscoveryServiceServer::new(AdsService::new(
            &registry,
        )))
        .serve_with_incoming(xds_in);
    let sweeping = Arc::clone(&registry);
    let every = (config.ttl / 3).max(Duration::from_millis(1));
    let serving = async move {
        let sweep = async {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                sweeping.sweep();
            }
        };
        tokio::select! {
            served = registrations => served.map_err(Error::from),
            served = discovery => served.map_err(Error::from),
            () = sweep => Ok(()),
            () = shutdown => Ok(()),
        }
    };
    Ok(Bound {
        registration,
        xds,
        registry,
        serving: Box::pin(serving),
    })
}
