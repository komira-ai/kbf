//! A service the helper installs releases of: kbf, and the other services a root-owned
//! configuration describes.
//!
//! kbf's own descriptor is built in ([`Descriptor::kbf`]) and is what every request without
//! a service has always meant. Any other service is described by a `[services.<name>]` table
//! of the same file as the channels ([`crate::mac::channel`]):
//!
//! ```toml
//! [services.js-agent]
//! artifact = "js-agent"            # the start of its release files' names
//! binary = "js-agent"              # the file of the tarball that is the program
//! tree = "js-agent"                # its releases are in /var/lib/<tree> (a Mac: /usr/local/<tree>)
//! units = ["js-agent.service"]     # restarted on a switch and on a revert
//! ```
//!
//! Every key is required, and none has a default. (A service that runs as a container says
//! `kind = "container"` and has a `container` table instead of `binary` and `units`; see
//! [`ContainerEntry`].) A descriptor can name only what is its
//! own: the tree is the service's name or starts with it and a dash, and so is the base of
//! each unit, so a service cannot be made to restart `sshd.service`, to write under another
//! service's tree, or to take over kbf's (`kbf` and the names that start with `kbf-` are
//! kbf's).

use std::net::SocketAddr;

use serde::Deserialize;

use crate::local::frontend::config::{FrontendConfig, RawRun, ServicePart, absolute_plain, waits};
use crate::mac::release::is_artifact_prefix;
use crate::request::{BadCommand, Unit};

/// The service every request without a service means.
pub const KBF: &str = "kbf";

/// The tree of kbf's own releases (`kbf_installer::layout::DEFAULT_TREE`; a test holds the
/// two together).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const KBF_TREE: &str = "kbf-installer";

/// The longest service name, in bytes.
pub const MAX_NAME_LEN: usize = 32;

/// A service name: `[a-z][a-z0-9-]{0,31}`, not ending in `-`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ServiceName(String);

impl ServiceName {
    /// Check `text` against the service grammar.
    ///
    /// # Errors
    /// [`BadCommand`] when it is not in the grammar.
    pub fn new(text: &str) -> Result<Self, BadCommand> {
        let mut bytes = text.bytes();
        let sound = text.len() <= MAX_NAME_LEN
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !text.ends_with('-');
        if sound {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadCommand)
        }
    }

    /// kbf.
    pub fn kbf() -> Self {
        Self(KBF.to_owned())
    }

    /// Whether this is kbf.
    pub fn is_kbf(&self) -> bool {
        self.0 == KBF
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a service is run: by units (the default), or as one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Process,
    Container,
}

/// The `[services.<name>.container]` table of a service that runs as a container: the run
/// configuration (the keys of the frontend's), how readiness is told, and the optional drain.
///
/// ```toml
/// [services.js-server]
/// kind = "container"
/// artifact = "js-server"         # the image file is js-server-image-<commit>-linux-<arch>.oci.tar
/// tree = "js-server"             # scratch and checks, below /var/lib/<tree>
/// [services.js-server.container]
/// name = "js-server"             # the container's name: the service's or starts with it and a dash
/// restart = "unless-stopped"
/// user = "65532:65532"           # required, and not root
/// ports = ["127.0.0.1:8080:8080"]
/// mounts = ["/etc/js:/etc/js:ro"]
/// command = ["--config", "/etc/js/server.toml"]
/// ready_url = "http://127.0.0.1:8080/readyz"     # must answer 2xx within ready_secs
/// version_url = "http://127.0.0.1:8080/version"  # optional: JSON with the release's "commit"
/// drain_token_file = "/etc/js/operator.token"    # optional: set, the operator drain runs first
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerEntry {
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
    ready_url: String,
    version_url: Option<String>,
    drain_token_file: Option<String>,
    drain_secs: Option<u64>,
    ready_secs: Option<u64>,
}

/// The `[services.<name>]` table, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    kind: Option<Kind>,
    artifact: String,
    binary: Option<String>,
    tree: String,
    units: Option<Vec<String>>,
    container: Option<ContainerEntry>,
}

/// What the helper needs to know of a service to install its releases. (The Linux backend
/// reads most of it.)
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Descriptor {
    pub name: ServiceName,
    /// The start of the names of the service's release files (`<artifact>-<commit>-...`).
    pub artifact: String,
    /// The file of the release tarball that is the program, and the file of the release
    /// directory it is installed as.
    pub binary: String,
    /// The name of the tree the service's releases, links, state and lock are in.
    pub tree: String,
    /// The units restarted on a switch and on a revert (none for a container).
    pub units: Vec<Unit>,
    /// What a service that runs as a container is made of and how it is probed; none for a
    /// service of units.
    pub container: Option<FrontendConfig>,
}

impl Descriptor {
    /// kbf's own: its release files are `kbf-<commit>-...`, the program is `kbf`, the tree is
    /// the installer's, and the units are the ones the helper was started with.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn kbf(units: Vec<Unit>) -> Self {
        Self {
            name: ServiceName::kbf(),
            artifact: KBF.to_owned(),
            binary: KBF.to_owned(),
            tree: KBF_TREE.to_owned(),
            units,
            container: None,
        }
    }

    /// The descriptor `entry` makes for the service `name`.
    ///
    /// # Errors
    /// Which part is not allowed, in words.
    pub fn from_entry(name: &str, entry: &Entry) -> Result<Self, String> {
        let service = ServiceName::new(name)
            .map_err(|BadCommand| format!("{name:?} is not a service name"))?;
        if service.is_kbf() || name.starts_with("kbf-") {
            return Err(format!(
                "{name:?} is kbf's: a service of its own has another name"
            ));
        }
        if !is_artifact_prefix(&entry.artifact) {
            return Err(format!(
                "{name}: {:?} is not an artifact name",
                entry.artifact
            ));
        }
        if !is_tree_of(name, &entry.tree) {
            return Err(format!(
                "{name}: the tree {:?} must be {name} or start with {name}-",
                entry.tree
            ));
        }
        let kind = entry.kind.unwrap_or(Kind::Process);
        if kind == Kind::Container {
            return Self::container(service, entry);
        }
        if entry.container.is_some() {
            return Err(format!(
                "{name}: a container table needs kind = \"container\""
            ));
        }
        let (Some(binary), Some(listed)) = (&entry.binary, &entry.units) else {
            return Err(format!("{name}: binary and units are required"));
        };
        if !is_member(binary) {
            return Err(format!("{name}: {binary:?} is not a file name"));
        }
        if listed.is_empty() {
            return Err(format!("{name}: no unit to restart"));
        }
        let units = listed
            .iter()
            .map(|text| {
                Unit::owned_by(name, text)
                    .map_err(|BadCommand| format!("{name}: {text:?} is not one of its units"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: service,
            artifact: entry.artifact.clone(),
            binary: binary.clone(),
            tree: entry.tree.clone(),
            units,
            container: None,
        })
    }

    /// The descriptor of a service that runs as a container.
    fn container(service: ServiceName, entry: &Entry) -> Result<Self, String> {
        let name = service.as_str();
        if entry.binary.is_some() || entry.units.is_some() {
            return Err(format!("{name}: a container has no binary and no units"));
        }
        let Some(table) = &entry.container else {
            return Err(format!(
                "{name}: kind = \"container\" needs a container table"
            ));
        };
        let config = container_config(name, &entry.artifact, table.clone())
            .map_err(|why| format!("{name}: {why}"))?;
        Ok(Self {
            name: service,
            artifact: entry.artifact.clone(),
            binary: entry.artifact.clone(),
            tree: entry.tree.clone(),
            units: Vec::new(),
            container: Some(config),
        })
    }
}

/// `http://<loopback ip>:<port>/<path>`: where this host reaches the service, and the path.
fn loopback_url(what: &str, text: &str) -> Result<(SocketAddr, String), String> {
    let rest = text
        .strip_prefix("http://")
        .ok_or_else(|| format!("{what} is not an http:// URL"))?;
    let (authority, path) = rest
        .find('/')
        .map(|at| rest.split_at(at))
        .ok_or_else(|| format!("{what} has no path"))?;
    let addr: SocketAddr = authority
        .parse()
        .map_err(|_| format!("{what} does not name an IP address and a port"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("{what} is not on loopback"));
    }
    let plain = path.len() <= 128
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b))
        && !path.split('/').any(|part| part == "..");
    if !plain {
        return Err(format!(
            "the path of {what} is letters, digits and / _ . - only, without .."
        ));
    }
    Ok((addr, path.to_owned()))
}

/// A user that is not root: `NAME` or `ID`, and an optional `:GROUP`, neither of them root.
fn unprivileged(user: &str) -> bool {
    user.split(':').all(|part| !matches!(part, "root" | "0"))
}

/// Whether a bind mount's source may be given to a service's container: not the root, the
/// container runtime's sockets or a directory that holds them, nor the host's `/proc`, `/sys`
/// or `/dev`.
fn mount_source_ok(mount: &str) -> bool {
    let source = mount.split(':').next().unwrap_or("");
    let trimmed = source.trim_end_matches('/');
    let under = |dir: &str| trimmed == dir || trimmed.starts_with(&format!("{dir}/"));
    let sockets = trimmed.ends_with(".sock");
    !trimmed.is_empty()
        && !sockets
        && !source.split('/').any(|part| part == "..")
        && !["/run", "/var", "/var/run", "/proc", "/sys", "/dev"].contains(&trimmed)
        && !["/proc", "/sys", "/dev", "/run/docker", "/var/run/docker"]
            .iter()
            .any(|dir| under(dir))
        && !under("/var/lib/docker")
        && !under("/var/lib/containerd")
}

/// The configuration a service's container table makes, held to what the frontend's is and to
/// what a service must not ask for.
fn container_config(
    name: &str,
    artifact: &str,
    table: ContainerEntry,
) -> Result<FrontendConfig, String> {
    let (api, ready_path) = loopback_url("ready_url", &table.ready_url)?;
    let version_path = match &table.version_url {
        None => None,
        Some(url) => {
            let (at, path) = loopback_url("version_url", url)?;
            if at != api {
                return Err("version_url is not at the address of ready_url".to_owned());
            }
            Some(path)
        }
    };
    let drain_file = match (&table.drain_token_file, table.drain_secs) {
        (Some(file), _) if !absolute_plain(file) => {
            return Err("drain_token_file is not an absolute path".to_owned());
        }
        (None, Some(_)) => return Err("drain_secs needs drain_token_file".to_owned()),
        (file, _) => file.clone(),
    };
    let (drain_secs, ready_secs) = waits(table.drain_secs, table.ready_secs)?;
    if !(table.name == name
        || table
            .name
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('-')))
    {
        return Err(format!(
            "the container name {:?} must be {name} or start with {name}-",
            table.name
        ));
    }
    let Some(user) = table.user.as_deref() else {
        return Err("user is required: a service does not run as the image's default".to_owned());
    };
    if !unprivileged(user) {
        return Err("user is root".to_owned());
    }
    if table.network.as_deref() == Some("host") {
        return Err("network host gives the container the host's network".to_owned());
    }
    if !table.mounts.iter().all(|m| mount_source_ok(m)) {
        return Err(
            "a mount has the root, a runtime socket, /proc, /sys, /dev or a runtime directory as its source"
                .to_owned(),
        );
    }
    let run = RawRun {
        name: table.name,
        restart: table.restart,
        network: table.network,
        user: table.user,
        group_add: table.group_add,
        read_only: table.read_only,
        tmpfs: table.tmpfs,
        cap_drop: table.cap_drop,
        security_opt: table.security_opt,
        ulimits: table.ulimits,
        ports: table.ports,
        mounts: table.mounts,
        command: table.command,
    }
    .into_spec()?;
    let drain = drain_file.is_some();
    Ok(FrontendConfig {
        run,
        api,
        drain_token_file: drain_file.unwrap_or_default(),
        peers: Vec::new(),
        drain_secs: if drain { drain_secs } else { 0 },
        ready_secs,
        service: Some(ServicePart {
            repository: artifact.to_owned(),
            ready_path,
            version_path,
            drain,
        }),
    })
}

/// Whether `tree` is a tree name `name` may use: its own name, or that and a dash and more.
fn is_tree_of(name: &str, tree: &str) -> bool {
    let own = tree == name
        || tree
            .strip_prefix(name)
            .is_some_and(|rest| rest.strip_prefix('-').is_some_and(|more| !more.is_empty()));
    own && tree.len() <= 64
        && tree
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !tree.ends_with('-')
}

/// Whether `text` is a plain file name of a release: letters, digits and `. _ -`, a letter or
/// digit first, at most 64 bytes, and not the marker the pipeline writes into a release.
fn is_member(text: &str) -> bool {
    text.len() <= 64
        && text != crate::mac::release::VERIFIED_MARKER
        && text.starts_with(|c: char| c.is_ascii_alphanumeric())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(test)]
mod container_tests;
#[cfg(test)]
mod tests;
