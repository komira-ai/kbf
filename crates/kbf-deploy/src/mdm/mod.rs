//! The `mdm` kit: an Apple MDM (NanoHUB, the SCEP server, NanoDEP, and Caddy as its
//! HTTPS proxy) as capped systemd services on one Linux host.
//!
//! - `kbf-deploy mdm fetch --dir DIR [--only TOOL]... [--apply]` ([`fetch`]): the
//!   pinned upstream archives ([`pins`]), downloaded over HTTPS as the operator's own
//!   user and checked against their sha256.
//! - `kbf-deploy mdm install --config FILE --bin-dir DIR [--apply]` and
//!   `kbf-deploy mdm install --config FILE --uninstall [--apply]` ([`install`]): as
//!   root, the binaries unpacked from the checked archives ([`archive`]), the secrets,
//!   the SCEP CA, the units and the Caddyfile rendered from compiled-in templates
//!   ([`render`]), and the services started.
//! - `kbf-deploy mdm verify --config FILE` ([`verify`]): read-only checks of the
//!   running MDM, one `ok` or `FAIL` line each.
//!
//! `fetch` and `install` check first and print their plan by default; `--apply`
//! carries it out. They exit 0 done (or nothing to do), 1 a step failed, 2 refused
//! (`REFUSED: ...` on stderr). `verify` exits 0 when every check passes, 1 when one
//! fails, 2 refused. The settings are a closed set of `KEY=VALUE` lines
//! ([`config::KEYS`], `templates/mdm/config.example`).

pub mod archive;
pub mod config;
pub mod fetch;
pub mod install;
pub mod pins;
pub mod render;
pub mod verify;

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;

pub use config::{MdmConfig, MdmConfigError};
pub use pins::{Pins, PinsError, Tool};

use crate::https::{HttpsError, HttpsUrlError};
use crate::linux::HostError;
use crate::path::{HostPath, PathError};
use crate::plan::DeployError;
use crate::step::{Perms, UrlError};
use crate::template::TemplateError;
use crate::user::UserError;
use archive::ArchiveError;

/// Why an `mdm` verb refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum MdmError {
    #[error(transparent)]
    Config(#[from] MdmConfigError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error(transparent)]
    Deploy(#[from] DeployError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Https(#[from] HttpsError),
    #[error(transparent)]
    HttpsUrl(#[from] HttpsUrlError),
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    User(#[from] UserError),
    #[error("cannot write the plan out: {0}")]
    Write(io::Error),

    // fetch
    #[error("run mdm fetch as yourself, not root: downloads never run as root")]
    FetchAsRoot,
    #[error("--dir {} is not absolute", .0.display())]
    Relative(PathBuf),
    #[error(
        "--dir {} is inside a git work tree ({} exists): keep archives and keys outside every repository",
        dir.display(),
        git.display()
    )]
    InGitTree { dir: PathBuf, git: PathBuf },
    #[error("{}: sha256 {got} is not the pin {want}: delete it and re-run", file.display())]
    KeptDiffers {
        file: PathBuf,
        got: String,
        want: String,
    },
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
    #[error(
        "{tool}: the download's sha256 {got} is not the pin {want} (kept as {} for inspection)",
        kept.display()
    )]
    Rejected {
        tool: Tool,
        got: String,
        want: String,
        kept: PathBuf,
    },

    // install
    #[error("mdm install needs --bin-dir DIR (where mdm fetch put the archives), or --uninstall")]
    NoBinDir,
    #[error(
        "mdm install reads root-only files (the secrets, the state): run it with sudo, even for a dry run (effective uid is {euid})"
    )]
    InstallNeedsRoot { euid: u32 },
    #[error("no interface carries MDM_BIND={0} (ip -4 addr)")]
    NotCarried(Ipv4Addr),
    #[error("port {0} is held by another process (ss -ltnp 'sport = :{0}')")]
    PortHeld(u16),
    #[error(
        "no user {user}: useradd --system --user-group --home-dir {home} --no-create-home --shell /usr/sbin/nologin {user}"
    )]
    NoUser { user: String, home: HostPath },
    #[error("{0} is missing (kbf-deploy mdm fetch --apply downloads it)")]
    NoArchive(HostPath),
    #[error("{path}: sha256 {got} is not the pin {want} of {tool}")]
    ArchiveDiffers {
        path: HostPath,
        tool: Tool,
        got: String,
        want: String,
    },
    #[error("{path} has mode {perms}, not 0600: chmod 600 {path}")]
    SecretMode { path: HostPath, perms: Perms },
    #[error("{path} is owned by uid {uid}, not root: chown root:root {path}")]
    SecretOwner { path: HostPath, uid: u32 },
    #[error("cannot read random bytes for a secret: {0}")]
    Random(io::Error),
}

impl MdmError {
    /// The exit code: 1 when a step, a download or a check failed, 2 when refused.
    pub fn exit_code(&self) -> u8 {
        match self {
            MdmError::Deploy(DeployError::Step { .. })
            | MdmError::Https(_)
            | MdmError::Io { .. }
            | MdmError::Status { .. }
            | MdmError::Rejected { .. } => 1,
            _ => 2,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../templates/mdm/config.example");

    /// A rack LAN address, built here: the hygiene lint refuses private addresses
    /// written out in the tree.
    pub(crate) fn lan(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 1, 0, last)
    }

    /// The example settings, with a LAN address and network filled in.
    pub(crate) fn config_text() -> String {
        EXAMPLE
            .replace("MDM_BIND=\n", &format!("MDM_BIND={}\n", lan(10)))
            .replace("MDM_LAN_CIDR=\n", &format!("MDM_LAN_CIDR={}/24\n", lan(0)))
    }

    pub(crate) fn config() -> MdmConfig {
        MdmConfig::parse(&config_text()).expect("valid settings")
    }

    /// Catches an exit code that calls a refusal a failure or the other way round: a
    /// script (or the operator) tells "fix the input" from "retry" by it.
    #[test]
    fn failures_exit_1_and_refusals_exit_2() {
        let step = DeployError::Step {
            index: 0,
            step: "run x".to_owned(),
            source: io::Error::other("x"),
        };
        let failed = [
            MdmError::Deploy(step),
            MdmError::Https(HttpsError::NoRoots),
            MdmError::Io {
                path: PathBuf::from("/x"),
                source: io::Error::other("x"),
            },
            MdmError::Status {
                url: "https://h/".to_owned(),
                status: 404,
            },
            MdmError::Rejected {
                tool: Tool::Caddy,
                got: String::new(),
                want: String::new(),
                kept: PathBuf::from("/x"),
            },
        ];
        for e in failed {
            assert_eq!(e.exit_code(), 1, "{e}");
        }
        let refused = [
            MdmError::FetchAsRoot,
            MdmError::Deploy(DeployError::IsRoot),
            MdmError::PortHeld(443),
        ];
        for e in refused {
            assert_eq!(e.exit_code(), 2, "{e}");
        }
    }
}
