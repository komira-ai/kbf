//! The `linux` kit's settings: its closed key set, the defaults, and the checks every
//! value passes before it reaches a unit.

use std::path::Path;

use kbf_types::{ImageError, ImageRef};

use crate::config::{Config, ConfigError};
use kbf_host::{HostPath, PathError};

/// The keys `templates/linux/config.example` documents; any other is refused.
pub const KEYS: &[&str] = &[
    "KBF_HOST_NAME",
    "KBF_LISTEN_ADDRESS",
    "KBF_NODE_ID",
    "KBF_REAPI_PORT",
    "KBF_WORKER_PORT",
    "KBF_API_PORT",
    "KBF_RAM_GIB",
    "MINIO_RESERVATION_GIB",
    "KBF_SERVER_RESERVATION_GIB",
    "KBF_OS_HEADROOM_GIB",
    "KBF_DAEMON_MEMORY_MIN_MIB",
    "MINIO_UNIT",
    "MINIO_S3_ENDPOINT",
    "KBF_S3_BUCKET",
    "KBF_META_LOG",
    "KBF_RAFT_DIR",
    "KBF_SCRATCH_DIR",
    "KBF_IMAGES",
    "KBF_USER",
    "KBF_SERVER_USER",
    "KBF_ETC",
    "KBF_BIN_DIR",
    "KBF_SWAP_FILE",
    "KBF_SWAP_SIZE",
];

/// The least RAM, in GiB, left to the kernel, systemd and the daemon.
const MIN_OS_HEADROOM_GIB: u64 = 4;

/// How kbf-server keeps its metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaLog {
    /// A single-voter Raft log on local disk (`--meta-log=raft --raft-dir=...`).
    Raft,
    /// The index in memory (no `--meta-log` flag).
    Memory,
}

/// The settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxConfig {
    pub host_name: String,
    pub listen_address: String,
    pub node_id: String,
    pub reapi_port: u16,
    pub worker_port: u16,
    pub api_port: u16,
    pub ram_gib: u64,
    pub minio_reservation_gib: u64,
    pub server_reservation_gib: u64,
    pub os_headroom_gib: u64,
    pub daemon_memory_min_mib: u64,
    /// RAM minus what MinIO, kbf-server and the OS keep: the daemon's
    /// `--actions-memory-max-gib` (docs/deploy/storage-node.md, "Memory on a converged
    /// host"). At least 1.
    pub actions_gib: u64,
    pub minio_unit: String,
    pub minio_s3_endpoint: String,
    pub s3_bucket: String,
    pub meta_log: MetaLog,
    pub raft_dir: HostPath,
    pub scratch_dir: HostPath,
    /// The pinned images, in podman's bare `<repo>@sha256:<digest>` form.
    pub images: Vec<ImageRef>,
    pub user: String,
    pub server_user: String,
    pub etc: HostPath,
    pub bin_dir: HostPath,
    pub swap_file: HostPath,
    pub swap_size: String,
}

/// Why the settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum LinuxError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{key} holds a character the units cannot carry: {value}")]
    Chars { key: &'static str, value: String },
    #[error("{key} must be a whole number, not '{value}'")]
    NotNumber { key: &'static str, value: String },
    #[error("{key} must be a port number from 1 to 65535, not '{value}'")]
    NotPort { key: &'static str, value: String },
    #[error("{key}: {source}")]
    Path {
        key: &'static str,
        source: PathError,
    },
    #[error("KBF_LISTEN_ADDRESS={0}: bind the host's pinned address, not every interface")]
    AnyAddress(String),
    #[error("MINIO_UNIT={0} is not a .service unit name")]
    MinioUnit(String),
    #[error("KBF_META_LOG={0}: raft or memory")]
    MetaLog(String),
    #[error("KBF_OS_HEADROOM_GIB={0}: keep at least {MIN_OS_HEADROOM_GIB} GiB for the OS")]
    Headroom(u64),
    #[error(
        "KBF_RAM_GIB={ram} leaves {left} GiB for builds after MinIO {minio}, kbf-server {server} and the OS {os}"
    )]
    NoRoom {
        ram: u64,
        minio: u64,
        server: u64,
        os: u64,
        left: i128,
    },
    #[error(
        "image '{0}': write the bare <repo>@sha256:<digest> (podman's form); render-units adds docker:// for kbf-daemon"
    )]
    ImageScheme(String),
    #[error(transparent)]
    Image(#[from] ImageError),
}

impl LinuxConfig {
    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<LinuxConfig, LinuxError> {
        LinuxConfig::from_config(&Config::load(path, KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<LinuxConfig, LinuxError> {
        LinuxConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<LinuxConfig, LinuxError> {
        for &key in KEYS {
            if let Some(value) = c.get(key) {
                plain(key, value)?;
            }
        }
        // An unset or empty key takes its default.
        let or = |key: &'static str, default: &str| -> String {
            c.get(key)
                .filter(|v| !v.is_empty())
                .unwrap_or(default)
                .to_owned()
        };
        let number = |key: &'static str, default: &str| whole(key, &or(key, default));
        let port = |key: &'static str, default: &str| port(key, &or(key, default));
        let path = |key: &'static str, default: &str| {
            HostPath::new(&or(key, default)).map_err(|source| LinuxError::Path { key, source })
        };

        let listen_address = c.require("KBF_LISTEN_ADDRESS")?.to_owned();
        if matches!(listen_address.as_str(), "0.0.0.0" | "::") {
            return Err(LinuxError::AnyAddress(listen_address));
        }
        let minio_unit = or("MINIO_UNIT", "minio.service");
        if minio_unit
            .strip_suffix(".service")
            .is_none_or(str::is_empty)
        {
            return Err(LinuxError::MinioUnit(minio_unit));
        }
        let meta_log = match or("KBF_META_LOG", "raft").as_str() {
            "raft" => MetaLog::Raft,
            "memory" => MetaLog::Memory,
            other => return Err(LinuxError::MetaLog(other.to_owned())),
        };
        let ram_gib = whole("KBF_RAM_GIB", c.require("KBF_RAM_GIB")?)?;
        let minio = number("MINIO_RESERVATION_GIB", "32")?;
        let server = number("KBF_SERVER_RESERVATION_GIB", "16")?;
        let os = number("KBF_OS_HEADROOM_GIB", "8")?;
        if os < MIN_OS_HEADROOM_GIB {
            return Err(LinuxError::Headroom(os));
        }
        let left = i128::from(ram_gib) - i128::from(minio) - i128::from(server) - i128::from(os);
        let actions_gib =
            u64::try_from(left)
                .ok()
                .filter(|&n| n >= 1)
                .ok_or(LinuxError::NoRoom {
                    ram: ram_gib,
                    minio,
                    server,
                    os,
                    left,
                })?;
        let images = or("KBF_IMAGES", "")
            .split_whitespace()
            .map(image)
            .collect::<Result<_, _>>()?;
        Ok(LinuxConfig {
            host_name: c.require("KBF_HOST_NAME")?.to_owned(),
            listen_address,
            node_id: c.require("KBF_NODE_ID")?.to_owned(),
            reapi_port: port("KBF_REAPI_PORT", "8980")?,
            worker_port: port("KBF_WORKER_PORT", "8981")?,
            api_port: port("KBF_API_PORT", "8982")?,
            ram_gib,
            minio_reservation_gib: minio,
            server_reservation_gib: server,
            os_headroom_gib: os,
            daemon_memory_min_mib: number("KBF_DAEMON_MEMORY_MIN_MIB", "256")?,
            actions_gib,
            minio_unit,
            minio_s3_endpoint: or("MINIO_S3_ENDPOINT", "http://127.0.0.1:9000"),
            s3_bucket: or("KBF_S3_BUCKET", "kbf-cas"),
            meta_log,
            raft_dir: path("KBF_RAFT_DIR", "/var/lib/kbf-server/raft")?,
            scratch_dir: path("KBF_SCRATCH_DIR", "/var/lib/kbf/leases")?,
            images,
            user: or("KBF_USER", "kbf"),
            server_user: or("KBF_SERVER_USER", "kbf-server"),
            etc: path("KBF_ETC", "/etc/kbf")?,
            bin_dir: path("KBF_BIN_DIR", "/usr/local/bin")?,
            swap_file: path("KBF_SWAP_FILE", "/swapfile")?,
            swap_size: or("KBF_SWAP_SIZE", "128G"),
        })
    }

    /// The listen address as the host part of a `host:port`: an IPv6 literal goes in
    /// brackets.
    pub fn listen_host(&self) -> String {
        if self.listen_address.contains(':') {
            format!("[{}]", self.listen_address)
        } else {
            self.listen_address.clone()
        }
    }
}

/// Refuse a value a rendered unit cannot carry as written: only letters, digits, spaces
/// and `. _ : / = , + @ -`. That leaves out `%` (systemd expands `%` specifiers), `$`
/// (it expands variables in `ExecStart=`), `\`, quotes, `;` and newlines.
fn plain(key: &'static str, value: &str) -> Result<(), LinuxError> {
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " ._:/=,+@-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(LinuxError::Chars {
            key,
            value: value.to_owned(),
        })
    }
}

fn whole(key: &'static str, value: &str) -> Result<u64, LinuxError> {
    value
        .parse()
        .ok()
        .filter(|_| value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(LinuxError::NotNumber {
            key,
            value: value.to_owned(),
        })
}

fn port(key: &'static str, value: &str) -> Result<u16, LinuxError> {
    value
        .parse()
        .ok()
        .filter(|&p| p != 0 && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(LinuxError::NotPort {
            key,
            value: value.to_owned(),
        })
}

/// A `KBF_IMAGES` entry: podman's bare `<repo>@sha256:<digest>`, which must pass
/// kbf-daemon's own `--image` parser once `docker://` is put in front.
pub fn image(entry: &str) -> Result<ImageRef, LinuxError> {
    if entry.starts_with("docker://") {
        return Err(LinuxError::ImageScheme(entry.to_owned()));
    }
    Ok(ImageRef::parse(&format!("docker://{entry}"))?)
}
