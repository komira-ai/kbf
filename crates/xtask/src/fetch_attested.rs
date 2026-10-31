//! `fetch-attested-tool`: a pinned release archive whose build attestation is checked before a program
//! is taken out of it (the `js-bundle` of the darwin job).
//!
//! `--url <https url> --sha256 <hex> --archive <path> --member <path in the archive> --out <path>
//! --gh <program> --repo <owner/repo> --signer-workflow <path> --source-ref <ref> --source-digest <sha>`
//! does, in this order, and stops at the first refusal:
//!
//! 1. refuses a `--member` that is empty, absolute, or has a `..` part, before anything is read;
//! 2. downloads the archive over HTTPS ([`crate::fetch::download`]) and refuses it unless its SHA-256 is the
//!    pinned one, before any byte is written;
//! 3. writes it to `--archive`, creating its directory (a file that must not exist: the file `gh`
//!    verifies is the file the program comes out of, never a second download);
//! 4. runs `gh attestation verify <archive>` with `--repo`, `--signer-workflow`, `--cert-oidc-issuer`
//!    (GitHub Actions), `--source-ref`, `--source-digest` and `--deny-self-hosted-runners`, and refuses the
//!    archive unless it accepts it;
//! 5. takes `--member` out of that file (a regular file of a gzip tar) and writes it to `--out`, mode 0755.
//!
//! So the program is never taken from bytes whose digest or provenance was not checked, and `--out` does
//! not exist when a check refuses.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use rustls::pki_types::CertificateDer;

use crate::bundles;
use crate::fetch::{self, Sha256, Unpack};
use crate::package::write_new;
use crate::{Error, fetch::Format};

/// What to fetch and what to hold it to.
pub struct Request<'a> {
    pub url: &'a str,
    pub sha256: Sha256,
    /// Where the archive is written (must not exist).
    pub archive: PathBuf,
    /// The file in the gzip tar to install.
    pub member: PathBuf,
    pub out: PathBuf,
    pub repo: &'a str,
    pub signer_workflow: &'a str,
    pub source_ref: &'a str,
    pub source_digest: &'a str,
}

/// Runs `gh attestation verify`; the real one starts `gh`, a test passes its own.
pub trait Verifier {
    /// Whether the verification of `file` with `args` (the words after `attestation verify <file>`)
    /// accepted it, and what was printed on stderr. An error when it could not be run.
    fn verify(&self, file: &Path, args: &[String]) -> Result<(bool, String), Error>;
}

/// The `gh` program.
pub struct Gh<'a>(pub &'a Path);

impl Verifier for Gh<'_> {
    fn verify(&self, file: &Path, args: &[String]) -> Result<(bool, String), Error> {
        let out = Command::new(self.0)
            .args(["attestation", "verify"])
            .arg(file)
            .args(args)
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        let said = String::from_utf8_lossy(&out.stderr).trim_end().to_owned();
        Ok((out.status.success(), said))
    }
}

/// The words after `attestation verify <file>` for `req`: the flags a host's installer passes, less the bundle.
pub fn verify_args(req: &Request<'_>) -> Vec<String> {
    [
        "--repo",
        req.repo,
        "--signer-workflow",
        req.signer_workflow,
        "--cert-oidc-issuer",
        bundles::ISSUER,
        "--source-ref",
        req.source_ref,
        "--source-digest",
        req.source_digest,
        "--deny-self-hosted-runners",
    ]
    .iter()
    .map(|w| (*w).to_owned())
    .collect()
}

/// Whether `member` is a relative path of plain names.
fn plain(member: &Path) -> bool {
    member.components().count() > 0
        && member
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// The steps above on the downloaded `bytes`: the line to print.
pub fn install_verified(
    req: &Request<'_>,
    bytes: &[u8],
    gh: &dyn Verifier,
) -> Result<String, Error> {
    let actual = Sha256::of(bytes);
    if actual != req.sha256 {
        return Err(fetch::Error::Mismatch {
            expected: req.sha256,
            actual,
        }
        .into());
    }
    if let Some(parent) = req.archive.parent() {
        std::fs::create_dir_all(parent)
            .map_err(Error::spawn(format!("create {}", parent.display())))?;
    }
    write_new(&req.archive, bytes)?;
    let (accepted, said) = gh.verify(&req.archive, &verify_args(req))?;
    if !accepted {
        return Err(Error::Failed(format!(
            "{}: no attestation signed by {} for {} at {} on {} verifies; nothing was installed: {said}",
            req.archive.display(),
            req.signer_workflow,
            req.repo,
            req.source_digest,
            req.source_ref
        )));
    }
    let unpack =
        Unpack::new(Some(Format::TarGz), Some(req.member.clone())).map_err(Error::Usage)?;
    fetch::install(bytes, &req.sha256, &unpack, &req.out)?;
    Ok(format!(
        "ok: {} has SHA-256 {} and a verified attestation; installed {} from {}",
        req.url,
        req.sha256,
        req.out.display(),
        req.member.display()
    ))
}

/// The `fetch-attested-tool` verb: the line to print.
pub fn fetch_attested_tool(req: &Request<'_>, gh: &Path) -> Result<String, Error> {
    download_and_install(req, &fetch::host_roots, &Gh(gh))
}

/// The verb with the TLS roots (read only after `--member` is accepted) and the verifier given.
pub fn download_and_install(
    req: &Request<'_>,
    roots: &dyn Fn() -> Vec<CertificateDer<'static>>,
    gh: &dyn Verifier,
) -> Result<String, Error> {
    if !plain(&req.member) {
        return Err(Error::Usage(format!(
            "--member {:?} is not a relative path of plain names",
            req.member
        )));
    }
    let bytes = fetch::download(req.url, &roots())?;
    install_verified(req, &bytes, gh)
}

#[cfg(test)]
mod tests;
