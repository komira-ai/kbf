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
//! read_only = true                           # optional: --read-only
//! tmpfs = ["/tmp:rw,size=256m"]              # optional: --tmpfs
//! cap_drop = ["ALL"]                         # optional: --cap-drop
//! security_opt = ["no-new-privileges"]       # optional: --security-opt
//! ulimits = ["nofile=1048576:1048576"]       # optional: --ulimit
//! ports = ["127.0.0.1:8982:8982"]            # docker's --publish forms
//! mounts = ["/etc/kbf:/etc/kbf:ro"]          # docker's --volume forms
//! command = ["--config", "/etc/kbf/kbf.toml"]  # the flags after the image
//! api = "127.0.0.1:8982"                     # where this host reaches the operator API
//! drain_token_file = "/etc/kbf/operator.token"  # a path inside the container
//! peers = ["203.0.113.11:8982"]              # optional: the other frontends' operator APIs
//! drain_secs = 180                           # optional
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

/// How long a drain is waited for, unless the file says.
pub const DEFAULT_DRAIN_SECS: u64 = 180;

/// How long a container has to be ready, unless the file says.
pub const DEFAULT_READY_SECS: u64 = 90;

/// How long `docker stop` lets the process finish after it is told to.
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
    /// `--read-only`. Hardening options (this and the four below) are not written when unset,
    /// for the reason `group_add` is not: a host that sets none writes the record it always
    /// did. A host that sets any of them must settle or revert any release on trial before the
    /// helper is rolled back to a version without them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// `--tmpfs`, one `PATH[:OPTIONS]` per entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
    /// `--cap-drop`, one capability name (or `ALL`) per entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    /// `--security-opt`; only `no-new-privileges` is accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security_opt: Vec<String>,
    /// `--ulimit`, one `NAME=SOFT[:HARD]` per entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ulimits: Vec<String>,
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
        if self.read_only {
            args.push("--read-only".to_owned());
        }
        for (flag, values) in [
            ("--tmpfs", &self.tmpfs),
            ("--cap-drop", &self.cap_drop),
            ("--security-opt", &self.security_opt),
            ("--ulimit", &self.ulimits),
            ("--publish", &self.ports),
            ("--volume", &self.mounts),
        ] {
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

/// What a described container service has instead of the frontend's fixed habits: the
/// repository its images are loaded as, the paths its probes `GET`, and whether it is drained.
///
/// The frontend's own configuration has none of this (`service` is `None`, and is not written
/// to its record), so the frontend's record is the bytes it always was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicePart {
    /// The images are loaded as `<repository>:<commit>`.
    pub repository: String,
    /// The path `GET` on `api` that must answer 2xx.
    pub ready_path: String,
    /// The path `GET` on `api` whose JSON `commit` must name the release, if the service has
    /// one.
    pub version_path: Option<String>,
    /// Whether the container is asked to drain (`drain_token_file`, `drain_secs`) before it is
    /// stopped.
    pub drain: bool,
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
    pub drain_secs: u64,
    pub ready_secs: u64,
    /// Set for a container service of a descriptor; the frontend's has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServicePart>,
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
    read_only: bool,
    #[serde(default)]
    tmpfs: Vec<String>,
    #[serde(default)]
    cap_drop: Vec<String>,
    #[serde(default)]
    security_opt: Vec<String>,
    #[serde(default)]
    ulimits: Vec<String>,
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

/// A `--tmpfs` entry: an absolute path of `A-Z a-z 0-9 _ . -` and `/` without a `..` part,
/// then optionally `:` and comma-separated options from a short list. `exec`, `suid` and `dev`
/// are not on it: they would loosen what docker's default for a tmpfs (`noexec,nosuid,nodev`)
/// holds shut.
fn tmpfs_ok(text: &str) -> bool {
    let (path, options) = match text.split_once(':') {
        Some((path, options)) => (path, Some(options)),
        None => (text, None),
    };
    let path_ok = path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b))
        && !path.split('/').any(|part| part == "..");
    let size = |v: &str| {
        let digits = v.strip_suffix(['k', 'm', 'g']).unwrap_or(v);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    let option_ok = |option: &str| match option.split_once('=') {
        None => matches!(option, "ro" | "rw" | "noexec" | "nosuid" | "nodev"),
        Some(("size", v)) => size(v),
        Some(("mode", v)) => {
            !v.is_empty() && v.len() <= 4 && v.bytes().all(|b| (b'0'..=b'7').contains(&b))
        }
        Some(("uid" | "gid" | "nr_inodes", v)) => {
            !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit())
        }
        Some(_) => false,
    };
    path_ok && options.is_none_or(|o| o.split(',').all(option_ok))
}

/// A capability name for `--cap-drop`: `ALL` or upper-case letters, digits and `_` (`NET_RAW`,
/// `CAP_NET_RAW`).
fn cap_ok(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_uppercase())
        && text.len() <= 32
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// A `--ulimit` entry: `NAME=SOFT` or `NAME=SOFT:HARD`, NAME one of the resources docker
/// knows, the limits decimal digits with the soft limit not above the hard one.
fn ulimit_ok(text: &str) -> bool {
    let Some((name, limits)) = text.split_once('=') else {
        return false;
    };
    let known = matches!(
        name,
        "core"
            | "cpu"
            | "data"
            | "fsize"
            | "locks"
            | "memlock"
            | "msgqueue"
            | "nice"
            | "nofile"
            | "nproc"
            | "rss"
            | "rtprio"
            | "rttime"
            | "sigpending"
            | "stack"
    );
    let number = |v: &str| -> Option<u64> {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        v.parse().ok()
    };
    let (soft, hard) = match limits.split_once(':') {
        Some((soft, hard)) => (number(soft), number(hard).map(Some)),
        None => (number(limits), Some(None)),
    };
    let ordered = match (soft, hard) {
        (Some(soft), Some(hard)) => hard.is_none_or(|hard| soft <= hard),
        _ => false,
    };
    known && ordered
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
    if !entry.tmpfs.iter().all(|t| tmpfs_ok(t)) {
        return Err(
            "tmpfs is not /PATH or /PATH:OPTIONS, with options from ro, rw, noexec, nosuid, nodev, size=, mode=, uid=, gid=, nr_inodes="
                .to_owned(),
        );
    }
    if !entry.cap_drop.iter().all(|c| cap_ok(c)) {
        return Err("cap_drop is not ALL or a capability name in capitals".to_owned());
    }
    // Only the option that tightens: seccomp, apparmor and label values (`unconfined`,
    // `disable`) and `no-new-privileges=false` would loosen what docker holds by default.
    if !entry.security_opt.iter().all(|o| {
        matches!(
            o.as_str(),
            "no-new-privileges" | "no-new-privileges=true" | "no-new-privileges:true"
        )
    }) {
        return Err("security_opt is not no-new-privileges (an option that loosens the container is refused)".to_owned());
    }
    if !entry.ulimits.iter().all(|u| ulimit_ok(u)) {
        return Err(
            "ulimits is not NAME=SOFT or NAME=SOFT:HARD, in digits, for a resource docker knows"
                .to_owned(),
        );
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

/// The run keys as a file spells them, before they are checked.
pub struct RawRun {
    pub name: String,
    pub restart: String,
    pub network: Option<String>,
    pub user: Option<String>,
    pub group_add: Vec<String>,
    pub read_only: bool,
    pub tmpfs: Vec<String>,
    pub cap_drop: Vec<String>,
    pub security_opt: Vec<String>,
    pub ulimits: Vec<String>,
    pub ports: Vec<String>,
    pub mounts: Vec<String>,
    pub command: Vec<String>,
}

impl RawRun {
    /// The run configuration these keys make.
    ///
    /// # Errors
    /// The line that says which value is not one.
    pub fn into_spec(self) -> Result<RunSpec, String> {
        let group_add = self
            .group_add
            .iter()
            .map(|g| gid(g).ok_or_else(|| format!("group_add {g:?} is not a group id from 1")))
            .collect::<Result<_, _>>()?;
        let run = RunSpec {
            name: self.name,
            restart: self.restart,
            network: self.network,
            user: self.user,
            group_add,
            read_only: self.read_only,
            tmpfs: self.tmpfs,
            cap_drop: self.cap_drop,
            security_opt: self.security_opt,
            ulimits: self.ulimits,
            ports: self.ports,
            mounts: self.mounts,
            command: self.command,
        };
        check(&run)?;
        Ok(run)
    }
}

/// The drain and ready waits, with the defaults for the ones left out.
///
/// # Errors
/// The line that says which is out of range.
pub fn waits(drain: Option<u64>, ready: Option<u64>) -> Result<(u64, u64), String> {
    let drain_secs = drain.unwrap_or(DEFAULT_DRAIN_SECS);
    if drain_secs > 1800 {
        return Err("drain_secs is more than 1800".to_owned());
    }
    let ready_secs = ready.unwrap_or(DEFAULT_READY_SECS);
    if !(10..=600).contains(&ready_secs) {
        return Err("ready_secs is not from 10 to 600".to_owned());
    }
    Ok((drain_secs, ready_secs))
}

/// Whether `text` is an absolute path without control characters (a file inside the
/// container).
pub fn absolute_plain(text: &str) -> bool {
    text.starts_with('/') && plain(text)
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
        let file: File = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        let entry = file.frontend;
        let run = RawRun {
            name: entry.name,
            restart: entry.restart,
            network: entry.network,
            user: entry.user,
            group_add: entry.group_add,
            read_only: entry.read_only,
            tmpfs: entry.tmpfs,
            cap_drop: entry.cap_drop,
            security_opt: entry.security_opt,
            ulimits: entry.ulimits,
            ports: entry.ports,
            mounts: entry.mounts,
            command: entry.command,
        }
        .into_spec()?;
        if !absolute_plain(&entry.drain_token_file) {
            return Err("drain_token_file is not an absolute path".to_owned());
        }
        let (drain_secs, ready_secs) = waits(entry.drain_secs, entry.ready_secs)?;
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
            service: None,
        })
    }

    /// The most a switch takes before it is on the new container or back on the old one:
    /// the drain, the stop, and a wait for readiness. It has to fit the window of a trial.
    pub fn switch_secs(&self) -> u64 {
        self.drain_secs + STOP_SECS + self.ready_secs
    }
}
