//! The provenance bundle of a release tarball (docs/artifacts.md): `<tarball>.sigstore.json`,
//! the Sigstore bundle of the one build provenance attestation the `attest-*` job signed
//! for that tarball alone, so a host can verify the tarball offline
//! (`gh attestation verify <tarball> --bundle <bundle>`) without fetching anything from
//! GitHub.
//!
//! A bundle is made after `SHA256SUMS` (the attestation signs the sums' files), so the
//! sums cannot list it: [`is_bundle`] is how `verify-assets sums` knows to leave it
//! out, and [`pairing`] is how `release check` and `verify-assets attestations` hold the
//! set to exactly one bundle per listed tarball, each the bundle of that tarball: the
//! statement it carries has the tarball's digest as its one subject ([`check`]).
//! Whether a bundle is a valid, signed attestation of its tarball for the commit is
//! `gh`'s answer, asked by `verify-assets attestations`
//! ([`crate::sums::verify_attestations`]), with the flags a host's installer uses.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::Error;
use crate::fetch::Sha256;
use crate::package::read;
use crate::sums::{Line, files_in};

/// What a bundle's file name adds to its tarball's.
pub const SUFFIX: &str = ".sigstore.json";

/// The most a bundle may be: what a host's order record has room for, per component.
pub const MAX_BYTES: usize = 256 * 1024;

/// The OIDC issuer of GitHub Actions, which signs the attestation's certificate.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The kind of attestation the bundle holds: SLSA build provenance.
pub const PROVENANCE: &str = "https://slsa.dev/provenance/v1";

/// A proxy that refuses every connection (the discard port on loopback): a verification
/// run under it that succeeds has not used the network.
pub const DEAD_PROXY: &str = "http://127.0.0.1:9";

/// What a Sigstore bundle's `mediaType` starts with.
const MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle";

/// The file name of the bundle of the tarball `tarball`.
pub fn name_of(tarball: &str) -> String {
    format!("{tarball}{SUFFIX}")
}

/// Whether the file name `name` is a bundle's (it is not an asset `SHA256SUMS` lists).
pub fn is_bundle(name: &str) -> bool {
    name.ends_with(SUFFIX)
}

/// `commit` with its last hex digit changed: a commit the bundle was not made for.
pub fn other_commit(commit: &str) -> String {
    let (head, last) = commit.split_at(commit.len().saturating_sub(1));
    format!("{head}{}", if last == "0" { "1" } else { "0" })
}

/// What is wrong with `bytes` as the file of the bundle of the tarball whose SHA-256 is
/// `sha256`: one line per problem, none when it is at most [`MAX_BYTES`], exactly one
/// line holding a JSON object with the three fields every Sigstore bundle has, and the
/// statement its DSSE envelope carries is a build provenance attestation with one
/// subject, that tarball ([`check_subject`]). (That is its shape and whose it is only;
/// `gh` judges the signature.) A shape problem is reported alone: the subject of
/// something that is not a bundle is not a second finding.
pub fn check(bytes: &[u8], sha256: &Sha256) -> Vec<String> {
    let mut problems = Vec::new();
    if bytes.len() > MAX_BYTES {
        problems.push(format!(
            "is {} bytes, over the {MAX_BYTES} bytes a host's order record allows",
            bytes.len()
        ));
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        problems.push("is not UTF-8".to_owned());
        return problems;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let [line] = lines.as_slice() else {
        problems.push(format!(
            "holds {} lines, not exactly one JSON line",
            lines.len()
        ));
        return problems;
    };
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            problems.push(format!("is not JSON: {e}"));
            return problems;
        }
    };
    let Some(bundle) = value.as_object() else {
        problems.push("is not a JSON object".to_owned());
        return problems;
    };
    let media = bundle.get("mediaType").and_then(|m| m.as_str());
    if !media.is_some_and(|m| m.starts_with(MEDIA_TYPE)) {
        problems.push(format!(
            "has no `mediaType` of a Sigstore bundle ({MEDIA_TYPE}...)"
        ));
    }
    for field in ["dsseEnvelope", "verificationMaterial"] {
        if !bundle.get(field).is_some_and(|v| v.is_object()) {
            problems.push(format!("has no `{field}` object"));
        }
    }
    if problems.is_empty() {
        problems.extend(check_subject(bundle, sha256));
    }
    problems
}

/// What is wrong with the statement in the DSSE envelope of `bundle` as the attestation
/// of the tarball whose SHA-256 is `sha256` alone: its `payload` (base64 of an in-toto
/// statement) is build provenance ([`PROVENANCE`]) with exactly one subject, whose
/// `sha256` digest is `sha256`. This is the comparison of a host's order record with its
/// bundle (docs/artifacts.md), made where the bundle is released, so a bundle copied
/// under another tarball's name never reaches a release.
fn check_subject(
    bundle: &serde_json::Map<String, serde_json::Value>,
    sha256: &Sha256,
) -> Vec<String> {
    let payload = bundle
        .get("dsseEnvelope")
        .and_then(|e| e.get("payload"))
        .and_then(|p| p.as_str());
    let Some(payload) = payload else {
        return vec!["has no `dsseEnvelope.payload` text".to_owned()];
    };
    let Ok(decoded) = STANDARD.decode(payload) else {
        return vec!["has a `dsseEnvelope.payload` that is not base64".to_owned()];
    };
    let Ok(statement) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return vec!["has a payload that is not a JSON statement".to_owned()];
    };
    let mut problems = Vec::new();
    let kind = statement.get("predicateType").and_then(|k| k.as_str());
    if kind != Some(PROVENANCE) {
        problems.push(format!(
            "has a payload of predicate type {:?}, not build provenance ({PROVENANCE})",
            kind.unwrap_or_default()
        ));
    }
    let subjects = statement
        .get("subject")
        .and_then(|s| s.as_array())
        .map_or(&[][..], Vec::as_slice);
    let [subject] = subjects else {
        problems.push(format!(
            "names {} subjects, not exactly one",
            subjects.len()
        ));
        return problems;
    };
    let got = subject
        .get("digest")
        .and_then(|d| d.get("sha256"))
        .and_then(|d| d.as_str());
    if got != Some(sha256.to_string().as_str()) {
        problems.push(format!(
            "has the subject digest sha256:{}, not the tarball's sha256:{sha256}",
            got.unwrap_or("(none)")
        ));
    }
    problems
}

/// What is wrong with the bundle files of `dir` against the tarballs `lines` (the
/// sums) lists: for each tarball its `<tarball>.sigstore.json` is there and is the
/// bundle of that tarball ([`check`]), and no bundle file is there for anything
/// else. One line per problem, none when the set is exactly one bundle per tarball.
///
/// # Errors
/// `dir` or a bundle in it cannot be read.
pub fn pairing(dir: &Path, lines: &[Line]) -> Result<Vec<String>, Error> {
    let tarballs: Vec<&Line> = lines
        .iter()
        .filter(|l| l.name.ends_with(".tar.gz"))
        .collect();
    let present: Vec<String> = files_in(dir)?
        .into_iter()
        .filter(|n| is_bundle(n))
        .collect();
    let mut problems = Vec::new();
    for Line { name, sha256 } in &tarballs {
        let bundle = name_of(name);
        if !present.contains(&bundle) {
            problems.push(format!("{name}: no provenance bundle {bundle}"));
            continue;
        }
        for p in check(&read(&dir.join(&bundle))?, sha256) {
            problems.push(format!("{bundle}: {p}"));
        }
    }
    for bundle in &present {
        let owner = bundle.strip_suffix(SUFFIX).unwrap_or(bundle);
        if !tarballs.iter().any(|l| l.name == owner) {
            problems.push(format!("{bundle}: not the bundle of a listed tarball"));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests;
