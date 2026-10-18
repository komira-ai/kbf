//! The `envoy` section of the release manifest (docs/design/kbf-installer.md, section 5):
//!
//! ```json
//! "envoy": {"linux-x86_64": {"version": "1.31.2", "url": "https://...", "sha256": "..."}}
//! ```
//!
//! The section is Linux and frontend only (an order for a node ignores it), so the only
//! platforms are `linux-x86_64` and `linux-arm64`; any other key, any other field of an
//! entry, a version that is not `<major>.<minor>.<patch>`, a URL that is not `https://`
//! and a sha256 that is not 64 lowercase hex digits (the contract says non-empty; a name
//! of a directory is made from it, so the form is held too) refuse the manifest. Every
//! entry is held to the rules, not only the one for this host's CPU.
//!
//! This reads the section of a manifest the caller has already verified (the attestation
//! of the manifest and its binding to the order's commit are the stage's, section 5
//! steps 3 and 4); the rest of the manifest is not looked at here.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::binary::Arch;

/// The platforms of the section, as the manifest spells them.
pub const LINUX_X86_64: &str = "linux-x86_64";
pub const LINUX_ARM64: &str = "linux-arm64";

/// One Envoy build the manifest pins for a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvoyPin {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// Why a manifest's `envoy` section is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestFault {
    #[error("the manifest is not a JSON object: {0}")]
    NotJson(String),
    #[error("the manifest has no envoy section")]
    NoEnvoy,
    /// The section is not an object of entries, or an entry has a field that is not
    /// `version`, `url` or `sha256`, lacks one, or has one that is not a string.
    #[error("the envoy section is malformed: {0}")]
    Shape(String),
    #[error("the envoy section names the platform {0:?}; only linux-x86_64 and linux-arm64")]
    Platform(String),
    #[error("{platform}: version {version:?} is not <major>.<minor>.<patch>")]
    Version { platform: String, version: String },
    #[error("{platform}: url {url:?} is not an https:// URL")]
    Url { platform: String, url: String },
    #[error("{platform}: sha256 {sha256:?} is not 64 lowercase hex digits")]
    Sha256 { platform: String, sha256: String },
    #[error("the envoy section has no entry for {0}")]
    Missing(&'static str),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: String,
    url: String,
    sha256: String,
}

/// The `envoy` section of the manifest `bytes` holds, for a host of `arch`.
pub fn pin_for(bytes: &[u8], arch: Arch) -> Result<EnvoyPin, ManifestFault> {
    let mut pins = section(bytes)?;
    let want = platform_of(arch);
    pins.remove(want).ok_or(ManifestFault::Missing(want))
}

/// The manifest key of the build for `arch`.
pub fn platform_of(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => LINUX_X86_64,
        Arch::Aarch64 => LINUX_ARM64,
    }
}

/// Every entry of the section, each checked.
pub fn section(bytes: &[u8]) -> Result<BTreeMap<String, EnvoyPin>, ManifestFault> {
    let manifest: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ManifestFault::NotJson(e.to_string()))?;
    let serde_json::Value::Object(mut manifest) = manifest else {
        return Err(ManifestFault::NotJson("not an object".to_owned()));
    };
    let section = manifest.remove("envoy").ok_or(ManifestFault::NoEnvoy)?;
    let entries: BTreeMap<String, Raw> =
        serde_json::from_value(section).map_err(|e| ManifestFault::Shape(e.to_string()))?;
    let mut pins = BTreeMap::new();
    for (platform, raw) in entries {
        if platform != LINUX_X86_64 && platform != LINUX_ARM64 {
            return Err(ManifestFault::Platform(platform));
        }
        if !super::valid_version(&raw.version) {
            return Err(ManifestFault::Version {
                platform,
                version: raw.version,
            });
        }
        if !https(&raw.url) {
            return Err(ManifestFault::Url {
                platform,
                url: raw.url,
            });
        }
        if !super::valid_sha256(&raw.sha256) {
            return Err(ManifestFault::Sha256 {
                platform,
                sha256: raw.sha256,
            });
        }
        pins.insert(
            platform,
            EnvoyPin {
                version: raw.version,
                url: raw.url,
                sha256: raw.sha256,
            },
        );
    }
    Ok(pins)
}

/// `https://<host>...` with a host, no credentials, and only printable ASCII without
/// spaces (the URL becomes one argument of the fetch program and a line of output).
fn https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if !rest.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty() && !authority.contains('@') && !authority.starts_with(':')
}
