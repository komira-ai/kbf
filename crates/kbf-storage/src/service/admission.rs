//! Who may call which endpoint (`docs/design/architecture-v2.md`, section 9: mTLS on
//! every hop with the role in the certificate; storage accepts only expected roles per
//! endpoint).
//!
//! **What is real.** The per-endpoint table of expected roles ([`Endpoint::allowed`]),
//! the check against the caller's verified client certificate, and the refusals:
//! UNAUTHENTICATED for no certificate, PERMISSION_DENIED for a certificate with no role,
//! two, an unknown one or one the endpoint does not take.
//!
//! **What is a seam for av2-14 (role certificates).** How a certificate spells its role.
//! Until that unit settles it, a role is a URI subjectAltName `urn:kbf:role:<name>`
//! ([`ROLE_URI_PREFIX`]); [`role_of`] is the one place that reads it. Not here, and
//! planned in section 9 and phase P7: the deny list and node-name checks of
//! `kbf-server`'s worker listener, and the signed storage-access grant.
//!
//! Plain text ([`Admission::Open`]) has no identity to read, so it admits every call: for
//! tests and trials on one machine, as the worker listener's `Peers::Unauthenticated`.

use tonic::{Request, Status};
use x509_parser::extensions::{GeneralName, ParsedExtension};

/// How a certificate names a role (provisional, see the module docs).
pub const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// The role a peer's certificate names (section 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRole {
    /// A frontend, on behalf of a client or of a daemon's result.
    Frontend,
    /// Another storage node (replication, repair).
    Storage,
    /// A build node.
    Daemon,
}

impl PeerRole {
    /// Every role, in the order section 9 lists them.
    pub const ALL: [Self; 3] = [Self::Frontend, Self::Storage, Self::Daemon];

    /// The name a certificate spells after [`ROLE_URI_PREFIX`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Storage => "storage",
            Self::Daemon => "daemon",
        }
    }
}

/// The endpoints with their own expectation of who calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `FindMissingBlobs`.
    FindMissing,
    /// ByteStream `Read`.
    Read,
    /// ByteStream `Write`.
    Write,
    /// ByteStream `QueryWriteStatus`: whether a `Write` of the blob has landed, so it
    /// is the roles of the `Write` it asks about.
    QueryWriteStatus,
    /// `ActionStore.Get`: a client's `GetActionResult`.
    ActionGet,
    /// `ActionStore.Prepare`: the first half of an action-cache write.
    ActionPrepare,
    /// `ActionStore.Commit`: the second half.
    ActionCommit,
    /// The `Segments` upload and locate calls.
    Segments,
}

impl Endpoint {
    /// Every endpoint.
    pub const ALL: [Self; 8] = [
        Self::FindMissing,
        Self::Read,
        Self::Write,
        Self::QueryWriteStatus,
        Self::ActionGet,
        Self::ActionPrepare,
        Self::ActionCommit,
        Self::Segments,
    ];

    /// The roles that call this endpoint (section 7: a client's calls come through a
    /// frontend; a daemon fetches inputs and uploads outputs directly; storage nodes
    /// call each other to replicate). A daemon never reads the action cache, and only a
    /// daemon uploads segments.
    #[must_use]
    pub const fn allowed(self) -> &'static [PeerRole] {
        use PeerRole::{Daemon, Frontend, Storage};
        match self {
            Self::FindMissing | Self::Read | Self::Write | Self::QueryWriteStatus => {
                &[Frontend, Storage, Daemon]
            }
            Self::ActionGet => &[Frontend],
            Self::ActionPrepare | Self::ActionCommit => &[Frontend, Daemon],
            Self::Segments => &[Daemon],
        }
    }
}

/// How a storage service learns who is calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Plain text: no identity, every call admitted.
    Open,
    /// Mutual TLS: the verified client certificate must name a role the endpoint takes.
    Certified,
}

impl Admission {
    /// Admits the call `request` is to `endpoint`, or says why not.
    ///
    /// # Errors
    /// See [`Admission::check_leaf`].
    pub fn check<T>(&self, request: &Request<T>, endpoint: Endpoint) -> Result<(), Status> {
        let certs = request.peer_certs();
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        self.check_leaf(leaf.map(AsRef::as_ref), endpoint)
    }

    /// [`Admission::check`] for a caller whose verified chain starts with `leaf`.
    ///
    /// # Errors
    /// Under [`Admission::Certified`], UNAUTHENTICATED if there is no certificate or it
    /// does not parse; PERMISSION_DENIED if it names no role, several, one that is not
    /// a role, or a role `endpoint` does not take.
    pub fn check_leaf(&self, leaf: Option<&[u8]>, endpoint: Endpoint) -> Result<(), Status> {
        if *self == Self::Open {
            return Ok(());
        }
        let der = leaf.ok_or_else(|| Status::unauthenticated("no client certificate"))?;
        let role = role_of(der)?;
        if endpoint.allowed().contains(&role) {
            return Ok(());
        }
        let refused =
            Status::permission_denied(format!("role {} may not call {endpoint:?}", role.name()));
        tracing::warn!(role = role.name(), ?endpoint, "storage call refused");
        Err(refused)
    }
}

/// The role a DER certificate names: its one `urn:kbf:role:<name>` URI subjectAltName.
///
/// # Errors
/// UNAUTHENTICATED: the bytes are not a certificate. PERMISSION_DENIED: it names no
/// role or several, or a name that is no role.
pub fn role_of(der: &[u8]) -> Result<PeerRole, Status> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| Status::unauthenticated(format!("the client certificate: {e}")))?;
    let names: Vec<&str> = cert
        .extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(&san.general_names),
            _ => None,
        })
        .flatten()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => uri.strip_prefix(ROLE_URI_PREFIX),
            _ => None,
        })
        .collect();
    let [name] = names.as_slice() else {
        return Err(Status::permission_denied(format!(
            "the client certificate must name exactly one role ({ROLE_URI_PREFIX}<name>); \
             it names {}",
            names.len()
        )));
    };
    PeerRole::ALL
        .into_iter()
        .find(|role| role.name() == *name)
        .ok_or_else(|| Status::permission_denied(format!("{name:?} is not a role")))
}
