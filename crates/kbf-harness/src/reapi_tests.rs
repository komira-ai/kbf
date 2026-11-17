//! The channel to a farm's frontend, against a TLS service under a certificate of the CA the
//! harness makes.

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::{ExecuteRequest, WaitExecutionRequest};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Identity, Server, ServerTlsConfig};

use super::connect;

/// An `Execution` service that answers every call `UNIMPLEMENTED`: a service to reach, over
/// TLS, and a status that only it sends.
struct Unimplemented;

#[tonic::async_trait]
impl Execution for Unimplemented {
    type ExecuteStream =
        futures::stream::Iter<std::vec::IntoIter<Result<Operation, tonic::Status>>>;
    type WaitExecutionStream = Self::ExecuteStream;

    async fn execute(
        &self,
        _: tonic::Request<ExecuteRequest>,
    ) -> Result<tonic::Response<Self::ExecuteStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }

    async fn wait_execution(
        &self,
        _: tonic::Request<WaitExecutionRequest>,
    ) -> Result<tonic::Response<Self::WaitExecutionStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not served"))
    }
}

/// Serves [`Unimplemented`] under a certificate of the CA the harness makes for `localhost`; the
/// channel [`connect`] makes to it.
async fn channel(name: &str) -> tonic::transport::Channel {
    let etc = crate::scratch(name);
    let pki = crate::pki::Pki::new();
    pki.write_ca(&etc);
    pki.issue_reapi(&etc, &["localhost", "127.0.0.1"]);
    let read = |file: &str| std::fs::read(etc.join(file)).expect("read");
    let tls =
        ServerTlsConfig::new().identity(Identity::from_pem(read("reapi.pem"), read("reapi.key")));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("TLS")
            .add_service(ExecutionServer::new(Unimplemented))
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    connect(&read("ca.pem"), &addr.to_string()).await
}

/// Catches: a channel that does not trust the farm CA or the name `localhost` (the
/// connection fails), or that is not made to the address it is given (the call is answered by
/// another service or by none).
#[tokio::test]
async fn the_channel_reaches_a_service_under_the_farm_ca() {
    let channel = channel("reapi-connect").await;
    let status = ExecutionClient::new(channel)
        .execute(ExecuteRequest::default())
        .await
        .expect_err("the service answers an error");
    assert_eq!(status.code(), tonic::Code::Unimplemented, "{status:?}");
}
