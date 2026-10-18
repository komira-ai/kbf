//! `SHA256SUMS`: the one file that covers a commit's assets (docs/artifacts.md), and the
//! verbs that make and check it.
//!
//! A sums file is `<64 lowercase hex digits><two spaces><file name>` lines, each ending
//! in a newline, as `sha256sum` writes them; a name holds no `/` ([`parse`]).
//!
//! - `sums assemble <dir> [--expect <file>] [--summary <file>]` ([`assemble`]): merges
//!   the `SHA256SUMS.<platform>` files `package` wrote into `<dir>/SHA256SUMS`, sorted
//!   by name, and removes them. A name in two of them, or a `SHA256SUMS.<suffix>` that
//!   names no platform, is refused. `--expect` refuses a result that is not byte for
//!   byte that file (the published one); `--summary` appends the result to a file (the
//!   job summary).
//! - `verify-assets sums <dir>` ([`verify_sums`]): `<dir>/SHA256SUMS` lists every other
//!   file in `<dir>` (but the provenance bundles, which are made after the sums and so
//!   cannot be listed: [`crate::bundles`]), each once, and each matches its line.
//!   Control: a copy of the first tarball with its last byte flipped must not match
//!   that line, so a check that accepts anything fails instead of passing.
//! - `verify-assets attestations <dir> --repo <owner/repo> --workflow-ref <ref> --commit
//!   <sha> [--source-ref refs/heads/<branch>] [--trusted-root <file>] [--gh <program>]`
//!   ([`verify_attestations`]): every listed tarball and SBOM has build provenance
//!   signed by the workflow `--workflow-ref` names, for the branch `--source-ref` names
//!   (default `refs/heads/main`; a value that is not `refs/heads/<branch>` is a usage
//!   error), on a GitHub-hosted runner, and every tarball has an SBOM attestation from
//!   the same workflow: the `gh attestation verify` commands docs/artifacts.md gives.
//!   First, before `gh` is run, `<dir>` holds exactly one bundle per listed tarball and
//!   no other, each the bundle of that tarball ([`bundles::pairing`]). Every tarball
//!   also verifies against its own bundle, as a host's installer does it (`--bundle`,
//!   with `--cert-oidc-issuer`, `--source-digest <sha>`, `--predicate-type` and the
//!   flags above); with `--trusted-root` (`gh attestation trusted-root`) that check
//!   runs under a proxy nothing listens on and with no bypass list (`NO_PROXY` removed),
//!   so it cannot have used the network.
//!   Controls: the first tarball must not verify as signed by `ci.yml`, the flipped
//!   copy must not verify, the second tarball's bundle must not verify the first (a
//!   bundle is one tarball's own), the first's bundle must not verify for another
//!   commit, and the flipped copy must not verify against its bundle.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::build_commit::full_commit;
use crate::bundles;
use crate::fetch::Sha256;
use crate::package::{Platform, read, write_new};

/// The file name of the merged sums file.
pub const SUMS: &str = "SHA256SUMS";

/// The predicate type of an SBOM attestation.
pub const SBOM_PREDICATE: &str = "https://cyclonedx.org/bom";

/// The workflow the signer control names: one of this repository's that never signs.
pub const OTHER_WORKFLOW: &str = ".github/workflows/ci.yml";

/// One line of a sums file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub sha256: Sha256,
    pub name: String,
}

/// The lines of the sums file `text`, or one problem per line that is off the form.
pub fn parse(text: &str) -> Result<Vec<Line>, Vec<String>> {
    let mut lines = Vec::new();
    let mut problems = Vec::new();
    let mut rest = text;
    let mut number = 0;
    while !rest.is_empty() {
        number += 1;
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        let ended = rest.len() > line.len();
        rest = after;
        let parsed = line
            .split_once("  ")
            .filter(|_| ended)
            .and_then(|(hex, name)| Some((hex.parse::<Sha256>().ok()?, name)))
            .filter(|(_, name)| !name.is_empty() && !name.contains('/'));
        match parsed {
            Some((sha256, name)) => lines.push(Line {
                sha256,
                name: name.to_owned(),
            }),
            None => problems.push(format!(
                "line {number}: {line:?} is not `<sha256>  <file name>` and a newline"
            )),
        }
    }
    if problems.is_empty() {
        Ok(lines)
    } else {
        Err(problems)
    }
}

/// `lines` as a sums file.
pub fn render(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| format!("{}  {}\n", l.sha256, l.name))
        .collect()
}

/// The names `lines` lists more than once, each once, sorted.
pub fn duplicates(lines: &[Line]) -> Vec<&str> {
    let mut seen = BTreeMap::new();
    for l in lines {
        *seen.entry(l.name.as_str()).or_insert(0) += 1;
    }
    seen.into_iter()
        .filter(|&(_, n)| n > 1)
        .map(|(name, _)| name)
        .collect()
}

/// The names of the regular files in `dir`, sorted.
pub(crate) fn files_in(dir: &Path) -> Result<Vec<String>, Error> {
    let listing = || format!("list {}", dir.display());
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(Error::spawn(listing()))? {
        let entry = entry.map_err(Error::spawn(listing()))?;
        let is_file = entry
            .file_type()
            .map_err(Error::spawn(listing()))?
            .is_file();
        if is_file {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The lines of the sums file at `path`; a missing, malformed or empty file is an error.
pub(crate) fn read_sums(path: &Path) -> Result<Vec<Line>, Error> {
    let text =
        std::fs::read_to_string(path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let lines = parse(&text).map_err(|problems| {
        Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", path.display()))
                .collect(),
        )
    })?;
    if lines.is_empty() {
        return Err(Error::Failed(format!("{} lists no file", path.display())));
    }
    Ok(lines)
}

/// The `sums assemble` verb: the lines to print.
pub fn assemble(
    dir: &Path,
    expect: Option<&Path>,
    summary: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let prefix = format!("{SUMS}.");
    let mut parts = Vec::new();
    for name in files_in(dir)? {
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        if Platform::named(suffix).is_none() {
            return Err(Error::Failed(format!(
                "{name}: {suffix:?} names no platform"
            )));
        }
        parts.push(dir.join(name));
    }
    if parts.is_empty() {
        return Err(Error::Failed(format!(
            "{} holds no {prefix}<platform> file",
            dir.display()
        )));
    }
    let mut lines = Vec::new();
    for part in &parts {
        lines.extend(read_sums(part)?);
    }
    let twice = duplicates(&lines);
    if !twice.is_empty() {
        return Err(Error::Refused(
            twice.iter().map(|n| format!("listed twice: {n}")).collect(),
        ));
    }
    lines.sort_by(|a, b| a.name.cmp(&b.name));
    let text = render(&lines);
    if let Some(expect) = expect {
        let published = read(expect)?;
        if published != text.as_bytes() {
            return Err(Error::Failed(format!(
                "the assembled {SUMS} is not {}:\n{text}",
                expect.display()
            )));
        }
    }
    write_new(&dir.join(SUMS), text.as_bytes())?;
    for part in &parts {
        std::fs::remove_file(part).map_err(Error::spawn(format!("remove {}", part.display())))?;
    }
    if let Some(summary) = summary {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(summary)
            .and_then(|mut f| write!(f, "```text\n{text}```\n"))
            .map_err(Error::spawn(format!("append to {}", summary.display())))?;
    }
    Ok(text.lines().map(str::to_owned).collect())
}

/// The first tarball `lines` lists, and its bytes with the last one flipped.
fn flipped_tarball<'a>(dir: &Path, lines: &'a [Line]) -> Result<(&'a Line, Vec<u8>), Error> {
    let Some(first) = lines.iter().find(|l| l.name.ends_with(".tar.gz")) else {
        return Err(Error::Failed(format!("{SUMS} lists no tarball")));
    };
    let mut bytes = read(&dir.join(&first.name))?;
    let Some(last) = bytes.last_mut() else {
        return Err(Error::Failed(format!("{} is empty", first.name)));
    };
    *last ^= 1;
    Ok((first, bytes))
}

/// The `verify-assets sums` verb, with the digest `digest` (the real one is
/// [`Sha256::of`]; tests pass a broken one to see the control refuse it): the line to
/// print.
pub fn verify_sums(dir: &Path, digest: fn(&[u8]) -> Sha256) -> Result<String, Error> {
    let lines = read_sums(&dir.join(SUMS))?;
    let mut problems: Vec<String> = duplicates(&lines)
        .iter()
        .map(|n| format!("listed twice: {n}"))
        .collect();
    // A bundle is made after the sums (the attestation signs their files), so the sums
    // cannot list it; `release check` holds the bundles to one per tarball.
    let present: Vec<String> = files_in(dir)?
        .into_iter()
        .filter(|n| n != SUMS && !bundles::is_bundle(n))
        .collect();
    for l in &lines {
        if !present.contains(&l.name) {
            problems.push(format!("listed, but not in {}: {}", dir.display(), l.name));
        } else if digest(&read(&dir.join(&l.name))?) != l.sha256 {
            problems.push(format!("{}: does not match its line", l.name));
        }
    }
    for name in &present {
        if !lines.iter().any(|l| &l.name == name) {
            problems.push(format!("not listed in {SUMS}: {name}"));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let (first, flipped) = flipped_tarball(dir, &lines)?;
    if digest(&flipped) == first.sha256 {
        return Err(Error::Failed(format!(
            "control failed: a flipped byte in {} still matched {SUMS}",
            first.name
        )));
    }
    Ok(format!(
        "sums: {} files match; the flipped copy does not",
        lines.len()
    ))
}

/// Where `verify-assets attestations` checks attestations, and with what.
pub struct Attestations<'a> {
    /// The repository, `<owner>/<repo>`.
    pub repo: &'a str,
    /// The workflow that signed them, `<owner>/<repo>/<path>@<ref>` as Actions sets
    /// `GITHUB_WORKFLOW_REF`.
    pub workflow_ref: &'a str,
    /// The branch they were signed on, as `refs/heads/<branch>`: `gh` verifies each
    /// attestation names it. A deployment of what a push to `main` built passes
    /// `refs/heads/main`.
    pub source_ref: &'a str,
    /// The commit the files were built from, a full SHA (on Actions, `GITHUB_SHA`):
    /// each tarball's bundle must attest this commit (`--source-digest`).
    pub commit: &'a str,
    /// A `trusted_root.jsonl` (`gh attestation trusted-root`) to verify the bundles
    /// with. With it the verification runs under a proxy nothing listens on, as a
    /// host's does; without it `gh` fetches the root itself.
    pub trusted_root: Option<&'a Path>,
    /// The `gh` program.
    pub gh: &'a Path,
    /// An empty directory for the flipped copy.
    pub scratch: &'a Path,
}

impl Attestations<'_> {
    /// Whether `gh attestation verify` accepts `file` as signed by `signer`, with
    /// `--predicate-type` `predicate` when given. An error when gh cannot be run.
    fn verifies(&self, file: &Path, signer: &str, predicate: Option<&str>) -> Result<bool, Error> {
        let mut gh = Command::new(self.gh);
        gh.args(["attestation", "verify"])
            .arg(file)
            .args(["--repo", self.repo, "--signer-workflow", signer])
            .args([
                "--source-ref",
                self.source_ref,
                "--deny-self-hosted-runners",
            ]);
        if let Some(p) = predicate {
            gh.args(["--predicate-type", p]);
        }
        let out = gh
            .output()
            .map_err(Error::spawn(self.gh.display().to_string()))?;
        Ok(out.status.success())
    }

    /// `verifies`, as a requirement: the error names the file and the signer.
    fn require(&self, file: &Path, signer: &str, predicate: Option<&str>) -> Result<(), Error> {
        if self.verifies(file, signer, predicate)? {
            return Ok(());
        }
        let what = predicate.unwrap_or("build provenance");
        Err(Error::Failed(format!(
            "{}: no {what} attestation signed by {signer} verifies",
            file.display()
        )))
    }

    /// Runs `gh attestation verify` on `file` against `bundle` for `commit`, with the
    /// flags a host's installer passes, in its order:
    ///
    /// ```text
    /// attestation verify <file> --bundle <bundle> [--custom-trusted-root <root>]
    ///   --repo R --signer-workflow S --cert-oidc-issuer I --source-ref REF
    ///   --source-digest <commit> --predicate-type P --deny-self-hosted-runners
    /// ```
    ///
    /// Returns whether it accepted the file, and what it printed on stderr. With a
    /// trusted root, the proxies point at nothing, so an acceptance used no network.
    /// An error when `gh` cannot be run.
    fn bundle_run(
        &self,
        file: &Path,
        bundle: &Path,
        signer: &str,
        commit: &str,
    ) -> Result<(bool, String), Error> {
        let mut gh = Command::new(self.gh);
        gh.args(["attestation", "verify"])
            .arg(file)
            .arg("--bundle")
            .arg(bundle);
        if let Some(root) = self.trusted_root {
            gh.arg("--custom-trusted-root").arg(root);
            // Both spellings of each variable (a Go program reads the upper case first),
            // and the bypass lists gone: a `NO_PROXY` the runner's environment holds
            // would let the call past the proxy to the network.
            for proxy in [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                gh.env(proxy, bundles::DEAD_PROXY);
            }
            gh.env_remove("NO_PROXY").env_remove("no_proxy");
            gh.env("GH_NO_UPDATE_NOTIFIER", "1");
        }
        gh.args(["--repo", self.repo, "--signer-workflow", signer])
            .args(["--cert-oidc-issuer", bundles::ISSUER])
            .args(["--source-ref", self.source_ref])
            .args(["--source-digest", commit])
            .args(["--predicate-type", bundles::PROVENANCE])
            .arg("--deny-self-hosted-runners");
        let out = gh
            .output()
            .map_err(Error::spawn(self.gh.display().to_string()))?;
        let said = String::from_utf8_lossy(&out.stderr).trim_end().to_owned();
        Ok((out.status.success(), said))
    }

    /// `bundle_run` on the tarball `name` of `dir` and its own bundle (which
    /// [`bundles::pairing`] has found there), as a requirement: the error names the
    /// tarball and its bundle, and holds what `gh` said (the tail of it: the reason is
    /// last).
    fn require_bundle(&self, dir: &Path, name: &str, signer: &str) -> Result<(), Error> {
        let bundle_name = bundles::name_of(name);
        let bundle = dir.join(&bundle_name);
        let file = dir.join(name);
        let (verified, said) = self.bundle_run(&file, &bundle, signer, self.commit)?;
        if verified {
            return Ok(());
        }
        let start = said.char_indices().rev().nth(999).unwrap_or((0, ' ')).0;
        let tail = &said[start..];
        Err(Error::Failed(format!(
            "{}: its bundle {bundle_name} does not verify: {tail}",
            file.display()
        )))
    }

    /// The controls on the bundle check, each a verification that must be refused:
    /// the tarball `first` against the bundle of the tarball `second` (when there is
    /// one: a bundle is one tarball's own), against its own for another commit, and
    /// the copy `flipped` of it (one byte changed) against its own.
    fn bundle_controls(
        &self,
        dir: &Path,
        first: &str,
        second: Option<&str>,
        flipped: &Path,
        signer: &str,
    ) -> Result<(), Error> {
        let file = dir.join(first);
        let own = dir.join(bundles::name_of(first));
        if let Some(second) = second {
            let theirs = dir.join(bundles::name_of(second));
            if self.bundle_run(&file, &theirs, signer, self.commit)?.0 {
                return Err(Error::Failed(format!(
                    "control failed: the bundle of {second} verified {first}: a bundle is to \
                     attest its own tarball only"
                )));
            }
        }
        let other = bundles::other_commit(self.commit);
        if self.bundle_run(&file, &own, signer, &other)?.0 {
            return Err(Error::Failed(format!(
                "control failed: the bundle of {first} verified for commit {other}, not the \
                 commit it attests"
            )));
        }
        if self.bundle_run(flipped, &own, signer, self.commit)?.0 {
            return Err(Error::Failed(format!(
                "control failed: a flipped byte in {first} still verified against its bundle"
            )));
        }
        Ok(())
    }
}

/// The `verify-assets attestations` verb: the lines to print.
pub fn verify_attestations(dir: &Path, a: &Attestations) -> Result<Vec<String>, Error> {
    let workflow = a
        .workflow_ref
        .split_once('@')
        .map_or(a.workflow_ref, |(w, _)| w);
    if a.source_ref
        .strip_prefix("refs/heads/")
        .is_none_or(str::is_empty)
    {
        return Err(Error::Usage(format!(
            "--source-ref {:?} is not a branch: expected refs/heads/<branch>",
            a.source_ref
        )));
    }
    let own = format!("{}/.github/workflows/", a.repo);
    if !workflow.starts_with(&own) {
        return Err(Error::Usage(format!(
            "--workflow-ref {:?} is not a workflow of {}",
            a.workflow_ref, a.repo
        )));
    }
    full_commit("--commit", a.commit)?;
    let lines = read_sums(&dir.join(SUMS))?;
    // One bundle per tarball and no other, each its tarball's: found here, before any
    // call to gh, as `release check` finds it (a stray bundle is otherwise a failure of
    // the release alone).
    let unpaired = bundles::pairing(dir, &lines)?;
    if !unpaired.is_empty() {
        return Err(Error::Refused(unpaired));
    }
    let mut printed = Vec::new();
    for l in &lines {
        let file = dir.join(&l.name);
        if l.name.ends_with(".tar.gz") {
            a.require(&file, workflow, None)?;
            a.require(&file, workflow, Some(SBOM_PREDICATE))?;
            a.require_bundle(dir, &l.name, workflow)?;
            printed.push(format!("verified: {} (provenance, SBOM, bundle)", l.name));
        } else if l.name.ends_with(".cdx.json") {
            a.require(&file, workflow, None)?;
            printed.push(format!("verified: {} (provenance)", l.name));
        } else {
            return Err(Error::Failed(format!(
                "{SUMS} lists {}, neither a tarball nor an SBOM",
                l.name
            )));
        }
    }
    let (first, flipped) = flipped_tarball(dir, &lines)?;
    let other = format!("{}/{OTHER_WORKFLOW}", a.repo);
    if a.verifies(&dir.join(&first.name), &other, None)? {
        return Err(Error::Failed(format!(
            "control failed: {} verified as signed by {other}",
            first.name
        )));
    }
    let flipped_path: PathBuf = a.scratch.join(&first.name);
    write_new(&flipped_path, &flipped)?;
    if a.verifies(&flipped_path, workflow, None)? {
        return Err(Error::Failed(format!(
            "control failed: a flipped byte in {} still verified",
            first.name
        )));
    }
    let second = lines
        .iter()
        .filter(|l| l.name.ends_with(".tar.gz"))
        .nth(1)
        .map(|l| l.name.as_str());
    a.bundle_controls(dir, &first.name, second, &flipped_path, workflow)?;
    printed.push(
        "attestations: every asset verifies; another signer workflow, another tarball's \
         bundle, another commit and a flipped byte do not"
            .to_owned(),
    );
    Ok(printed)
}

#[cfg(test)]
mod tests;
