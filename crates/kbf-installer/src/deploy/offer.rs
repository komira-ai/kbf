//! `deploy --offer offer.json`: what a deploy checks of the offer (`kbf_gate::OfferDoc`,
//! written by `cargo xtask release offer`) before it moves the farm.
//!
//! The offer is a record, never a command, and the file's name is never trusted. A deploy
//! that has one refuses unless:
//!
//! - the document parses through [`OfferDoc`]'s validated path (an unknown field, another
//!   version, a prod offer that waived beta and a `reversible` the `meta_log` numbers do
//!   not give are all refused there);
//! - its `target` is the stage this host is ([`Stage`], from `--stage` or the host's
//!   config, never from the offer);
//! - its `commit` is the commit being deployed, and it is neither older than the allowed
//!   age nor dated in the future beyond clock skew (`created`);
//! - the bundle carries the release set's `SHA256SUMS`, whose hash is the offer's
//!   `set_hash`, and every tarball in the bundle is the file that sums name;
//! - the `kbf` of the host's own platform in the bundle, run as the unprivileged user,
//!   names the offer's `meta_log` in `--version --json` (a mismatch refuses, and so does
//!   an offer that claims a number where the binary names none, or the reverse);
//! - every platform the farm runs is one the offer found complete.
//!
//! The probe runs bytes the stage has not attested yet, so it runs as the installer's
//! `RUN_AS` user, never as root, in a scratch directory of its own. Its answer can only
//! refuse a deploy: the frontend's own update still verifies the attestation offline.
//! Without `--offer`, a deploy that names a stage is refused unless `--no-offer` says so;
//! the summary records either.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_gate::{OfferDoc, SetHash, Target};
use sha2::{Digest, Sha256};

use super::bundle::{Bundle, Entry};
use super::summary::OfferRecord;
use super::{smoke, why};
use crate::CommitId;
use crate::binary::Version;
use crate::layout::{Layout, Platform};
use crate::run::{RunAs, Runner, Spawn, SystemRunner};
use crate::settings::Settings;

/// The most an offer file may be.
pub const MAX_OFFER: u64 = 64 * 1024;
/// How far ahead of the host's clock an offer's `created` may be.
pub const SKEW: Duration = Duration::from_secs(300);
/// How old an offer may be unless `--offer-max-age-h` says otherwise, in hours.
pub const DEFAULT_MAX_AGE_H: u64 = 72;

/// The stage this host is. It decides which offers apply, so it comes from the host (the
/// root-owned config) or the operator's flag, and a caller through `sudo` cannot name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Gamma,
    Prod,
}

impl Stage {
    pub fn parse(text: &str) -> Result<Stage, String> {
        match text {
            "gamma" => Ok(Stage::Gamma),
            "prod" => Ok(Stage::Prod),
            other => Err(format!("stage {other:?} is not gamma or prod")),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Stage::Gamma => "gamma",
            Stage::Prod => "prod",
        }
    }

    fn target(self) -> Target {
        match self {
            Stage::Gamma => Target::Gamma,
            Stage::Prod => Target::Prod,
        }
    }
}

/// What the deploy requires of an offer.
#[derive(Clone, Debug)]
pub enum OfferPolicy {
    /// No stage is named, so there is nothing to bind an offer to. Only a plan may run so;
    /// the command line refuses an apply with no stage.
    Unchecked,
    /// The stage is named and there is no offer and no `--no-offer`: refused.
    Missing(Stage),
    /// `--no-offer`: a deliberate deploy with no offer, recorded in the summary.
    Waived(Stage),
    /// The offer, parsed.
    Offer {
        stage: Stage,
        doc: Box<OfferDoc>,
        max_age: Duration,
    },
}

/// A refusal: the summary's reason and detail.
pub(super) type Refused = (&'static str, String);

fn refuse(reason: &'static str, detail: impl Into<String>) -> Refused {
    (reason, detail.into())
}

/// Read `offer.json`: a regular file of at most [`MAX_OFFER`] bytes, parsed only through
/// [`OfferDoc::from_bytes`].
pub fn read(path: &Path) -> Result<OfferDoc, String> {
    let meta = fs::symlink_metadata(path).map_err(why("--offer"))?;
    if !meta.file_type().is_file() {
        return Err("--offer is not a regular file".to_owned());
    }
    if meta.len() > MAX_OFFER {
        return Err(format!("--offer is larger than {MAX_OFFER} bytes"));
    }
    let bytes = fs::read(path).map_err(why("--offer"))?;
    OfferDoc::from_bytes(&bytes).map_err(why("--offer"))
}

impl OfferPolicy {
    /// The summary's record of the policy; `None` when nothing is checked.
    pub(super) fn record(&self) -> Option<OfferRecord> {
        match self {
            OfferPolicy::Unchecked | OfferPolicy::Missing(_) => None,
            OfferPolicy::Waived(stage) => Some(OfferRecord::waived(stage.word())),
            OfferPolicy::Offer { stage, doc, .. } => Some(OfferRecord::of(stage.word(), doc)),
        }
    }
}

/// The checks that need no bundle: the offer is for this stage and this commit, and it is
/// neither stale nor from the future.
pub(super) fn check_header(
    stage: Stage,
    doc: &OfferDoc,
    commit: &CommitId,
    now: u64,
    max_age: Duration,
) -> Result<(), Refused> {
    if doc.target != stage.target() {
        return Err(refuse(
            "offer_wrong_stage",
            format!(
                "the offer is for {:?}, this host is {}",
                doc.target,
                stage.word()
            ),
        ));
    }
    if doc.commit.as_str() != commit.as_str() {
        return Err(refuse(
            "offer_wrong_commit",
            format!(
                "the offer is for {}, the deploy is of {commit}",
                doc.commit.as_str()
            ),
        ));
    }
    if doc.created > now.saturating_add(SKEW.as_secs()) {
        return Err(refuse(
            "offer_future",
            format!(
                "the offer is dated {}s ahead of this host's clock",
                doc.created - now
            ),
        ));
    }
    let age = now.saturating_sub(doc.created);
    if age > max_age.as_secs() {
        return Err(refuse(
            "offer_stale",
            format!(
                "the offer is {age}s old, the limit is {}s",
                max_age.as_secs()
            ),
        ));
    }
    Ok(())
}

/// The file name a bundle entry has in the release (and in `SHA256SUMS`).
fn asset_name(entry: &Entry, commit: &CommitId) -> String {
    let (component, attestation) = match entry.name.strip_suffix(".attestation") {
        Some(c) => (c, ".attestation"),
        None => (entry.name.as_str(), ""),
    };
    format!(
        "{component}-{commit}-{}.tar.gz{attestation}",
        entry.platform
    )
}

/// The digests `SHA256SUMS` lists, by file name (`<hex>  <name>` per line, a path in front
/// of the name ignored). A name listed twice with two digests, or a line of another shape,
/// is an error: a reader that took the first would not see the second.
fn parse_sums(sums: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(sums).map_err(|_| "SHA256SUMS is not UTF-8".to_owned())?;
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || format!("SHA256SUMS line {}: not `<sha256>  <name>`", n + 1);
        let (digest, rest) = line.split_once(' ').ok_or_else(bad)?;
        let name = rest.strip_prefix([' ', '*']).ok_or_else(bad)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad());
        }
        let name = name.rsplit('/').next().unwrap_or(name);
        if name.is_empty() {
            return Err(bad());
        }
        if out
            .insert(name.to_owned(), digest.to_owned())
            .is_some_and(|old| old != digest)
        {
            return Err(format!("SHA256SUMS lists {name} twice with other digests"));
        }
    }
    Ok(out)
}

/// The checks that need the bundle: its `SHA256SUMS` is the offer's set, each tarball is
/// the file the sums name, and the host's `kbf` names the offer's `meta_log`.
pub(super) fn check_bundle(
    doc: &OfferDoc,
    bundle: &Bundle,
    commit: &CommitId,
    host_platform: &str,
    probe: &dyn Probe,
) -> Result<(), Refused> {
    let Some(sums) = &bundle.sums else {
        return Err(refuse(
            "offer_set_unverified",
            "the bundle carries no SHA256SUMS, so the offer's release set cannot be checked",
        ));
    };
    let set = SetHash::of_sums(sums);
    if set != doc.set_hash {
        return Err(refuse(
            "offer_set_mismatch",
            format!(
                "the bundle's release set is {}, the offer is for {}",
                set.as_str(),
                doc.set_hash.as_str()
            ),
        ));
    }
    let listed = parse_sums(sums).map_err(|e| refuse("offer_set_mismatch", e))?;
    for entry in bundle
        .entries
        .iter()
        .filter(|e| !e.name.ends_with(".attestation"))
    {
        let name = asset_name(entry, commit);
        let have = hex::encode(Sha256::digest(&entry.bytes));
        match listed.get(&name) {
            Some(want) if *want == have => {}
            Some(_) => {
                return Err(refuse(
                    "offer_file_mismatch",
                    format!("{name} is not the file the release set names"),
                ));
            }
            None => {
                return Err(refuse(
                    "offer_file_mismatch",
                    format!("{name} is not in the release set"),
                ));
            }
        }
    }
    let host = bundle
        .entries
        .iter()
        .find(|e| e.platform == host_platform && e.name == "kbf")
        .ok_or_else(|| {
            refuse(
                "offer_no_host_binary",
                format!("the bundle has no kbf for {host_platform} to read the meta_log from"),
            )
        })?;
    let staged = probe
        .meta_log(commit, &host.bytes)
        .map_err(|e| refuse("offer_probe_failed", e))?;
    if staged != doc.meta_log {
        return Err(refuse(
            "offer_meta_log_mismatch",
            format!(
                "the offer says meta_log {:?}, the staged kbf says {staged:?}",
                doc.meta_log
            ),
        ));
    }
    Ok(())
}

/// Every platform the farm runs is one the offer found complete.
pub(super) fn check_platforms<'a>(
    doc: &OfferDoc,
    wanted: impl IntoIterator<Item = &'a String>,
) -> Result<(), Refused> {
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|p| !doc.platforms.contains(p))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(refuse(
        "offer_platform",
        format!(
            "the farm runs {}, which the offer did not find complete",
            missing.join(", ")
        ),
    ))
}

/// What the staged `kbf` says of itself.
pub trait Probe {
    /// The `meta_log` of `kbf --version --json` for the tarball `tarball` (the `kbf` of the
    /// host's platform), which must name `commit`'s release build.
    fn meta_log(&self, commit: &CommitId, tarball: &[u8]) -> Result<Option<u64>, String>;
}

/// The probe where none is built: it cannot answer, so an offer is refused.
pub struct NoProbe;

impl Probe for NoProbe {
    fn meta_log(&self, _: &CommitId, _: &[u8]) -> Result<Option<u64>, String> {
        Err("this build cannot read a binary's meta_log".to_owned())
    }
}

/// The most a binary may unpack to.
const MAX_BINARY: u64 = 512 * 1024 * 1024;
/// How long the unpacking and the run may each take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Unpacks the tarball into a scratch directory under the installer's work directory and
/// runs `kbf --version --json` there, as the installer's `RUN_AS` user when root.
pub struct HostProbe {
    work: PathBuf,
    run_as: Option<RunAs>,
    runner: Box<dyn Runner>,
}

impl HostProbe {
    /// The host's probe: its settings name the user; a deploy that is not root runs the
    /// binary as itself (it is a plan, and cannot switch).
    pub fn host(root: bool) -> Result<HostProbe, String> {
        let base = kbf_host::Root::new("/").map_err(|e| e.to_string())?;
        let layout = Layout::new(base.clone(), Platform::current());
        let settings = Settings::load(&kbf_host::HostExec::new(base), &layout)
            .map_err(|e| format!("installer settings: {e}"))?;
        Ok(HostProbe {
            work: layout.join(&layout.work_dir()),
            run_as: root.then_some(settings.run_as),
            runner: Box::new(SystemRunner),
        })
    }

    /// A probe that works in `work`, as `run_as`, starting the binary with `runner`.
    pub fn new(work: PathBuf, run_as: Option<RunAs>, runner: Box<dyn Runner>) -> HostProbe {
        HostProbe {
            work,
            run_as,
            runner,
        }
    }
}

/// The scratch directory, removed when the probe ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Probe for HostProbe {
    fn meta_log(&self, commit: &CommitId, tarball: &[u8]) -> Result<Option<u64>, String> {
        let io = |e: std::io::Error| format!("probe: {e}");
        fs::create_dir_all(&self.work).map_err(io)?;
        let name = format!("deploy-probe-{}", hex::encode(smoke::random(8)?));
        let dir = Scratch(self.work.join(name));
        fs::DirBuilder::new()
            .mode(0o755)
            .create(&dir.0)
            .map_err(io)?;
        if let Some(user) = self.run_as {
            std::os::unix::fs::chown(&dir.0, Some(user.uid), Some(user.gid)).map_err(io)?;
        }
        let packed = dir.0.join("kbf.tar.gz");
        fs::write(&packed, tarball).map_err(io)?;
        let binary = dir.0.join("kbf");
        crate::archive::unpack_one(&packed, "kbf", &binary, MAX_BINARY, PROBE_TIMEOUT)
            .map_err(|e| format!("probe: the kbf tarball: {e:?}"))?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).map_err(io)?;
        let ran = self
            .runner
            .run(&Spawn {
                program: binary,
                args: vec!["--version".into(), "--json".into()],
                env: Vec::new(),
                cwd: dir.0.clone(),
                run_as: self.run_as,
                timeout: PROBE_TIMEOUT,
            })
            .map_err(io)?;
        if ran.timed_out || ran.code != Some(0) {
            return Err(format!("probe: kbf --version --json ended {:?}", ran.code));
        }
        let version = Version::parse(&ran.stdout).map_err(|e| format!("probe: {e:?}"))?;
        version.check(commit).map_err(|e| format!("probe: {e:?}"))?;
        Ok(version.meta_log)
    }
}
