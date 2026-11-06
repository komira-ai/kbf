//! `fetch-pinned-js`: a program of the jobscheduler release that `kbf-js-version.toml` pins,
//! fetched and verified as that file says, for the real-service tests.
//!
//! `--pin <file> --program <name> --target <triple> --gh <program> --dir <dir>` reads the pin
//! ([`Pin::read`]), finds the row of `--program` and `--target` (no row is a refusal), and then
//! does what `fetch-attested-tool` does ([`crate::fetch_attested`]) with the values of that row:
//! downloads `<program>-<version>-<target>.tar.gz` of the release over HTTPS and refuses it unless
//! its SHA-256 is the pinned one; writes it to `<dir>/<archive name>` (a file that must not exist);
//! runs `gh attestation verify` on that file, signed by the pinned workflow of the pinned
//! repository, built from the pinned commit on the pinned ref, on a hosted runner; and only then
//! takes `<program>` out of that file into `<dir>/<program>`, mode 0755.
//!
//! The pin also names the attestation itself: the row's `attestation_log_index`, the entry of
//! the transparency log the attestation was recorded under. `gh` is run with `--format json`
//! ([`PinnedGh`]), and an attestation that verifies is accepted only when it is that entry
//! ([`logged`]); a release attested again later, even by the same workflow, is a refusal until
//! the pin is moved.
//!
//! Every value of the file is checked before anything is fetched ([`Pin::parse`]): a key the file
//! should not have, a missing one, a commit or digest of the wrong shape, two rows for one program
//! and target, are each a refusal naming the key.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::fetch::{self, Sha256};
use crate::fetch_attested::{self, Request, Verifier};

/// The release a pin names, and the archives of it the tests fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The repository the release is in, `<owner>/<repo>`.
    pub repository: String,
    /// The release's version, without the `v` of its tag.
    pub version: String,
    /// The commit the release was built from: 40 lowercase hex digits.
    pub commit: String,
    /// The workflow that signed the attestations, `<owner>/<repo>/.github/workflows/<file>`.
    pub signer_workflow: String,
    /// The ref the release was built on, `refs/...`.
    pub source_ref: String,
    /// One row per program and target, in the file's order.
    pub archives: Vec<Archive>,
}

/// One archive of the release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The program the archive holds.
    pub program: String,
    /// The target triple it was built for.
    pub target: String,
    /// The SHA-256 of the archive as downloaded.
    pub sha256: Sha256,
    /// The transparency log entry of its build attestation.
    pub attestation_log_index: u64,
}

/// The keys of `[release]`.
const RELEASE_KEYS: [&str; 5] = [
    "repository",
    "version",
    "commit",
    "signer_workflow",
    "source_ref",
];

/// The keys of an `[[archive]]` row.
const ARCHIVE_KEYS: [&str; 4] = ["program", "target", "sha256", "attestation_log_index"];

/// A refusal of the pin, naming what is wrong.
fn refused(what: impl std::fmt::Display) -> Error {
    Error::Failed(format!("kbf-js-version.toml: {what}"))
}

/// Refuses a key of `table` (named `name`) that is not one of `keys`.
fn only(table: &toml::Table, name: &str, keys: &[&str]) -> Result<(), Error> {
    match table.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(refused(format!("`{name}` has the unknown key `{key}`"))),
        None => Ok(()),
    }
}

/// The string `key` of `table` (named `name`).
fn text<'a>(table: &'a toml::Table, name: &str, key: &str) -> Result<&'a str, Error> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| refused(format!("`{name}` has no string `{key}`")))
}

/// Whether `word` is a plain name: letters, digits, `.`, `_` and `-`, not empty, not `.` or `..`.
fn plain(word: &str) -> bool {
    !word.is_empty()
        && word != "."
        && word != ".."
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `commit` is a full commit id: 40 lowercase hex digits.
fn full_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Pin {
    /// Reads and checks the pin at `path`.
    ///
    /// # Errors
    /// The file cannot be read, or [`Pin::parse`] refuses it.
    pub fn read(path: &Path) -> Result<Pin, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(Error::spawn(format!("read the pin {}", path.display())))?;
        Pin::parse(&text)
    }

    /// The pin `text` says, every value checked.
    ///
    /// # Errors
    /// Not TOML; a key other than those of [`Pin`] and [`Archive`], or one missing or of
    /// another type; a repository that is not `<owner>/<repo>` of plain names; a version that is
    /// not a plain name; a commit that is not 40 lowercase hex digits; a signer workflow outside
    /// the repository's `.github/workflows/`; a source ref that does not start `refs/`; no
    /// archive; a program or target that is not a plain name; a SHA-256 that is not 64 lowercase
    /// hex digits; a log index that is not a whole number of zero or more; two rows for one
    /// program and target.
    pub fn parse(source: &str) -> Result<Pin, Error> {
        let doc: toml::Table = source
            .parse()
            .map_err(|e| refused(format!("not TOML: {e}")))?;
        only(&doc, "the file", &["release", "archive"])?;
        let release = doc
            .get("release")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| refused("no `[release]` table"))?;
        only(release, "[release]", &RELEASE_KEYS)?;
        let repository = text(release, "[release]", "repository")?;
        let parts: Vec<&str> = repository.split('/').collect();
        if parts.len() != 2 || !parts.iter().all(|part| plain(part)) {
            return Err(refused(format!(
                "repository {repository:?} is not <owner>/<repo>"
            )));
        }
        let version = text(release, "[release]", "version")?;
        if !plain(version) || version.starts_with('v') {
            return Err(refused(format!(
                "version {version:?} is not a plain version (no `v`)"
            )));
        }
        let commit = text(release, "[release]", "commit")?;
        if !full_commit(commit) {
            return Err(refused(format!(
                "commit {commit:?} is not 40 lowercase hex digits"
            )));
        }
        let signer_workflow = text(release, "[release]", "signer_workflow")?;
        let workflows = format!("{repository}/.github/workflows/");
        let file = signer_workflow.strip_prefix(&workflows).unwrap_or_default();
        if !plain(file) {
            return Err(refused(format!(
                "signer_workflow {signer_workflow:?} is not a file of {workflows}"
            )));
        }
        let source_ref = text(release, "[release]", "source_ref")?;
        if !source_ref.starts_with("refs/") || source_ref.len() == "refs/".len() {
            return Err(refused(format!(
                "source_ref {source_ref:?} does not name a ref (refs/...)"
            )));
        }
        let rows = doc
            .get("archive")
            .and_then(toml::Value::as_array)
            .filter(|rows| !rows.is_empty())
            .ok_or_else(|| refused("no `[[archive]]` row"))?;
        let mut archives = Vec::new();
        let mut seen = BTreeSet::new();
        for (n, row) in rows.iter().enumerate() {
            let name = format!("[[archive]] {}", n + 1);
            let row = row
                .as_table()
                .ok_or_else(|| refused(format!("{name} is not a table")))?;
            let archive = Archive::parse(row, &name)?;
            if !seen.insert((archive.program.clone(), archive.target.clone())) {
                return Err(refused(format!(
                    "{name} repeats {} for {}",
                    archive.program, archive.target
                )));
            }
            archives.push(archive);
        }
        Ok(Pin {
            repository: repository.to_owned(),
            version: version.to_owned(),
            commit: commit.to_owned(),
            signer_workflow: signer_workflow.to_owned(),
            source_ref: source_ref.to_owned(),
            archives,
        })
    }

    /// The row of `program` built for `target`.
    ///
    /// # Errors
    /// The pin has no such row: nothing unpinned is fetched.
    pub fn archive(&self, program: &str, target: &str) -> Result<&Archive, Error> {
        self.archives
            .iter()
            .find(|a| a.program == program && a.target == target)
            .ok_or_else(|| refused(format!("no [[archive]] row for {program} on {target}")))
    }

    /// The file name of `archive` in the release: `<program>-<version>-<target>.tar.gz`.
    #[must_use]
    pub fn file_name(&self, archive: &Archive) -> String {
        format!("{}.tar.gz", self.stem(archive))
    }

    /// The release's URL of `archive`.
    #[must_use]
    pub fn url(&self, archive: &Archive) -> String {
        format!(
            "https://github.com/{}/releases/download/v{}/{}",
            self.repository,
            self.version,
            self.file_name(archive)
        )
    }

    /// The path of the program inside `archive`: `<program>-<version>-<target>/<program>`.
    #[must_use]
    pub fn member(&self, archive: &Archive) -> PathBuf {
        Path::new(&self.stem(archive)).join(&archive.program)
    }

    fn stem(&self, archive: &Archive) -> String {
        format!("{}-{}-{}", archive.program, self.version, archive.target)
    }

    /// What `fetch-pinned-js` asks of [`fetch_attested`] for `archive`, with its files in
    /// `dir`: the archive at `<dir>/<file name>`, the program at `<dir>/<program>`.
    #[must_use]
    pub fn request<'a>(&'a self, archive: &Archive, url: &'a str, dir: &Path) -> Request<'a> {
        Request {
            url,
            sha256: archive.sha256,
            archive: dir.join(self.file_name(archive)),
            member: self.member(archive),
            out: dir.join(&archive.program),
            repo: &self.repository,
            signer_workflow: &self.signer_workflow,
            source_ref: &self.source_ref,
            source_digest: &self.commit,
        }
    }
}

impl Archive {
    fn parse(row: &toml::Table, name: &str) -> Result<Archive, Error> {
        only(row, name, &ARCHIVE_KEYS)?;
        let program = text(row, name, "program")?;
        let target = text(row, name, "target")?;
        for (key, value) in [("program", program), ("target", target)] {
            if !plain(value) {
                return Err(refused(format!(
                    "{name}: {key} {value:?} is not a plain name"
                )));
            }
        }
        let sha256 = text(row, name, "sha256")?
            .parse::<Sha256>()
            .map_err(|e| refused(format!("{name}: sha256: {e}")))?;
        let attestation_log_index = row
            .get("attestation_log_index")
            .and_then(toml::Value::as_integer)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| {
                refused(format!(
                    "{name} has no attestation_log_index of zero or more"
                ))
            })?;
        Ok(Archive {
            program: program.to_owned(),
            target: target.to_owned(),
            sha256,
            attestation_log_index,
        })
    }
}

/// Whether the JSON `gh attestation verify --format json` printed lists an attestation recorded
/// under the transparency log entry `index` (`attestation.bundle.verificationMaterial.tlogEntries[].
/// logIndex`, a decimal string, as Sigstore's bundle writes it, or a number).
#[must_use]
pub fn logged(json: &[u8], index: u64) -> bool {
    let Ok(serde_json::Value::Array(verified)) = serde_json::from_slice(json) else {
        return false;
    };
    let wanted = index.to_string();
    verified.iter().any(|one| {
        one.pointer("/attestation/bundle/verificationMaterial/tlogEntries")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| match entry.get("logIndex") {
                    Some(serde_json::Value::String(text)) => *text == wanted,
                    Some(serde_json::Value::Number(number)) => number.as_u64() == Some(index),
                    _ => false,
                })
            })
    })
}

/// `gh attestation verify` run with `--format json`, accepting the file only when `gh` does and
/// the attestations it verified include the pinned log entry.
pub struct PinnedGh<'a> {
    /// The `gh` program.
    pub gh: &'a Path,
    /// The transparency log entry the attestation must be.
    pub log_index: u64,
}

impl PinnedGh<'_> {
    /// The verdict on what `gh` answered: whether it `succeeded`, what it printed on `stdout` and
    /// on `stderr`; and what to say when the file is refused.
    #[must_use]
    pub fn judge(&self, succeeded: bool, stdout: &[u8], stderr: &str) -> (bool, String) {
        if !succeeded {
            return (false, stderr.to_owned());
        }
        if logged(stdout, self.log_index) {
            return (true, String::new());
        }
        (
            false,
            format!(
                "none of the attestations gh verified is the pinned transparency log entry {}",
                self.log_index
            ),
        )
    }
}

impl Verifier for PinnedGh<'_> {
    fn verify(&self, file: &Path, args: &[String]) -> Result<(bool, String), Error> {
        let out = Command::new(self.gh)
            .args(["attestation", "verify"])
            .arg(file)
            .args(args)
            .args(["--format", "json"])
            .output()
            .map_err(Error::spawn(self.gh.display().to_string()))?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_owned();
        Ok(self.judge(out.status.success(), &out.stdout, &stderr))
    }
}

/// The `fetch-pinned-js` verb: the line to print.
///
/// # Errors
/// The pin is refused or has no row for `program` on `target`; the download fails; its SHA-256
/// is not the pinned one; `gh` does not verify the pinned attestation of it; the program cannot
/// be taken out of it or written.
pub fn fetch_pinned_js(
    pin: &Path,
    program: &str,
    target: &str,
    gh: &Path,
    dir: &Path,
) -> Result<String, Error> {
    let pin = Pin::read(pin)?;
    let archive = pin.archive(program, target)?;
    let url = pin.url(archive);
    let verifier = PinnedGh {
        gh,
        log_index: archive.attestation_log_index,
    };
    let request = pin.request(archive, &url, dir);
    fetch_attested::download_and_install(&request, &fetch::host_roots, &verifier)
}

#[cfg(test)]
mod tests;
