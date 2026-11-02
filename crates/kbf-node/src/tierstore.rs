//! `--tierstore-endpoint`: `kbf daemon` reads the blobs of its leases through a tierstore on
//! loopback first, and from the frontend when the tierstore does not serve them.
//!
//! A tierstore in front of the farm's object store keeps on the node's disk the objects read
//! through it (its `[local_cache]` mode). The packs are objects of that store, and the frontend's
//! pack index says where each blob lies. With `--tierstore-endpoint` the daemon's data plane is
//! wrapped in [`kbf_daemon::PackedCas`], as with `--pack-read`: a read asks the frontend where
//! the blob is (`kbf.worker.v1.PackIndex/Locate`, over the connection to `--cas`), reads that
//! range of that pack through the tierstore with a key that may only read, and checks it against
//! the digest. The frontend's CAS is not read for a blob the tierstore serves.
//!
//! **The frontend stays the source of truth.** Every read the tierstore does not serve goes to
//! the plane underneath (the frontend over `--cas`, or the storage nodes of the map it sends),
//! exactly as without the flag: a blob the index does not place, a frontend that serves no
//! locations, a pack the tierstore answers 404 for, bytes that are not the digest
//! ([`kbf_daemon::PackedCas::falling_back`]), a tierstore that is down or answers an error, and a
//! blob over 64 MiB (a read from a pack holds the blob whole). A dead tierstore never fails a
//! build: each read costs one refused connection on loopback and then goes to the frontend (the
//! client tries nothing twice); one that accepts and never answers holds a read for the request
//! timeout (60 s) first. Nothing checks the
//! tierstore at start for the same reason: one that is down at start is read around, and is used
//! once it is up.
//! Outputs are stored where they always were.
//!
//! **Loopback only.** The endpoint is `http://` to a loopback address (any `127.*` address or
//! `::1`, as an IP literal: a name is refused, because a name is not known to stay on the host),
//! and anything else is refused at start with [`NotLoopback`]: a tierstore elsewhere is a store
//! the daemon was not meant to read, over a network its key and bytes would cross in plain
//! text. The client ([`client::TierClient`]) connects to that address only: it takes no proxy
//! (from the environment or the system) and follows no redirect.
//!
//! **A warm read asks nothing of the frontend.** The places the frontend's index answered are
//! kept ([`placed::Placed`]), so a blob read through the tierstore once is found again without
//! a `Locate`, also while the frontend's index cannot be asked; a place whose pack turns out gone
//! or damaged is forgotten.
//!
//! The bucket (`--tierstore-bucket`, the tierstore's `server.bucket`) and the credentials file
//! (`--tierstore-credentials-file`, in the form of `--pack-store-credentials-file`, mode 0600 or
//! 0400) are required with the endpoint and refused without it; requests are signed for
//! `us-east-1`, the tierstore's default `server.region`. The flags are refused with `--pack-read`,
//! which reads the same packs from a store it names, and by every start but `kbf daemon`'s:
//! `kbf storage-and-daemon` and `kbf all-in-one` ([`TierstoreArgs::refuse`]). Left out, no store
//! is built or contacted and the daemon starts as it did before the flags existed.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use kbf_daemon::{MapHolder, PackCounter, PackedCas};
use kbf_objstore::ObjectStore;
use kbf_pack::{Reader, ReaderConfig};
use kbf_pack_index::PackIndex;
use kbf_transfer::Cas;

use super::pack_read::{self, CONNECT_TIMEOUT, DEFAULT_REGION, KeyFileError, REQUEST_TIMEOUT};
use super::{Cli, Error, NodeHandle, farm, start_over, transfer};

mod client;
mod placed;

use client::TierClient;
use placed::{Placed, TierReads};

/// The flag of the credentials file.
const CREDENTIALS_FLAG: &str = "--tierstore-credentials-file";

/// The `--tierstore-*` flags.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct TierstoreArgs {
    /// A tierstore on this host, as `http://<loopback IP>:<port>` (any `127.*` address or `[::1]`;
    /// a name such as `localhost` is refused), that `kbf daemon` reads the blobs of its leases
    /// through first: the daemon asks the frontend where a blob lies (over the connection to
    /// `--cas`), reads that range of that pack through the tierstore and checks it against the
    /// digest. Every blob the tierstore does not serve (not in a pack, a 404, bytes that are not
    /// the digest, a tierstore that is down or answers an error, a blob over 64 MiB) is read from
    /// the frontend as without this flag, so a dead tierstore slows reads and never fails a
    /// build. Outputs are stored where they were. An endpoint that is not on loopback is refused
    /// at start. Needs
    /// `--tierstore-bucket` and `--tierstore-credentials-file`; refused with `--pack-read`, and by
    /// `kbf storage-and-daemon` and `kbf all-in-one`. Left out, no tierstore is contacted.
    #[arg(
        long,
        value_name = "URL",
        requires_all = ["tierstore_bucket", "tierstore_credentials_file"],
        conflicts_with = "pack_read"
    )]
    pub tierstore_endpoint: Option<String>,
    /// The bucket the tierstore of `--tierstore-endpoint` serves (its `server.bucket`), under
    /// whose keys `cas/` the packs are. Needs `--tierstore-endpoint`.
    #[arg(long, value_name = "BUCKET", requires = "tierstore_endpoint")]
    pub tierstore_bucket: Option<String>,
    /// A file with a key pair the tierstore of `--tierstore-endpoint` takes, as JSON
    /// `{"access_key_id": "...", "secret_access_key": "..."}`, readable by its owner alone (mode
    /// 0600 or 0400): a read-only key of the tierstore. The secret is never a flag. Needs
    /// `--tierstore-endpoint`.
    #[arg(long, value_name = "PATH", requires = "tierstore_endpoint")]
    pub tierstore_credentials_file: Option<PathBuf>,
}

/// What the `--tierstore-*` flags ask for, once clap has checked them and the endpoint is on
/// loopback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tierstore {
    endpoint: Endpoint,
    bucket: String,
    credentials: PathBuf,
}

/// A tierstore's endpoint that is on loopback: the address the client connects to, and the
/// authority its requests name (`Host`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// The endpoint as given.
    given: String,
    /// The IP address and port, port 80 if none was given.
    addr: SocketAddr,
    /// The host and port as given, without the scheme and a last `/`.
    authority: String,
}

/// The refusal of a `--tierstore-endpoint` that is not `http://` to a loopback address.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "--tierstore-endpoint {endpoint:?} is not a tierstore on loopback: {why}; give \
     http://127.0.0.1:<port> or http://[::1]:<port>"
)]
pub(crate) struct NotLoopback {
    /// The endpoint as given.
    endpoint: String,
    /// What is wrong with it.
    why: &'static str,
}

/// Why the tierstore flags cannot be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TierstoreError {
    /// The endpoint is not `http://` to a loopback address and a port.
    #[error(transparent)]
    NotLoopback(#[from] NotLoopback),
    /// The flags were given to a start that is not `kbf daemon`'s.
    #[error("--tierstore-endpoint: only `kbf daemon` reads through a local tierstore, not {start}")]
    Unsupported {
        /// The start that refused them.
        start: &'static str,
    },
    /// The bucket is not one a tierstore serves.
    #[error(
        "--tierstore-bucket {0:?}: a bucket is 3 to 63 of a-z 0-9 . - and starts and ends with a \
         letter or digit"
    )]
    Bucket(String),
    /// The credentials file cannot be used.
    #[error(transparent)]
    KeyFile(#[from] KeyFileError),
    /// The HTTP client cannot be made.
    #[error("--tierstore-endpoint: the HTTP client cannot be made: {0}")]
    Client(#[source] reqwest::Error),
}

impl TierstoreArgs {
    /// What the flags ask for: `None` without `--tierstore-endpoint`. clap makes the bucket and
    /// the credentials file come with it.
    ///
    /// # Errors
    /// [`TierstoreError::NotLoopback`]: the endpoint is not `http://` to a loopback address and
    /// a port; [`TierstoreError::Bucket`]: the bucket is not one a tierstore serves.
    pub(crate) fn settings(&self) -> Result<Option<Tierstore>, TierstoreError> {
        let Some(endpoint) = &self.tierstore_endpoint else {
            return Ok(None);
        };
        let endpoint = loopback(endpoint)?;
        let bucket = self.tierstore_bucket.clone().unwrap_or_default();
        if !bucket_name(&bucket) {
            return Err(TierstoreError::Bucket(bucket));
        }
        Ok(Some(Tierstore {
            endpoint,
            bucket,
            credentials: self.tierstore_credentials_file.clone().unwrap_or_default(),
        }))
    }

    /// Refuses `--tierstore-endpoint` in a start that is not `kbf daemon`'s (`start` names it),
    /// which would otherwise ignore it.
    ///
    /// # Errors
    /// [`TierstoreError::Unsupported`]: `--tierstore-endpoint` was given.
    pub(crate) fn refuse(&self, start: &'static str) -> Result<(), TierstoreError> {
        if self.tierstore_endpoint.is_some() {
            return Err(TierstoreError::Unsupported { start });
        }
        Ok(())
    }
}

/// Whether `bucket` is a bucket name an S3 service takes: 3 to 63 of `a-z 0-9 . -`, starting and
/// ending with a letter or a digit.
fn bucket_name(bucket: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let inner = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-';
    (3..=63).contains(&bucket.len())
        && bucket.chars().all(inner)
        && edge(bucket.chars().next())
        && edge(bucket.chars().last())
}

/// `endpoint` as an [`Endpoint`], if it is `http://` to a loopback IP address and an optional
/// port of 1 to 65535, with nothing after the authority but an optional `/`.
///
/// # Errors
/// [`NotLoopback`], saying what is wrong.
fn loopback(endpoint: &str) -> Result<Endpoint, NotLoopback> {
    let refused = |why| NotLoopback {
        endpoint: endpoint.to_owned(),
        why,
    };
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| refused("not http://"))?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#', '@']) {
        return Err(refused("not a bare host and port"));
    }
    let bracketed = authority.starts_with('[');
    let (host, port) = match authority.strip_prefix('[') {
        Some(inside) => inside
            .split_once(']')
            .filter(|(_, port)| port.is_empty() || port.starts_with(':'))
            .map(|(host, port)| (host, port.strip_prefix(':')))
            .ok_or_else(|| refused("not a bare host and port"))?,
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if host.contains(':') && !bracketed {
        return Err(refused("an IPv6 address goes in brackets"));
    }
    let ip: IpAddr = host
        .parse()
        .map_err(|_| refused("the host is not an IP address"))?;
    if bracketed && ip.is_ipv4() {
        return Err(refused("an IPv4 address goes without brackets"));
    }
    if !ip.to_canonical().is_loopback() {
        return Err(refused("the address is not a loopback address"));
    }
    let port = match port {
        None => 80,
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| refused("the port is not a number from 1 to 65535"))?,
    };
    Ok(Endpoint {
        given: endpoint.to_owned(),
        addr: SocketAddr::new(ip, port),
        authority: authority.to_owned(),
    })
}

/// [`super::start`] for `--tierstore-endpoint`: the daemon's data plane is the farm's, wrapped
/// so that reads go through the tierstore first and fall back to it (the module docs).
pub(crate) fn start(cli: &Cli, settings: Tierstore) -> Result<NodeHandle, Error> {
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    let counter = PackCounter::default();
    let counted = counter.clone();
    let mut node = start_over(cli, Some(Arc::clone(&maps)), move |cli, liveness| {
        let (plane, _) = farm(cli, &held, liveness)?;
        let index: Arc<dyn PackIndex> = pack_read::index(cli, transfer::endpoint)?;
        let store = store(&settings)?;
        let line = format!(
            "data plane: blobs are read through the tierstore on loopback first (endpoint {}, \
             bucket {}), and from --cas or the storage nodes when it does not serve them; \
             outputs go where they went",
            settings.endpoint.given, settings.bucket
        );
        tracing::info!("{line}");
        Ok(over(plane, Arc::new(store), index, &counted))
    })?;
    node.daemon = node.daemon.with_pack_reads(counter);
    Ok(node.checking_reach(maps, None))
}

/// `plane`, its reads going first to the packs of `store` found through `index` (whose answers
/// are kept, [`Placed`]), falling back to `plane` on whatever the store does not serve, counting
/// into `counter`.
fn over<C: Cas, S: ObjectStore>(
    plane: Arc<C>,
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    counter: &PackCounter,
) -> Arc<PackedCas<C>> {
    over_placed(plane, store, Arc::new(Placed::new(index)), counter)
}

/// [`over`], with the places kept in `placed`.
fn over_placed<C: Cas, S: ObjectStore>(
    plane: Arc<C>,
    store: Arc<S>,
    placed: Arc<Placed>,
    counter: &PackCounter,
) -> Arc<PackedCas<C>> {
    let finding: Arc<dyn PackIndex> = placed.clone();
    let reader = Reader::new(store, finding, None, ReaderConfig::default());
    let reads = TierReads::new(reader, placed);
    let packed = PackedCas::with_counter(plane, Arc::new(reads), counter);
    Arc::new(packed.falling_back())
}

/// The client of the tierstore `settings` names, over its read-only key.
///
/// # Errors
/// [`TierstoreError::KeyFile`]: the credentials file cannot be used;
/// [`TierstoreError::Client`]: the HTTP client cannot be made.
fn store(settings: &Tierstore) -> Result<TierClient, TierstoreError> {
    let keys = pack_read::read_key_pair(&settings.credentials, CREDENTIALS_FLAG)?;
    let timeouts = (CONNECT_TIMEOUT, REQUEST_TIMEOUT);
    let client = TierClient::new(
        &settings.endpoint,
        settings.bucket.clone(),
        keys,
        DEFAULT_REGION,
        timeouts,
    );
    client.map_err(TierstoreError::Client)
}

#[cfg(test)]
mod tests;
