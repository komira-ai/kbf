//! The flags of `setup`, checked before anything is read from or written to the host.
//! Every problem is reported, one line each, not just the first.

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

use crate::CommitId;

/// What the host runs. Each role is a `kbf` mode (`kbf <role>`); the roles that run actions
/// (`daemon`, `storage-and-daemon`, `all-in-one`) need the rootless-container prerequisites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Frontend,
    Storage,
    Daemon,
    StorageAndDaemon,
    AllInOne,
}

impl Role {
    pub const ALL: [Role; 5] = [
        Role::Frontend,
        Role::Storage,
        Role::Daemon,
        Role::StorageAndDaemon,
        Role::AllInOne,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Role::Frontend => "frontend",
            Role::Storage => "storage",
            Role::Daemon => "daemon",
            Role::StorageAndDaemon => "storage-and-daemon",
            Role::AllInOne => "all-in-one",
        }
    }

    /// Whether the host runs actions in containers: it then needs a subid range and lingering.
    pub fn runs_actions(self) -> bool {
        matches!(self, Role::Daemon | Role::StorageAndDaemon | Role::AllInOne)
    }

    /// Whether the host serves the frontend: restarting it fails every build in flight, so
    /// setup starts it only when asked.
    pub fn serves_frontend(self) -> bool {
        matches!(self, Role::Frontend | Role::AllInOne)
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Role, String> {
        Role::ALL
            .into_iter()
            .find(|r| r.name() == s)
            .ok_or_else(|| {
                let all: Vec<_> = Role::ALL.iter().map(|r| r.name()).collect();
                format!("--role {s:?} is not one of {}", all.join(", "))
            })
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where the installer fetches releases from (`[update] source`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleaseSource {
    /// The frontend's release store, over the worker listener.
    Frontend,
    /// An HTTPS file server serving the release store's layout; the base URL, no trailing `/`.
    Url(String),
}

/// The operator floor of `[update]`: releases below it are refused. Either, both or neither.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Floor {
    pub min_commit: Option<CommitId>,
    pub min_protocol: Option<u32>,
}

/// The raw flags, as the command line gives them.
#[derive(Clone, Debug, Default)]
pub struct Raw {
    pub frontend_url: String,
    pub role: String,
    pub token_file: String,
    pub release_source: String,
    pub release_url: Option<String>,
    pub min_commit: Option<String>,
    pub min_protocol: Option<u32>,
}

/// The flags, valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Valid {
    /// `https://host[:port]`, in its one spelling.
    pub frontend_url: String,
    pub role: Role,
    /// The operator's token file, an absolute path.
    pub token_file: String,
    pub source: ReleaseSource,
    pub floor: Floor,
}

/// `https://` + authority + path, taken apart. The host is lower-cased.
struct Https {
    authority: String,
    path: String,
}

fn https(flag: &str, text: &str, path_ok: bool) -> Result<Https, String> {
    let bad = |why: &str| format!("{flag} {text:?}: {why}");
    let rest = text
        .strip_prefix("https://")
        .ok_or_else(|| bad("must start with https://"))?;
    if rest.contains('@') {
        return Err(bad("must not carry user information"));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(bad("must not carry a query or a fragment"));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let path = path.trim_end_matches('/');
    if !path.is_empty() && !path_ok {
        return Err(bad("must be https://host[:port] and nothing after it"));
    }
    if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~/-".contains(&b)) || path.contains("//")
        || path.split('/').any(|c| c == "." || c == "..")
    {
        return Err(bad("the path holds a character or component that is not allowed"));
    }
    let (host, port) = split_port(authority).ok_or_else(|| bad("the port is not 1 to 65535"))?;
    let host = host.to_ascii_lowercase();
    if !valid_host(&host) {
        return Err(bad("the host is not a DNS name or an address"));
    }
    let authority = match port {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    Ok(Https {
        authority,
        path: path.to_owned(),
    })
}

/// `host` and the port, if any; `None` when the port is not a number in 1 to 65535.
fn split_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let colon = if authority.starts_with('[') {
        authority.rfind("]:").map(|i| i + 1)
    } else {
        authority.rfind(':')
    };
    let Some(i) = colon else {
        return Some((authority, None));
    };
    let (host, port) = (&authority[..i], &authority[i + 1..]);
    match port.parse::<u16>() {
        Ok(p) if p != 0 && port.bytes().all(|b| b.is_ascii_digit()) => Some((host, Some(p))),
        _ => None,
    }
}

fn valid_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner.parse::<Ipv6Addr>().is_ok();
    }
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn keep<T>(problems: &mut Vec<String>, r: Result<T, String>) -> Option<T> {
    r.map_err(|e| problems.push(e)).ok()
}

impl Raw {
    /// Check every flag; the `Err` lists each problem.
    pub fn validate(&self) -> Result<Valid, Vec<String>> {
        let mut problems = Vec::new();
        let frontend = keep(
            &mut problems,
            https("--frontend-url", &self.frontend_url, false)
                .map(|u| format!("https://{}", u.authority)),
        );
        let role = keep(&mut problems, self.role.parse::<Role>());
        let token = keep(&mut problems, if self.token_file.starts_with('/') {
            Ok(self.token_file.clone())
        } else {
            Err(format!(
                "--token-file {:?} must be an absolute path",
                self.token_file
            ))
        });
        let source = keep(&mut problems, self.source());
        let min_commit = match &self.min_commit {
            None => None,
            Some(c) => keep(
                &mut problems,
                CommitId::new(c)
                    .map_err(|_| format!("--min-commit {c:?} is not 40 lowercase hex characters")),
            ),
        };
        let min_protocol = match self.min_protocol {
            Some(0) => keep(&mut problems, Err("--min-protocol 0 is no floor: leave it out".to_owned())),
            other => other,
        };
        match (frontend, role, token, source) {
            (Some(frontend_url), Some(role), Some(token_file), Some(source))
                if problems.is_empty() =>
            {
                Ok(Valid {
                    frontend_url,
                    role,
                    token_file,
                    source,
                    floor: Floor {
                        min_commit,
                        min_protocol,
                    },
                })
            }
            _ => Err(problems),
        }
    }

    fn source(&self) -> Result<ReleaseSource, String> {
        match (self.release_source.as_str(), &self.release_url) {
            ("frontend", None) => Ok(ReleaseSource::Frontend),
            ("frontend", Some(_)) => {
                Err("--release-url is only for --release-source url".to_owned())
            }
            ("url", None) => Err("--release-source url needs --release-url".to_owned()),
            ("url", Some(u)) => {
                let h = https("--release-url", u, true)?;
                Ok(ReleaseSource::Url(format!("https://{}{}", h.authority, h.path)))
            }
            (other, _) => Err(format!(
                "--release-source {other:?} is not frontend or url"
            )),
        }
    }
}
