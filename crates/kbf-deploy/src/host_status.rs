//! `kbf-deploy host-status`: one host's row from the coordinator's `GET /v1/status`, for an
//! operator rolling agents node by node. It only reads.
//!
//! The status is the control listener's operator read (`kbf-coordinator`'s `http` module): the
//! operator token as the bearer, a JSON answer. The verb reads it as JSON text and prints what
//! the row of the node has, so a field the coordinator adds shows in `--json` without a change
//! here. The row is the node's entry in `per_service.<service>.hosts` (which has the `hold`),
//! or, from a coordinator that has no `per_service`, kbf's `hosts`.
//!
//! The token is read from a file ([`read_token`]) and goes into one request header. It is not an
//! argument, has no `Display` and a `Debug` that hides it, and no error text holds it or the
//! file's path. A redirect is not followed ([`Client::get_with`] does not), so the header goes
//! to the coordinator's address only.
//!
//! Exit 0 found; 1 the node (or the service) is not in the status, or the coordinator could not
//! be asked: `cannot connect`, a `TLS error`, or an HTTP status (401 says the token was
//! refused); 2 refused: bad arguments or a bad token file (`REFUSED: ...` on stderr).

use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use rustix::fs::OFlags;
use rustix::io::Errno;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::Value;

use crate::https::{Client, HttpsError, HttpsUrl, roots};

/// The flag that names the token file, which error texts name instead of its path.
const TOKEN_FLAG: &str = "--operator-token-file";
/// The path the operator reads the status from.
const STATUS_PATH: &str = "/v1/status";
/// The shortest and longest token, as `kbf-updater` takes one.
const TOKEN_MIN: usize = 32;
const TOKEN_MAX: u64 = 4096;
/// How long a connect, read or write may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The arguments of `kbf-deploy host-status`.
#[derive(Args)]
pub struct HostStatusArgs {
    /// The coordinator's control listener: an `https://` URL or `host:port`.
    #[arg(long, value_name = "URL|HOST:PORT")]
    coordinator: String,
    /// The file holding the operator token (mode 0600 or 0400, yours).
    #[arg(long, value_name = "FILE")]
    operator_token_file: PathBuf,
    /// The node id of the host.
    #[arg(long, value_name = "NODE_ID")]
    node: String,
    /// The service whose view of the host is printed.
    #[arg(long, value_name = "NAME", default_value = "kbf")]
    service: String,
    /// The farm CA certificate (PEM text) the coordinator's certificate must chain to,
    /// instead of the system store's roots. Empty: the system store.
    #[arg(
        long,
        value_name = "PEM",
        default_value = "",
        allow_hyphen_values = true
    )]
    ca_pem: String,
    /// Print the host's row as one JSON object.
    #[arg(long)]
    json: bool,
}

/// Why a row was not found.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RowError {
    #[error("service {0:?} is not in the status")]
    NoService(String),
    #[error("node {0:?} is not in the status of service {1:?}")]
    NoNode(String, String),
}

/// Why the verb ended other than found.
#[derive(Debug, thiserror::Error)]
enum HostStatusError {
    /// Bad arguments or a bad token file: exit 2.
    #[error("{0}")]
    Refused(String),
    #[error("cannot connect to the coordinator: {0}")]
    Connect(HttpsError),
    #[error("TLS error talking to the coordinator: {0}")]
    Tls(HttpsError),
    #[error("the coordinator says the operator token was refused (HTTP 401)")]
    TokenRefused,
    #[error("the coordinator answered HTTP {0}")]
    Http(u16),
    #[error("the coordinator's answer is not a status: {0}")]
    Answer(String),
    #[error(transparent)]
    Row(#[from] RowError),
}

impl HostStatusError {
    fn code(&self) -> u8 {
        if matches!(self, Self::Refused(_)) {
            2
        } else {
            1
        }
    }
}

/// The operator token. It has no `Display`, and its `Debug` hides it.
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The rule for the token file itself: a regular file opened without following a link, owned by `me`,
/// mode exactly `0600` or `0400`.
pub(crate) fn token_rule(is_file: bool, uid: u32, mode: u32, me: u32) -> Result<(), &'static str> {
    if !is_file {
        return Err("it is not a regular file");
    }
    if uid != me {
        return Err("it is not owned by your user");
    }
    if !matches!(mode & 0o7777, 0o600 | 0o400) {
        return Err("its mode must be 0600 or 0400");
    }
    Ok(())
}

/// The token in the file at `path`, owned by user `me`: one token of 32 to 4096 visible ASCII
/// characters, surrounding whitespace dropped. The error text names the flag and the rule,
/// never the path or the content.
fn read_token(path: &Path, me: u32) -> Result<Token, String> {
    let fail = |why: &str| format!("{TOKEN_FLAG}: {why}");
    let io = |e: std::io::Error| fail(&e.kind().to_string());
    // Open first, without following a link and without blocking on a FIFO, and check the open
    // file: what is checked is what is read, whatever the path is swapped for meanwhile.
    let flags = (OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(Errno::LOOP.raw_os_error()) {
                fail("it is a symbolic link")
            } else {
                io(e)
            }
        })?;
    let meta = file.metadata().map_err(io)?;
    token_rule(meta.is_file(), meta.uid(), meta.mode(), me).map_err(fail)?;
    let mut bytes = Vec::new();
    file.take(TOKEN_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    let shape = "it must hold one token of 32 to 4096 visible ASCII characters";
    let text = String::from_utf8(bytes).map_err(|_| fail(shape))?;
    let token = text.trim();
    let visible = token.bytes().all(|b| b.is_ascii_graphic());
    if text.len() as u64 > TOKEN_MAX || token.len() < TOKEN_MIN || !visible {
        return Err(fail(shape));
    }
    Ok(Token(token.to_owned()))
}

/// The URL of the status at `coordinator`: an `https://` URL with no path, or `host:port`.
fn status_url(coordinator: &str) -> Result<HttpsUrl, String> {
    let url = if coordinator.contains("://") {
        coordinator.to_owned()
    } else {
        format!("https://{coordinator}")
    };
    let given = HttpsUrl::new(&url).map_err(|e| format!("--coordinator: {e}"))?;
    if given.path() != "/" {
        return Err("--coordinator: name the host and port, not a path".to_owned());
    }
    HttpsUrl::new(&format!("{}{STATUS_PATH}", url.trim_end_matches('/')))
        .map_err(|e| format!("--coordinator: {e}"))
}

/// The client: it trusts the certificates in `ca_pem`, or, when that is empty, the system roots.
fn client(ca_pem: &str) -> Result<Client, HostStatusError> {
    if ca_pem.trim().is_empty() {
        return Client::system(TIMEOUT).map_err(HostStatusError::Tls);
    }
    let refused =
        || HostStatusError::Refused("--ca-pem: it holds no usable certificate".to_owned());
    let certs = CertificateDer::pem_slice_iter(ca_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| refused())?;
    let store = roots(certs).map_err(|_| refused())?;
    Ok(Client::with_roots(store, TIMEOUT))
}

/// One `GET` of the status with `token` as the bearer: the answer's JSON.
fn fetch(client: &Client, url: &HttpsUrl, token: &Token) -> Result<Value, HostStatusError> {
    let bearer = format!("Bearer {}", token.0);
    let mut body = Vec::new();
    let answered = client.get_with(url, None, &[("Authorization", &bearer)], &mut body);
    let answer = answered.map_err(|e| match e {
        HttpsError::Io { ref source, .. }
            if !source
                .get_ref()
                .is_some_and(|inner| inner.is::<rustls::Error>()) =>
        {
            HostStatusError::Connect(e)
        }
        HttpsError::Malformed { .. } | HttpsError::TooLong { .. } => {
            HostStatusError::Answer(e.to_string())
        }
        e => HostStatusError::Tls(e),
    })?;
    match answer.status {
        200..=299 => {}
        401 => return Err(HostStatusError::TokenRefused),
        other => return Err(HostStatusError::Http(other)),
    }
    serde_json::from_slice(&body).map_err(|e| HostStatusError::Answer(e.to_string()))
}

/// The row of `node` in the status of `service`.
pub(crate) fn find_row<'a>(
    status: &'a Value,
    service: &str,
    node: &str,
) -> Result<&'a Value, RowError> {
    let hosts = match status.get("per_service") {
        Some(per_service) => per_service.get(service).and_then(|s| s.get("hosts")),
        // A coordinator from before the per-service section: kbf's hosts are the top level's.
        None if service == "kbf" => status.get("hosts"),
        None => None,
    };
    let no_service = || RowError::NoService(service.to_owned());
    let hosts = hosts.and_then(Value::as_array).ok_or_else(no_service)?;
    hosts
        .iter()
        .find(|h| h.get("node_id").and_then(Value::as_str) == Some(node))
        .ok_or_else(|| RowError::NoNode(node.to_owned(), service.to_owned()))
}

/// The keys of a row, in the order they are printed (`service` is not in the row, and
/// `last_seen` is its `silent_secs`).
const FIELDS: [&str; 17] = [
    "node_id",
    "service",
    "role",
    "platform",
    "version",
    "health",
    "live",
    "up",
    "updating",
    "last_update",
    "last_seen",
    "holds_slot",
    "hold",
    "step",
    "commit",
    "promotion_pending",
    "deferred",
];

/// A value as one line: text as it is (JSON-quoted when it has a control character, which a
/// terminal would act on), `none` for null, anything else as compact JSON.
fn line_of(value: &Value) -> String {
    match value {
        Value::Null => "none".to_owned(),
        Value::String(s) if !s.chars().any(char::is_control) => s.clone(),
        other => other.to_string(),
    }
}

/// The row as `key: value` lines, then this node's exclusion and the service's standing halt
/// from the status's `ops`, when it has them. A key the row lacks is not printed.
pub(crate) fn render_text(row: &Value, status: &Value, service: &str) -> String {
    let mut text = String::new();
    for key in FIELDS {
        let value = match key {
            "service" => Some(Value::String(service.to_owned())),
            "last_seen" => row.get("silent_secs").cloned(),
            _ => row.get(key).cloned(),
        };
        let Some(value) = value else { continue };
        let shown = match (key, value.as_u64()) {
            ("last_seen", Some(secs)) => format!("{secs}s ago"),
            _ => line_of(&value),
        };
        text.push_str(&format!("{key}: {shown}\n"));
    }
    let node = row.get("node_id").and_then(Value::as_str);
    let ops = status.get("ops");
    let excluded = ops
        .and_then(|o| o.get("excluded"))
        .and_then(Value::as_array);
    for e in excluded.into_iter().flatten() {
        let same = |key: &str, want: Option<&str>| e.get(key).and_then(Value::as_str) == want;
        if same("service", Some(service)) && same("node_id", node) {
            let reason = e.get("reason").map_or_else(|| "none".to_owned(), line_of);
            text.push_str(&format!("excluded: {reason}\n"));
        }
    }
    let halt = ops
        .and_then(|o| o.get("services"))
        .and_then(|s| s.get(service))
        .and_then(|s| s.get("halt"))
        .filter(|h| !h.is_null());
    if let Some(halt) = halt {
        text.push_str(&format!("halt: {halt}\n"));
    }
    text
}

fn run_checked(a: &HostStatusArgs, me: u32, out: &mut dyn Write) -> Result<(), HostStatusError> {
    let refuse = HostStatusError::Refused;
    if a.node.is_empty() {
        return Err(refuse("--node: it must not be empty".to_owned()));
    }
    if a.service.is_empty() {
        return Err(refuse("--service: it must not be empty".to_owned()));
    }
    let url = status_url(&a.coordinator).map_err(refuse)?;
    let token = read_token(&a.operator_token_file, me).map_err(refuse)?;
    let client = client(&a.ca_pem)?;
    let status = fetch(&client, &url, &token)?;
    let row = find_row(&status, &a.service, &a.node)?;
    let text = if a.json {
        format!("{row}\n")
    } else {
        render_text(row, &status, &a.service)
    };
    // A reader that has gone (a closed pipe) is not the coordinator's failure.
    let _ = out.write_all(text.as_bytes());
    Ok(())
}

/// Run the verb as user `me`.
pub fn run(a: &HostStatusArgs, me: u32, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match run_checked(a, me, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let word = if e.code() == 2 { "REFUSED" } else { "FAIL" };
            let _ = writeln!(err, "{word}: {e}");
            ExitCode::from(e.code())
        }
    }
}

#[cfg(test)]
#[path = "host_status_tests.rs"]
mod tests;
