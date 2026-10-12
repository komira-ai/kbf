//! The `envoy` kit: a client front for a kbf farm, a Tailscale operator Ingress in
//! front of one Envoy that routes REAPI calls by gRPC method to the kbf-servers over
//! TLS to the farm's CA.
//!
//! - `kbf-deploy envoy render --config FILE --out DIR` writes two files into `DIR`
//!   ([`render`]): `manifests.json`, a v1 List of the Kubernetes objects (the Envoy
//!   ConfigMap, Deployment, Service and NetworkPolicy, and the Ingress), and
//!   `envoy.json`, the Envoy bootstrap the ConfigMap carries, the same bytes, for a
//!   reviewer. It writes nothing else and touches no cluster, so it needs no
//!   `--apply`.
//!
//! Both files are JSON, which kubectl and Envoy read, printed as [`json`] describes:
//! the Deployment's pod template carries the sha256 of `envoy.json`, so the same
//! settings must always give the same bytes, or every apply would restart Envoy.
//!
//! Exit codes: 0 rendered; 2 refused (a bad setting, a file that cannot be read or
//! written), with a `REFUSED:` line on stderr.
//!
//! The settings are a closed set of `KEY=VALUE` lines ([`config::KEYS`],
//! `templates/envoy/config.example`); every key must be set.

pub mod config;
pub mod json;
pub mod render;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use config::{EnvoyConfig, EnvoyError, Server};
pub use render::Rendered;

/// The Kubernetes objects file `render` writes.
pub const MANIFESTS: &str = "manifests.json";
/// The Envoy bootstrap file `render` writes.
pub const ENVOY: &str = "envoy.json";

/// Why `render` was refused (exit 2).
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error(transparent)]
    Config(#[from] EnvoyError),
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// `envoy render`: render the settings in `config` into `out`, and return the line to
/// print.
pub fn render_files(config: &Path, out: &Path) -> Result<String, Refusal> {
    let c = EnvoyConfig::load(config)?;
    let r = render::render(&c);
    std::fs::create_dir_all(out).map_err(|source| Refusal::Write {
        path: out.to_owned(),
        source,
    })?;
    for (name, text) in [(MANIFESTS, &r.manifests), (ENVOY, &r.envoy)] {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(|source| Refusal::Write { path, source })?;
    }
    Ok(format!(
        "rendered {MANIFESTS} {ENVOY} into {}",
        out.display()
    ))
}
