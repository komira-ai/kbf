//! `linux verify-cert`: check, read only, that one certificate is what its row of the
//! farm's certificate table says (docs/deploy/kbf-units.md, "A node's certificate").
//! The checks, the table and their failure text are kbf-ca's
//! ([`kbf_ca::verify_cert`]); this is the verb's flags, its output and its exit codes:
//! 0 the certificate is the row's, 1 a check failed, 2 refused (a file that cannot be
//! read, no certificate in `--cert`, no usable CA, `--node-id` for a row that is not
//! `daemon`), with a `REFUSED:` line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use clap::{Args, ValueEnum};
use kbf_ca::verify_cert::{ROLE_URI_PREFIX, Row, VerifyError, verify};

/// The `--row` values: kbf-ca's [`Row`], with the flag's spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RowArg {
    /// The frontend's worker listener, and the frontend when it calls storage.
    FrontendWorker,
    /// A storage node's listener, and its calls to other storage nodes.
    Storage,
    /// A daemon's client certificate.
    Daemon,
}

impl From<RowArg> for Row {
    fn from(a: RowArg) -> Row {
        match a {
            RowArg::FrontendWorker => Row::FrontendWorker,
            RowArg::Storage => Row::Storage,
            RowArg::Daemon => Row::Daemon,
        }
    }
}

#[derive(Args)]
pub struct VerifyArgs {
    /// The row of the certificate table the certificate is for. The frontend's REAPI
    /// certificate is not the farm CA's and is no row.
    #[arg(long, value_enum)]
    row: RowArg,
    /// The certificate (PEM; any certificates after the first are its intermediates).
    #[arg(long)]
    cert: PathBuf,
    /// The farm CA's certificate (PEM).
    #[arg(long)]
    ca_cert: PathBuf,
    /// The node id a daemon's certificate names, its one DNS name (`--row daemon`
    /// only, and required there).
    #[arg(long, required_if_eq("row", "daemon"))]
    node_id: Option<String>,
}

/// `verify-cert`: the lines to print and whether every check passed.
///
/// # Errors
/// [`VerifyError`].
pub fn run(a: &VerifyArgs, now: SystemTime) -> Result<(Vec<String>, bool), VerifyError> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|e| VerifyError::Read {
            path: path.to_owned(),
            why: e.to_string(),
        })
    };
    let row = Row::from(a.row);
    let failures = verify(
        &read(&a.ca_cert)?,
        &read(&a.cert)?,
        row,
        a.node_id.as_deref(),
        now,
    )?;
    let mut lines: Vec<String> = failures.iter().map(ToString::to_string).collect();
    let ok = failures.is_empty();
    lines.push(if ok {
        format!(
            "CERT OK: {} is a {} certificate: role {ROLE_URI_PREFIX}{}, extended key usage \
             {}, chains to {}",
            a.cert.display(),
            row.name(),
            row.role(),
            row.ekus().join(","),
            a.ca_cert.display()
        )
    } else {
        format!("CERT BROKEN: {} failed check(s)", failures.len())
    });
    Ok((lines, ok))
}

/// The verb: 0 the certificate is the row's, 1 a check failed, 2 refused.
pub fn cli(a: &VerifyArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match run(a, SystemTime::now()) {
        Ok((lines, ok)) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "verify_cert_tests.rs"]
mod tests;
