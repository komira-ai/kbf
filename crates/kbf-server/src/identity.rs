//! Who is on the worker listener: the node a daemon's client certificate names, and
//! the deny list (issue #79). The same rules admit a worker stream and each blob call
//! a daemon makes on that listener ([`crate::blobs`]).
//!
//! **The role** is checked before any of this, per method, by the listener's table
//! ([`crate::worker_roles`]): a worker stream or a blob call reaches these rules only
//! from a certificate naming the daemon role.
//!
//! **The rule.** Under mutual TLS a daemon's client certificate must carry exactly one
//! DNS name in its subjectAltName extension, and that name must equal, byte for byte,
//! the `node_id` of every `Hello` the stream sends. The subject's common name is not
//! read. A certificate for one node therefore cannot open, or take over, another
//! node's session. A blob call carries no node id: its certificate must still carry
//! exactly one DNS name, and that name is the node the deny list is checked against.
//!
//! **Behind the farm's proxy** (`--proxy-auth-file`, [`crate::proxy_auth`]) there is no
//! certificate on the connection: the proxy terminated the TLS and forwards the node and
//! role it verified. [`presented`] is the one function that says which a call has, and every
//! check above (the role table, the `Hello` binding, the blob calls, `Membership`) goes
//! through it: a proxied identity names its node as a certificate would, so the same
//! `Hello.node_id` rule applies (a mismatch is PERMISSION_DENIED), and a `node` deny entry
//! names it; it has no serial or key hash, so those entries cannot.
//!
//! **The deny list** is a file the server looks at again at every check, so an edit
//! takes effect without a restart. Each check reads the file's metadata (device,
//! inode, size, mode, owner, modification and change time) and reads and parses the
//! file again only when that changed since the last read. Each line is blank, a `#` comment, or one entry:
//!
//! ```text
//! serial 0a:1b:2c            # the certificate serial, hex (colons and case ignored)
//! spki-sha256 <64 hex>       # SHA-256 of the certificate's DER SubjectPublicKeyInfo
//! node mac-07                # a node id, whatever certificate it comes with
//! ```
//!
//! A stream whose certificate or node is listed is refused PERMISSION_DENIED at its
//! first `Hello`, and ended at the next `Hello`, `Heartbeat` or `Result` it sends after
//! the entry is added, and a blob call is refused PERMISSION_DENIED from the first
//! call after it. A `node` entry is compared with a node id or a DNS name up to ASCII case,
//! as DNS compares names. A deny list that cannot be read or parsed refuses every check (fail
//! closed) until it is fixed, which ends every connected stream within one heartbeat.
//! A storage node's `Register` and `Heartbeat` ([`crate::members_rpc`]) are checked against
//! the same list by its certificate ([`DenyList::admit_cert`]): a listed serial or key,
//! or any DNS name in it that is a listed node, is refused PERMISSION_DENIED, and an
//! unusable list UNAVAILABLE.
//!
//! **The store's rows.** The operator's list the storage nodes enforce is the `deny/` rows
//! of the state store ([`crate::members`]): `POST /v1/denied`, and every node revoked through
//! the enroll API (its id, its serials and its key hashes). The worker listener reads those
//! rows at every check as well ([`Peers::with_denied_rows`]), and refuses what either the
//! file or the rows name, with the same matchers. The rows are the membership's in-memory
//! snapshot (no store read per check), as fresh as its newest refresh: a revoke and
//! `POST /v1/denied` refresh before they answer, and the sweeper every beat. A refresh that
//! fails keeps the earlier snapshot in use for [`DENY_SNAPSHOT_SWEEPS`] beats; after that, or
//! before any refresh has read the rows, every check is refused UNAVAILABLE (fail closed), as
//! an unusable file does. A corrupt `deny/` row makes every refresh fail, so it ends in that
//! state until the row is repaired (see `docs/api.md`). A revoked daemon's stream therefore
//! ends at its next message, and its blob calls are refused from the next call.
//! Both are compared with a certificate through the one reader,
//! `kbf_peer::identity_of` ([`PeerCert`] wraps it).
//! Replace the file by renaming a new one over it: an in-place edit that keeps the
//! size and lands within the filesystem's timestamp granularity may go unseen until
//! the next change.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::DenyEntry;
use kbf_peer::PeerIdentity;
use tonic::Status;
use tonic::codegen::http::Extensions;

use crate::members::Members;
use crate::proxy_auth::{Identity, Trust};

/// What the server takes from a client certificate: the identity a deny entry is compared
/// with (`kbf-peer`'s reader, which the storage service uses too) and, for a daemon, the
/// one node it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCert {
    identity: PeerIdentity,
}

impl PeerCert {
    /// Reads a DER certificate (TLS has already verified it against the client CA).
    ///
    /// # Errors
    /// The bytes are not an X.509 certificate.
    pub fn from_der(der: &[u8]) -> Result<Self, String> {
        kbf_peer::identity_of(der)
            .map(|identity| Self { identity })
            .map_err(|e| e.to_string())
    }

    /// The caller a proxy verified, which names `node_id` and the IP addresses `ips` of its
    /// leaf (`x-kbf-node-ips`): no serial or key hash, so a `serial` or `spki-sha256` deny
    /// entry cannot name it (a `node` entry can). A Membership registration by IP address is
    /// covered exactly when that address is in `ips`.
    #[must_use]
    pub fn proxied(node_id: &str, ips: &[std::net::IpAddr]) -> Self {
        Self {
            identity: PeerIdentity::proxied(node_id, ips),
        }
    }

    /// What a deny entry is compared with: the names, the serial and the key hash.
    #[must_use]
    pub fn identity(&self) -> &PeerIdentity {
        &self.identity
    }

    /// The certificate's serial, as the deny list spells it.
    #[must_use]
    pub fn serial(&self) -> &str {
        self.identity.serial()
    }

    /// The SHA-256 of its SubjectPublicKeyInfo, as the deny list spells it.
    #[must_use]
    pub fn spki_sha256(&self) -> &str {
        self.identity.spki_sha256()
    }

    /// Whether this certificate names `node_id` under the rule in the module docs.
    ///
    /// # Errors
    /// PERMISSION_DENIED: the certificate has no DNS name, more than one, or another.
    pub fn names(&self, node_id: &str) -> Result<(), Status> {
        match self.node()? {
            only if only == node_id => Ok(()),
            only => Err(Status::permission_denied(format!(
                "the client certificate is for node {only:?}, not {node_id:?}"
            ))),
        }
    }

    /// The node this certificate names: its one DNS subjectAltName.
    ///
    /// # Errors
    /// PERMISSION_DENIED: the certificate has no DNS name, or more than one.
    pub fn node(&self) -> Result<&str, Status> {
        match self.identity.names().dns() {
            [only] => Ok(only),
            names => Err(Status::permission_denied(format!(
                "the client certificate must name exactly one node as a DNS subjectAltName; \
                 it names {}",
                names.len()
            ))),
        }
    }
}

/// What a call presents as who it is: what a proxy vouched for, or the client certificate
/// TLS verified on the connection. Made by [`presented`] and nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presented {
    /// The identity a proxy forwarded under its secret ([`crate::proxy_auth`]).
    Proxy(Identity),
    /// The leaf of the verified client certificate chain; `None` in plain text or with no
    /// client certificate.
    Leaf(Option<Vec<u8>>),
}

impl Presented {
    /// The DER of the leaf certificate, if the call presents one.
    #[must_use]
    pub fn leaf(&self) -> Option<&[u8]> {
        match self {
            Self::Leaf(leaf) => leaf.as_deref(),
            Self::Proxy(_) => None,
        }
    }
}

/// Who the call whose request extensions are `extensions` presents itself as: the identity a
/// proxy verified ([`Trust::Proxied`]), else the client certificate. The one function that
/// decides, so the role check, the `Hello` binding, the blob calls and `Membership` cannot
/// disagree about it.
#[must_use]
pub fn presented(extensions: &Extensions) -> Presented {
    match extensions.get::<Trust>() {
        Some(Trust::Proxied(identity)) => Presented::Proxy(identity.clone()),
        _ => {
            let chain = kbf_tls::peer_certs_of(extensions);
            Presented::Leaf(chain.and_then(|chain| chain.first().map(|leaf| leaf.to_vec())))
        }
    }
}

/// How the worker listener knows who is on a stream.
#[derive(Clone, Debug)]
pub enum Peers {
    /// Plain text: no certificate, so no node is bound to one. For tests and trials on
    /// one machine only.
    Unauthenticated,
    /// Mutual TLS: every stream's certificate must name its node, and is checked
    /// against the deny list if there is one.
    Certified {
        /// Read again at every check.
        deny_list: Option<DenyList>,
        /// The operator's `deny/` rows in the state store, which a node revocation writes;
        /// read again at every check, and merged with the file's entries.
        denied_rows: Option<DeniedRows>,
    },
}

/// How many membership sweeps old the deny list a refresh last read may be and still be
/// used. The sweeper refreshes every beat ([`crate::members_rpc::sweep`]), so a refresh or
/// two that fail (a store hiccup) change nothing, and one that keeps failing (the store
/// down, or a corrupt `deny/` row) refuses every caller after this many.
pub const DENY_SNAPSHOT_SWEEPS: u32 = 3;

/// The `deny/` rows of the state store as the membership's newest refresh read them
/// ([`Members::denied_snapshot`]): checked from memory at every call, never by a store read
/// of its own. A revoke and `POST /v1/denied` refresh before they answer, and the sweeper
/// refreshes every beat.
#[derive(Clone)]
pub struct DeniedRows {
    members: Arc<Members>,
    /// How old the snapshot may be: [`DENY_SNAPSHOT_SWEEPS`] beats.
    max_age: std::time::Duration,
}

impl DeniedRows {
    /// The rows `members` keeps.
    #[must_use]
    pub fn new(members: Arc<Members>) -> Self {
        let max_age = members.config().beat() * DENY_SNAPSHOT_SWEEPS;
        Self { members, max_age }
    }

    /// What the rows denied at the newest refresh that read them.
    ///
    /// # Errors
    /// UNAVAILABLE: no refresh has read the rows yet, or the newest is older than
    /// [`DENY_SNAPSHOT_SWEEPS`] beats: nobody is admitted on a list that may be out of date.
    /// The text is fixed: the store's own error is not for a node.
    fn current(&self) -> Result<Denied, Status> {
        let unusable = |why: &str| {
            tracing::error!(
                why,
                "the deny rows refuse every check until a refresh reads them"
            );
            Status::unavailable("the server's denied nodes could not be read")
        };
        let Some((rows, at)) = self.members.denied_snapshot() else {
            return Err(unusable("no refresh has read them yet"));
        };
        if at.elapsed() > self.max_age {
            return Err(unusable("the newest refresh that read them is too old"));
        }
        Ok(Denied::from_entries(rows.as_ref().clone()))
    }
}

impl std::fmt::Debug for DeniedRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeniedRows")
    }
}

impl Peers {
    /// These peers, also refusing what the `deny/` rows of `members` name (the operator's
    /// list and every node revoked through the enroll API), read at every check beside the
    /// file. Plain-text peers are unchanged: they name no one.
    #[must_use]
    pub fn with_denied_rows(self, members: Option<Arc<Members>>) -> Self {
        match (self, members) {
            (Self::Certified { deny_list, .. }, Some(members)) => Self::Certified {
                deny_list,
                denied_rows: Some(DeniedRows::new(members)),
            },
            (peers, _) => peers,
        }
    }

    /// The certificate of a new stream or blob call, from the chain TLS verified (`leaf`
    /// is its first certificate; `None` in plain text). Its role was checked before the
    /// call was routed ([`crate::worker_roles`]).
    ///
    /// # Errors
    /// UNAUTHENTICATED: under mutual TLS, there is no certificate or it does not parse.
    pub fn peer(&self, leaf: Option<&[u8]>) -> Result<Option<PeerCert>, Status> {
        match (self, leaf) {
            (Self::Unauthenticated, _) => Ok(None),
            (Self::Certified { .. }, None) => Err(Status::unauthenticated("no client certificate")),
            (Self::Certified { .. }, Some(der)) => PeerCert::from_der(der)
                .map(Some)
                .map_err(Status::unauthenticated),
        }
    }

    /// [`Peers::peer`] for what a call presents: a proxied identity must name its node.
    ///
    /// # Errors
    /// As [`Peers::peer`]; and, for a proxied identity under mutual TLS or behind the proxy,
    /// UNAUTHENTICATED if it names no node.
    pub fn peer_of(&self, presented: &Presented) -> Result<Option<PeerCert>, Status> {
        match (self, presented) {
            (Self::Unauthenticated, _) => Ok(None),
            (Self::Certified { .. }, Presented::Proxy(identity)) => identity
                .node_id
                .as_deref()
                .map(|node| Some(PeerCert::proxied(node, &identity.ips)))
                .ok_or_else(|| Status::unauthenticated("no client identity from the proxy")),
            (Self::Certified { .. }, Presented::Leaf(leaf)) => self.peer(leaf.as_deref()),
        }
    }

    /// Whether the stream whose certificate is `peer` may speak as `node_id` now: the
    /// certificate names it, and neither is on the deny list.
    ///
    /// # Errors
    /// PERMISSION_DENIED: the certificate names another node, or is (or the node is)
    /// denied. UNAVAILABLE: the deny list cannot be read or parsed.
    pub async fn admit(&self, peer: Option<&PeerCert>, node_id: &str) -> Result<(), Status> {
        if let Some(peer) = peer {
            peer.names(node_id)?;
        }
        let Self::Certified {
            deny_list,
            denied_rows,
        } = self
        else {
            return Ok(());
        };
        // Both sources are read before either answer is used: one that cannot be read
        // is UNAVAILABLE even when the other would have refused.
        let file = match deny_list {
            Some(list) => Some(list.current().await.map_err(|e| unusable(&e))?),
            None => None,
        };
        let rows = match denied_rows {
            Some(rows) => Some(rows.current()?),
            None => None,
        };
        let why = file
            .and_then(|file| file.refuses(peer, node_id))
            .or_else(|| rows.and_then(|rows| rows.refuses(peer, node_id)));
        match why {
            Some(why) => Err(Status::permission_denied(format!("denied: {why}"))),
            None => Ok(()),
        }
    }

    /// Whether a blob call on the worker listener, presenting `presented` (its verified
    /// chain's leaf, or the identity a proxy forwarded), may go on now: there is mutual TLS
    /// and a certificate (or a proxied node), it names exactly one node, and neither the
    /// certificate nor that node is denied. Returns the node.
    ///
    /// # Errors
    /// UNAUTHENTICATED: plain text, or no certificate (or one that does not parse).
    /// PERMISSION_DENIED: the certificate names no node or several, or it (or its
    /// node) is denied. UNAVAILABLE: the deny list cannot be read or parsed.
    pub async fn admit_call(&self, presented: &Presented) -> Result<String, Status> {
        // Under mutual TLS a missing certificate is an error already; `None` is plain text.
        let Some(peer) = self.peer_of(presented)? else {
            return Err(Status::unauthenticated(
                "blob calls on the worker listener need mutual TLS, which this server \
                 does not serve there",
            ));
        };
        let node = peer.node()?.to_owned();
        self.admit(Some(&peer), &node).await?;
        Ok(node)
    }
}

/// The refusal for a deny list that cannot be read: every check fails, and says so.
fn unusable(e: &DenyListError) -> Status {
    tracing::error!(error = %e, "the deny list refuses every check until it is fixed");
    Status::unavailable(format!("the server's deny list is unusable: {e}"))
}

/// The deny list file. Clones share what was last read.
#[derive(Clone, Debug)]
pub struct DenyList {
    path: PathBuf,
    /// The file's metadata at the last good read, and what that read gave.
    last: Arc<Mutex<Option<LastRead>>>,
}

/// The file's metadata at a good read, and what that read gave.
type LastRead = (FileKey, Arc<Denied>);

/// Why the deny list cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum DenyListError {
    /// The file cannot be read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A line is not an entry.
    #[error("{path}:{line}: {why}")]
    Parse {
        path: PathBuf,
        line: usize,
        why: String,
    },
}

impl DenyList {
    /// The deny list at `path`, read once now so that a bad file stops the server at
    /// start rather than at the first daemon.
    ///
    /// # Errors
    /// The file cannot be read or parsed.
    pub fn open(path: &Path) -> Result<Self, DenyListError> {
        let list = Self {
            path: path.to_owned(),
            last: Arc::default(),
        };
        let text = std::fs::read_to_string(path).map_err(|source| list.unreadable(source))?;
        list.parse(&text)?;
        Ok(list)
    }

    /// How many entries the file holds that name a certificate (`serial` or `spki-sha256`),
    /// read now. Behind a proxy none of them can match: the proxy forwards a node id, not a
    /// certificate.
    ///
    /// # Errors
    /// The file cannot be read or parsed.
    pub fn certificate_entries(&self) -> Result<usize, DenyListError> {
        let text = std::fs::read_to_string(&self.path).map_err(|source| self.unreadable(source))?;
        let denied = self.parse(&text)?;
        Ok(denied.serials.len() + denied.spkis.len())
    }

    /// What the file denies now: what the last read gave if the file's metadata is
    /// unchanged since, else what it holds now.
    ///
    /// # Errors
    /// The file cannot be read or parsed (no earlier read is used then).
    pub async fn current(&self) -> Result<Arc<Denied>, DenyListError> {
        let fail = |source| self.unreadable(source);
        let key = file_key(&tokio::fs::metadata(&self.path).await.map_err(fail)?);
        if let Some((seen, denied)) = &*self.lock()
            && *seen == key
        {
            return Ok(Arc::clone(denied));
        }
        // Read after the metadata: an edit in between is cached under the older key,
        // so the next check sees a key that differs and reads again.
        let read = tokio::fs::read_to_string(&self.path).await;
        let parsed = read.map_err(fail).and_then(|text| self.parse(&text));
        let mut last = self.lock();
        match parsed {
            Ok(denied) => {
                let denied = Arc::new(denied);
                *last = Some((key, Arc::clone(&denied)));
                Ok(denied)
            }
            Err(e) => {
                *last = None;
                Err(e)
            }
        }
    }

    /// Whether the certificate `peer` is denied now, for a caller that speaks for every
    /// name in it, as a storage node does when it registers and heartbeats
    /// ([`crate::members_rpc`]): its serial or key is listed, or any DNS name in it is a
    /// listed node.
    ///
    /// # Errors
    /// PERMISSION_DENIED: it is denied. UNAVAILABLE: the file cannot be read or parsed.
    pub async fn admit_cert(&self, peer: &PeerCert) -> Result<(), Status> {
        let denied = self.current().await.map_err(|e| unusable(&e))?;
        match denied.refuses_cert(peer) {
            Some(why) => Err(Status::permission_denied(format!("denied: {why}"))),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<LastRead>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn unreadable(&self, source: std::io::Error) -> DenyListError {
        DenyListError::Read {
            path: self.path.clone(),
            source,
        }
    }

    fn parse(&self, text: &str) -> Result<Denied, DenyListError> {
        Denied::parse(text).map_err(|(line, why)| DenyListError::Parse {
            path: self.path.clone(),
            line,
            why,
        })
    }
}

/// The entries of a deny list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Denied {
    serials: BTreeSet<String>,
    spkis: BTreeSet<String>,
    nodes: BTreeSet<String>,
}

impl Denied {
    /// Parses a deny list (format in the module docs).
    ///
    /// # Errors
    /// The 1-based number of the first bad line, and what is wrong with it.
    pub fn parse(text: &str) -> Result<Self, (usize, String)> {
        let mut denied = Self::default();
        for (at, line) in text.lines().enumerate() {
            match DenyEntry::parse_line(line).map_err(|e| (at + 1, e.to_string()))? {
                None => {}
                Some(DenyEntry::Serial(v)) => {
                    denied.serials.insert(v);
                }
                Some(DenyEntry::SpkiSha256(v)) => {
                    denied.spkis.insert(v);
                }
                Some(DenyEntry::Node(v)) => {
                    denied.nodes.insert(v);
                }
            }
        }
        Ok(denied)
    }

    /// The entries `rows` hold, as the matchers read them.
    #[must_use]
    pub fn from_entries(rows: Vec<DenyEntry>) -> Self {
        let mut denied = Self::default();
        for row in rows {
            match row {
                DenyEntry::Serial(v) => denied.serials.insert(v),
                DenyEntry::SpkiSha256(v) => denied.spkis.insert(v),
                DenyEntry::Node(v) => denied.nodes.insert(v),
            };
        }
        denied
    }

    /// Why a stream with certificate `peer` speaking as `node_id` is denied, if it is: a
    /// listed node is `node_id` up to ASCII case (a DNS name is, as for the storage nodes'
    /// map: [`kbf_peer::PeerNames::has_dns_name`]), then its serial and key.
    #[must_use]
    pub fn refuses(&self, peer: Option<&PeerCert>, node_id: &str) -> Option<String> {
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.eq_ignore_ascii_case(node_id))
        {
            return Some(format!("node {node}"));
        }
        self.refuses_key(peer?)
    }

    /// Why the certificate `peer` is denied, if it is, for a caller that speaks for every
    /// name in it: a listed node is any of its DNS names
    /// ([`kbf_peer::PeerNames::has_dns_name`], up to ASCII case), then its serial and key as
    /// in [`Denied::refuses`].
    #[must_use]
    pub fn refuses_cert(&self, peer: &PeerCert) -> Option<String> {
        let listed = self
            .nodes
            .iter()
            .find(|node| peer.identity.names().has_dns_name(node));
        match listed {
            Some(node) => Some(format!("node {node}")),
            None => self.refuses_key(peer),
        }
    }

    /// Why `peer` is denied by its serial or its key.
    fn refuses_key(&self, peer: &PeerCert) -> Option<String> {
        if self.serials.contains(peer.serial()) {
            return Some(format!("certificate serial {}", peer.serial()));
        }
        self.spkis
            .contains(peer.spki_sha256())
            .then(|| format!("public key {}", peer.spki_sha256()))
    }
}

/// The metadata a change of the deny list shows in: device, inode, size, mode, owner,
/// and modification and change times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    uid: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

#[cfg(unix)]
fn file_key(meta: &std::fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt;
    FileKey {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mode: meta.mode(),
        uid: meta.uid(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    }
}

/// Elsewhere than Unix the metadata carries none of these: every look is a change.
#[cfg(not(unix))]
fn file_key(_: &std::fs::Metadata) -> FileKey {
    FileKey {
        dev: 0,
        ino: 0,
        size: 0,
        mode: 0,
        uid: 0,
        mtime: (0, 0),
        ctime: (0, 0),
    }
}
