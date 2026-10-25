//! `--pack-read`: the daemon reads the blobs of its leases from packs first.
//!
//! A pack is one object in the farm's object store that holds many blobs, and the frontend's
//! pack index (`--pack-index`) says where each lies. With `--pack-read` the daemon's data
//! plane is wrapped in [`kbf_daemon::PackedCas`], whose reads ask the frontend where a blob is
//! (`kbf.worker.v1.PackIndex/Locate`), read that range of that pack from the object store of
//! `--pack-store-endpoint` with a key that may only read, and check it against the digest. A
//! blob the frontend does not locate, and a frontend that serves no locations (it has no
//! `--pack-index`), leave the read where it was. Everything that is not a read is the plane's
//! own, and **the daemon still uploads its outputs to the storage members**.
//!
//! Two daemons read packs, and they differ in the plane under the packs and in the connection
//! that asks for locations:
//!
//! - **`kbf daemon`** ([`start`]): the plane is [`kbf_daemon::FarmCas`], and `Locate` goes over
//!   the connection to `--cas`.
//! - **The daemon half of `kbf storage-and-daemon` with a `--frontend`** ([`start_member`]):
//!   the plane is the node's member plane (the blobs of the local member in-process, the
//!   others over TLS, by the map the frontend sends), and `Locate` goes over a connection to
//!   the first `--server`, the frontend's worker listener that the session is made to. It
//!   presents the certificate the session presents (`--cert`, `--key`), whose role is the
//!   daemon's, the only role the listener lets call `Locate`; the node's storage half holds
//!   a certificate of its own, with the storage role, and does not make this call.
//!
//! Off by default: without `--pack-read` no object store is built or contacted and the plane is
//! the one that was. The endpoint, the bucket and the credentials file are required with it and
//! refused without it (clap checks both, in `kbf.toml` too, which becomes flags).
//!
//! The credentials are a file and never a flag, as a command line is world-readable: JSON with
//! `access_key_id` and `secret_access_key`, readable by its owner alone (mode 0600 or 0400).
//! No message carries the secret.
//!
//! The daemon half of `kbf all-in-one`, and that of `kbf storage-and-daemon` without a
//! `--frontend`, read blobs in-process from a store that is the whole farm: there is no
//! frontend to ask where a blob lies, and no pack index that holds it. They refuse the flag
//! ([`PackReadArgs::refuse_in_process`]) rather than ignore it.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{LocalMember, MapHolder, PackedCas, RemoteIndex};
use kbf_objstore::Capabilities;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_pack::{ReadCache, Reader, ReaderConfig};
use kbf_transfer::Cas;
use tonic::transport::Endpoint;
use zeroize::Zeroizing;

use super::{Cli, Error, NodeHandle, farm, member, start_over, transfer};

/// What makes the endpoint of the connection that asks the frontend where blobs lie.
type Locate = fn(&Cli) -> Result<Endpoint, Error>;

/// The signing region when `--pack-store-region` is left out: the one S3-compatible stores
/// accept unless they are configured to another.
const DEFAULT_REGION: &str = "us-east-1";

/// How long opening a connection to the object store may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request to the object store may go without an answer or a byte of one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What a credentials file that is not JSON of the key pair is told.
const NOT_KEYS: &str = "not JSON with access_key_id and secret_access_key";

/// What a daemon that reads blobs in-process is told of `--pack-read`.
const IN_PROCESS: &str =
    "--pack-read: no frontend to ask where blobs lie (needs `kbf storage-and-daemon --frontend`)";

/// The `--pack-read` flags.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct PackReadArgs {
    /// Reads the blobs of a lease from the packs of the farm's object store before the storage
    /// nodes: the daemon asks the frontend where a blob lies (over the connection to `--cas`;
    /// for `kbf storage-and-daemon`, which has none, over one to the first `--server`), reads
    /// that range of that pack and checks it against the digest. A blob the frontend does not
    /// locate, and a frontend that serves no locations (it was started without
    /// `--pack-index`), leave the read where it was, and outputs are stored where they were.
    /// A pack that is gone, or whose bytes are not the digest, fails the read. Needs
    /// `--pack-store-endpoint`, `--pack-store-bucket` and `--pack-store-credentials-file`.
    /// Refused by `kbf all-in-one` and by `kbf storage-and-daemon` started without
    /// `--frontend`, whose daemon half reads blobs in-process. Left out, no object store is
    /// built or contacted and every blob is read as it always was.
    #[arg(
        long,
        requires_all = ["pack_store_endpoint", "pack_store_bucket", "pack_store_credentials_file"]
    )]
    pub pack_read: bool,
    /// The S3-compatible object store the packs are kept in, as `http://host:port`. The daemon
    /// only reads from it. Needs `--pack-read`.
    #[arg(long, value_name = "URL", requires = "pack_read")]
    pub pack_store_endpoint: Option<String>,
    /// The bucket of `--pack-store-endpoint` that holds the packs, under the keys `cas/`. Needs
    /// `--pack-read`.
    #[arg(long, value_name = "BUCKET", requires = "pack_read")]
    pub pack_store_bucket: Option<String>,
    /// The region the requests to `--pack-store-endpoint` are signed for; `us-east-1` if left
    /// out.
    #[arg(
        long,
        value_name = "REGION",
        default_value = DEFAULT_REGION,
        hide_default_value = true
    )]
    pub pack_store_region: String,
    /// A file with the key pair for `--pack-store-endpoint`, as JSON `{"access_key_id": "...",
    /// "secret_access_key": "..."}`, readable by its owner alone (mode 0600 or 0400): a key that
    /// may read the objects under `cas/` and write none. The secret is never a flag: a command
    /// line is world-readable. Needs `--pack-read`.
    #[arg(long, value_name = "PATH", requires = "pack_read")]
    pub pack_store_credentials_file: Option<PathBuf>,
    /// A directory this process owns where the blobs read from packs are kept, one file each,
    /// checked by digest on every read; the least recently used are removed to stay within
    /// `--pack-read-cache-bytes`. The daemon's input cache already keeps blobs by digest, so
    /// left out nothing more is kept. Needs `--pack-read-cache-bytes`.
    #[arg(
        long,
        value_name = "DIR",
        requires_all = ["pack_read_cache_bytes", "pack_read"]
    )]
    pub pack_read_cache_dir: Option<PathBuf>,
    /// The most bytes of blobs `--pack-read-cache-dir` holds.
    #[arg(
        long,
        value_name = "BYTES",
        value_parser = clap::value_parser!(u64).range(1..),
        requires = "pack_read_cache_dir"
    )]
    pub pack_read_cache_bytes: Option<u64>,
}

/// What the `--pack-read` flags ask for, once clap has checked them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PackRead {
    endpoint: String,
    bucket: String,
    region: String,
    credentials: PathBuf,
    /// `--pack-read-cache-dir` and `--pack-read-cache-bytes`, both or neither.
    cache: Option<(PathBuf, u64)>,
}

impl PackReadArgs {
    /// What the flags ask for: `None` unless `--pack-read` is given. clap makes the endpoint,
    /// the bucket and the credentials file come with it, and the two cache flags come together.
    pub(crate) fn settings(&self) -> Option<PackRead> {
        if !self.pack_read {
            return None;
        }
        let credentials = self.pack_store_credentials_file.clone();
        let cache_dir = self.pack_read_cache_dir.clone();
        Some(PackRead {
            endpoint: self.pack_store_endpoint.clone().unwrap_or_default(),
            bucket: self.pack_store_bucket.clone().unwrap_or_default(),
            region: self.pack_store_region.clone(),
            credentials: credentials.unwrap_or_default(),
            cache: cache_dir.zip(self.pack_read_cache_bytes),
        })
    }

    /// Refuses `--pack-read` in a daemon that reads blobs in-process, which would otherwise
    /// ignore it.
    ///
    /// # Errors
    /// `--pack-read` was given.
    pub(crate) fn refuse_in_process(&self) -> Result<(), Error> {
        if self.pack_read {
            return Err(IN_PROCESS.into());
        }
        Ok(())
    }
}

/// [`super::start`] for `--pack-read`: the daemon's data plane is the farm's, wrapped so that
/// reads ask the packs first (the module docs).
pub(crate) fn start(cli: &Cli, settings: PackRead) -> Result<NodeHandle, Error> {
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    let node = start_over(cli, Some(Arc::clone(&maps)), move |cli, liveness| {
        let plane = farm(cli, &held, liveness);
        plane.and_then(|(plane, _)| over(cli, &settings, plane, transfer::endpoint))
    })?;
    Ok(node.checking_reach(maps, None))
}

/// [`super::start_member`] for `--pack-read`: the daemon half's data plane is the node's member
/// plane, wrapped so that reads ask the packs first (the module docs). The frontend is asked
/// over a connection to the first `--server`.
pub(crate) fn start_member(
    cli: &Cli,
    local: LocalMember,
    settings: PackRead,
) -> Result<NodeHandle, Error> {
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    let own = local.addr().to_owned();
    let node = start_over(cli, Some(Arc::clone(&maps)), move |cli, liveness| {
        let plane = member(cli, &held, &local, liveness);
        plane.and_then(|plane| over(cli, &settings, plane, transfer::server_endpoint))
    })?;
    Ok(node.checking_reach(maps, Some(own)))
}

/// `plane`, its reads going to the packs `settings` names first, the frontend asked over the
/// connection `locate` makes.
///
/// # Errors
/// The credentials file, the endpoint, the bucket, the read cache's directory or the TLS
/// files of the connection to the frontend cannot be used.
fn over<C: Cas>(
    cli: &Cli,
    settings: &PackRead,
    plane: Arc<C>,
    locate: Locate,
) -> Result<Arc<PackedCas<C>>, Error> {
    pack_reader(cli, settings, locate).map(|packs| Arc::new(PackedCas::new(plane, packs)))
}

/// The reader of the packs of `settings`, logged as the one the plane reads with. Not generic,
/// so that the data planes that read packs share these lines.
fn pack_reader(
    cli: &Cli,
    settings: &PackRead,
    locate: Locate,
) -> Result<Arc<Reader<S3Store>>, Error> {
    let reader = reader(cli, settings, locate)?;
    // Built before the macro, which evaluates its fields only when a subscriber takes the line.
    let line = format!(
        "data plane: blobs are read from packs first (endpoint {}, bucket {}); outputs go to \
         the storage nodes",
        settings.endpoint, settings.bucket
    );
    tracing::info!("{line}");
    Ok(Arc::new(reader))
}

/// The reader of the packs: the object store of `settings` over a read-only key, the frontend's
/// index over the connection `locate` makes (lazily, on the current runtime), and the cache if
/// asked.
fn reader(cli: &Cli, settings: &PackRead, locate: Locate) -> Result<Reader<S3Store>, Error> {
    let credentials = read_credentials(&settings.credentials)?;
    let config = S3Config {
        endpoint: settings.endpoint.clone(),
        region: settings.region.clone(),
        bucket: settings.bucket.clone(),
        credentials,
        capabilities: Capabilities::default(),
        connect_timeout: CONNECT_TIMEOUT,
        request_timeout: REQUEST_TIMEOUT,
        min_body_rate: None,
    };
    let built = S3Store::new(config);
    let store = built.map_err(|e| said("--pack-store-endpoint", e))?;
    let channel = locate(cli)?.connect_lazy();
    let index = RemoteIndex::new(channel, tokio::runtime::Handle::current());
    let cache = cache(settings)?;
    Ok(Reader::new(
        Arc::new(store),
        Arc::new(index),
        cache,
        ReaderConfig::default(),
    ))
}

/// The read cache `settings` names, opened, if it names one.
fn cache(settings: &PackRead) -> Result<Option<Arc<ReadCache>>, Error> {
    let Some((dir, bytes)) = &settings.cache else {
        return Ok(None);
    };
    let opened = ReadCache::open(dir.clone(), *bytes);
    let cache = opened.map_err(|e| said("--pack-read-cache-dir", e))?;
    Ok(Some(Arc::new(cache)))
}

/// `error`, said as the failure of `flag`.
fn said(flag: &str, error: impl std::fmt::Display) -> String {
    format!("{flag}: {error}")
}

/// The key pair in the file at `path`, which only its owner may be able to read. No message
/// carries the secret.
fn read_credentials(path: &Path) -> Result<Credentials, Error> {
    let bad = |why: &str| format!("--pack-store-credentials-file {}: {why}", path.display());
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.metadata().and_then(|meta| {
            let mut text = String::new();
            let read = file.read_to_string(&mut text);
            read.map(|_| (meta.permissions().mode(), text))
        })
    });
    let (mode, text) = read.map_err(|e| bad(&e.to_string()))?;
    // Only the text is wiped when it goes: the parsed pair is the object-store client's, for as
    // long as the process runs.
    let text = Zeroizing::new(text);
    if mode & 0o077 != 0 {
        let shown = mode & 0o777;
        let why = format!("mode {shown:04o} lets others read a secret: use 0600");
        return Err(bad(&why).into());
    }
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|_| bad(NOT_KEYS))?;
    let field = |name: &str| parsed.get(name).and_then(serde_json::Value::as_str);
    let (Some(id), Some(secret)) = (field("access_key_id"), field("secret_access_key")) else {
        return Err(bad(NOT_KEYS).into());
    };
    Ok(Credentials::new(id, secret))
}

#[cfg(test)]
mod tests;
