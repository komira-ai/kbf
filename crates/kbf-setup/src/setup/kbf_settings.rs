//! The `kbf.toml` settings `setup` takes as named flags, for the hosts that do not enroll.
//!
//! A node mode (`storage`, `daemon`, `storage-and-daemon`) gets its listeners, certificates and
//! storage URL from enrollment, so its file is complete with the mode, the frontend URL and
//! the token path. A `frontend` host serves the public REAPI listener, the worker listener and
//! the operator API and proxies blobs to a storage node. An `all-in-one` host is always behind
//! the proxy: its public listeners and certificates are the proxy's, its `kbf.toml` is the
//! `storage-and-daemon` sibling's (the cold store, the metadata directory and the daemon
//! half's identity, and a loopback listener `setup` adds from `--storage-port`), and the
//! operator API, the state database and the sibling as storage node are the frontend child's
//! `frontend.toml`, which `plan.rs` derives from `--database` and `--storage-port` and not from
//! flags of this table. Their certificates and paths are the operator's, and those are the flags
//! here. A flag sets the `kbf` flag of its key's name (a few carry a longer name for the setup
//! command line: `--storage-url` is `storage`, `--daemon-cert` is `cert`) and is written as that
//! key in the table of its group (`crates/kbf-cli/src/config.rs`, `GROUPS`). A test checks every
//! `(table, key)` against `kbf`'s own per-mode goldens, so a renamed or moved flag fails there.
//!
//! The rules, all checked before the host is touched:
//!
//! - an address is `IP:PORT`;
//! - a file flag names a file by absolute path, never holds its contents: PEM text in a flag
//!   is refused, and so is a URL with a user name or password in it;
//! - a certificate, its key and its CA come together (the REAPI pair; the worker trio; the
//!   storage client trio with an `https` storage URL, none with `http`, which `kbf` allows
//!   only to a loopback host);
//! - a file must exist and be what its kind needs ([`check_files`]): a key is a regular file
//!   (a link is not followed) owned by the service user with no group or other bits and
//!   readable by its owner, as `kbf` requires; a certificate or CA is a regular file owned by
//!   root or the service user that neither group nor others can write;
//! - what the mode cannot start without is required: a frontend its storage URL, an
//!   all-in-one host its cold store, metadata directory and the daemon half's identity.
//!
//! `setup` also writes what the mode needs and has one answer for: on a frontend, the operator
//! API address (default `127.0.0.1:8982`, as `kbf` documents; the token file `setup` writes
//! requires it) and, for `all-in-one`, `driver = "container"`, the only driver it accepts.
//!
//! No message carries a value, only the flag and the rule: a path or a URL a person typed can
//! hold a secret, and a log keeps what is printed.

use std::net::{IpAddr, SocketAddr};

use kbf_host::{Exec, HostPath, Kind};

use super::SetupError;
use crate::cli::Mode;

/// One key `setup` writes into `kbf.toml`: `[table] key = "value"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub table: &'static str,
    pub key: &'static str,
    pub value: String,
}

/// What a flag's value must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    /// `IP:PORT`.
    Addr,
    /// A certificate or CA file: public.
    Cert,
    /// A private key file.
    Key,
    /// `http(s)://host[:port]`; plain `http` only to a loopback host.
    Url,
    /// `https://host[:port]`.
    Https,
    /// `file:///absolute/path`.
    FileUri,
    /// An absolute directory path.
    Dir,
    /// A node id: a DNS label.
    NodeId,
}

/// Which modes take a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Takes {
    /// `frontend` and `all-in-one`: the files the proxy reads (an all-in-one host is always
    /// behind it, so on that host these move to `[proxy]` and are not written to its `kbf.toml`).
    Serving,
    /// `frontend` only.
    Frontend,
    /// `all-in-one` only.
    AllInOne,
}

impl Takes {
    fn mode(self, mode: Mode) -> bool {
        match self {
            Takes::Serving => matches!(mode, Mode::Frontend | Mode::AllInOne),
            Takes::Frontend => mode == Mode::Frontend,
            Takes::AllInOne => mode == Mode::AllInOne,
        }
    }

    fn who(self) -> &'static str {
        match self {
            Takes::Serving => "the modes that serve the API (frontend, all-in-one)",
            Takes::Frontend => "--mode=frontend",
            Takes::AllInOne => "--mode=all-in-one",
        }
    }
}

/// Where a setting's value comes from when no flag gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Need {
    /// Written only when the flag is given.
    Optional,
    /// The mode cannot start without it: the flag is required.
    Required,
    /// This value when the flag is not given.
    Default(&'static str),
    /// Always this value, for the modes that take it; there is no flag.
    Fixed(&'static str),
}

struct Spec {
    /// The setup flag, if there is one.
    flag: Option<&'static str>,
    /// The table of the key's group in `kbf`.
    table: &'static str,
    /// The key: the name of the `kbf` flag.
    key: &'static str,
    kind: Want,
    takes: Takes,
    need: Need,
}

// Macros, not `const fn`s: the table is a constant, and a function run only at compile time
// has no coverage.
macro_rules! spec {
    ($flag:expr, $table:expr, $key:expr, $kind:expr, $takes:expr, $need:expr $(,)?) => {
        Spec {
            flag: $flag,
            table: $table,
            key: $key,
            kind: $kind,
            takes: $takes,
            need: $need,
        }
    };
}

/// A setting whose flag has the name of its key.
macro_rules! same {
    ($name:expr, $table:expr, $kind:expr, $takes:expr, $need:expr $(,)?) => {
        spec!(Some($name), $table, $name, $kind, $takes, $need)
    };
}

/// A setting whose flag is named differently from its key.
macro_rules! renamed {
    ($flag:expr, $table:expr, $key:expr, $kind:expr, $takes:expr, $need:expr $(,)?) => {
        spec!(Some($flag), $table, $key, $kind, $takes, $need)
    };
}

/// The settings, in the order of the written file within each table.
const SPECS: [Spec; 20] = [
    same!(
        "listen",
        "listener",
        Want::Addr,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "worker-listen",
        "listener",
        Want::Addr,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "api-listen",
        "listener",
        Want::Addr,
        Takes::Frontend,
        Need::Default("127.0.0.1:8982"),
    ),
    same!(
        "reapi-tls-cert",
        "tls",
        Want::Cert,
        Takes::Serving,
        Need::Optional,
    ),
    same!(
        "reapi-tls-key",
        "tls",
        Want::Key,
        Takes::Serving,
        Need::Optional,
    ),
    same!(
        "worker-tls-cert",
        "tls",
        Want::Cert,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "worker-tls-key",
        "tls",
        Want::Key,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "worker-client-ca",
        "tls",
        Want::Cert,
        Takes::Serving,
        Need::Optional,
    ),
    renamed!(
        "storage-url",
        "storage-client",
        "storage",
        Want::Url,
        Takes::Frontend,
        Need::Required,
    ),
    same!(
        "storage-ca",
        "storage-client",
        Want::Cert,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "storage-client-cert",
        "storage-client",
        Want::Cert,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "storage-client-key",
        "storage-client",
        Want::Key,
        Takes::Frontend,
        Need::Optional,
    ),
    same!(
        "cold-store",
        "stores",
        Want::FileUri,
        Takes::AllInOne,
        Need::Required,
    ),
    same!(
        "meta-dir",
        "stores",
        Want::Dir,
        Takes::AllInOne,
        Need::Required,
    ),
    renamed!(
        "daemon-server",
        "identity",
        "server",
        Want::Https,
        Takes::AllInOne,
        Need::Required,
    ),
    renamed!(
        "daemon-ca-cert",
        "identity",
        "ca-cert",
        Want::Cert,
        Takes::AllInOne,
        Need::Required,
    ),
    renamed!(
        "daemon-cert",
        "identity",
        "cert",
        Want::Cert,
        Takes::AllInOne,
        Need::Required,
    ),
    renamed!(
        "daemon-key",
        "identity",
        "key",
        Want::Key,
        Takes::AllInOne,
        Need::Required,
    ),
    same!(
        "node-id",
        "identity",
        Want::NodeId,
        Takes::AllInOne,
        Need::Required,
    ),
    spec!(
        None,
        "daemon",
        "driver",
        Want::Dir,
        Takes::AllInOne,
        Need::Fixed("container"),
    ),
];

/// The flags a `--proxy` host does not take: the listeners (the proxy owns the public address
/// and gives each child loopback ports on its command line) and the worker listener's
/// certificate pair (the children serve plain h2c).
const PROXY_REFUSED: [&str; 5] = [
    "listen",
    "worker-listen",
    "api-listen",
    "worker-tls-cert",
    "worker-tls-key",
];

/// Whether an all-in-one host of the proxy layout writes `[table] key` somewhere: in the sibling's
/// `kbf.toml`, or, for the REAPI pair and the worker client CA, in the `[proxy]` table.
pub fn all_in_one_writes(table: &str, key: &str) -> bool {
    SPECS
        .iter()
        .any(|s| s.takes.mode(Mode::AllInOne) && s.table == table && s.key == key)
}

/// The `(table, key)` pairs `mode` writes.
pub fn written(mode: Mode) -> Vec<(&'static str, &'static str)> {
    SPECS
        .iter()
        .filter(|s| s.takes.mode(mode))
        .map(|s| (s.table, s.key))
        .collect()
}

/// The flags as the command line gave them. Each sets the `kbf` key of its name, in the
/// `kbf.toml` of a `frontend` or `all-in-one` host.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct KbfFlags {
    /// The public REAPI listener, as IP:PORT (kbf's --listen). frontend.
    #[arg(long)]
    pub listen: Option<String>,
    /// The worker listener (daemons and storage nodes dial it), as IP:PORT. frontend.
    #[arg(long)]
    pub worker_listen: Option<String>,
    /// The operator API listener, as IP:PORT; default 127.0.0.1:8982. frontend.
    #[arg(long)]
    pub api_listen: Option<String>,
    /// The REAPI listener's certificate (PEM file, absolute path); needs --reapi-tls-key.
    #[arg(long)]
    pub reapi_tls_cert: Option<String>,
    /// The REAPI listener's private key (a file of mode 0600 owned by the service user).
    #[arg(long)]
    pub reapi_tls_key: Option<String>,
    /// The worker listener's certificate; needs --worker-tls-key and --worker-client-ca.
    #[arg(long)]
    pub worker_tls_cert: Option<String>,
    /// The worker listener's private key (a file of mode 0600 owned by the service user).
    #[arg(long)]
    pub worker_tls_key: Option<String>,
    /// The CA that daemons' and storage nodes' certificates must chain to.
    #[arg(long)]
    pub worker_client_ca: Option<String>,
    /// The storage node the frontend proxies blobs to (kbf's --storage), as
    /// http(s)://host[:port] with nothing after it. Required for frontend. https needs
    /// --storage-ca, --storage-client-cert and --storage-client-key; http is for a loopback
    /// host only.
    #[arg(long)]
    pub storage_url: Option<String>,
    /// The CA the storage node's certificate chains to. frontend.
    #[arg(long)]
    pub storage_ca: Option<String>,
    /// The frontend's certificate toward the storage node. frontend.
    #[arg(long)]
    pub storage_client_cert: Option<String>,
    /// The private key of --storage-client-cert (a file of mode 0600 owned by the service
    /// user). frontend.
    #[arg(long)]
    pub storage_client_key: Option<String>,
    /// The cold store, as file:///absolute/path. Required for all-in-one.
    #[arg(long)]
    pub cold_store: Option<String>,
    /// The directory of the metadata log, an absolute path outside the data directory.
    /// Required for all-in-one.
    #[arg(long)]
    pub meta_dir: Option<String>,
    /// The worker listener the daemon half dials, as https://host[:port] (kbf's --server); the
    /// host must be a name of the worker certificate. Required for all-in-one.
    #[arg(long)]
    pub daemon_server: Option<String>,
    /// The CA of the worker certificate, for the daemon half (kbf's --ca-cert). Required for
    /// all-in-one.
    #[arg(long)]
    pub daemon_ca_cert: Option<String>,
    /// The daemon half's client certificate (kbf's --cert), role daemon. Required for
    /// all-in-one.
    #[arg(long)]
    pub daemon_cert: Option<String>,
    /// The private key of --daemon-cert (a file of mode 0600 owned by the service user).
    /// Required for all-in-one.
    #[arg(long)]
    pub daemon_key: Option<String>,
    /// The daemon half's node name, a DNS label unique in the cell. Required for all-in-one.
    #[arg(long)]
    pub node_id: Option<String>,
}

impl KbfFlags {
    /// The flags by name, as given.
    fn slots(&self) -> [(&'static str, &Option<String>); 19] {
        [
            ("listen", &self.listen),
            ("worker-listen", &self.worker_listen),
            ("api-listen", &self.api_listen),
            ("reapi-tls-cert", &self.reapi_tls_cert),
            ("reapi-tls-key", &self.reapi_tls_key),
            ("worker-tls-cert", &self.worker_tls_cert),
            ("worker-tls-key", &self.worker_tls_key),
            ("worker-client-ca", &self.worker_client_ca),
            ("storage-url", &self.storage_url),
            ("storage-ca", &self.storage_ca),
            ("storage-client-cert", &self.storage_client_cert),
            ("storage-client-key", &self.storage_client_key),
            ("cold-store", &self.cold_store),
            ("meta-dir", &self.meta_dir),
            ("daemon-server", &self.daemon_server),
            ("daemon-ca-cert", &self.daemon_ca_cert),
            ("daemon-cert", &self.daemon_cert),
            ("daemon-key", &self.daemon_key),
            ("node-id", &self.node_id),
        ]
    }

    /// The flags by name, to be filled.
    fn slots_mut(&mut self) -> [(&'static str, &mut Option<String>); 19] {
        [
            ("listen", &mut self.listen),
            ("worker-listen", &mut self.worker_listen),
            ("api-listen", &mut self.api_listen),
            ("reapi-tls-cert", &mut self.reapi_tls_cert),
            ("reapi-tls-key", &mut self.reapi_tls_key),
            ("worker-tls-cert", &mut self.worker_tls_cert),
            ("worker-tls-key", &mut self.worker_tls_key),
            ("worker-client-ca", &mut self.worker_client_ca),
            ("storage-url", &mut self.storage_url),
            ("storage-ca", &mut self.storage_ca),
            ("storage-client-cert", &mut self.storage_client_cert),
            ("storage-client-key", &mut self.storage_client_key),
            ("cold-store", &mut self.cold_store),
            ("meta-dir", &mut self.meta_dir),
            ("daemon-server", &mut self.daemon_server),
            ("daemon-ca-cert", &mut self.daemon_ca_cert),
            ("daemon-cert", &mut self.daemon_cert),
            ("daemon-key", &mut self.daemon_key),
            ("node-id", &mut self.node_id),
        ]
    }

    pub(super) fn get(&self, flag: &str) -> Option<&str> {
        self.slots()
            .into_iter()
            .find(|(name, _)| *name == flag)
            .and_then(|(_, value)| value.as_deref())
    }

    /// Gives each flag an all-in-one host takes that was not given the value `earlier` has for
    /// its key, `(table, key)`: the settings of the earlier layout's `kbf.toml` carried over,
    /// where a flag of this run takes precedence. A key that host had and this layout takes no
    /// flag for (the listeners and the worker listener's certificate, which the proxy owns) is
    /// not a flag to fill: `validate` refuses those with `--proxy`.
    pub fn fill_from(&mut self, earlier: impl Fn(&str, &str) -> Option<String>) {
        for spec in SPECS.iter().filter(|s| s.takes.mode(Mode::AllInOne)) {
            for (flag, slot) in self.slots_mut() {
                if spec.flag == Some(flag) && slot.is_none() {
                    *slot = earlier(spec.table, spec.key);
                }
            }
        }
    }

    /// Check the flags for `mode`: the settings to write, or a line per problem pushed to
    /// `problems`. Files are checked later ([`check_files`]); this needs no host.
    ///
    /// On a `--proxy` host (`proxy`) the children are told their loopback listeners by the
    /// proxy and speak plain h2c behind it, so the listener flags and the worker listener's
    /// certificate are refused (a value nothing would read); the REAPI pair and the worker
    /// client CA are the proxy's, and come back among the settings for the caller to move
    /// out of `kbf.toml`.
    pub fn validate(&self, mode: Mode, proxy: bool, problems: &mut Vec<String>) -> Vec<Setting> {
        let name = super::validate::mode_name(mode);
        let mut out = Vec::new();
        let mut bad = false;
        for s in &SPECS {
            let given = s.flag.and_then(|f| self.get(f));
            if let (true, Some(flag), Some(_)) = (proxy, s.flag, given)
                && PROXY_REFUSED.contains(&flag)
            {
                problems.push(format!(
                    "--{flag} is not taken with --proxy: the proxy tells its children their loopback listeners and they serve plain h2c behind it"
                ));
                bad = true;
                continue;
            }
            if !s.takes.mode(mode) {
                if let (Some(flag), Some(_)) = (s.flag, given) {
                    problems.push(format!(
                        "--{flag} is for {}; --mode={name} does not take it",
                        s.takes.who()
                    ));
                    bad = true;
                }
                continue;
            }
            if let Need::Fixed(v) = s.need {
                out.push(Setting {
                    table: s.table,
                    key: s.key,
                    value: v.to_owned(),
                });
                continue;
            }
            let value = match (given, s.need) {
                (Some(v), _) => v,
                (None, Need::Default(v)) => v,
                (None, Need::Required) => {
                    problems.push(format!(
                        "--{}: required for --mode={name}",
                        s.flag.unwrap_or(s.key)
                    ));
                    bad = true;
                    continue;
                }
                (None, _) => continue,
            };
            match check_value(s.kind, value) {
                Ok(()) => out.push(Setting {
                    table: s.table,
                    key: s.key,
                    value: value.to_owned(),
                }),
                Err(rule) => {
                    problems.push(format!("--{}: {rule}", s.flag.unwrap_or(s.key)));
                    bad = true;
                }
            }
        }
        if !bad {
            self.together(mode, proxy, problems);
        }
        out
    }

    /// The flags that only work in company.
    fn together(&self, mode: Mode, proxy: bool, problems: &mut Vec<String>) {
        let groups: [&[&str]; 2] = [
            &["reapi-tls-cert", "reapi-tls-key"],
            &["worker-tls-cert", "worker-tls-key", "worker-client-ca"],
        ];
        // With the proxy the worker group is not one: the client CA is the proxy's alone.
        for group in groups.iter().take(if proxy { 1 } else { 2 }) {
            let given = group.iter().filter(|f| self.get(f).is_some()).count();
            if given != 0 && given != group.len() {
                problems.push(format!(
                    "{}: give all of them or none",
                    group
                        .iter()
                        .map(|f| format!("--{f}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if mode != Mode::Frontend {
            return;
        }
        let client = ["storage-ca", "storage-client-cert", "storage-client-key"];
        let given = client.iter().filter(|f| self.get(f).is_some()).count();
        let https = self.get("storage-url").map(|u| u.starts_with("https://"));
        match https {
            Some(true) if given != client.len() => problems.push(
                "--storage-url with https needs --storage-ca, --storage-client-cert and --storage-client-key"
                    .to_owned(),
            ),
            Some(false) if given != 0 => problems.push(
                "--storage-ca, --storage-client-cert and --storage-client-key are for an https --storage-url"
                    .to_owned(),
            ),
            _ => {}
        }
    }
}

/// The rule a value breaks, without the value.
fn check_value(kind: Want, value: &str) -> Result<(), &'static str> {
    match kind {
        Want::Addr => value
            .parse::<SocketAddr>()
            .map_err(|_| "must be IP:PORT, as 0.0.0.0:8980 or [::1]:8980")
            .and_then(|a| {
                if a.port() == 0 {
                    Err("must have a port from 1 to 65535")
                } else {
                    Ok(())
                }
            }),
        Want::Cert | Want::Key => check_path(value),
        Want::Dir => check_path(value),
        Want::Url => check_url(value, false),
        Want::Https => check_url(value, true),
        Want::FileUri => match value.strip_prefix("file://") {
            Some(path) if path.starts_with('/') => check_path(path),
            Some(_) => Err("must be file:///absolute/path"),
            None if value.starts_with("s3://") => Err(
                "an s3:// store needs credentials in the environment, which setup does not manage: use file:///absolute/path",
            ),
            None => Err("must be file:///absolute/path"),
        },
        Want::NodeId => check_node_id(value),
    }
}

/// A path of plain characters. Text that looks like what a file holds is called that.
pub(super) fn check_path(value: &str) -> Result<(), &'static str> {
    if value.contains("-----") || value.contains('\n') || value.len() > 4096 {
        return Err("must be the path of a file, not its contents: secrets stay in files");
    }
    let plain = value
        .bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
    if !plain || HostPath::new(value).is_err() {
        return Err("must be an absolute path of plain characters (no space, quote or backslash)");
    }
    Ok(())
}

/// A DNS label, as `kbf` makes a node id: `[a-z0-9-]`, 1 to 63 bytes, no `-` at either end.
fn check_node_id(value: &str) -> Result<(), &'static str> {
    let ok = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err("must be a DNS label: a-z, 0-9 and -, 1 to 63 bytes, no - at either end")
    }
}

/// Whether `rest` is a host with a lone trailing `/` and nothing else odd.
fn plain_but_slash(rest: &str) -> bool {
    rest.trim_end_matches('/')
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// `http://host[:port]` or `https://host[:port]` (`https` only when `tls_only`), with no
/// user, password, path, query or fragment. Plain `http` is for a loopback host, as `kbf`
/// refuses it anywhere else.
fn check_url(value: &str, tls_only: bool) -> Result<(), &'static str> {
    let (secure, rest) = match value.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None if tls_only => return Err("must be an https:// URL"),
        None => (
            false,
            value
                .strip_prefix("http://")
                .ok_or("must be an http:// or https:// URL")?,
        ),
    };
    if rest.contains('@') {
        return Err("must carry no user name or password: credentials never go in kbf.toml");
    }
    let plain = rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    if rest.ends_with('/') && plain_but_slash(rest) {
        return Err("must have no trailing slash: http(s)://host[:port] and nothing after it");
    }
    if rest.is_empty() || !plain {
        return Err("must be http(s)://host[:port] and nothing after it");
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !p.ends_with(']') => (h, Some(p)),
        _ => (rest, None),
    };
    if host.is_empty() || port.is_some_and(|p| p.parse::<u16>().map_or(true, |p| p == 0)) {
        return Err("must be http(s)://host[:port] with a port from 1 to 65535");
    }
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !secure && !loopback {
        return Err(
            "plain text http:// is for a loopback host only: use https:// with the client certificate flags",
        );
    }
    Ok(())
}

/// Check the files the settings name against the host: each exists, is a regular file (a
/// link is not followed), and has the owner and mode its kind needs. `service` is the uid of
/// the user the mode's unit runs as. Every problem is reported; a file's contents are never
/// read.
pub fn check_files(
    exec: &dyn Exec,
    settings: &[Setting],
    service: Option<u32>,
) -> Result<(), SetupError> {
    let mut problems = Vec::new();
    push_problems(exec, settings, service, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(SetupError::Invalid(problems))
    }
}

/// [`check_files`], adding each problem to `problems`, so a host whose files belong to two
/// users is checked whole: every problem of both is reported at once.
pub fn push_problems(
    exec: &dyn Exec,
    settings: &[Setting],
    service: Option<u32>,
    problems: &mut Vec<String>,
) {
    for s in SPECS
        .iter()
        .filter(|s| matches!(s.kind, Want::Cert | Want::Key))
    {
        let Some(setting) = settings
            .iter()
            .find(|setting| setting.table == s.table && setting.key == s.key)
        else {
            continue;
        };
        if let Err(rule) = check_file(exec, s.kind, &setting.value, service) {
            problems.push(format!("--{}: {rule}", s.flag.unwrap_or(s.key)));
        }
    }
}

fn check_file(
    exec: &dyn Exec,
    kind: Want,
    path: &str,
    service: Option<u32>,
) -> Result<(), &'static str> {
    let path = HostPath::new(path).map_err(|_| "not an absolute path")?;
    let meta = exec
        .stat(&path)
        .map_err(|_| "cannot be looked at")?
        .ok_or("no such file")?;
    if meta.kind != Kind::File {
        return Err("is not a regular file (a symbolic link is not followed)");
    }
    let bits = meta.perms.bits();
    let service_owns = service == Some(meta.owner.uid);
    match kind {
        Want::Key => {
            if !service_owns {
                return Err("a key must be owned by the user the service runs as");
            }
            // As `kbf`'s preflight: no group or other bit, and the owner can read it.
            if bits & 0o077 != 0 || bits & 0o400 == 0 {
                return Err(
                    "a key must have no group or other permission bits and be readable by its owner (mode 0600 or 0400)",
                );
            }
        }
        _ => {
            if !service_owns && meta.owner.uid != 0 {
                return Err("must be owned by root or the user the service runs as");
            }
            if bits & 0o022 != 0 {
                return Err("must not be writable by its group or by others");
            }
        }
    }
    Ok(())
}
