//! The `pilot` kit's settings: the kit's closed key set, the defaults, and the checks
//! each value passes before it reaches a unit or the overlay.
//!
//! Each verb reads only its own keys and refuses only those: `server-install` reads
//! the kbf-server keys ([`ServerSettings`]), `buckconfig` the buck2 client keys
//! ([`OverlaySettings`]). The other keys of [`KEYS`] belong to the kit's other steps;
//! they are accepted so one file serves every step, and read by neither verb.

use std::net::Ipv4Addr;

use crate::config::Config;
use kbf_host::HostPath;

use super::PilotError;

/// The keys `templates/pilot/config.example` documents, and the ones it names as the
/// kit's other steps'; any other is refused.
pub const KEYS: &[&str] = &[
    "KBF_SERVER_HOST",
    "KBF_SERVER_BIND",
    "KBF_TAILNET_IFACE",
    "KBF_REAPI_PORT",
    "KBF_WORKER_PORT",
    "KBF_TLS_SERVER_NAME",
    "KBF_SERVER_STATE",
    "KBF_SERVER_MEMORY_MAX",
    "KBF_SERVER_CPU_QUOTA",
    "KBF_SERVER_TASKS_MAX",
    "KBF_DAEMON_NODES",
    "KBF_DRIVER",
    "KBF_DAEMON_EXTRA_ARGS",
    "KBF_USER",
    "KBF_ROOT",
    "KBF_LABEL_PREFIX",
    "KBF_MIN_MACOS",
    "KBF_MIN_FREE_GB",
    "MINIO_ENDPOINT",
    "KBF_S3_BUCKET",
    "KBF_S3_REGION",
    "KBF_S3_PREFIX",
    "KBF_S3_CONDITIONAL_PUT",
    "MINIO_NAMESPACE",
    "MINIO_SERVICE",
    "MINIO_NODEPORT",
    "KBF_INSTANCE_NAME",
    "KBF_CONCURRENCY",
    "KBF_LINUX_PROPERTIES",
    "KBF_DARWIN_PROPERTIES",
    "KBF_DARWIN_LABELS",
    "KBF_DARWIN_MACOS_HOSTS",
    "KBF_EXIT_TARGET",
];

/// What `server-install` reads, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSettings {
    /// The address kbf-server binds: `KBF_SERVER_BIND`, else `KBF_SERVER_HOST`.
    pub bind: Ipv4Addr,
    /// The interface that must carry [`ServerSettings::bind`].
    pub iface: String,
    pub reapi_port: u16,
    pub worker_port: u16,
    /// The name the server certificate must be valid for.
    pub tls_server_name: String,
    /// `KBF_SERVER_STATE`; `None`: the default under `$HOME`.
    pub state: Option<HostPath>,
    pub memory_max: String,
    pub cpu_quota: String,
    pub tasks_max: String,
    pub minio_endpoint: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub s3_prefix: String,
    pub conditional_put: bool,
}

impl ServerSettings {
    /// `KBF_SERVER_STATE`, the one key `--uninstall` reads; `None` when unset or empty.
    pub fn state_of(c: &Config) -> Result<Option<HostPath>, PilotError> {
        value(c, "KBF_SERVER_STATE")
            .map(|s| HostPath::new(s).map_err(|e| PilotError::path("KBF_SERVER_STATE", e)))
            .transpose()
    }

    /// Check `c`'s kbf-server keys; `bind`, when given, wins over the file's.
    pub fn from_config(c: &Config, bind: Option<&str>) -> Result<ServerSettings, PilotError> {
        let bind = match bind {
            Some(b) => b,
            None => value(c, "KBF_SERVER_BIND")
                .or(value(c, "KBF_SERVER_HOST"))
                .ok_or(PilotError::NoBind)?,
        };
        let bind = bind
            .parse::<Ipv4Addr>()
            .map_err(|_| PilotError::NotIpv4(bind.to_owned()))?;
        let state = ServerSettings::state_of(c)?;
        let caps = [
            ("KBF_SERVER_MEMORY_MAX", "4G", &["K", "M", "G"][..]),
            ("KBF_SERVER_CPU_QUOTA", "200%", &["%"][..]),
            ("KBF_SERVER_TASKS_MAX", "512", &[""][..]),
        ];
        let mut checked = Vec::new();
        for (key, default, suffixes) in caps {
            let v = value(c, key).unwrap_or(default);
            if !suffixes
                .iter()
                .any(|s| v.strip_suffix(s).is_some_and(whole))
            {
                return Err(PilotError::Cap {
                    key,
                    value: v.to_owned(),
                    like: default,
                });
            }
            checked.push(v.to_owned());
        }
        let [memory_max, cpu_quota, tasks_max] =
            <[String; 3]>::try_from(checked).expect("one value per cap");
        let conditional_put = match value(c, "KBF_S3_CONDITIONAL_PUT").unwrap_or("1") {
            "1" => true,
            "0" => false,
            other => return Err(PilotError::ZeroOrOne(other.to_owned())),
        };
        Ok(ServerSettings {
            bind,
            iface: require(c, "KBF_TAILNET_IFACE")?,
            reapi_port: port(c, "KBF_REAPI_PORT", "8980")?,
            worker_port: port(c, "KBF_WORKER_PORT", "8981")?,
            tls_server_name: text(c, "KBF_TLS_SERVER_NAME", "kbf-pilot-server"),
            state,
            memory_max,
            cpu_quota,
            tasks_max,
            minio_endpoint: require(c, "MINIO_ENDPOINT")?,
            s3_bucket: require(c, "KBF_S3_BUCKET")?,
            s3_region: text(c, "KBF_S3_REGION", "us-east-1"),
            s3_prefix: text(c, "KBF_S3_PREFIX", "kbf/"),
            conditional_put,
        })
    }
}

/// What `buckconfig` reads, checked for the overlay's sake by
/// [`super::buckconfig`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlaySettings {
    pub server_host: String,
    pub reapi_port: u16,
    pub instance_name: String,
    pub concurrency: String,
    pub linux_properties: String,
    pub darwin_properties: String,
    /// The daemons' labels, comma separated; may be empty.
    pub darwin_labels: String,
    pub darwin_macos_hosts: String,
}

impl OverlaySettings {
    pub fn from_config(c: &Config) -> Result<OverlaySettings, PilotError> {
        let concurrency = text(c, "KBF_CONCURRENCY", "8");
        if !whole(&concurrency) {
            return Err(PilotError::Cap {
                key: "KBF_CONCURRENCY",
                value: concurrency,
                like: "8",
            });
        }
        Ok(OverlaySettings {
            server_host: require(c, "KBF_SERVER_HOST")?,
            reapi_port: port(c, "KBF_REAPI_PORT", "8980")?,
            instance_name: require(c, "KBF_INSTANCE_NAME")?,
            concurrency,
            linux_properties: require(c, "KBF_LINUX_PROPERTIES")?,
            darwin_properties: require(c, "KBF_DARWIN_PROPERTIES")?,
            darwin_labels: text(c, "KBF_DARWIN_LABELS", ""),
            darwin_macos_hosts: require(c, "KBF_DARWIN_MACOS_HOSTS")?,
        })
    }
}

/// `c`'s value of `key`, unless the file left it out or empty.
fn value<'a>(c: &'a Config, key: &str) -> Option<&'a str> {
    c.get(key).filter(|v| !v.is_empty())
}

fn text(c: &Config, key: &str, default: &str) -> String {
    value(c, key).unwrap_or(default).to_owned()
}

fn require(c: &Config, key: &'static str) -> Result<String, PilotError> {
    Ok(c.require(key)?.to_owned())
}

fn port(c: &Config, key: &'static str, default: &str) -> Result<u16, PilotError> {
    let v = value(c, key).unwrap_or(default);
    v.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(PilotError::NotPort {
            key,
            value: v.to_owned(),
        })
}

/// A whole number from 1 up, written without a leading zero: `512`, not `0512`.
fn whole(v: &str) -> bool {
    v.bytes().next().is_some_and(|b| (b'1'..=b'9').contains(&b))
        && v.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
