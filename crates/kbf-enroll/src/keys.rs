//! The registered cold store and its keys, which a storage node holds in memory only
//! (design section 4.3).
//!
//! The frontend does not put the object store's keys in `NodeConfig`. A storage node that
//! has enrolled calls `ColdStore.GetStoreConfig` on the worker listener over its
//! mutual-TLS identity, when the generation the frontend announces in its `Membership`
//! answers differs from the one the node applied. The answer is one snapshot (the store's
//! identity and its keys of one generation) in a [`StoreConfig`], which prints its secret
//! as `Secret(..)` and overwrites it when dropped; the node hands the keys to the
//! object-store client in-process. They are never a file, an argument, an environment
//! variable or a log line, and [`harden_process`] turns off core dumps for the process.

use kbf_proto::kbf::enroll::v1::cold_store_client::ColdStoreClient;
use kbf_proto::kbf::enroll::v1::{GetStoreConfigRequest, GetStoreConfigResponse};
use kbf_types::Role;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use crate::client::CallError;
use crate::store::Store;

/// Why the registered store could not be fetched.
#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    /// The node holds no storage identity, or its files do not read.
    #[error("storage identity: {0}")]
    Identity(String),
    /// The worker endpoint is not a URL, or the connection failed.
    #[error("worker endpoint: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// The connection or the call failed; `Unavailable` is worth retrying.
    #[error(transparent)]
    Call(#[from] CallError),
}

/// Fetches the registered store from the worker listener at `endpoint` (an `https` URL), as
/// the node's storage identity in `store`, trusting the CA bundle it holds. `have_generation`
/// is the generation the node applied (0: none).
///
/// The answer's `config` is absent when `have_generation` equals the frontend's generation
/// and when nothing is registered (generation 0). Any other generation, lower ones too,
/// comes whole.
///
/// # Errors
/// [`KeysError`]; a revoked node is `Call(Status(PermissionDenied))`, and a frontend that
/// cannot answer now (not the writer, the secret unreadable) is `Unavailable`.
pub async fn fetch(
    store: &Store,
    endpoint: &str,
    have_generation: u64,
) -> Result<GetStoreConfigResponse, KeysError> {
    let read = |path: std::path::PathBuf| {
        std::fs::read(&path).map_err(|e| KeysError::Identity(format!("{}: {e}", path.display())))
    };
    let ca = read(store.ca_path())?;
    let cert = read(store.cert_path(Role::Storage))?;
    let key = read(store.key_path(Role::Storage))?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key));
    let channel = Endpoint::from_shared(endpoint.to_owned())?
        .tls_config(tls)?
        .connect()
        .await?;
    let reply = ColdStoreClient::new(channel)
        .get_store_config(GetStoreConfigRequest { have_generation })
        .await
        .map_err(CallError::from)?
        .into_inner();
    Ok(reply)
}

/// Makes the process refuse to leave its memory in a file: no core dump, and where the
/// system has the control, not dumpable. Call it before the keys are fetched.
///
/// # Errors
/// The limit cannot be set.
pub fn harden_process() -> std::io::Result<()> {
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    Ok(())
}
