//! `release stage-offer`: what the automatic gamma deploy is handed for one commit, made from
//! the release of that commit and from nothing else (docs/design/gamma-deploy.md):
//! `offer.json`, and `bundle.tar`, the tar `kbf-installer deploy --offer - --bundle -` reads.
//!
//! The offer is for gamma only. There is no `--target`, `--waive-beta` or `--operator`: beta
//! is always waived, in the name [`WAIVER_OPERATOR`] and for the reason [`waiver_reason`]
//! gives, and `release offer` refuses a prod offer that waives beta, so nothing here can ask
//! for prod. The verb makes no network write and sets nothing on any farm.
//!
//! In order, each step only after the ones before it passed:
//!
//! 1. the inputs: `--sha` is a full commit, and `--out` does not exist;
//! 2. the release ([`released_tag`]): the highest release tag must name `--sha`
//!    ([`tags::superseded`]), and be an annotated tag that names it. A commit with no release
//!    tag, or whose highest release tag names an ancestor (a push that released nothing, a
//!    change to the docs), has no release: the verb prints `staged=false` and
//!    `reason=no-release` and succeeds. A release newer than `--sha`, and any call that
//!    fails, is an error, never "no release";
//! 3. the release's assets are downloaded into a directory of this process alone;
//! 4. they are verified as `release offer` verifies them: `release check` (each `kbf` tarball
//!    has its version facts listed), then the attestations of every file, the bundles offline
//!    against the trusted root. Nothing of the release is run before this;
//! 5. only then the `kbf` of `--runner-platform` is taken out of its tarball and run
//!    ([`version_json`]). What it prints must be, byte for byte, the version facts the
//!    release holds for that platform (`kbf-<sha>-<p>.version.json`): the bundle carries
//!    the facts, the offer's `meta_log` is read from the run, and a release whose two
//!    disagree is refused;
//! 6. the release before it ([`previous`]) is fetched and verified the same way, except that
//!    its version facts are not required (it may predate them), and its `kbf` run too. Its commit is the offer's `previous`: the offer says whether going back to
//!    that release is reversible, and the host compares its own running build when it
//!    applies it. With no earlier release the offer is not reversible; any error is an error;
//! 7. the platforms the release completes ([`farm_platforms`]): those with every tarball a
//!    host of the platform needs ([`ReleasePlatform::components`]), each with its SBOM and
//!    bundle, and the version facts of its `kbf`. A platform that lacks one is left out and
//!    printed (`platform-skipped=<p>`), not an error: the host refuses an offer only when a
//!    node of a left-out platform exists. If no platform is complete the verb refuses;
//! 8. [`offer::plan`] makes the offer, for gamma, with beta waived. It checks the release
//!    again, hashes and attestations of every file, now that both `kbf` have run (both run
//!    as the user that can write the downloaded files): a file a run changed is refused,
//!    never bundled;
//! 9. `--out` is made, now that only writes can fail, and holds `offer.json` and
//!    `bundle.tar`;
//! 10. `--output` gets the lines, last: if it cannot be written the run fails with `--out`
//!     already made.
//!
//! `bundle.tar` is what the installer's bundle reader takes (`kbf-installer deploy --offer -
//! --bundle -`, which reads the offer back from it): `SHA256SUMS` (the release's, byte for
//! byte), `offer.json`, then for each complete platform, in [`ReleasePlatform::ALL`] order,
//! its version facts `kbf-<sha>-<p>.version.json` (the release's, byte for byte, which the
//! installer uploads to the release store and the store's offer to a node is made from), and
//! for each of its components `<c>-<sha>-<p>.tar.gz` and `<c>-<sha>-<p>.tar.gz.attestation`:
//! the release's `<c>-<sha>-<p>.tar.gz.sigstore.json` under the name the installer wants. No
//! SBOM and no other file goes in. Every entry is mode 0644, owner and group 0, time 0, so the
//! same release and offer give the same bytes.
//!
//! Prints `key=value` lines and appends them to `--output` (on Actions, `GITHUB_OUTPUT`):
//! `staged=true`, `tag`, `commit`, `reversible`, `previous-source` (`release` or `none`) and a
//! `platform-skipped` for each platform left out; or `staged=false`, `reason=no-release` and
//! a `detail`. Exit status: 0 for both, 1 for an error (a superseded commit among them), 2
//! for a usage error. GitHub has no neutral result for a job, and the workflow reads
//! `staged`, so "nothing to stage" is a success here and the caller needs no exit code map.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbf_gate::{Target, system_clock};
use kbf_types::ReleasePlatform;

use super::fetch_previous::{self, FetchPreviousArgs, tag_commit};
use super::offer::{self, Attester, Evidence, Gh, IDENTITY_CAP, Request, incomplete, render};
use super::tags::{self, Standing};
use crate::Error;
use crate::build_commit::{full_commit, write_executable};
use crate::bundles;
use crate::darwin::packaged_binary;
use crate::package::{Platform, read, write_new};
use crate::sums::{self, SUMS};
use crate::version_facts::{self, Pairing};

/// Who the beta waiver of every staged offer is recorded under.
pub const WAIVER_OPERATOR: &str = "gamma-pipeline";

/// Why beta is waived, before the release it is waived for ([`waiver_reason`]).
pub const WAIVER_REASON: &str = "beta is not attested yet; this release is offered to gamma only";

/// The reason the offer of the release `tag` records for waiving beta: it names the tag.
pub fn waiver_reason(tag: &str) -> String {
    format!("{WAIVER_REASON} (release {tag})")
}

/// The offer, in `--out` and in the bundle.
pub const OFFER_FILE: &str = "offer.json";

/// The bundle, in `--out`.
pub const BUNDLE_FILE: &str = "bundle.tar";

/// The most a tarball in the bundle may be: the installer's cap (a test holds the two equal).
pub const MAX_TARBALL: u64 = 512 * 1024 * 1024;

/// The most an attestation in the bundle may be: the installer's cap.
pub const MAX_ATTESTATION: u64 = 256 * 1024;

/// The most a platform's version facts in the bundle may be: the installer's cap, which is
/// the most a `kbf --version --json` may print.
pub const MAX_FACTS: u64 = kbf_types::VERSION_CAP as u64;

/// The most `SHA256SUMS` in the bundle may be: the installer's cap.
pub const MAX_SUMS: u64 = 1024 * 1024;

/// The most `offer.json` may be: the installer's cap.
pub const MAX_OFFER: u64 = 64 * 1024;

/// The mode of every entry of the bundle.
const MODE: u32 = 0o644;

/// How long a `kbf --version --json` may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a running `kbf` is looked at.
const POLL: Duration = Duration::from_millis(20);

/// The arguments of `release stage-offer`.
#[derive(clap::Args)]
pub struct StageOfferArgs {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The commit whose release is offered, a full SHA (on Actions, the release run's
    /// `head_sha`).
    #[arg(long)]
    pub sha: String,
    /// The directory `offer.json` and `bundle.tar` go in; it must not exist, and is made once
    /// everything is verified.
    #[arg(long)]
    pub out: PathBuf,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// A `trusted_root.jsonl` to verify the bundles with; default: `gh attestation
    /// trusted-root` asked of GitHub.
    #[arg(long)]
    pub trusted_root: Option<PathBuf>,
    /// The platform of the machine this runs on, whose `kbf` is run; default: this host's.
    #[arg(long, value_enum)]
    pub runner_platform: Option<Platform>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// What a release left out of the offer and why it was staged as it was.
#[derive(Debug)]
pub struct Staged {
    /// The release tag, annotated, naming `commit`.
    pub tag: String,
    /// The commit offered.
    pub commit: String,
    /// Whether the offer says going back to the release before is reversible.
    pub reversible: bool,
    /// `release` when there is a release before, `none` when there is not.
    pub previous_source: &'static str,
    /// The platforms the release does not complete, and what each lacks.
    pub skipped: Vec<(ReleasePlatform, String)>,
}

/// What `release stage-offer` made.
#[derive(Debug)]
pub enum Outcome {
    /// `--out` holds the offer and the bundle.
    Staged(Staged),
    /// The commit has no release (the sentence says why); nothing was downloaded or written.
    NoRelease(String),
}

impl Outcome {
    /// The `key=value` lines the verb prints.
    fn lines(&self) -> Vec<String> {
        match self {
            Outcome::NoRelease(why) => vec![
                "staged=false".to_owned(),
                "reason=no-release".to_owned(),
                format!("detail={why}"),
            ],
            Outcome::Staged(s) => {
                let mut lines = vec![
                    "staged=true".to_owned(),
                    format!("tag={}", s.tag),
                    format!("commit={}", s.commit),
                    format!("reversible={}", s.reversible),
                    format!("previous-source={}", s.previous_source),
                ];
                for (platform, _) in &s.skipped {
                    lines.push(format!("platform-skipped={platform}"));
                }
                lines
            }
        }
    }
}

/// The check of the attestations of every file of a release, with `scratch` an empty
/// directory for the flipped copy it makes.
fn attester<'a>(a: &'a StageOfferArgs, root: &'a Path, scratch: &'a Path) -> Gh<'a> {
    Gh {
        repo: &a.repo,
        trusted_root: Some(root),
        gh: &a.gh,
        scratch,
    }
}

/// Creates `dir` and its parents.
fn make_dir(dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))
}

/// A directory of this process alone, under the temporary directory.
fn scratch_path() -> PathBuf {
    std::env::temp_dir().join(format!("xtask-stage-offer-{}", std::process::id()))
}

/// The release tag of `sha`: the highest release tag, when it names `sha` itself. `None` when
/// there is no release tag, or the highest one names an ancestor of `sha` (a commit that
/// released nothing).
///
/// # Errors
/// A release newer than `sha` ([`tags::superseded`]), a call that fails, and a highest tag
/// that is not an annotated tag naming `sha`.
fn released_tag(gh: &Path, repo: &str, sha: &str) -> Result<Option<String>, Error> {
    let Standing::Is(version) = tags::superseded(gh, repo, sha)? else {
        return Ok(None);
    };
    let tag = version.tag();
    // The compare resolved the tag's name; the annotated tag object is what carries the
    // release, and it must name the commit too.
    let named = tag_commit(gh, repo, &tag)?;
    if named != sha {
        return Err(Error::Failed(format!(
            "{tag} is the highest release tag and compares identical to {sha}, but its tag \
             object names {named}, not {sha}"
        )));
    }
    Ok(Some(tag))
}

/// The platform a host that `uname` says is `os` on `machine` is, if it is one.
fn platform_named(os: &str, machine: &str) -> Result<Platform, Error> {
    match (os, machine) {
        ("Linux", "x86_64") => Ok(Platform::LinuxX86_64),
        ("Linux", "aarch64" | "arm64") => Ok(Platform::LinuxArm64),
        ("Darwin", "arm64") => Ok(Platform::DarwinArm64),
        _ => Err(Error::Usage(format!(
            "{os} on {machine} is no platform of a release: give --runner-platform"
        ))),
    }
}

/// The platform of this host.
fn host_platform() -> Result<Platform, Error> {
    let uname = rustix::system::uname();
    let os = uname.sysname().to_string_lossy();
    platform_named(&os, &uname.machine().to_string_lossy())
}

/// The platform of the installer's and the release's naming that `platform` is.
fn platform_of(platform: ReleasePlatform) -> Platform {
    match platform {
        ReleasePlatform::LinuxX86_64 => Platform::LinuxX86_64,
        ReleasePlatform::LinuxArm64 => Platform::LinuxArm64,
        ReleasePlatform::DarwinArm64 => Platform::DarwinArm64,
    }
}

/// The platforms a deploy of this release can serve, and the ones it cannot with what each
/// lacks. A platform is complete when [`incomplete`] finds nothing missing (a tarball, its
/// SBOM, its bundle, the version facts of its `kbf`) and every tarball a host of it needs
/// ([`ReleasePlatform::components`]) is one of `listed` (the names in `SHA256SUMS`): a
/// tarball of another binary (a `kbf-daemon` with no `kbf`) does not make a platform
/// complete.
fn farm_platforms(
    dir: &Path,
    listed: &[String],
    sha: &str,
) -> (Vec<ReleasePlatform>, Vec<(ReleasePlatform, String)>) {
    let mut complete = Vec::new();
    let mut skipped = Vec::new();
    for platform in ReleasePlatform::ALL {
        let mut missing = incomplete(dir, listed, sha, platform_of(platform));
        for component in platform.components() {
            let tarball = format!("{component}-{sha}-{platform}.tar.gz");
            if !listed.contains(&tarball) {
                missing.push(format!("platform {platform}: {tarball} is not in {SUMS}"));
            }
        }
        if missing.is_empty() {
            complete.push(platform);
        } else {
            skipped.push((platform, missing.join("; ")));
        }
    }
    (complete, skipped)
}

/// `binary --version --json` into the file `out`, run as little as it can be: in its own
/// directory, with no environment, no input, no stderr (it would go to the job's public log),
/// at most `wait`, and at most [`IDENTITY_CAP`] bytes of answer. It must exit 0. A binary that
/// outlasts `wait` is killed, and one whose output does not end within `wait` of its exit (a
/// process it left holding the pipe) is an error.
///
/// Run only on a binary taken from a release that was verified: it is the attested build, but
/// it is still not given the job's credentials.
///
/// # Errors
/// The binary does not start, outlasts `wait`, leaves its output open, answers with more than
/// the cap, fails, or `out` cannot be written.
pub fn version_json(binary: &Path, out: &Path, wait: Duration) -> Result<(), Error> {
    let what = || format!("{} --version --json", binary.display());
    let failed = |e: std::io::Error| Error::spawn(what())(e);
    let mut child = Command::new(binary)
        .args(["--version", "--json"])
        .env_clear()
        .current_dir(binary.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Error::spawn(binary.display().to_string()))?;
    let stdout = child.stdout.take().expect("the child's stdout is piped");
    // Read while it runs, so that a full pipe cannot hold it; past the cap the pipe closes
    // and the child's next write fails. The answer comes back by channel, not by join: a
    // process the child left holding the pipe would hold a join for as long as it lives.
    let (send, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let got = stdout.take(IDENTITY_CAP + 1).read_to_end(&mut bytes);
        // The receiver is gone once the wait ran out, and then nobody needs the answer.
        let _ = send.send(got.map(|_| bytes));
    });
    let deadline = Instant::now() + wait;
    let mut late = false;
    loop {
        if child.try_wait().map_err(failed)?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            late = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(POLL);
    }
    // Reaped in every case; a killed child closes its pipe, which ends the reader.
    let status = child.wait().map_err(failed)?;
    if late {
        return Err(Error::Failed(format!(
            "{}: no answer within {wait:?}",
            what()
        )));
    }
    let open = |_| Error::Failed(format!("{}: its output did not end", what()));
    let ended = answer.recv_timeout(wait).map_err(open)?;
    let bytes = ended.map_err(failed)?;
    if bytes.len() as u64 > IDENTITY_CAP {
        return Err(Error::Failed(format!(
            "{}: more than {IDENTITY_CAP} bytes",
            what()
        )));
    }
    if !status.success() {
        return Err(Error::Failed(format!("{}: {status}", what())));
    }
    std::fs::write(out, &bytes).map_err(Error::spawn(format!("write {}", out.display())))
}

/// `gh attestation trusted-root` into a file of `scratch`: its path.
fn trusted_root(gh: &Path, scratch: &Path) -> Result<PathBuf, Error> {
    let out = Command::new(gh)
        .args(["attestation", "trusted-root"])
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "gh attestation trusted-root: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    let path = scratch.join("trusted_root.jsonl");
    let wrote = std::fs::write(&path, &out.stdout);
    wrote.map_err(Error::spawn(format!("write {}", path.display())))?;
    Ok(path)
}

/// The release before `--sha`, as `release fetch-previous` finds it: its commit, and the file its
/// `kbf --version --json` wrote. `None` when there is no release before.
struct Before {
    sha: String,
    version: PathBuf,
}

/// Fetches, verifies and runs the `kbf` of the release before `a.sha`.
fn previous_build(
    a: &StageOfferArgs,
    platform: Platform,
    root: &Path,
    scratch: &Path,
) -> Result<Option<Before>, Error> {
    let unpacked = scratch.join("previous");
    let lines = fetch_previous::run(&FetchPreviousArgs {
        repo: a.repo.clone(),
        sha: a.sha.clone(),
        platform,
        binaries: vec!["kbf".to_owned()],
        out: unpacked.clone(),
        trusted_root: Some(root.to_path_buf()),
        output: None,
        gh: a.gh.clone(),
    })?;
    let Some(sha) = lines.iter().find_map(|l| l.strip_prefix("previous-sha=")) else {
        return Ok(None);
    };
    let version = scratch.join("previous-version.json");
    version_json(&unpacked.join("kbf"), &version, RUN_TIMEOUT)?;
    Ok(Some(Before {
        sha: sha.to_owned(),
        version,
    }))
}

/// Steps 3 to 9, in the directory `scratch`, for the release `tag` of `a.sha`.
fn stage_release(a: &StageOfferArgs, tag: &str, scratch: &Path) -> Result<Staged, Error> {
    let platform = match a.runner_platform {
        Some(platform) => platform,
        None => host_platform()?,
    };
    let assets = scratch.join("assets");
    // Each check of the attestations flips a copy of the release into a directory of its own.
    let (flipped, rechecked) = (scratch.join("flipped"), scratch.join("rechecked"));
    for dir in [&assets, &flipped, &rechecked] {
        make_dir(dir)?;
    }
    let root = match &a.trusted_root {
        Some(root) => root.clone(),
        None => trusted_root(&a.gh, scratch)?,
    };
    // 3. The release.
    let download = assets.display().to_string();
    super::gh_run(
        &a.gh,
        &[
            "release", "download", tag, "--repo", &a.repo, "--dir", &download,
        ],
    )?;
    // 4. Verified, before any of it is run. A release made since the build jobs record version
    // facts has them for every `kbf` tarball.
    super::check(&assets, &a.sha, Pairing::Required)?;
    attester(a, &root, &flipped).artifacts(&assets, &a.sha)?;
    // 5. The runner's own `kbf`, out of the verified tarball.
    let tarball_name = format!("kbf-{}-{}.tar.gz", a.sha, platform.name());
    let tarball = assets.join(&tarball_name);
    let bytes = packaged_binary(&tarball, &read(&tarball)?, "kbf")?;
    let run_dir = scratch.join("run");
    make_dir(&run_dir)?;
    let kbf = run_dir.join("kbf");
    write_executable(&kbf, &bytes)?;
    let version = scratch.join("version.json");
    version_json(&kbf, &version, RUN_TIMEOUT)?;
    // The facts the build job kept for this platform are what its `kbf` prints: they go in the
    // bundle and a node's offer is made from them, while the offer's `meta_log` is read from
    // the run. Both come from the one build, or the release is refused.
    let facts_file = version_facts::name(&a.sha, platform);
    if read(&assets.join(&facts_file))? != read(&version)? {
        return Err(Error::Failed(format!(
            "{facts_file} is not what the kbf in {tarball_name} printed for --version --json"
        )));
    }
    // 6. The release before it.
    let before = previous_build(a, platform, &root, scratch)?;
    // 7. The platforms the release completes.
    let listed: Vec<String> = sums::read_sums(&assets.join(SUMS))?
        .into_iter()
        .map(|l| l.name)
        .collect();
    let (complete, skipped) = farm_platforms(&assets, &listed, &a.sha);
    if complete.is_empty() {
        return Err(Error::Refused(
            skipped.into_iter().map(|(_, why)| why).collect(),
        ));
    }
    // 8. The offer, which checks the release a second time: the runs above could write to it.
    let farm: Vec<Platform> = complete.iter().map(|p| platform_of(*p)).collect();
    let reason = waiver_reason(tag);
    let doc = offer::plan(
        &Request {
            dir: &assets,
            commit: &a.sha,
            target: Target::Gamma,
            platforms: &farm,
            beta: Evidence::Waived {
                reason: &reason,
                operator: WAIVER_OPERATOR,
            },
            version_json: Some(version.as_path()),
            previous: before.as_ref().map(|b| b.sha.as_str()),
            previous_version_json: before.as_ref().map(|b| b.version.as_path()),
            now: system_clock(),
        },
        &attester(a, &root, &rechecked),
    )?;
    // 9. Everything is in memory; `--out` is made, and written.
    let offer_bytes = render(&doc)?;
    let bundle = build_bundle(&assets, &a.sha, &complete, &offer_bytes)?;
    make_dir(&a.out)?;
    offer::write(&a.out.join(OFFER_FILE), &doc)?;
    write_new(&a.out.join(BUNDLE_FILE), &bundle)?;
    Ok(Staged {
        tag: tag.to_owned(),
        commit: a.sha.clone(),
        reversible: doc.reversible,
        previous_source: before.as_ref().map_or("none", |_| "release"),
        skipped,
    })
}

/// The `release stage-offer` verb: what was made.
///
/// # Errors
/// As the module says.
pub fn stage(a: &StageOfferArgs) -> Result<Outcome, Error> {
    full_commit("--sha", &a.sha)?;
    if a.out.exists() {
        return Err(Error::Usage(format!(
            "--out {} exists: an offer is staged into a directory of its own",
            a.out.display()
        )));
    }
    let Some(tag) = released_tag(&a.gh, &a.repo, &a.sha)? else {
        return Ok(Outcome::NoRelease(format!(
            "no release tag names {}: nothing to stage",
            a.sha
        )));
    };
    let scratch = scratch_path();
    // Nothing in it is needed after the run, whatever its result; and a directory an earlier
    // process of this id left is not this run's.
    let _ = std::fs::remove_dir_all(&scratch);
    let staged = stage_release(a, &tag, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    staged.map(Outcome::Staged)
}

/// The `release stage-offer` verb: the lines to print.
pub fn run(a: &StageOfferArgs) -> Result<Vec<String>, Error> {
    let outcome = stage(a)?;
    let lines = outcome.lines();
    if let Outcome::Staged(staged) = &outcome {
        for (_, why) in &staged.skipped {
            eprintln!("xtask: left out of the offer: {why}");
        }
    }
    if let Some(output) = &a.output {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
            .map_err(Error::spawn(format!("append to {}", output.display())))?;
    }
    Ok(lines)
}

/// The content of `path`, refused when it is longer than `cap`: what the installer's reader
/// would refuse, found before anything is made.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, Error> {
    let len = std::fs::metadata(path)
        .map_err(Error::spawn(format!("stat {}", path.display())))?
        .len();
    if len > cap {
        return Err(Error::Failed(format!(
            "{} is {len} bytes, more than the {cap} a deploy takes",
            path.display()
        )));
    }
    read(path)
}

/// Appends `bytes` to `tar` as the file `name`: mode 0644, owner and group 0, time 0.
fn append(tar: &mut tar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_ustar();
    // Every name is of the forms above and fits a ustar header.
    header
        .set_path(name)
        .expect("a name that fits a ustar header");
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_mode(MODE);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append(&header, bytes).expect("tar into memory");
}

/// The bundle of the release in `dir` for `sha`: `SHA256SUMS`, `offer` as `offer.json`, then
/// for each of `platforms` its version facts, and for each of its components the tarball and
/// its attestation (the release's bundle of it, renamed). Entries are in that order, with fixed
/// headers, so the same inputs give the same bytes.
///
/// # Errors
/// A file is missing, or is larger than the installer takes (`MAX_*`).
pub fn build_bundle(
    dir: &Path,
    sha: &str,
    platforms: &[ReleasePlatform],
    offer: &[u8],
) -> Result<Vec<u8>, Error> {
    let sums = read_capped(&dir.join(SUMS), MAX_SUMS)?;
    if offer.len() as u64 > MAX_OFFER {
        return Err(Error::Failed(format!(
            "{OFFER_FILE} is {} bytes, more than the {MAX_OFFER} a deploy takes",
            offer.len()
        )));
    }
    let mut tar = tar::Builder::new(Vec::new());
    append(&mut tar, SUMS, &sums);
    append(&mut tar, OFFER_FILE, offer);
    for platform in platforms {
        let facts = version_facts::name(sha, platform_of(*platform));
        let held = read_capped(&dir.join(&facts), MAX_FACTS)?;
        append(&mut tar, &facts, &held);
        for component in platform.components() {
            let name = format!("{component}-{sha}-{platform}.tar.gz");
            let tarball = read_capped(&dir.join(&name), MAX_TARBALL)?;
            let attestation = read_capped(&dir.join(bundles::name_of(&name)), MAX_ATTESTATION)?;
            append(&mut tar, &name, &tarball);
            append(&mut tar, &format!("{name}.attestation"), &attestation);
        }
    }
    Ok(tar.into_inner().expect("tar into memory"))
}

#[cfg(test)]
mod tests;
