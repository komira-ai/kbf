//! `kbf-deploy host-status`: one host's row from the coordinator's `GET /v1/status`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;
use serde_json::Value;

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
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RowError {
    NoService(String),
    NoNode(String, String),
}

pub(crate) fn token_rule(
    _is_file: bool,
    _uid: u32,
    _mode: u32,
    _me: u32,
) -> Result<(), &'static str> {
    Ok(())
}

pub(crate) fn find_row<'a>(
    _status: &'a Value,
    service: &str,
    node: &str,
) -> Result<&'a Value, RowError> {
    if node.is_empty() {
        return Err(RowError::NoNode(node.to_owned(), service.to_owned()));
    }
    Err(RowError::NoService(service.to_owned()))
}

pub(crate) fn render_text(_row: &Value, _status: &Value, _service: &str) -> String {
    String::new()
}

/// Run the verb. Not written yet.
pub fn run(a: &HostStatusArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = (
        a.json,
        &a.coordinator,
        &a.node,
        &a.service,
        &a.ca_pem,
        &a.operator_token_file,
    );
    let _ = (out, err, Path::new(""));
    let _ = token_rule(true, 0, 0, 0);
    let _ = find_row(&Value::Null, &a.service, &a.node);
    let _ = render_text(&Value::Null, &Value::Null, &a.service);
    ExitCode::from(3)
}

#[cfg(test)]
#[path = "host_status_tests.rs"]
mod tests;
