//! `release identity`: `kbf --version --json` of the linux-x86_64 build in a directory of
//! release files, for `release notes` (what is not reversible is read from the builds,
//! not from what a human typed into a tag).
//!
//! This RUNS a binary, so the caller has verified the directory first: `release check` and
//! `verify-assets attestations` on it, in the same job and before this verb (the workflow
//! lint, `crates/kbf-integration-test/tests/release_workflow`, holds the order). The binary comes from the
//! tarball `kbf-<commit>-linux-x86_64.tar.gz`, never from anywhere else, and runs with
//! `--version --json` only.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use crate::Error;
use crate::build_commit::full_commit;

/// The binary whose identity is read, and the platform whose tarball holds the one that
/// runs on the machine that reads it.
const BINARY: &str = "kbf";
const PLATFORM: &str = "linux-x86_64";

/// The most bytes of the binary read from its tarball (a larger one is cut, and does not
/// run), and of the identity it prints.
const BINARY_CAP: u64 = 1 << 30;
const IDENTITY_CAP: usize = 8192;

/// The identity `kbf` of `commit` in `dir` prints: one JSON object, exactly as printed
/// (the line end dropped). `scratch` is an empty directory the binary is unpacked into.
pub fn identity(dir: &Path, commit: &str, scratch: &Path) -> Result<String, Error> {
    full_commit("--commit", commit)?;
    let tarball = dir.join(format!("{BINARY}-{commit}-{PLATFORM}.tar.gz"));
    let file = std::fs::File::open(&tarball)
        .map_err(Error::spawn(format!("open {}", tarball.display())))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(Error::spawn(format!("read {}", tarball.display())))?;
    let mut bytes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(Error::spawn(format!("read {}", tarball.display())))?;
        if entry.path_bytes().as_ref() == BINARY.as_bytes() {
            entry
                .take(BINARY_CAP)
                .read_to_end(&mut bytes)
                .map_err(Error::spawn(format!(
                    "read {BINARY} of {}",
                    tarball.display()
                )))?;
            break;
        }
    }
    if bytes.is_empty() {
        return Err(Error::Failed(format!(
            "{}: no {BINARY} to run",
            tarball.display()
        )));
    }
    let program = scratch.join(BINARY);
    std::fs::write(&program, &bytes)
        .map_err(Error::spawn(format!("write {}", program.display())))?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .map_err(Error::spawn(format!("chmod {}", program.display())))?;
    let out = Command::new(&program)
        .args(["--version", "--json"])
        .output()
        .map_err(Error::spawn(program.display().to_string()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "{BINARY} --version --json of {commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end();
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(text);
    if text.len() > IDENTITY_CAP || !parsed.is_ok_and(|v| v.is_object()) {
        return Err(Error::Failed(format!(
            "{BINARY} --version --json of {commit} did not print one JSON object of under \
             {IDENTITY_CAP} bytes"
        )));
    }
    Ok(text.to_owned())
}
