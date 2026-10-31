//! Who a certificate says its holder is, read from its subjectAltNames and from nothing else.
//!
//! The farm CA makes an updater's certificate with exactly these subjectAltNames
//! (`kbf_ca::Authority::issue_updater`):
//!
//! - one URI `urn:kbf:role:updater`, the certificate's role;
//! - one URI `urn:kbf:host-role:<name>`, what the host runs (`frontend`, `storage`, `daemon`
//!   or `mac-daemon`);
//! - one DNS name, the host's node id.
//!
//! The coordinator reads the certificate a caller presents, which TLS has already checked
//! chains to the farm CA and may be used as a client's: a certificate of any other role (a
//! storage node's, a frontend's, the coordinator's), or one that does not name exactly one of
//! each of the above, is no updater, and the caller is refused. The heartbeat's body is never
//! asked who sent it. The updater reads its own certificate the same way, to know which node
//! it is and what its host runs, so that neither is a flag that could disagree with the
//! certificate (and neither goes in the heartbeat).

use kbf_peer::{ROLE_URI_PREFIX, RoleError, names_of, uri_values_of};
use kbf_types::{HostRole, NodeId};

/// The role of an updater's certificate.
pub const UPDATER_ROLE: &str = "updater";

/// An updater, as its certificate names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The host.
    pub node_id: NodeId,
    /// What the host runs.
    pub role: HostRole,
}

/// Why a certificate is no updater's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The bytes are not a certificate.
    #[error("the client certificate does not parse: {0}")]
    NotACertificate(String),
    /// The certificate presents no role, several, or one that is not `updater`.
    #[error("the client certificate's role is not `updater` (it names {0:?})")]
    NotAnUpdater(Vec<String>),
    /// It does not name exactly one host role the farm has.
    #[error("the client certificate must name exactly one host role (it names {0:?})")]
    HostRole(Vec<String>),
    /// It does not name exactly one node id.
    #[error("the client certificate must name exactly one node id (it names {0:?})")]
    NodeId(Vec<String>),
}

impl From<RoleError> for IdentityError {
    fn from(e: RoleError) -> Self {
        Self::NotACertificate(e.to_string())
    }
}

/// The updater a DER certificate names.
///
/// # Errors
/// [`IdentityError`]: see its variants.
pub fn identity_of(der: &[u8]) -> Result<Identity, IdentityError> {
    let roles = uri_values_of(der, ROLE_URI_PREFIX)?;
    if roles != [UPDATER_ROLE] {
        return Err(IdentityError::NotAnUpdater(roles));
    }
    let host_roles = uri_values_of(der, HostRole::URI_PREFIX)?;
    let role = match host_roles.as_slice() {
        [name] => HostRole::parse_named(name).ok(),
        _ => None,
    }
    .ok_or(IdentityError::HostRole(host_roles))?;
    let dns = names_of(der)?;
    let node_id = match dns.dns() {
        [name] => name.parse().ok(),
        _ => None,
    }
    .ok_or_else(|| IdentityError::NodeId(dns.dns().to_vec()))?;
    Ok(Identity { node_id, role })
}
