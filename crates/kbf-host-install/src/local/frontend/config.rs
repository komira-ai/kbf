//! `/etc/kbf/host-install-frontend.toml`: the frontend container this host runs, as root
//! describes it.
//!
//! ```toml
//! [frontend]
//! name = "kbf-frontend"                      # the container's name
//! restart = "unless-stopped"                 # docker's restart policy
//! network = "bridge"                         # optional
//! user = "65532:65532"                       # optional
//! group_add = ["990"]                        # optional: supplementary gids, as quoted numbers
//! ports = ["127.0.0.1:8982:8982"]            # docker's --publish forms
//! mounts = ["/etc/kbf:/etc/kbf:ro"]          # docker's --volume forms
//! command = ["--config", "/etc/kbf/kbf.toml"]  # the flags after the image
//! api = "127.0.0.1:8982"                     # where this host reaches the operator API
//! drain_token_file = "/etc/kbf/operator.token"  # a path inside the container
//! peers = ["203.0.113.11:8982"]              # optional: the other frontends' operator APIs
//! drain_secs = 3600                          # optional: no deadline when absent
//! ready_secs = 90                            # optional
//! ```
//!
//! The file is read as root through [`Trust`], so it is root's alone to write. At the start of
//! a switch its run part ([`RunSpec`]) is copied into the pending record, and every container
//! made during that switch, the new one and the old one put back, is made from the copy: what
//! the container is made of is never guessed, and never changes under a switch in progress.
//!
//! Nothing in it is a shell word: each entry becomes one argument of `docker`.

use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::trust::Trust;

/// Where the file is.
pub const CONFIG: &str = "/etc/kbf/host-install-frontend.toml";

/// How long a container has to be ready, unless the file says.
pub const DEFAULT_READY_SECS: u64 = 90;

/// How long `docker stop` lets the process finish after it is told to: the container has
/// already ended by then (the drain finished), so this is never what ends a drain.
pub const STOP_SECS: u64 = 30;

/// What a container is made of, and nothing about where it is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub name: String,
    pub restart: String,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// Supplementary groups of the container's process (`--group-add`), never 0.
    ///
    /// Not written when empty, so that a record of a host that sets none is the bytes the
    /// helper wrote before this key existed, and an older helper (which refuses unknown keys)
    /// still reads it. A host that sets `group_add` must settle or revert any release on trial
    /// before the helper is rolled back to a version without it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_add: Vec<u32>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
}

impl RunSpec {
    /// The arguments of `docker` that make and start the container from `image`.
    pub fn run_args(&self, image: &str) -> Vec<String> {
        let mut args: Vec<String> = ["run", "--detach", "--name", &self.name, "--restart"]
            .map(str::to_owned)
            .to_vec();
        args.push(self.restart.clone());
        for (flag, value) in [("--network", &self.network), ("--user", &self.user)] {
            if let Some(value) = value {
                args.push(flag.to_owned());
                args.push(value.clone());
            }
        }
        for gid in &self.group_add {
            args.push("--group-add".to_owned());
            args.push(gid.to_string());
        }
        for (flag, values) in [("--publish", &self.ports), ("--volume", &self.mounts)] {
            for value in values {
                args.push(flag.to_owned());
                args.push(value.clone());
            }
        }
        args.push(image.to_owned());
        args.extend(self.command.iter().cloned());
        args
    }
}

/// The file, checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendConfig {
    pub run: RunSpec,
    /// The operator API as this host reaches it, for `/readyz` and `/healthz`.
    pub api: SocketAddr,
    /// The file inside the container that holds the operator token, for the drain.
    pub drain_token_file: String,
    /// The operator APIs of the other frontends, which must be ready before this one stops.
    pub peers: Vec<SocketAddr>,
    /// How long a drain is waited for before the switch stops (and cuts nothing). None: the
    /// drain is waited for until the old container has ended, however long that takes.
    pub drain_secs: Option<u64>,
    pub ready_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    frontend: Entry,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    restart: String,
    network: Option<String>,
    user: Option<String>,
    #[serde(default)]
    group_add: Vec<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    mounts: Vec<String>,
    #[serde(default)]
    command: Vec<String>,
    api: String,
    drain_token_file: String,
    #[serde(default)]
    peers: Vec<String>,
    drain_secs: Option<u64>,
    ready_secs: Option<u64>,
}

/// A word of a name: letters, digits and `_ . -`.
fn word_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.starts_with(|c: char| c.is_ascii_alphanumeric())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn restart_ok(text: &str) -> bool {
    match text.split_once(':') {
        None => matches!(text, "always" | "unless-stopped" | "on-failure"),
        Some(("on-failure", count)) => count.parse::<u32>().is_ok(),
        Some(_) => false,
    }
}

fn user_ok(text: &str) -> bool {
    let mut parts = text.split(':');
    let ok = |part: Option<&str>| part.is_some_and(word_ok);
    ok(parts.next()) && parts.next().is_none_or(word_ok) && parts.next().is_none()
}

/// A group id: decimal digits only (no name, sign or space), from 1 to `u32::MAX`.
fn gid(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|gid| *gid != 0)
}

fn plain(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_control)
}

/// Why the file is not usable, in a line.
fn check(entry: &RunSpec) -> Result<(), String> {
    if !word_ok(&entry.name) {
        return Err("name is not a container name".to_owned());
    }
    if !restart_ok(&entry.restart) {
        return Err(
            "restart is not always, unless-stopped or on-failure[:N] (\"no\" would leave the frontend down after a reboot)"
                .to_owned(),
        );
    }
    if !entry.network.as_deref().is_none_or(word_ok) {
        return Err("network is not a network name".to_owned());
    }
    if !entry.user.as_deref().is_none_or(user_ok) {
        return Err("user is not NAME or ID, with an optional :GROUP".to_owned());
    }
    if !entry.ports.iter().all(|p| plain(p)) {
        return Err("a port is empty or holds a control character".to_owned());
    }
    if !entry.mounts.iter().all(|m| plain(m) && m.starts_with('/')) {
        return Err("a mount does not start with the absolute path of its source".to_owned());
    }
    if !entry.command.iter().all(|c| !c.contains('\0')) {
        return Err("a command word holds a NUL".to_owned());
    }
    Ok(())
}

fn addr(what: &str, text: &str) -> Result<SocketAddr, String> {
    text.parse()
        .map_err(|_| format!("{what} {text:?} is not an IP address and a port"))
}

impl FrontendConfig {
    /// Read and check the file.
    ///
    /// # Errors
    /// The file is not root's alone to write or cannot be read, does not parse, or holds a
    /// value that is not one: the line says which.
    pub fn load(trust: &Trust, path: &Path) -> Result<Self, String> {
        let text = trust
            .read(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|why| format!("{}: {why}", path.display()))
    }

    /// Check the text of the file.
    ///
    /// # Errors
    /// Why it is not a usable file.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_with(text, true)
    }

    /// Like [`FrontendConfig::load`], for the start of the program: a `drain_secs` of 0, which
    /// older programs accepted, is let through, so that an upgrade does not stop the program
    /// from starting. The switch reads the file with [`FrontendConfig::load`] and refuses it.
    ///
    /// # Errors
    /// As [`FrontendConfig::load`], but for `drain_secs = 0`.
    pub fn load_for_start(trust: &Trust, path: &Path) -> Result<Self, String> {
        let text = trust
            .read(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse_with(&text, false).map_err(|why| format!("{}: {why}", path.display()))
    }

    fn parse_with(text: &str, strict: bool) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        let entry = file.frontend;
        let group_add = entry
            .group_add
            .iter()
            .map(|g| gid(g).ok_or_else(|| format!("group_add {g:?} is not a group id from 1")))
            .collect::<Result<_, _>>()?;
        let run = RunSpec {
            name: entry.name,
            restart: entry.restart,
            network: entry.network,
            user: entry.user,
            group_add,
            ports: entry.ports,
            mounts: entry.mounts,
            command: entry.command,
        };
        check(&run)?;
        if !entry.drain_token_file.starts_with('/') || !plain(&entry.drain_token_file) {
            return Err("drain_token_file is not an absolute path".to_owned());
        }
        if strict && entry.drain_secs == Some(0) {
            return Err("drain_secs is 0; leave it out to wait for the drain".to_owned());
        }
        let drain_secs = entry.drain_secs;
        let ready_secs = entry.ready_secs.unwrap_or(DEFAULT_READY_SECS);
        if !(10..=600).contains(&ready_secs) {
            return Err("ready_secs is not from 10 to 600".to_owned());
        }
        let peers = entry
            .peers
            .iter()
            .map(|p| addr("peer", p))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            run,
            api: addr("api", &entry.api)?,
            drain_token_file: entry.drain_token_file,
            peers,
            drain_secs,
            ready_secs,
        })
    }
}
