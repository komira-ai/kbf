//! The `envoy` kit's settings: its closed key set and the shape each value must have.
//! `templates/envoy/config.example` documents every key.

use std::path::Path;

use crate::config::{Config, ConfigError};

/// Every key the config must set; any other is refused.
pub const KEYS: &[&str] = &[
    "KBF_FRONT_KUBE_CONTEXT",
    "KBF_FRONT_NAMESPACE",
    "KBF_FRONT_HOSTNAME",
    "KBF_FRONT_TAILNET_DNS",
    "KBF_TS_OPERATOR_NAMESPACE",
    "KBF_TS_OPERATOR_SECRET",
    "KBF_FRONT_CA_SECRET",
    "KBF_SERVERS",
    "KBF_ENVOY_IMAGE",
];

/// One kbf-server REAPI listener: the DNS name Envoy resolves and dials (also its SNI
/// and the name its certificate must carry) and the port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
}

/// The settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvoyConfig {
    /// The kubectl context the front is applied to.
    pub kube_context: String,
    /// The namespace of Envoy and the Ingress.
    pub namespace: String,
    /// The MagicDNS label the Ingress asks for.
    pub hostname: String,
    /// The tailnet's DNS name, `<label>.ts.net`.
    pub tailnet_dns: String,
    /// The Tailscale operator's namespace; only its pods may reach Envoy.
    pub operator_namespace: String,
    /// The Secret holding the operator's OAuth client.
    pub operator_secret: String,
    /// The Secret whose `ca.pem` is the CA the servers' certificates chain to.
    pub ca_secret: String,
    /// The servers, in the order given, no name twice.
    pub servers: Vec<Server>,
    /// The Envoy image, pinned by digest.
    pub image: String,
}

/// The shapes a value can be required to have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Letters, digits and `._@:/-`.
    KubeContext,
    /// A DNS label: 1 to 63 lower-case letters, digits and `-`, not starting or
    /// ending with `-`.
    DnsLabel,
    /// DNS labels joined by `.`.
    DnsName,
    /// A DNS label followed by `.ts.net`.
    Tailnet,
    /// `<DNS name>:<port>` items separated by one space.
    Servers,
    /// `<repo>[:<tag>]@sha256:<64 hex>`.
    PinnedImage,
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Shape::KubeContext => "a kubectl context (letters, digits and ._@:/-)",
            Shape::DnsLabel => {
                "a DNS label (1 to 63 lower-case letters, digits and -, no - at either end)"
            }
            Shape::DnsName => "a DNS name (DNS labels joined by .)",
            Shape::Tailnet => "<DNS label>.ts.net",
            Shape::Servers => "<DNS name>:<port> items separated by one space",
            Shape::PinnedImage => "<repo>[:<tag>]@sha256:<64 lower-case hex digits>",
        })
    }
}

/// Why the settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum EnvoyError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{key}='{value}' is not {shape}")]
    Shape {
        key: &'static str,
        value: String,
        shape: Shape,
    },
    #[error("KBF_SERVERS: port {port} of {host} is not from 1 to 65535")]
    Port { host: String, port: String },
    #[error("KBF_SERVERS names {0} twice")]
    TwiceNamed(String),
}

impl EnvoyConfig {
    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<EnvoyConfig, EnvoyError> {
        EnvoyConfig::from_config(&Config::load(path, KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<EnvoyConfig, EnvoyError> {
        EnvoyConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<EnvoyConfig, EnvoyError> {
        let get = |key: &'static str, shape: Shape| -> Result<String, EnvoyError> {
            let value = c.require(key)?;
            if fits(shape, value) {
                Ok(value.to_owned())
            } else {
                Err(EnvoyError::Shape {
                    key,
                    value: value.to_owned(),
                    shape,
                })
            }
        };
        Ok(EnvoyConfig {
            kube_context: get("KBF_FRONT_KUBE_CONTEXT", Shape::KubeContext)?,
            namespace: get("KBF_FRONT_NAMESPACE", Shape::DnsLabel)?,
            hostname: get("KBF_FRONT_HOSTNAME", Shape::DnsLabel)?,
            tailnet_dns: get("KBF_FRONT_TAILNET_DNS", Shape::Tailnet)?,
            operator_namespace: get("KBF_TS_OPERATOR_NAMESPACE", Shape::DnsLabel)?,
            operator_secret: get("KBF_TS_OPERATOR_SECRET", Shape::DnsName)?,
            ca_secret: get("KBF_FRONT_CA_SECRET", Shape::DnsName)?,
            servers: servers(&get("KBF_SERVERS", Shape::Servers)?)?,
            image: get("KBF_ENVOY_IMAGE", Shape::PinnedImage)?,
        })
    }
}

/// `KBF_SERVERS`, already of [`Shape::Servers`]: each port from 1 to 65535 (leading
/// zeros allowed), no host twice.
fn servers(value: &str) -> Result<Vec<Server>, EnvoyError> {
    let mut out: Vec<Server> = Vec::new();
    for item in value.split(' ') {
        let (host, port) = item.rsplit_once(':').unwrap_or((item, ""));
        let parsed = port.parse::<u16>().ok().filter(|&p| p != 0);
        let Some(port) = parsed else {
            return Err(EnvoyError::Port {
                host: host.to_owned(),
                port: port.to_owned(),
            });
        };
        if out.iter().any(|s| s.host == host) {
            return Err(EnvoyError::TwiceNamed(host.to_owned()));
        }
        out.push(Server {
            host: host.to_owned(),
            port,
        });
    }
    Ok(out)
}

/// Whether `value` has `shape`.
pub fn fits(shape: Shape, value: &str) -> bool {
    match shape {
        Shape::KubeContext => {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._@:/-".contains(&b))
        }
        Shape::DnsLabel => dns_label(value),
        Shape::DnsName => value.split('.').all(dns_label),
        Shape::Tailnet => value.strip_suffix(".ts.net").is_some_and(dns_label),
        Shape::Servers => value.split(' ').all(|item| {
            item.rsplit_once(':').is_some_and(|(host, port)| {
                fits(Shape::DnsName, host)
                    && (1..=5).contains(&port.len())
                    && port.bytes().all(|b| b.is_ascii_digit())
            })
        }),
        Shape::PinnedImage => pinned_image(value),
    }
}

fn dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && b.first() != Some(&b'-')
        && b.last() != Some(&b'-')
}

fn pinned_image(s: &str) -> bool {
    let Some((name, digest)) = s.split_once("@sha256:") else {
        return false;
    };
    let (repo, tag) = name.split_once(':').unwrap_or((name, ""));
    let repo_ok = !repo.is_empty()
        && repo
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"./_-".contains(&b));
    let tag_ok = !name.contains(':')
        || (!tag.is_empty()
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
    let digest_ok = digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    repo_ok && tag_ok && digest_ok
}
