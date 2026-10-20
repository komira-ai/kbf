//! The enroll listener (`docs/design/install-and-enrollment.md`, section 5.2): one gRPC
//! service, `kbf.enroll.v1.Enroll`, on a socket of its own, over TLS that authenticates
//! the server only.
//!
//! A node that has no certificate yet cannot present one, and the worker listener asks
//! every client for one, so enrollment cannot share it; the REAPI listener's certificate
//! is a public name's, which the farm CA does not issue; the operator API is loopback.
//! This listener's certificate is the frontend's own, issued by the CA, and a node pins
//! the CA through its token ([`super::tls::FrontendTls`]).
//!
//! The server serves exactly the two methods of `Enroll` (a method outside the service is
//! `UNIMPLEMENTED`, as tonic answers for any path it has no route for). Every call passes
//! the guard ([`super::limits::GuardLayer`]: rate limit, calls in flight, request size)
//! before it reaches the service, and a request that takes longer than [`CALL_TIMEOUT`] is
//! ended. The TLS handshake has the deadline `kbf_tls::HANDSHAKE_TIMEOUT`.

use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use kbf_proto::kbf::enroll::v1::enroll_server::EnrollServer;
use tonic::transport::Server;

use super::limits::{GuardLayer, Limits, MAX_REQUEST_BYTES};
use super::service::EnrollService;
use super::tls::FrontendTls;
use crate::listen::{Latch, Socket};
use crate::serve::ServeError;

/// The longest a call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A bound enroll listener, not yet serving.
pub struct EnrollListener {
    socket: Socket,
    service: EnrollService,
    tls: std::sync::Arc<FrontendTls>,
    limits: Limits,
}

impl EnrollListener {
    /// The listener over `socket`.
    #[must_use]
    pub(crate) fn new(
        socket: Socket,
        service: EnrollService,
        tls: std::sync::Arc<FrontendTls>,
        limits: Limits,
    ) -> Self {
        Self {
            socket,
            service,
            tls,
            limits,
        }
    }

    /// The future that serves until the latch closes the socket, and that latch. Must be
    /// called inside a Tokio runtime.
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub(crate) fn serve(
        self,
    ) -> Result<
        (
            BoxFuture<'static, Result<(), tonic::transport::Error>>,
            Latch,
        ),
        ServeError,
    > {
        let (incoming, latch) = self.socket.grpc(Some(self.tls.source()))?;
        let service = EnrollServer::new(self.service).max_decoding_message_size(MAX_REQUEST_BYTES);
        let serving = Server::builder()
            .timeout(CALL_TIMEOUT)
            .layer(GuardLayer::new(&self.limits))
            .add_service(service)
            .serve_with_incoming(incoming)
            .boxed();
        Ok((serving, latch))
    }
}
