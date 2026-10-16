//! The `kbf.gateway.v1.Registration` service: servers tell the gateway where their REAPI
//! listener is and whether they lead. See the proto file for the contract; the
//! certificate rule is in [`crate::cert`].

use std::net::SocketAddr;
use std::sync::Arc;

use kbf_proto::gateway::registration_server::Registration;
use kbf_proto::gateway::{RegisterRequest, RegisterResponse};
use tonic::{Request, Response, Status};

use crate::cert::server_node;
use crate::registry::{Claim, Registry};

/// The service over a [`Registry`].
#[derive(Debug)]
pub struct RegistrationService {
    registry: Arc<Registry>,
}

impl RegistrationService {
    /// The service that records calls in `registry`.
    #[must_use]
    pub const fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[tonic::async_trait]
impl Registration for RegistrationService {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let certs = request.peer_certs();
        let leaf = certs
            .as_deref()
            .and_then(|chain| chain.first())
            .ok_or_else(|| Status::unauthenticated("registration needs a client certificate"))?;
        let name = server_node(leaf.as_ref())?;
        let body = request.into_inner();
        let address: SocketAddr = body.reapi_address.parse().map_err(|_| {
            Status::invalid_argument(format!(
                "reapi_address {:?} is not an ip:port",
                body.reapi_address
            ))
        })?;
        self.registry.register(Claim {
            name,
            address,
            is_leader: body.is_leader,
            term: body.term,
        });
        let ttl_ms = u64::try_from(self.registry.ttl().as_millis()).unwrap_or(u64::MAX);
        Ok(Response::new(RegisterResponse { ttl_ms }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::registry::Clock;

    /// Catches: a call that reaches the service without a verified client certificate
    /// (plain text, or a listener without client authentication) being recorded as if it
    /// had one: nothing says which server it is.
    #[tokio::test]
    async fn a_call_without_a_certificate_is_unauthenticated() {
        let registry = Arc::new(Registry::new(Duration::from_secs(6), Clock::system()));
        let service = RegistrationService::new(Arc::clone(&registry));
        let call = Request::new(RegisterRequest {
            reapi_address: "192.0.2.10:8980".to_owned(),
            is_leader: true,
            term: 1,
        });
        let refused = service.register(call).await.expect_err("no certificate");
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        assert!(registry.servers().is_empty());
    }
}
