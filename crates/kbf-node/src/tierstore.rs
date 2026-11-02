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
//! build: each read costs the S3 client's three tries of a refused connection on loopback (about
//! 150 ms of back-off) and then goes to the frontend; one that accepts and never answers holds a
//! read for the client's request timeout (60 s, not tried again) first. Nothing checks the
//! tierstore at start for the same reason: one that is down at start is read around, and is used
//! once it is up.
//! Outputs are stored where they always were.
//!
//! **Loopback only.** The endpoint is `http://` to a loopback address (any `127.*` address or
//! `::1`, as an IP literal: a name is refused, because a name is not known to stay on the host),
//! and anything else is refused at start with [`NotLoopback`]: a tierstore elsewhere is a store
//! the daemon was not meant to read, over a network its key and bytes would cross in plain
//! text.
//!
//! The bucket (`--tierstore-bucket`, the tierstore's `server.bucket`) and the credentials file
//! (`--tierstore-credentials-file`, in the form of `--pack-store-credentials-file`, mode 0600 or
//! 0400) are required with the endpoint and refused without it; requests are signed for
//! `us-east-1`, the tierstore's default `server.region`. The flags are refused with `--pack-read`,
//! which reads the same packs from a store it names, and by every start but `kbf daemon`'s:
//! `kbf storage-and-daemon` and `kbf all-in-one` ([`TierstoreArgs::refuse`]). Left out, no store
//! is built or contacted and the daemon starts as it did before the flags existed.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_daemon::{MapHolder, PackCounter, PackedCas};
use kbf_objstore::ObjectStore;
use kbf_objstore::s3::{S3Config, S3ConfigError, S3Store};
use kbf_pack::{Reader, ReaderConfig};
use kbf_pack_index::PackIndex;
use kbf_transfer::Cas;

use super::pack_read::{self, CONNECT_TIMEOUT, DEFAULT_REGION, REQUEST_TIMEOUT};
use super::{Cli, Error, NodeHandle, farm, start_over, transfer};

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
    endpoint: String,
    bucket: String,
    credentials: PathBuf,
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

impl TierstoreArgs {
    /// What the flags ask for: `None` without `--tierstore-endpoint`. clap makes the bucket and
    /// the credentials file come with it.
    ///
    /// # Errors
    /// [`NotLoopback`]: the endpoint is not `http://` to a loopback address.
    pub(crate) fn settings(&self) -> Result<Option<Tierstore>, NotLoopback> {
        let Some(endpoint) = &self.tierstore_endpoint else {
            return Ok(None);
        };
        loopback(endpoint)?;
        Ok(Some(Tierstore {
            endpoint: endpoint.clone(),
            bucket: self.tierstore_bucket.clone().unwrap_or_default(),
            credentials: self.tierstore_credentials_file.clone().unwrap_or_default(),
        }))
    }

    /// Refuses `--tierstore-endpoint` in a start that is not `kbf daemon`'s (`start` names it),
    /// which would otherwise ignore it.
    ///
    /// # Errors
    /// `--tierstore-endpoint` was given.
    pub(crate) fn refuse(&self, start: &str) -> Result<(), Error> {
        if self.tierstore_endpoint.is_some() {
            let said = format!(
                "--tierstore-endpoint: only `kbf daemon` reads through a local tierstore, not \
                 {start}"
            );
            return Err(said.into());
        }
        Ok(())
    }
}

/// Whether `endpoint` is `http://` to a loopback IP address, with nothing after the authority
/// but an optional `/`.
///
/// # Errors
/// [`NotLoopback`], saying what is wrong.
fn loopback(endpoint: &str) -> Result<(), NotLoopback> {
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
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed
            .split_once(']')
            .filter(|(_, port)| port.is_empty() || port.starts_with(':'))
            .map(|(host, _)| host)
            .ok_or_else(|| refused("not a bare host and port"))?,
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    if host.contains(':') && !authority.starts_with('[') {
        return Err(refused("an IPv6 address goes in brackets"));
    }
    let ip: IpAddr = host
        .parse()
        .map_err(|_| refused("the host is not an IP address"))?;
    if authority.starts_with('[') && ip.is_ipv4() {
        return Err(refused("an IPv4 address goes without brackets"));
    }
    if !ip.to_canonical().is_loopback() {
        return Err(refused("the address is not a loopback address"));
    }
    Ok(())
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
            settings.endpoint, settings.bucket
        );
        tracing::info!("{line}");
        Ok(over(plane, Arc::new(store), index, &counted))
    })?;
    node.daemon = node.daemon.with_pack_reads(counter);
    Ok(node.checking_reach(maps, None))
}

/// `plane`, its reads going first to the packs of `store` found through `index`, falling back to
/// `plane` on whatever the store does not serve, counting into `counter`.
fn over<C: Cas, S: ObjectStore>(
    plane: Arc<C>,
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    counter: &PackCounter,
) -> Arc<PackedCas<C>> {
    let reader = Reader::new(store, index, None, ReaderConfig::default());
    let packed = PackedCas::with_counter(plane, Arc::new(reader), counter);
    Arc::new(packed.falling_back())
}

/// The S3 client of the tierstore `settings` names, over its read-only key.
///
/// # Errors
/// The credentials file cannot be used, or the endpoint or the bucket is not one the client
/// takes.
fn store(settings: &Tierstore) -> Result<S3Store, Error> {
    let flag = "--tierstore-credentials-file";
    let credentials = pack_read::read_credentials(&settings.credentials, flag)?;
    let config = S3Config {
        endpoint: settings.endpoint.clone(),
        region: DEFAULT_REGION.to_owned(),
        bucket: settings.bucket.clone(),
        credentials,
        capabilities: kbf_objstore::Capabilities::default(),
        connect_timeout: CONNECT_TIMEOUT,
        request_timeout: REQUEST_TIMEOUT,
        min_body_rate: None,
    };
    let built = S3Store::new(config);
    Ok(built.map_err(|e| match e {
        S3ConfigError::Bucket(_) => format!("--tierstore-bucket: {e}"),
        _ => format!("--tierstore-endpoint: {e}"),
    })?)
}

#[cfg(test)]
mod tests;
