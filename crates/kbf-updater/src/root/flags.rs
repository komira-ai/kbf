//! The flags of `kbf-updater root`. Every setting is a flag; none is read from the environment.
//!
//! ```text
//! kbf-updater root --socket PATH --socket-user NAME
//!                  [--declarations DIR] [--approvals FILE]
//!                  [--state-dir DIR] [--releases-dir DIR]
//!                  [--register-socket PATH --register-group NAME]
//! ```
//!
//! - `--socket` is the agent's socket, made mode 0600 and owned by `--socket-user`, the agent's
//!   account (3.4).
//! - `--declarations` is where the declarations are installed, `/etc/updater.d`, and
//!   `--approvals` the record of approved declaration digests, `/etc/updater.d/approved`.
//! - `--state-dir` is the root part's own (records, params, registrations; mode 0700), default
//!   `/var/lib/updater/state`. `--releases-dir` holds the staged releases
//!   ([`super::releases`]), default `/var/lib/updater/releases`.
//! - `--register-socket` serves the registration socket (2.1) for the members of
//!   `--register-group`; its directory must be root's and closed to others.
//!
//! The account that must own the declarations, the approvals and the seals is root's: no flag
//! changes it.

use std::path::PathBuf;

use crate::agent::flags::collect;

const FLAGS: [&str; 8] = [
    "--socket",
    "--socket-user",
    "--declarations",
    "--approvals",
    "--state-dir",
    "--releases-dir",
    "--register-socket",
    "--register-group",
];

pub const DEFAULT_STATE_DIR: &str = "/var/lib/updater/state";
pub const DEFAULT_RELEASES_DIR: &str = "/var/lib/updater/releases";

pub const USAGE: &str = "usage: kbf-updater root --socket PATH --socket-user NAME \
[--declarations DIR] [--approvals FILE] [--state-dir DIR] [--releases-dir DIR] \
[--register-socket PATH --register-group NAME]\n";

/// The registration socket and the group whose members may connect to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    pub socket: PathBuf,
    pub group: String,
}

/// The settings of the root part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flags {
    pub socket: PathBuf,
    pub socket_user: String,
    pub declarations: PathBuf,
    pub approvals: PathBuf,
    pub state: PathBuf,
    pub releases: PathBuf,
    pub register: Option<Register>,
}

/// Parses the words after `root`.
///
/// # Errors
/// What is wrong with the flags, in a line.
pub fn parse(args: &[String]) -> Result<Flags, String> {
    let mut found = collect(args, &FLAGS)?;
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let socket = PathBuf::from(take("--socket")?);
    let socket_user = take("--socket-user")?;
    let register = match (
        found.remove("--register-socket"),
        found.remove("--register-group"),
    ) {
        (Some(socket), Some(group)) => Some(Register {
            socket: PathBuf::from(socket),
            group,
        }),
        (None, None) => None,
        _ => {
            return Err(
                "--register-socket and --register-group are given together, or not at all"
                    .to_owned(),
            );
        }
    };
    let mut path = |flag: &str, default: &str| {
        PathBuf::from(found.remove(flag).unwrap_or_else(|| default.to_owned()))
    };
    Ok(Flags {
        socket,
        socket_user,
        declarations: path("--declarations", update_rights::DECLARATION_DIR),
        approvals: path("--approvals", update_rights::APPROVALS_FILE),
        state: path("--state-dir", DEFAULT_STATE_DIR),
        releases: path("--releases-dir", DEFAULT_RELEASES_DIR),
        register,
    })
}
