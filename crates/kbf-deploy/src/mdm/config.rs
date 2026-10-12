//! The `mdm` kit's settings: `KEY=VALUE` lines over [`KEYS`], checked into
//! [`MdmConfig`] before anything reads the host.
//!
//! Every key the verbs here read must be set; the kit has no defaults, because each
//! default would be one site's value. The keys of the kit's other steps (the TLS
//! certificate and the erase path) are accepted and not read, so one file serves the
//! whole kit.

use std::net::Ipv4Addr;
use std::path::Path;

use crate::config::{Config, ConfigError};
use kbf_host::path::HostPath;

/// Every key the kit's config file may set (`templates/mdm/config.example`).
pub const KEYS: &[&str] = &[
    "MDM_DNS_NAME",
    "MDM_BIND",
    "MDM_LAN_CIDR",
    "MDM_USER",
    "MDM_STATE",
    "MDM_ETC",
    "MDM_BIN",
    "MDM_ERASE_STATE",
    "MDM_NANOHUB_PORT",
    "MDM_SCEP_PORT",
    "MDM_DEP_PORT",
    "MDM_MEMORY_MAX",
    "MDM_CPU_QUOTA",
    "MDM_TASKS_MAX",
    "MDM_DEP_NAME",
    "MDM_ORG",
    "MDM_ACME_EMAIL",
    "MDM_ACME_SERVER",
    "MDM_ACME_DNS_PROVIDER",
    "MDM_ERASE_NAMESPACE",
    "MDM_ERASE_PRINCIPAL",
    "MDM_ERASE_MAX_AGE",
];

/// The settings `mdm install` and `mdm verify` read, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdmConfig {
    /// The name the Macs dial; the TLS certificate is for it.
    pub dns_name: String,
    /// The LAN address the HTTPS proxy binds: private, inside `lan`.
    pub bind: Ipv4Addr,
    /// The rack LAN, the only network besides loopback the proxy talks to.
    pub lan: Cidr,
    /// The services' system user.
    pub user: String,
    pub state: HostPath,
    pub etc: HostPath,
    pub bin: HostPath,
    pub nanohub_port: u16,
    pub scep_port: u16,
    pub dep_port: u16,
    /// The slice's caps, as systemd writes them: `1G`, `100%`, `256`.
    pub memory_max: String,
    pub cpu_quota: String,
    pub tasks_max: String,
    /// NanoDEP's name for the Apple Business Manager token.
    pub dep_name: String,
    /// The organisation in the SCEP CA's name.
    pub org: String,
}

/// A private IPv4 network, `a.b.c.d/bits` with 8 to 30 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub bits: u8,
}

impl Cidr {
    pub fn contains(self, a: Ipv4Addr) -> bool {
        let mask = u32::MAX << (32 - u32::from(self.bits));
        u32::from(a) & mask == u32::from(self.addr) & mask
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.bits)
    }
}

/// Why the settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum MdmConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("MDM_{key}={value}: {why}")]
    Bad {
        key: &'static str,
        value: String,
        why: &'static str,
    },
}

impl MdmConfig {
    pub fn load(path: &Path) -> Result<MdmConfig, MdmConfigError> {
        MdmConfig::from_config(&Config::load(path, KEYS)?)
    }

    pub fn parse(text: &str) -> Result<MdmConfig, MdmConfigError> {
        MdmConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<MdmConfig, MdmConfigError> {
        let get = |key: &'static str| -> Result<String, MdmConfigError> {
            Ok(c.require(&format!("MDM_{key}"))?.to_owned())
        };
        let bad = |key, value: &str, why| MdmConfigError::Bad {
            key,
            value: value.to_owned(),
            why,
        };
        let dns_name = get("DNS_NAME")?;
        if !is_dns_name(&dns_name) {
            return Err(bad(
                "DNS_NAME",
                &dns_name,
                "not a lower-case DNS name with a dot",
            ));
        }
        let text = get("BIND")?;
        let bind = lan_address(&text).ok_or(bad(
            "BIND",
            &text,
            "not a private LAN address (10/8, 172.16/12, 192.168/16): the proxy binds the rack LAN only, never 0.0.0.0, loopback or another range",
        ))?;
        let text = get("LAN_CIDR")?;
        let lan = cidr(&text).ok_or(bad(
            "LAN_CIDR",
            &text,
            "not a private IPv4 network (10/8, 172.16/12, 192.168/16) in CIDR form with 8 to 30 bits",
        ))?;
        if !lan.contains(bind) {
            return Err(bad(
                "BIND",
                &bind.to_string(),
                "not inside MDM_LAN_CIDR (a container bridge's or another network's address?)",
            ));
        }
        let user = get("USER")?;
        if !is_user_name(&user) {
            return Err(bad(
                "USER",
                &user,
                "not a user name ([a-z_][a-z0-9_-]{0,30})",
            ));
        }
        let path = |key| -> Result<HostPath, MdmConfigError> {
            let text = get(key)?;
            if !text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
            {
                return Err(bad(
                    key,
                    &text,
                    "a path the units cannot carry (letters, digits, / . _ -)",
                ));
            }
            HostPath::new(&text).map_err(|_| {
                bad(
                    key,
                    &text,
                    "not an absolute path with no empty, . or .. part",
                )
            })
        };
        let (state, etc, bin) = (path("STATE")?, path("ETC")?, path("BIN")?);
        let port = |key| -> Result<u16, MdmConfigError> {
            let text = get(key)?;
            text.parse().ok().filter(|p| *p != 0).ok_or(bad(
                key,
                &text,
                "not a port from 1 to 65535",
            ))
        };
        let (nanohub_port, scep_port, dep_port) =
            (port("NANOHUB_PORT")?, port("SCEP_PORT")?, port("DEP_PORT")?);
        let capped = |key, suffixes: &[char], why| -> Result<String, MdmConfigError> {
            let text = get(key)?;
            if !is_count(&text, suffixes) {
                return Err(bad(key, &text, why));
            }
            Ok(text)
        };
        let memory_max = capped("MEMORY_MAX", &['K', 'M', 'G'], "not like 1G")?;
        let cpu_quota = capped("CPU_QUOTA", &['%'], "not like 100%")?;
        let tasks_max = capped("TASKS_MAX", &[], "not a number from 1")?;
        let dep_name = get("DEP_NAME")?;
        if !dep_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return Err(bad(
                "DEP_NAME",
                &dep_name,
                "not a name of letters, digits, . _ -",
            ));
        }
        let org = get("ORG")?;
        if org.chars().any(char::is_control) {
            return Err(bad("ORG", &org, "holds a control character"));
        }
        Ok(MdmConfig {
            dns_name,
            bind,
            lan,
            user,
            state,
            etc,
            bin,
            nanohub_port,
            scep_port,
            dep_port,
            memory_max,
            cpu_quota,
            tasks_max,
            dep_name,
            org,
        })
    }
}

/// A private LAN address: 10/8, 172.16/12 or 192.168/16.
pub fn lan_address(text: &str) -> Option<Ipv4Addr> {
    let a: Ipv4Addr = text.parse().ok()?;
    a.is_private().then_some(a)
}

fn cidr(text: &str) -> Option<Cidr> {
    let (addr, bits) = text.split_once('/')?;
    let bits: u8 = bits.parse().ok().filter(|b| (8..=30).contains(b))?;
    Some(Cidr {
        addr: lan_address(addr)?,
        bits,
    })
}

/// A lower-case DNS name with at least one dot; no label starts or ends with `-`.
fn is_dns_name(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// A whole number from 1, without leading zeros, then one of `suffixes` (none when
/// `suffixes` is empty).
fn is_count(text: &str, suffixes: &[char]) -> bool {
    let digits = if suffixes.is_empty() {
        Some(text)
    } else {
        text.strip_suffix(suffixes)
    };
    digits.is_some_and(|d| {
        !d.is_empty() && !d.starts_with('0') && d.chars().all(|c| c.is_ascii_digit())
    })
}

fn is_user_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 31
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
