//! The enroll listener at a frontend's start: the gate, the socket, the CA, the listener's
//! certificate, and then, once the state store is open, the service and the operator API's
//! half ([`super::api::EnrollApi`], whose tokens carry this CA's pin).

use std::error::Error;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use kbf_ca::{Policy, SystemEntropy};
use kbf_signer::Started;
use kbf_state::Store;

use super::OsRandom;
use super::api::EnrollApi;
use super::ca::{Ca, CaPins, SignerChoice};
use super::flags::EnrollFlags;
use super::gate::start_gate;
use super::listener::EnrollListener;
use super::service::EnrollService;
use super::tls::{FrontendTls, unix_now};
use crate::listen::{Latch, Socket};

/// What a frontend holds of enrollment before its state store is open: the socket bound,
/// the CA opened, the listener's certificate issued.
pub struct Prepared {
    flags: EnrollFlags,
    socket: Socket,
    ca: Arc<Ca>,
    tls: Arc<FrontendTls>,
}

/// What a started enrollment hands the frontend to serve.
pub struct Serving {
    /// The operator API's enrollment routes.
    pub api: Arc<EnrollApi>,
    /// The enroll listener, serving until its latch closes.
    pub listener: BoxFuture<'static, Result<(), tonic::transport::Error>>,
    /// Closes the listener's socket.
    pub(crate) latch: Latch,
    /// Replaces the listener's certificate at half its life.
    pub renewal: BoxFuture<'static, ()>,
}

impl Prepared {
    /// Where the enroll listener listens.
    #[must_use]
    pub fn local(&self) -> std::net::SocketAddr {
        self.socket.local
    }

    /// The CA this frontend issues from.
    #[must_use]
    pub fn ca(&self) -> Arc<Ca> {
        Arc::clone(&self.ca)
    }
}

/// Checks the gate, binds the listener, opens (or makes) the CA and issues the listener's
/// certificate. Nothing is written before the gate and the bind have passed.
///
/// # Errors
/// The gate refuses ([`super::gate::DenyNotEnforced`]), the address cannot be bound, the
/// CA cannot be opened, or its certificate cannot be issued.
pub fn prepare(flags: &EnrollFlags) -> Result<Prepared, Box<dyn Error>> {
    start_gate(flags.allow_unenforced_deny)?;
    let socket = Socket::bind(flags.listen)?;
    let policy = Policy::new(flags.cert_lifetime_secs)?;
    let now = unix_now();
    let ca = Ca::open(
        &flags.ca_dir,
        &flags.signer,
        policy,
        now,
        &mut SystemEntropy,
    )?;
    if flags.signer == SignerChoice::DevFile {
        tracing::warn!(
            "the CA key is a file in this process (--ca-dev-file-signer): development only"
        );
    }
    if ca.started() == Started::Created {
        tracing::info!(dir = %flags.ca_dir.display(), "made a new CA");
    }
    let tls = FrontendTls::start(&ca, &flags.frontend_names, now, &mut SystemEntropy)?;
    Ok(Prepared {
        flags: flags.clone(),
        socket,
        ca: Arc::new(ca),
        tls: Arc::new(tls),
    })
}

impl Prepared {
    /// The service, the listener and the API over the open state store `store` (the
    /// frontend's fenced handle). Must be called inside a Tokio runtime.
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub fn serve(self, store: Arc<dyn Store>) -> Result<Serving, Box<dyn Error>> {
        let Self {
            flags,
            socket,
            ca,
            tls,
        } = self;
        let service = EnrollService::new(
            Arc::clone(&store),
            Arc::clone(&ca),
            &flags.frontend_names,
            flags.settings.clone(),
        );
        let api = Arc::new(EnrollApi::new(
            store,
            Arc::new(CaPins(Arc::clone(&ca))),
            Arc::new(OsRandom),
        ));
        let (listener, latch) =
            EnrollListener::new(socket, service, Arc::clone(&tls), flags.limits).serve()?;
        Ok(Serving {
            api,
            listener,
            latch,
            renewal: tls.keep_fresh(ca).boxed(),
        })
    }
}
