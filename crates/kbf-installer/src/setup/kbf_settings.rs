//! The `kbf.toml` settings `setup` takes as named flags, for the hosts that do not enroll.
//!
//! A node mode (`storage`, `daemon`, `storage-and-daemon`) gets its listeners, certificates and
//! storage URL from enrollment, so its file is complete with the mode, the frontend URL and
//! the token path. A `frontend` or `all-in-one` host serves the public REAPI listener, the
//! worker listener and the operator API, with certificates the operator provides; those are the
//! flags here. Each flag has the name of the `kbf` flag it sets, and `setup` writes it as that
//! flag's key in the table of its group (`crates/kbf-cli/src/config.rs`, `GROUPS`; a test
//! checks every key against the per-mode goldens, so a renamed flag fails there).
//!
//! The rules, all checked before the host is touched:
//!
//! - an address is `IP:PORT`; the admin address is a loopback one, as `kbf` refuses any other;
//! - a file flag names a file by absolute path, never holds its contents: PEM text in a flag
//!   is refused, and so is a URL with a user name or password in it;
//! - a certificate, its key and its CA come together (the REAPI pair; the worker trio; the
//!   storage client trio with an `https` storage URL, none with `http`);
//! - a file must exist and be what its kind needs ([`check_files`]): a key is a regular file
//!   (a link is not followed) owned by the service user with mode 0600 or 0400; a certificate
//!   or CA is a regular file owned by root or the service user that neither group nor others
//!   can write.
//!
//! No message carries a value, only the flag and the rule: a path or a URL a person typed can
//! hold a secret, and a log keeps what is printed.

use std::net::SocketAddr;

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
    /// `IP:PORT` with a loopback IP.
    Loopback,
    /// A certificate or CA file: public.
    Cert,
    /// A private key file.
    Key,
    /// `http(s)://host[:port]`.
    Url,
}

/// Which modes take a flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Takes {
    /// `frontend` and `all-in-one`.
    Serving,
    /// `frontend` only.
    Frontend,
}

impl Takes {
    fn mode(self, mode: Mode) -> bool {
        match self {
            Takes::Serving => matches!(mode, Mode::Frontend | Mode::AllInOne),
            Takes::Frontend => mode == Mode::Frontend,
        }
    }

    fn who(self) -> &'static str {
        match self {
            Takes::Serving => "the modes that serve the API (frontend, all-in-one)",
            Takes::Frontend => "--mode=frontend",
        }
    }
}

struct Spec {
    /// The `kbf` flag, and the key.
    flag: &'static str,
    /// The table of the flag's group.
    table: &'static str,
    kind: Want,
    takes: Takes,
}

/// The flags, in the order of [`KbfFlags::values`].
const SPECS: [Spec; 13] = [
    spec("listen", "listener", Want::Addr, Takes::Serving),
    spec("worker-listen", "listener", Want::Addr, Takes::Serving),
    spec("api-listen", "listener", Want::Addr, Takes::Serving),
    spec("admin-listen", "handover", Want::Loopback, Takes::Frontend),
    spec("reapi-tls-cert", "tls", Want::Cert, Takes::Serving),
    spec("reapi-tls-key", "tls", Want::Key, Takes::Serving),
    spec("worker-tls-cert", "tls", Want::Cert, Takes::Serving),
    spec("worker-tls-key", "tls", Want::Key, Takes::Serving),
    spec("worker-client-ca", "tls", Want::Cert, Takes::Serving),
    spec("storage", "storage-client", Want::Url, Takes::Serving),
    spec("storage-ca", "storage-client", Want::Cert, Takes::Frontend),
    spec(
        "storage-client-cert",
        "storage-client",
        Want::Cert,
        Takes::Frontend,
    ),
    spec(
        "storage-client-key",
        "storage-client",
        Want::Key,
        Takes::Frontend,
    ),
];

const fn spec(flag: &'static str, table: &'static str, kind: Want, takes: Takes) -> Spec {
    Spec {
        flag,
        table,
        kind,
        takes,
    }
}

/// Every `(table, key)` these flags can write, for the rule that `setup` owns them.
pub fn keys() -> impl Iterator<Item = (&'static str, &'static str)> {
    SPECS.iter().map(|s| (s.table, s.flag))
}

/// The `(table, key)` pairs `mode` takes as flags.
pub fn taken(mode: Mode) -> Vec<(&'static str, &'static str)> {
    SPECS
        .iter()
        .filter(|s| s.takes.mode(mode))
        .map(|s| (s.table, s.flag))
        .collect()
}

/// The flags as the command line gave them. Each sets the `kbf` flag of its name, in the
/// `kbf.toml` of a `frontend` or `all-in-one` host.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct KbfFlags {
    /// The public REAPI listener, as IP:PORT (kbf's --listen). frontend, all-in-one.
    #[arg(long)]
    pub listen: Option<String>,
    /// The worker listener (daemons and storage nodes dial it), as IP:PORT. frontend, all-in-one.
    #[arg(long)]
    pub worker_listen: Option<String>,
    /// The operator API listener, as IP:PORT. frontend, all-in-one.
    #[arg(long)]
    pub api_listen: Option<String>,
    /// The handover admin listener, as a loopback IP:PORT. frontend.
    #[arg(long)]
    pub admin_listen: Option<String>,
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
    /// The storage node to proxy to, as http(s)://host[:port] with nothing after it. With
    /// https (frontend only) --storage-ca, --storage-client-cert and --storage-client-key are
    /// required.
    #[arg(long)]
    pub storage: Option<String>,
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
}

impl KbfFlags {
    fn values(&self) -> [&Option<String>; 13] {
        [
            &self.listen,
            &self.worker_listen,
            &self.api_listen,
            &self.admin_listen,
            &self.reapi_tls_cert,
            &self.reapi_tls_key,
            &self.worker_tls_cert,
            &self.worker_tls_key,
            &self.worker_client_ca,
            &self.storage,
            &self.storage_ca,
            &self.storage_client_cert,
            &self.storage_client_key,
        ]
    }

    fn get(&self, flag: &str) -> Option<&str> {
        SPECS
            .iter()
            .zip(self.values())
            .find(|(s, _)| s.flag == flag)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Check the flags for `mode`: the settings to write, or a line per problem pushed to
    /// `problems`. Files are checked later ([`check_files`]); this needs no host.
    pub fn validate(&self, mode: Mode, problems: &mut Vec<String>) -> Vec<Setting> {
        let name = super::validate::mode_name(mode);
        let mut out = Vec::new();
        let mut bad = false;
        for (s, value) in SPECS.iter().zip(self.values()) {
            let Some(value) = value else { continue };
            if !s.takes.mode(mode) {
                problems.push(format!(
                    "--{} is for {}; --mode={name} does not take it",
                    s.flag,
                    s.takes.who()
                ));
                bad = true;
                continue;
            }
            match check_value(s.kind, value) {
                Ok(()) => out.push(Setting {
                    table: s.table,
                    key: s.flag,
                    value: value.clone(),
                }),
                Err(rule) => {
                    problems.push(format!("--{}: {rule}", s.flag));
                    bad = true;
                }
            }
        }
        if !bad {
            self.together(problems);
        }
        out
    }

    /// The flags that only work in company.
    fn together(&self, problems: &mut Vec<String>) {
        let groups: [&[&str]; 2] = [
            &["reapi-tls-cert", "reapi-tls-key"],
            &["worker-tls-cert", "worker-tls-key", "worker-client-ca"],
        ];
        for group in groups {
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
        let client = ["storage-ca", "storage-client-cert", "storage-client-key"];
        let given = client.iter().filter(|f| self.get(f).is_some()).count();
        match self.get("storage") {
            Some(url) if url.starts_with("https://") => {
                if given != client.len() {
                    problems.push(
                        "--storage with https needs --storage-ca, --storage-client-cert and --storage-client-key"
                            .to_owned(),
                    );
                }
            }
            Some(_) if given != 0 => problems.push(
                "--storage-ca, --storage-client-cert and --storage-client-key are for an https --storage"
                    .to_owned(),
            ),
            None if given != 0 => problems.push(
                "--storage-ca, --storage-client-cert and --storage-client-key need --storage"
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
            .map(drop)
            .map_err(|_| "must be IP:PORT, as 0.0.0.0:8980 or [::1]:8980"),
        Want::Loopback => match value.parse::<SocketAddr>() {
            Ok(a) if a.ip().is_loopback() => Ok(()),
            Ok(_) => Err("must be a loopback address: the calls it serves move the scheduler's role"),
            Err(_) => Err("must be IP:PORT, as 127.0.0.1:8983"),
        },
        Want::Cert | Want::Key => check_path(value),
        Want::Url => check_url(value),
    }
}

/// A path of plain characters. Text that looks like what a file holds is called that.
fn check_path(value: &str) -> Result<(), &'static str> {
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

/// `http://host[:port]` or `https://host[:port]`, with no user, password, path, query or
/// fragment.
fn check_url(value: &str) -> Result<(), &'static str> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .ok_or("must be an http:// or https:// URL")?;
    if rest.contains('@') {
        return Err("must carry no user name or password: credentials never go in kbf.toml");
    }
    let plain = rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
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
    for setting in settings {
        let Some(s) = SPECS.iter().find(|s| s.flag == setting.key) else {
            continue;
        };
        if !matches!(s.kind, Want::Cert | Want::Key) {
            continue;
        }
        if let Err(rule) = check_file(exec, s, &setting.value, service) {
            problems.push(format!("--{}: {rule}", s.flag));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(SetupError::Invalid(problems))
    }
}

fn check_file(
    exec: &dyn Exec,
    s: &Spec,
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
    match s.kind {
        Want::Key => {
            if !service_owns {
                return Err("a key must be owned by the user the service runs as");
            }
            if bits & 0o177 != 0 || bits & 0o400 == 0 {
                return Err("a key must have mode 0600 or 0400");
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
