//! The S3 front of a start, whatever it fronts: the settings of the address, the names, the key
//! files and the limits, and the server settings made of them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::Clock;

use super::ServerConfig;
use super::cli::{Cli, Error};
use super::gc::{ensure_distinct as gc_distinct, gc_keys, ids as gc_ids};
use super::grant::{ensure_distinct as grant_distinct, grant_keys, ids as grant_ids};
use super::keys::{ensure_distinct, read_credentials, read_only_keys};
use super::presign::{ensure_distinct as presign_distinct, presign_keys};
use super::scoped::{ensure_distinct as scoped_distinct, ids as scoped_ids, scoped_keys};

/// Flags that cannot go together, which only the files they name show: the process exits with
/// status 2, as for flags the parser refused, since a restart cannot mend them.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct InvalidFlags(pub String);

/// The access key id and the secret access key in the file of `--access-key-file`.
pub(super) fn read_key(path: &Path) -> Result<(String, String), String> {
    read_credentials("--access-key-file", path)
}

/// What the S3 front is made of, which a storage node and a build node both are: the address,
/// the names, the key files and the limits.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct FrontSettings {
    pub listen: SocketAddr,
    pub proxy_id: String,
    pub bucket: String,
    pub region: String,
    pub access_key_file: PathBuf,
    pub read_only_key_file: Vec<PathBuf>,
    pub presign_key_file: Vec<PathBuf>,
    pub gc_key_file: Vec<PathBuf>,
    pub scoped_key_file: Vec<PathBuf>,
    pub grant_key_file: Vec<PathBuf>,
    pub presign_max_expires: Duration,
    pub max_request_bytes: u64,
    pub max_concurrent_puts: u32,
    pub put_min_bytes_per_sec: u64,
    pub put_rate_grace: Duration,
    pub body_idle_timeout: Duration,
    pub drain_lb_wait: Duration,
    pub drain_deadline: Duration,
}

impl Cli {
    /// The settings of the S3 front.
    pub(super) fn front(&self) -> FrontSettings {
        FrontSettings {
            listen: self.listen,
            proxy_id: self.proxy_id.clone(),
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            access_key_file: self.access_key_file.clone(),
            read_only_key_file: self.read_only_key_file.clone(),
            presign_key_file: self.presign_key_file.clone(),
            gc_key_file: self.gc_key_file.clone(),
            scoped_key_file: self.scoped_key_file.clone(),
            grant_key_file: self.grant_key_file.clone(),
            presign_max_expires: self.presign_max_expires,
            max_request_bytes: self.max_request_bytes,
            max_concurrent_puts: self.max_concurrent_puts,
            put_min_bytes_per_sec: self.put_min_bytes_per_sec,
            put_rate_grace: self.put_rate_grace,
            body_idle_timeout: self.body_idle_timeout,
            drain_lb_wait: self.drain_lb_wait,
            drain_deadline: self.drain_deadline,
        }
    }
}

/// The settings of the server for `front`: reads its key files (which must differ in id), and
/// refuses a bucket that is the name of a probe path and a body timeout of zero.
pub(super) fn server_config(
    front: &FrontSettings,
    clock: Arc<dyn Clock>,
) -> Result<ServerConfig, Error> {
    if matches!(
        front.bucket.as_str(),
        "healthz" | "readyz" | "usagez" | "metricz"
    ) {
        let name = &front.bucket;
        return Err(format!("--bucket {name:?} is the name of a probe path").into());
    }
    if front.body_idle_timeout.is_zero() {
        return Err("--body-idle-timeout must be longer than zero".into());
    }
    let (key_id, secret) = read_key(&front.access_key_file)?;
    let read_only = read_only_keys(&front.read_only_key_file)?;
    ensure_distinct(&key_id, &read_only).map_err(InvalidFlags)?;
    let presign = presign_keys(&front.presign_key_file)?;
    let gc = gc_keys(&front.gc_key_file)?;
    let scoped = scoped_keys(&front.scoped_key_file)?;
    let grant = grant_keys(&front.grant_key_file)?;
    let mut others: Vec<&str> = std::iter::once(key_id.as_str())
        .chain(read_only.iter().map(|key| key.id.as_str()))
        .collect();
    gc_distinct(&others, &gc).map_err(InvalidFlags)?;
    others.extend(gc_ids(&gc));
    scoped_distinct(&others, &scoped).map_err(InvalidFlags)?;
    others.extend(scoped_ids(&scoped));
    grant_distinct(&others, &grant).map_err(InvalidFlags)?;
    others.extend(grant_ids(&grant));
    presign_distinct(&others, &presign).map_err(InvalidFlags)?;
    let mut server = ServerConfig::new(&front.bucket, &key_id, &secret, clock);
    server.read_only_keys = read_only;
    server.presign_keys = presign;
    server.gc_keys = gc;
    server.scoped_keys = scoped;
    server.grant_keys = grant;
    server.presign_max_expires = front.presign_max_expires;
    server.region = front.region.clone();
    server.max_request_bytes = usize::try_from(front.max_request_bytes).unwrap_or(usize::MAX);
    server.body_idle_timeout = front.body_idle_timeout;
    server.max_concurrent_puts = front.max_concurrent_puts as usize;
    server.put_min_bytes_per_sec = front.put_min_bytes_per_sec;
    server.put_rate_grace = front.put_rate_grace;
    server.drain_lb_wait = front.drain_lb_wait;
    server.drain_deadline = front.drain_deadline;
    Ok(server)
}
