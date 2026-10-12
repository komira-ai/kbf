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
//! - `kbf-deploy mdm tls --config FILE [--apply]` ([`tls`]): as root, the TLS
//!   certificate from an ACME CA over DNS-01 by the pinned lego, its renewal timer,
//!   and the proxy started; `--check-godaddy` asks, read only, whether a GoDaddy
//!   account may use GoDaddy's DNS API.
//! - `kbf-deploy mdm push-cert --config FILE (--cert PEM --key PEM | --profile-only
//!   [--topic TOPIC]) [--apply]` ([`push_cert`]): as root, the APNs push certificate
//!   given to NanoHUB, and the enrollment profile ([`profile`]) the proxy serves.
//! - `kbf-deploy mdm dep (get-cert --out FILE | upload-token --token FILE |
//!   define-profile | assign --serial SERIAL...) --config FILE [--apply]` ([`dep`]): as
//!   root, NanoDEP linked to Apple Business Manager and given the Automated Device
//!   Enrollment profile.
//! - `kbf-deploy mdm verify --config FILE` ([`verify`]): read-only checks of the
//!   running MDM, one `ok` or `FAIL` line each.
//! - `kbf-deploy mdm erase-request --serial SERIAL --reason TEXT [--lease ID] --key KEY
//!   --out FILE [--valid-for MINUTES] [--ssh-keygen PATH] [--apply]` ([`erase`]): on
//!   the operator's own machine, as the operator, an erase request for one Mac in the
//!   gate's `kbf-erase-v1` text, signed with their security key by `ssh-keygen -Y
//!   sign` and checked as the gate checks it.
//!
//! The verbs that change something check first and print their plan by default;
//! `--apply` carries it out. A call to a service's API on loopback ([`api`]) is
//! printed as a step is, and made only with `--apply`. They exit 0 done (or nothing
//! to do), 1 a step, a download or a call failed (or a signed erase request does
//! not check), 2 refused (`REFUSED: ...` on stderr); `dep assign` also exits 1 when Apple did not assign every device.
//! `verify` exits 0 when every check passes, 1 when one fails, 2 refused;
//! `tls --check-godaddy` 0 when the account may use the API, else 1. The settings are
//! a closed set of `KEY=VALUE` lines ([`config::KEYS`], `templates/mdm/config.example`).

pub mod api;
pub mod archive;
pub mod config;
pub mod dep;
pub mod erase;
pub mod fetch;
pub mod install;
pub mod pins;
pub mod profile;
pub mod push_cert;
pub mod render;
pub mod secret;
pub mod tls;
pub mod verify;

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;

pub use config::{MdmConfig, MdmConfigError};
pub use pins::{Pins, PinsError, Tool};

use crate::https::{HttpsError, HttpsUrlError};
use crate::linux::HostError;
use crate::template::TemplateError;
use archive::ArchiveError;
use kbf_host::path::{HostPath, PathError};
use kbf_host::step::{Perms, UrlError};
use kbf_host::user::UserError;

/// Why an `mdm` verb refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum MdmError {
    #[error(transparent)]
    Config(#[from] MdmConfigError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error(transparent)]
    Deploy(#[from] kbf_host::HostError),
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
    #[error(transparent)]
    Erase(#[from] erase::EraseError),
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

    // the verbs after install
    #[error(
        "{verb} reads root-only files (the API keys, the secrets, the state): run it with sudo, even for a dry run (effective uid is {euid})"
    )]
    NeedsRoot { verb: &'static str, euid: u32 },
    #[error("{0} is not there: run mdm install --apply first")]
    NotInstalled(HostPath),
    #[error("{path} has no {key}= line with a value")]
    NoSecret { path: HostPath, key: String },
    #[error("{path}: the value of {key} holds a control character")]
    SecretValue { path: HostPath, key: String },
    #[error(
        "{path} is inside a git work tree ({git} exists): keep keys, tokens and certificates outside every repository"
    )]
    InRepository { path: HostPath, git: HostPath },
    #[error("{0} is not a file")]
    NoFile(HostPath),
    #[error("{0} is there and is not a PEM certificate")]
    NotCertificate(HostPath),
    #[error(transparent)]
    Pair(#[from] push_cert::PairError),
    #[error(transparent)]
    Profile(#[from] profile::ProfileError),
    #[error("no push topic: {0} is not there; give --cert and --key first, or --topic")]
    NoTopic(HostPath),
    #[error("{call}: {why}")]
    Api { call: String, why: String },
    #[error("{what}: {why}")]
    Answer { what: &'static str, why: String },
    #[error("NanoHUB answered topic {got:?} for a certificate of {want}")]
    TopicDiffers { got: String, want: String },
    #[error("no ADE profile yet ({0} is not there): run mdm dep define-profile --apply first")]
    NoProfile(HostPath),
    #[error("MDM_{key}={value}: {why}")]
    Acme {
        key: &'static str,
        value: String,
        why: &'static str,
    },
    #[error("{0} is missing: mdm install --apply installs it")]
    NoLego(HostPath),
    #[error("{lego} run --help lists no {flag}: not the lego the pins name")]
    LegoFlag { lego: HostPath, flag: &'static str },
    #[error(
        "no certificate at {0} after kbf-mdm-cert.service ran: journalctl -u kbf-mdm-cert.service"
    )]
    NoCertificate(HostPath),
}

impl MdmError {
    /// The exit code: 1 when a step, a download or a check failed, 2 when refused.
    pub fn exit_code(&self) -> u8 {
        match self {
            MdmError::Deploy(kbf_host::HostError::Step { .. })
            | MdmError::Https(_)
            | MdmError::Io { .. }
            | MdmError::Status { .. }
            | MdmError::Rejected { .. }
            | MdmError::Api { .. }
            | MdmError::Answer { .. }
            | MdmError::TopicDiffers { .. }
            | MdmError::NoCertificate(_)
            | MdmError::Erase(erase::EraseError::Check { .. }) => 1,
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
        let step = kbf_host::HostError::Step {
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
            MdmError::Api {
                call: "GET x".to_owned(),
                why: "answered 500".to_owned(),
            },
            MdmError::Answer {
                what: "x",
                why: String::new(),
            },
            MdmError::TopicDiffers {
                got: String::new(),
                want: String::new(),
            },
            MdmError::NoCertificate(HostPath::new("/x").expect("a path")),
        ];
        for e in failed {
            assert_eq!(e.exit_code(), 1, "{e}");
        }
        let refused = [
            MdmError::FetchAsRoot,
            MdmError::Deploy(kbf_host::HostError::IsRoot),
            MdmError::PortHeld(443),
            MdmError::NeedsRoot {
                verb: "mdm tls",
                euid: 501,
            },
        ];
        for e in refused {
            assert_eq!(e.exit_code(), 2, "{e}");
        }
    }
}
