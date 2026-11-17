//! `kbf-deploy job-service agent-egress`: the egress lines of a farm agent's flags file
//! (`/etc/js-agent/flags`) for the jobs a put-dir frontend submits
//! (`docs/deploy/job-service-cutover.md`, "The farm agents' egress").
//!
//! A put-dir job commits its object into its put directory and reads its inputs from its
//! mounted tree, so it reaches no address: the frontend's settings name none, and the verb
//! prints the ceiling `--egress=allow-list` with no `--egress-allow` line. A destination is
//! never added, and `open` is never printed. The frontend's `kbf.toml` is read and parsed as
//! TOML: a file that cannot be read, or is not TOML, is refused naming the file. Its keys are
//! not read, so nothing else about the file is checked.
//!
//! It only reads the file and prints; it changes nothing. Exit 0 printed; 2 refused
//! (`REFUSED: ...` on stderr, nothing on stdout).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;

/// The arguments of `kbf-deploy job-service agent-egress`.
#[derive(Args)]
pub struct AgentEgressArgs {
    /// The frontend's `kbf.toml`: the file the frontend that submits the jobs runs with.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

/// Why the lines were not rendered. Every text names the file.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EgressError {
    /// The file could not be read.
    #[error("--config {file}: cannot be read: {cause}")]
    Read { file: String, cause: String },
    /// The file is not TOML.
    #[error("--config {file}: not TOML: {cause}")]
    Syntax { file: String, cause: String },
}

/// The flags-file lines for the frontend whose `kbf.toml` (at `file`) is `text`.
pub(crate) fn render(text: &str, file: &Path) -> Result<String, EgressError> {
    let file = file.display().to_string();
    text.parse::<toml::Table>()
        .map_err(|e: toml::de::Error| EgressError::Syntax {
            file: file.clone(),
            cause: e.message().to_owned(),
        })?;
    Ok(format!(
        "# The egress of kbf's jobs, from {file}. Put these lines in /etc/js-agent/flags on\n\
         # every agent of the farm (the Linux js-agent, which runs crun): a put-dir job has no\n\
         # destination, so no allow line follows.\n\
         --egress=allow-list\n"
    ))
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
