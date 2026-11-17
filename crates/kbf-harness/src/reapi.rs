//! A client channel to a farm's frontend for the black-box test crates: the REAPI listener
//! under the farm CA ([`connect`]), over which a test's `kbf_transfer::Transfers` writes and
//! reads blobs the way a deployer's build client does, with nothing of the product's server
//! code; the farm itself is [`crate::farm::Farm`]. It lives here, not in `kbf-testkit`,
//! because it names a gRPC channel, and `kbf-testkit` links no other kbf crate.

use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// A channel to the REAPI listener at `reapi` (`host:port`), TLS under the CA `ca_pem`
/// (a farm's [`crate::farm::Farm::ca_pem`] and [`crate::farm::Farm::reapi`]).
///
/// # Panics
/// When the address is not an endpoint, or the listener cannot be reached: a test cannot
/// go on without it.
pub async fn connect(ca_pem: &[u8], reapi: &str) -> Channel {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name("localhost");
    Endpoint::from_shared(format!("https://{reapi}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect to the frontend")
}

#[cfg(test)]
#[path = "reapi_tests.rs"]
mod tests;
