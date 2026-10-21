//! `pkg-seed --dist <dir> --commit <sha> --platform <platform> --trust <dir> --out <dir>`: the
//! seed directory a `kbf-installer` package can carry (`kbf_installer::seed`), and the check
//! of one.
//!
//! A seed is one signed `kbf` release of the commit the installer is built from. The verb
//! makes `<out>`, which must not exist, from the three files the release already has in
//! `<dist>`, and puts four files in it. Every name derives from `--commit` and `--platform`
//! alone, never from what a file holds, so the seed is the release of that commit or it is
//! refused (a `<dist>` of another commit's files lacks the names this commit's seed has):
//!
//! - `manifest.json` ([`MANIFEST`]): what `kbf_installer::seed::Manifest` says, filled in from
//!   the files below and the digests of the trust directory, as `serde_json` pretty-prints it
//!   and a newline, with no time in it, so the same inputs are the same bytes;
//! - `kbf-<commit>-<platform>.tar.gz`: the release tarball `package` wrote, byte for byte (the
//!   attestation binds those bytes); it must hold the one regular file `kbf`, as the
//!   installer's shape check requires;
//! - `kbf-<commit>-<platform>.tar.gz.sigstore.json`: its attestation bundle, byte for byte; it
//!   must be a JSON object;
//! - `kbf-<commit>-<platform>.version.json` ([`crate::version_facts::name`]): what `kbf
//!   --version --json` printed for the build of that commit and platform, as the release's
//!   `version-facts` step kept it, byte for byte. Nothing here ties its bytes to the
//!   tarball's binary beyond the shared name; the installer's own probe of the staged binary
//!   (`kbf_installer::stage::Pipeline`) does. The installer does not read this file, and the
//!   manifest has no key for it. It is held here to the release the seed carries (the
//!   release build of the commit, a counted `seq` above 0, this platform), the manifest
//!   takes its numbers from it, and [`check`] refuses a seed whose facts and manifest
//!   disagree.
//!
//! [`build`] accumulates every problem of the inputs into one [`Error::Refused`] and writes
//! nothing. It reads the manifest it made with the installer's own parser and requires the
//! file names the installer derives from that manifest to be the names written, so a rename
//! on either side is caught here and not on a host. [`check`] judges a directory that
//! exists, for the package builder and the asset check to ask of what they are handed:
//! exactly the four names, each a regular file (a link is refused), the manifest this
//! module's reader accepts, the facts as `build` judges them, the tarball's and the bundle's
//! digests as the manifest says, and the manifest's commit, platform and trust digests as
//! [`Expect`] says. It does not open the tarball or parse the bundle.
//!
//! No package carries the seed yet.

use std::path::{Path, PathBuf};

use kbf_installer::CommitId;
use kbf_installer::binary::Version;
use kbf_installer::record::Sha256Hex;
use kbf_installer::seed::Manifest;
use serde_json::Value;

use crate::fetch::Sha256;
use crate::package::{self, Platform, read};
use crate::pkg_asset::members;
use crate::pkg_linux::TRUST_FILES;
use crate::{Error, build_commit, version_facts};

/// The manifest's file name in a seed directory: the installer's.
pub const MANIFEST: &str = kbf_installer::seed::MANIFEST;

/// Why a facts file without a `seq`, or with `seq` 0, is refused.
const SEQ_ZERO: &str = "the version facts have no seq, or seq 0: a build that does not count \
                        its commits (an old build, or a shallow checkout) cannot be put in order";

/// The sha256 of the three trust files a package ships beside the seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustDigests {
    pub gh: Sha256,
    pub trusted_root: Sha256,
    pub installer_conf: Sha256,
}

impl TrustDigests {
    /// The digests of the files [`TRUST_FILES`] names in `dir`.
    ///
    /// # Errors
    /// One of the three cannot be read.
    pub fn of_dir(dir: &Path) -> Result<TrustDigests, Error> {
        let [(gh, _), (conf, _), (root, _)] = TRUST_FILES;
        let digest = |name: &str| read(&dir.join(name)).map(|bytes| Sha256::of(&bytes));
        Ok(TrustDigests {
            gh: digest(gh)?,
            trusted_root: digest(root)?,
            installer_conf: digest(conf)?,
        })
    }
}

/// What [`build`] makes a seed from.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    /// The directory holding the release tarball, its attestation bundle and its version
    /// facts, each under the name the seed has it by.
    pub dist: &'a Path,
    /// The commit the release was built from, a full SHA.
    pub commit: &'a str,
    /// The platform of the release: a Linux one.
    pub platform: Platform,
    /// The digests of the trust files the package ships beside the seed.
    pub trust: TrustDigests,
}

/// What [`check`] holds a seed directory to.
#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    /// The commit the seed must be for, a full SHA.
    pub commit: &'a str,
    /// The platform the seed must be for: a Linux one.
    pub platform: Platform,
    /// The digests the manifest must give of the package's trust files.
    pub trust: TrustDigests,
}

/// A seed, whole and in memory: the files [`write`] puts in the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    /// The first 12 digits of the commit: the name of the seed's directory in a package.
    pub commit12: String,
    /// Each file's name and bytes: `manifest.json` first, then the other three by name.
    pub files: Vec<(String, Vec<u8>)>,
}

/// The arguments of `pkg-seed`.
#[derive(clap::Args)]
pub struct Args {
    /// The directory holding the release's tarball, bundle and version facts, each named
    /// `kbf-<commit>-<platform>` and `.tar.gz`, `.tar.gz.sigstore.json`, `.version.json`.
    #[arg(long)]
    pub dist: PathBuf,
    /// The commit the release was built from, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub commit: String,
    /// The platform of the release (`linux-x86_64`, `linux-arm64`).
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The trust directory of the package (`pkg-trust`'s `--out`): the digests of its three
    /// files go in the manifest.
    #[arg(long)]
    pub trust: PathBuf,
    /// The seed directory to make; it must not exist.
    #[arg(long)]
    pub out: PathBuf,
}

/// The names of the four files of the seed of a commit on a platform.
struct Names {
    tarball: String,
    bundle: String,
    facts: String,
}

impl Names {
    fn of(commit: &str, platform: Platform) -> Names {
        let stem = package::asset_stem(version_facts::BINARY, commit, platform);
        let tarball = format!("{stem}.tar.gz");
        Names {
            bundle: format!("{tarball}.sigstore.json"),
            facts: version_facts::name(commit, platform),
            tarball,
        }
    }

    /// All four names, the manifest's first.
    fn all(&self) -> [&str; 4] {
        [MANIFEST, &self.tarball, &self.bundle, &self.facts]
    }
}

/// What the facts file states, once it is judged: the numbers the manifest takes from it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stated {
    seq: u64,
    protocol: u64,
    min_protocol: u64,
    state_schema: u64,
    profile: String,
}

/// The commit as an id, when the commit and the platform are ones a seed is made for.
fn usage(commit: &str, platform: Platform) -> Result<CommitId, Error> {
    let Ok(id) = CommitId::new(commit) else {
        return Err(build_commit::not_full_commit("--commit", commit));
    };
    if platform == Platform::DarwinArm64 {
        let why = "--platform darwin-arm64: only Linux seeds are made";
        return Err(Error::Usage(why.to_owned()));
    }
    Ok(id)
}

/// `value` when it is `Ok`; otherwise its problems are added to `problems`.
fn settle<T>(value: Result<T, Vec<String>>, problems: &mut Vec<String>) -> Option<T> {
    value.map_err(|why| problems.extend(why)).ok()
}

/// The bytes of the regular file `name` in `dir`, or why it is not one. A link is not
/// followed: a symlink is refused.
fn take(dir: &Path, name: &str) -> Result<Vec<u8>, Vec<String>> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path);
    if !meta.is_ok_and(|m| m.file_type().is_file()) {
        let why = format!("{} is missing or is not a regular file", path.display());
        return Err(vec![why]);
    }
    read(&path).map_err(|e| vec![e.to_string()])
}

/// The file `name` in `dir`, if `check` accepts what it holds.
fn checked(
    dir: &Path,
    name: &str,
    check: fn(Vec<u8>) -> Result<Vec<u8>, Vec<String>>,
) -> Result<Vec<u8>, Vec<String>> {
    check(take(dir, name)?)
}

/// `bytes` as the release tarball: a gzip tar holding the one regular file `kbf`, as the
/// installer requires of a staged tarball. Entries that are not regular files are not seen
/// by `members`, so the installer's shape check on a host is what refuses those.
fn tarball_checked(bytes: Vec<u8>) -> Result<Vec<u8>, Vec<String>> {
    let binary = version_facts::BINARY;
    let found = match members(&bytes) {
        Ok(found) => found,
        Err(why) => return Err(vec![format!("the tarball: {why}")]),
    };
    let paths: Vec<&str> = found.iter().map(|m| m.path.as_str()).collect();
    if paths == [binary] {
        return Ok(bytes);
    }
    let why = format!("the tarball holds {paths:?}, not the one file {binary}");
    Err(vec![why])
}

/// `bytes` as the attestation bundle: a JSON object (not a sigstore bundle's schema; `gh`
/// judges that, on a host, against the tarball).
fn bundle_checked(bytes: Vec<u8>) -> Result<Vec<u8>, Vec<String>> {
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(_)) => Ok(bytes),
        Ok(_) => Err(vec!["the bundle is JSON, but not an object".to_owned()]),
        Err(e) => Err(vec![format!("the bundle is not JSON: {e}")]),
    }
}

/// What `bytes` state, if they are the version facts of the release build of `commit` for
/// `platform` with a counted `seq`; otherwise every reason they are not.
fn judge_facts(bytes: &[u8], commit: &str, platform: Platform) -> Result<Stated, Vec<String>> {
    let version = match Version::parse(bytes) {
        Ok(version) => version,
        Err(e) => return Err(vec![format!("the version facts: {e}")]),
    };
    let mut problems = Vec::new();
    if let Err(e) = version.check(commit) {
        problems.push(format!("the version facts: {e}"));
    }
    if matches!(version.seq, None | Some(0)) {
        problems.push(SEQ_ZERO.to_owned());
    }
    if version.platform.as_deref() != Some(platform.name()) {
        let said = version.platform.as_deref().unwrap_or("no platform");
        let why = format!("the version facts say {said}, not {}", platform.name());
        problems.push(why);
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Stated {
        seq: version.seq.unwrap_or_default(),
        protocol: version.protocol,
        min_protocol: version.min_protocol,
        state_schema: version.state_schema,
        profile: version.profile,
    })
}

/// The facts file `name` in `dir`, whole, and what it states.
fn facts_read(
    dir: &Path,
    name: &str,
    commit: &str,
    platform: Platform,
) -> Result<(Vec<u8>, Stated), Vec<String>> {
    let bytes = take(dir, name)?;
    let stated = judge_facts(&bytes, commit, platform)?;
    Ok((bytes, stated))
}

/// The manifest in `bytes`, by the installer's reader.
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, Vec<String>> {
    Manifest::parse(bytes).map_err(|why| vec![why])
}

/// The manifest file of the seed directory `dir`, by the installer's reader.
fn manifest_in(dir: &Path) -> Result<Manifest, Vec<String>> {
    parse_manifest(&take(dir, MANIFEST)?)
}

/// `n` as the manifest holds a protocol or a schema number; one that does not fit is a
/// problem.
fn narrow(what: &str, n: u64, problems: &mut Vec<String>) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| {
        problems.push(format!("the version facts' {what} {n} is over 32 bits"));
        0
    })
}

/// A digest as the manifest holds it.
fn digest_hex(digest: Sha256) -> Sha256Hex {
    // A `Sha256` shows as 64 lowercase hex digits, which is all a `Sha256Hex` is.
    Sha256Hex::try_from(digest.to_string()).expect("hex")
}

/// A problem, unless `found` is `expected`: `what` says of which.
fn unequal(what: &str, found: &str, expected: &str) -> Option<String> {
    if found == expected {
        return None;
    }
    Some(format!("{what} is {found}, not {expected}"))
}

/// The file names the installer derives from the manifest, against the ones written: the
/// builder and the reader must name the files alike.
fn naming(m: &Manifest, names: &Names) -> Vec<String> {
    let reads = m.tarball_name();
    let tarball = unequal("the installer's tarball name", &reads, &names.tarball);
    let reads = m.bundle_name();
    let bundle = unequal("the installer's bundle name", &reads, &names.bundle);
    tarball.into_iter().chain(bundle).collect()
}

/// The manifest's digests of the trust files, against `t`.
fn trust_unequal(m: &Manifest, t: &TrustDigests) -> Vec<String> {
    let conf = &m.installer_conf_sha256;
    let rows = [
        ("gh", &m.gh_sha256, t.gh),
        ("trusted_root", &m.trusted_root_sha256, t.trusted_root),
        ("installer_conf", conf, t.installer_conf),
    ];
    let mut problems = Vec::new();
    for (name, found, expected) in rows {
        let what = format!("manifest {name} sha256");
        problems.extend(unequal(&what, found.as_str(), &expected.to_string()));
    }
    problems
}

/// What the manifest says of the seed's commit, platform and trust files that `want` does not.
fn manifest_unequal(m: &Manifest, want: &Expect) -> Vec<String> {
    let commit = unequal("manifest commit", m.commit.as_str(), want.commit);
    let platform = want.platform.name();
    let mut problems = Vec::new();
    problems.extend(commit);
    problems.extend(unequal("manifest platform", &m.platform, platform));
    problems.extend(trust_unequal(m, &want.trust));
    problems
}

/// The tarball's digest and size against the manifest's.
fn tarball_unequal(m: &Manifest, bytes: &[u8]) -> Vec<String> {
    let sha256 = Sha256::of(bytes).to_string();
    let size = bytes.len().to_string();
    let digest = unequal("tarball sha256", &sha256, m.tarball_sha256.as_str());
    let length = unequal("tarball size", &size, &m.tarball_size.to_string());
    digest.into_iter().chain(length).collect()
}

/// The bundle's digest against the manifest's.
fn bundle_unequal(m: &Manifest, bytes: &[u8]) -> Vec<String> {
    let sha256 = Sha256::of(bytes).to_string();
    let digest = unequal("bundle sha256", &sha256, m.bundle_sha256.as_str());
    digest.into_iter().collect()
}

/// What the facts file states against the manifest's `seq`, numbers and profile.
fn facts_unequal(m: &Manifest, s: &Stated) -> Vec<String> {
    let rows = [
        ("seq", s.seq, m.seq),
        ("protocol", s.protocol, m.protocol.into()),
        ("min_protocol", s.min_protocol, m.min_protocol.into()),
        ("state_schema", s.state_schema, m.state_schema.into()),
    ];
    let mut problems = Vec::new();
    for (name, found, expected) in rows {
        let what = format!("version facts {name}");
        problems.extend(unequal(&what, &found.to_string(), &expected.to_string()));
    }
    problems.extend(unequal("version facts profile", &s.profile, &m.profile));
    problems
}

/// The entries of `dir` that are none of the four files of a seed.
fn unexpected(dir: &Path, names: &Names) -> Result<Vec<String>, Error> {
    let what = format!("read {}", dir.display());
    let listed = std::fs::read_dir(dir).map_err(Error::spawn(what))?;
    let mut extra: Vec<String> = listed
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !names.all().contains(&n.as_str()))
        .collect();
    extra.sort();
    let lines = extra.iter().map(|n| format!("unexpected file {n}"));
    Ok(lines.collect())
}

/// The seed of the release in `i.dist`, in memory.
///
/// # Errors
/// The commit or the platform is not one a seed is made for ([`Error::Usage`]), or an input
/// is refused ([`Error::Refused`], with every problem found).
pub fn build(i: &Inputs) -> Result<Seed, Error> {
    let id = usage(i.commit, i.platform)?;
    let names = Names::of(i.commit, i.platform);
    let mut problems = Vec::new();
    let tarball = checked(i.dist, &names.tarball, tarball_checked);
    let tarball = settle(tarball, &mut problems);
    let bundle = checked(i.dist, &names.bundle, bundle_checked);
    let bundle = settle(bundle, &mut problems);
    let facts = facts_read(i.dist, &names.facts, i.commit, i.platform);
    let facts = settle(facts, &mut problems);
    let (Some(tarball), Some(bundle), Some((facts, stated))) = (tarball, bundle, facts) else {
        return Err(Error::Refused(problems));
    };
    let manifest = Manifest {
        commit: id,
        seq: stated.seq,
        platform: i.platform.name().to_owned(),
        tarball_sha256: digest_hex(Sha256::of(&tarball)),
        tarball_size: tarball.len() as u64,
        bundle_sha256: digest_hex(Sha256::of(&bundle)),
        protocol: narrow("protocol", stated.protocol, &mut problems),
        min_protocol: narrow("min_protocol", stated.min_protocol, &mut problems),
        state_schema: narrow("state_schema", stated.state_schema, &mut problems),
        profile: stated.profile,
        gh_sha256: digest_hex(i.trust.gh),
        trusted_root_sha256: digest_hex(i.trust.trusted_root),
        installer_conf_sha256: digest_hex(i.trust.installer_conf),
    };
    // A struct of text and numbers always serializes.
    let mut json = serde_json::to_vec_pretty(&manifest).expect("json");
    json.push(b'\n');
    let reread = settle(parse_manifest(&json), &mut problems);
    problems.extend(reread.iter().flat_map(|m| naming(m, &names)));
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let mut rest = vec![
        (names.tarball, tarball),
        (names.bundle, bundle),
        (names.facts, facts),
    ];
    rest.sort();
    let mut files = vec![(MANIFEST.to_owned(), json)];
    files.extend(rest);
    Ok(Seed {
        commit12: i.commit[..12].to_owned(),
        files,
    })
}

/// The `pkg-seed` verb on `i`: [`build`], then the four files in the new directory `out`
/// (its parents are made when missing). The lines to print: where, and each file's sha256.
/// A file that cannot be written leaves the files before it, and the directory then exists:
/// the rerun needs another `out`.
///
/// # Errors
/// `out` exists, [`build`] refuses, or a file cannot be written.
pub fn write(i: &Inputs, out: &Path) -> Result<Vec<String>, Error> {
    if std::fs::symlink_metadata(out).is_ok() {
        return Err(Error::Failed(format!("{} already exists", out.display())));
    }
    let seed = build(i)?;
    let what = format!("create {}", out.display());
    std::fs::create_dir_all(out).map_err(Error::spawn(what))?;
    let first = format!("wrote {} for {}:", out.display(), seed.commit12);
    let mut lines = vec![first];
    for (name, bytes) in &seed.files {
        package::write_new(&out.join(name), bytes)?;
        lines.push(format!("{}  {name}", Sha256::of(bytes)));
    }
    Ok(lines)
}

/// What is wrong with the seed directory `dir` as the seed `want` describes: one line per
/// problem, none when it is whole.
///
/// # Errors
/// The commit or the platform is not one a seed is made for ([`Error::Usage`]), or `dir`
/// cannot be listed.
pub fn check(dir: &Path, want: &Expect) -> Result<Vec<String>, Error> {
    usage(want.commit, want.platform)?;
    let names = Names::of(want.commit, want.platform);
    let mut problems = unexpected(dir, &names)?;
    let manifest = settle(manifest_in(dir), &mut problems);
    let tarball = settle(take(dir, &names.tarball), &mut problems);
    let bundle = settle(take(dir, &names.bundle), &mut problems);
    let facts = facts_read(dir, &names.facts, want.commit, want.platform);
    let facts = settle(facts, &mut problems);
    if let Some(m) = &manifest {
        problems.extend(manifest_unequal(m, want));
        problems.extend(tarball.iter().flat_map(|t| tarball_unequal(m, t)));
        problems.extend(bundle.iter().flat_map(|b| bundle_unequal(m, b)));
        problems.extend(facts.iter().flat_map(|(_, s)| facts_unequal(m, s)));
    }
    Ok(problems)
}

/// The `pkg-seed` verb: the lines to print.
pub fn run(a: &Args) -> Result<Vec<String>, Error> {
    let trust = TrustDigests::of_dir(&a.trust)?;
    let inputs = Inputs {
        dist: &a.dist,
        commit: &a.commit,
        platform: a.platform,
        trust,
    };
    write(&inputs, &a.out)
}

#[cfg(test)]
mod tests;
