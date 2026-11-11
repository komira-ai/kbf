//! `kbf-deploy job-service agent-egress`: the egress lines of a farm agent's flags file
//! (`/etc/js-agent/flags`), rendered from the frontend's own `kbf.toml`, so that a job kbf
//! submits with the network level `allow-list` reaches the places its task must reach and
//! nothing else (`docs/deploy/job-service-cutover.md`, "The farm agents' egress").
//!
//! A task reaches, by the frontend's settings:
//!
//! - the tier store's proxy, `task-put-endpoint`: every pack and large object is PUT there;
//! - the job-facing read listener, `task-read-url`: the inputs are read there, with
//!   `executor-locate` or without it (the frontend signs no store read grants since tierstore
//!   v0.3.29 removed them, so a locating task reads from the listener too).
//!
//! The verb prints `--egress=allow-list` and one `--egress-allow=IPV4:PORT` per destination,
//! each once, in that order. The agent's ceiling `allow-list` admits the jobs kbf submits; its
//! list is the agent's only route out for them, so a destination left off is unreachable and
//! a destination not named here is never added. `open` is never printed.
//!
//! The job network has no DNS, and js-agent takes IPv4 endpoints only: a host that is not an
//! IPv4 address is refused, naming the key, as are an address a job can never reach through
//! the host (loopback, unspecified, link-local, multicast, broadcast). A missing key the task
//! needs is refused naming the key: the mistake is fixed in the frontend's file, where it is.
//!
//! It only reads the file and prints; it changes nothing. Exit 0 printed; 2 refused
//! (`REFUSED: ...` on stderr, nothing on stdout).

use std::fmt;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;

/// The key of the tier store's proxy, where a task PUTs its packs.
const PUT_KEY: &str = "task-put-endpoint";
/// The key of the job-facing read listener, where a task reads its inputs.
const READ_KEY: &str = "task-read-url";

/// The arguments of `kbf-deploy job-service agent-egress`.
#[derive(Args)]
pub struct AgentEgressArgs {
    /// The frontend's `kbf.toml`: the file the frontend that submits the jobs runs with.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

/// Why the lines were not rendered. Every text names the file, and the key when one is at
/// fault.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EgressError {
    /// The file could not be read.
    #[error("--config {file}: cannot be read: {cause}")]
    Read { file: String, cause: String },
    /// The file is not TOML.
    #[error("--config {file}: not TOML: {cause}")]
    Syntax { file: String, cause: String },
    /// A key the task's network needs is not in the file.
    #[error("--config {file}: no `{key}`: {why}")]
    Missing {
        file: String,
        key: &'static str,
        why: &'static str,
    },
    /// A key is in more than one table, which the frontend refuses too.
    #[error("--config {file}: `{key}` is in more than one table ([{first}] and [{second}])")]
    Twice {
        file: String,
        key: &'static str,
        first: String,
        second: String,
    },
    /// A key holds a value of the wrong type.
    #[error("--config {file}: `{key}` is not {wanted}")]
    Type {
        file: String,
        key: &'static str,
        wanted: &'static str,
    },
    /// A url's address is not one an agent can allow.
    #[error("--config {file}: `{key}` = {url:?}: {why}")]
    Url {
        file: String,
        key: &'static str,
        url: String,
        why: UrlError,
    },
}

/// What is wrong with the address of a url.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlError {
    /// Not `http://` or `https://`.
    Scheme,
    /// An IPv6 literal: the job network is IPv4 only.
    Ipv6,
    /// A host name or anything else that is not an IPv4 address.
    NotIpv4,
    /// The port is not a number from 1 to 65535.
    Port,
    /// An address no job reaches through the host.
    Unreachable,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Scheme => "not an http:// or https:// url",
            Self::Ipv6 => "an IPv6 address: a job's network is IPv4 only",
            Self::NotIpv4 => {
                "the host is not an IPv4 address: a job's network has no DNS, so the agent \
                 allows addresses only"
            }
            Self::Port => "the port is not a number from 1 to 65535",
            Self::Unreachable => {
                "a loopback, unspecified, link-local, multicast or broadcast address, which no \
                 job reaches through the host"
            }
        })
    }
}

/// One destination: the address and port the agent allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Endpoint {
    addr: Ipv4Addr,
    port: u16,
}

/// The endpoint of `url`: `http(s)://IPV4[:PORT][/...]`, the port 80 or 443 by default.
fn endpoint(url: &str) -> Result<Endpoint, UrlError> {
    let (rest, default) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80)
    } else {
        return Err(UrlError::Scheme);
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.starts_with('[') {
        return Err(UrlError::Ipv6);
    }
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => {
            let port: u16 = port.parse().map_err(|_| UrlError::Port)?;
            if port == 0 {
                return Err(UrlError::Port);
            }
            (host, port)
        }
        None => (authority, default),
    };
    let addr: Ipv4Addr = host.parse().map_err(|_| UrlError::NotIpv4)?;
    if addr.is_loopback()
        || addr.is_unspecified()
        || addr.is_link_local()
        || addr.is_multicast()
        || addr.is_broadcast()
    {
        return Err(UrlError::Unreachable);
    }
    Ok(Endpoint { addr, port })
}

/// The value of `key` in whichever table of `doc` holds it (a key is a flag's long name, and a
/// flag is in one table only), or `None`.
fn find<'a>(
    doc: &'a toml::Table,
    key: &'static str,
    file: &str,
) -> Result<Option<&'a toml::Value>, EgressError> {
    let mut found: Option<(&str, &toml::Value)> = None;
    for (table, value) in doc {
        let Some(value) = value.as_table().and_then(|t| t.get(key)) else {
            continue;
        };
        if let Some((first, _)) = found {
            return Err(EgressError::Twice {
                file: file.to_owned(),
                key,
                first: first.to_owned(),
                second: table.clone(),
            });
        }
        found = Some((table, value));
    }
    Ok(found.map(|(_, value)| value))
}

/// The url of `key`, which must be there.
fn url_of(
    doc: &toml::Table,
    key: &'static str,
    why: &'static str,
    file: &str,
) -> Result<(String, Endpoint), EgressError> {
    let value = find(doc, key, file)?.ok_or(EgressError::Missing {
        file: file.to_owned(),
        key,
        why,
    })?;
    let url = value.as_str().ok_or(EgressError::Type {
        file: file.to_owned(),
        key,
        wanted: "a string",
    })?;
    let at = endpoint(url).map_err(|why| EgressError::Url {
        file: file.to_owned(),
        key,
        url: url.to_owned(),
        why,
    })?;
    Ok((url.to_owned(), at))
}

/// The flags-file lines for the frontend whose `kbf.toml` (at `file`) is `text`.
pub(crate) fn render(text: &str, file: &Path) -> Result<String, EgressError> {
    let file = file.display().to_string();
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| EgressError::Syntax {
            file: file.clone(),
            cause: e.message().to_owned(),
        })?;
    let wanted = vec![
        (
            PUT_KEY,
            url_of(&doc, PUT_KEY, "a task PUTs its packs there", &file)?,
        ),
        (
            READ_KEY,
            url_of(&doc, READ_KEY, "a task reads its inputs there", &file)?,
        ),
    ];
    // One line per endpoint, in the order above; a later key that names one already listed
    // is added to its comment.
    let mut lines: Vec<(Endpoint, Vec<String>)> = Vec::new();
    for (key, (url, at)) in wanted {
        let named = format!("{key} = {url}");
        match lines.iter_mut().find(|(seen, _)| *seen == at) {
            Some((_, names)) => names.push(named),
            None => lines.push((at, vec![named])),
        }
    }
    let mut out = format!(
        "# The egress of kbf's tasks, from {file}. Put these lines in /etc/js-agent/flags on\n\
         # every agent of the farm (js-agent --runtime crun): a job of kbf's reaches these and\n\
         # nothing else.\n\
         --egress=allow-list\n"
    );
    for (at, names) in lines {
        out.push_str(&format!(
            "# {}\n--egress-allow={}:{}\n",
            names.join(", "),
            at.addr,
            at.port
        ));
    }
    Ok(out)
}

/// Read the file and render it.
fn render_file(path: &Path) -> Result<String, EgressError> {
    let text = std::fs::read_to_string(path).map_err(|e| EgressError::Read {
        file: path.display().to_string(),
        cause: e.to_string(),
    })?;
    render(&text, path)
}

/// Run the verb: the lines on `out`, or `REFUSED: ...` on `err` and exit 2.
pub(crate) fn run(a: &AgentEgressArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match render_file(&a.config) {
        Ok(lines) => {
            // A reader that has gone (a closed pipe) is not a refusal.
            let _ = out.write_all(lines.as_bytes());
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "agent_egress_tests.rs"]
mod tests;
