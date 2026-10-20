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
//! - the `kbf` of the host's own platform in the bundle is attested (offline, by the
//!   installer's pinned `gh` and trust root, the flags the stage uses), and then, run as
//!   the unprivileged user, names the offer's `meta_log` in `--version --json` (a mismatch refuses, and so does
//!   an offer that claims a number where the binary names none, or the reverse);
//! - every platform the farm runs is one the offer found complete;
//!
//! An offer that claims `reversible` is also asked of the build this host runs now: it is
//! reversible only if that build's `meta_log` is at least the offer's
//! ([`check_reversible`]). This refuses nothing. A claim the host cannot bear out is
//! deployed and recorded as `host_reversible: false`; an irreversible release is never held.
//!
//! The offer is a file (`--offer offer.json`) or the bundle's own `offer.json`
//! (`--offer -`, [`OfferPolicy::InBundle`]); the checks are the same. A caller through
//! `sudo` can give only the second: it has no file of its own to name.
//!
//! No byte is run before it is attested: the host tarball's attestation is verified first
//! ([`Attest`], by [`GhAttest`] through `GhCheck::verify`, the stage's own function), and a
//! tarball that is not attested, or has no attestation in the bundle, is refused before it
//! is unpacked. Only then does the probe unpack it and run it, as the installer's `RUN_AS`
//! user, never as root, in a scratch directory of its own. Its answer can only refuse a
//! deploy: the frontend's own update still verifies the attestation offline.
//! Without `--offer`, a deploy that names a stage is refused unless `--no-offer` says so;
//! the summary records either.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_gate::{DocError, OfferDoc, SetHash, Target, reversible};
use sha2::{Digest, Sha256};

use super::bundle::{Bundle, Entry};
use super::summary::OfferRecord;
use super::{smoke, why};
use crate::CommitId;
use crate::attest::{GhCheck, Tools};
use crate::binary::Version;
use crate::layout::{Layout, Platform, RELEASE_BINARY};
use crate::run::{RunAs, Runner, Spawn, SystemRunner};
use crate::settings::Settings;
use kbf_host::{Exec, HostExec};

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
    /// `--offer -`: the offer is the bundle's `offer.json`, read (and judged as a file's is)
    /// once the bundle is. Through `sudo` this is the only offer a caller can name.
    InBundle { stage: Stage, max_age: Duration },
}

/// A refusal: the summary's reason and detail.
pub(super) type Refused = (&'static str, String);

fn refuse(reason: &'static str, detail: impl Into<String>) -> Refused {
    (reason, detail.into())
}

/// Read `offer.json`: a regular file of at most [`MAX_OFFER`] bytes, parsed only through
/// [`OfferDoc::from_bytes`]. The file is opened once, without following a link, and judged
/// and read through that descriptor, so nothing can be swapped between the check and the
/// read. A refusal never repeats the file's contents.
pub fn read(path: &Path) -> Result<OfferDoc, String> {
    // O_NONBLOCK so that opening a FIFO cannot hang; a regular file reads the same.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => "--offer is a symbolic link".to_owned(),
            _ => format!("--offer: {e}"),
        })?;
    let meta = file.metadata().map_err(why("--offer"))?;
    if !meta.file_type().is_file() {
        return Err("--offer is not a regular file".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(MAX_OFFER + 1)
        .read_to_end(&mut bytes)
        .map_err(why("--offer"))?;
    if bytes.len() as u64 > MAX_OFFER {
        return Err(format!("--offer is larger than {MAX_OFFER} bytes"));
    }
    parsed(&bytes).map_err(|e| format!("--offer: {e}"))
}

/// `bytes` as an offer, only through [`OfferDoc::from_bytes`]. The error never repeats
/// them: a JSON error can quote the value it met, so it says only that they are not an offer.
fn parsed(bytes: &[u8]) -> Result<OfferDoc, String> {
    OfferDoc::from_bytes(bytes).map_err(|e| match e {
        DocError::Offer(refusal) => format!("offer document: {refusal}"),
        _ => "not a valid offer document".to_owned(),
    })
}

/// The offer a bundle carried (`--offer -`): `entry` is the bytes of its `offer.json`, which
/// the bundle reader has capped at [`MAX_OFFER`] and found not empty. Parsed as [`read`]
/// parses a file, and a refusal repeats nothing of the entry. No entry is `offer_missing`,
/// one that is not an offer `offer_unreadable`.
pub fn from_bundle(entry: Option<&[u8]>) -> Result<OfferDoc, Refused> {
    let Some(bytes) = entry else {
        return Err(refuse(
            "offer_missing",
            "the bundle carries no offer.json, so there is no offer to check",
        ));
    };
    parsed(bytes).map_err(|e| refuse("offer_unreadable", e))
}

impl OfferPolicy {
    /// The summary's record of the policy; `None` when nothing is checked.
    pub(super) fn record(&self) -> Option<OfferRecord> {
        match self {
            OfferPolicy::Unchecked | OfferPolicy::Missing(_) => None,
            // The offer is not known until the bundle is read; the deploy records it then.
            OfferPolicy::InBundle { .. } => None,
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

/// The file name a tarball has in the release (and in `SHA256SUMS`).
fn asset_name(entry: &Entry, commit: &CommitId) -> String {
    format!("{}-{commit}-{}.tar.gz", entry.name, entry.platform)
}

/// The digests `SHA256SUMS` lists, by file name (`<hex>  <name>` per line, a path in front
/// of the name ignored). A name listed twice with two digests, or a line of another shape,
/// is an error: a reader that took the first would not see the second.
pub(super) fn parse_sums(sums: &[u8]) -> Result<BTreeMap<String, String>, String> {
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
    let attestation = bundle
        .entries
        .iter()
        .find(|e| e.platform == host_platform && e.name == "kbf.attestation")
        .map(|e| e.bytes.as_slice());
    let staged = probe
        .meta_log(commit, &host.bytes, attestation)
        .map_err(|e| match e {
            ProbeFault::Unattested(d) => refuse("offer_set_unverified", d),
            ProbeFault::Failed(d) => refuse("offer_probe_failed", d),
        })?;
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

/// The host's own word on an offer that claims `reversible`: does the build this host runs
/// now bear the claim out? `Some(true)` if so, `Some(false)` if not, `None` when the offer
/// claims no reversibility (the probe is not called).
///
/// The offer's `previous` and `previous_meta_log` are its author's word about a farm the
/// author could not see, so the host does not take them. Going back from the offered build
/// to the one this host runs is reversible only if `meta_log(offered) <= meta_log(running)`,
/// and the host reads the second number from its own `current` build. Unknown is never
/// reversible: a running build that names no `meta_log`, or cannot be asked, gives
/// `Some(false)`.
///
/// This never refuses. An irreversible release is deployed all the same, and the answer is
/// recorded as `host_reversible` for the bake's hand-off and the release notes, which must
/// treat the release as one that cannot be rolled back. Holding a release for it would stop
/// every following release too, since each makes the same claim against the same host.
///
/// This is the frontend host's build only. A node at another commit than this host's
/// `current` one is not covered.
pub fn check_reversible(doc: &OfferDoc, probe: &dyn Probe) -> Option<bool> {
    if !doc.reversible {
        return None;
    }
    let answer = probe.running_meta_log();
    Some(answer.is_ok_and(|held| reversible(doc.meta_log, held)))
}

/// Why a probe gave no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeFault {
    /// The tarball is not attested (or has no attestation): nothing was run.
    Unattested(String),
    /// The attested binary could not be unpacked or run, or named another build.
    Failed(String),
}

/// What the staged `kbf` says of itself.
pub trait Probe {
    /// The `meta_log` of `kbf --version --json` for the tarball `tarball` (the `kbf` of the
    /// host's platform), which must name `commit`'s release build. `attestation` is the
    /// bundle's attestation of that tarball; the tarball is not run unless it verifies.
    fn meta_log(
        &self,
        commit: &CommitId,
        tarball: &[u8],
        attestation: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault>;

    /// The `meta_log` of `kbf --version --json` of the build this host runs now (its
    /// `current` release), which an offer that claims `reversible` is checked against
    /// ([`check_reversible`]). `Ok(None)` is a build that names none. A probe that cannot
    /// say gives the default, which reads as not reversible.
    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        Err(ProbeFault::Failed(
            "this build cannot read the running meta_log".to_owned(),
        ))
    }
}

/// The probe where none is built: it cannot answer, so an offer is refused.
pub struct NoProbe;

impl Probe for NoProbe {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        Err(ProbeFault::Failed(
            "this build cannot read a binary's meta_log".to_owned(),
        ))
    }
}

/// The most a binary may unpack to.
const MAX_BINARY: u64 = 512 * 1024 * 1024;
/// How long the unpacking and the run may each take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Unpacks the tarball into a scratch directory under the installer's work directory and
/// runs `kbf --version --json` there, as the installer's `RUN_AS` user when root. It also
/// runs the `kbf` of the host's `current` release the same way, for its `meta_log`.
pub struct HostProbe {
    work: PathBuf,
    run_as: Option<RunAs>,
    runner: Box<dyn Runner>,
    attest: Box<dyn Attest>,
    /// The `kbf` the host runs now (`<current>/kbf`); none where the probe names none.
    running: Option<PathBuf>,
}

/// Whether a tarball is the attested build of a commit.
pub trait Attest {
    fn verify(&self, commit: &CommitId, tarball: &[u8], attestation: &[u8]) -> Result<(), String>;
}

/// The installer's pinned `gh` and trust root, through [`GhCheck::verify`] (the function
/// the stage uses), run as the `RUN_AS` user on copies in a scratch directory.
pub struct GhAttest {
    work: PathBuf,
    gh: GhCheck,
    trust: Box<dyn Exec>,
    runner: Box<dyn Runner>,
    switch_user: bool,
}

impl GhAttest {
    /// The check of `gh`, working in `work`; `switch_user` hands the scratch files to
    /// `gh.run_as` (only root can).
    pub fn new(
        work: PathBuf,
        gh: GhCheck,
        trust: Box<dyn Exec>,
        runner: Box<dyn Runner>,
        switch_user: bool,
    ) -> GhAttest {
        GhAttest {
            work,
            gh,
            trust,
            runner,
            switch_user,
        }
    }
}

impl GhAttest {
    /// The scratch directory with `gh`'s home and the two files, for `owner` to read.
    fn prepare(
        &self,
        owner: Option<RunAs>,
        tarball: &[u8],
        attestation: &[u8],
    ) -> std::io::Result<(Scratch, [PathBuf; 3])> {
        let dir = scratch_in(&self.work, "deploy-attest", owner)?;
        let home = dir.0.join("gh");
        fs::DirBuilder::new().mode(0o700).create(&home)?;
        if let Some(user) = owner {
            std::os::unix::fs::chown(&home, Some(user.uid), Some(user.gid))?;
        }
        let packed = dir.0.join("kbf.tar.gz");
        let bundle = dir.0.join("kbf.bundle.jsonl");
        for (path, bytes) in [(&packed, tarball), (&bundle, attestation)] {
            fs::write(path, bytes)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
        }
        Ok((dir, [home, packed, bundle]))
    }
}

impl Attest for GhAttest {
    fn verify(&self, commit: &CommitId, tarball: &[u8], attestation: &[u8]) -> Result<(), String> {
        let owner = self.switch_user.then_some(self.gh.run_as);
        let (_dir, [home, packed, bundle]) = self
            .prepare(owner, tarball, attestation)
            .map_err(|e| format!("attestation: {e}"))?;
        let tools = Tools {
            trust: self.trust.as_ref(),
            runner: self.runner.as_ref(),
        };
        self.gh
            .verify(&tools, &home, &packed, &bundle, commit)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }
}

/// A fresh scratch directory under `work`, handed to `owner` when there is one.
fn scratch_in(work: &Path, prefix: &str, owner: Option<RunAs>) -> std::io::Result<Scratch> {
    fs::create_dir_all(work)?;
    let name = format!(
        "{prefix}-{}",
        hex::encode(smoke::random(8).map_err(std::io::Error::other)?)
    );
    let dir = Scratch(work.join(name));
    fs::DirBuilder::new().mode(0o755).create(&dir.0)?;
    if let Some(user) = owner {
        std::os::unix::fs::chown(&dir.0, Some(user.uid), Some(user.gid))?;
    }
    Ok(dir)
}

impl HostProbe {
    /// The probe of the host whose files are under `root` (`/` for the real one): its
    /// settings name the user; a deploy that is not root runs the binary as itself (it is a
    /// plan, and cannot switch).
    pub fn at(root: &Path, as_root: bool) -> Result<HostProbe, String> {
        let base = kbf_host::Root::new(root).map_err(|e| e.to_string())?;
        let layout = Layout::new(base.clone(), Platform::current());
        let settings = Settings::load(&HostExec::new(base.clone()), &layout)
            .map_err(|e| format!("installer settings: {e}"))?;
        let work = layout.join(&layout.work_dir());
        let attest = GhAttest::new(
            work.clone(),
            GhCheck::pinned(&layout, &settings),
            Box::new(HostExec::new(base.clone())),
            Box::new(SystemRunner),
            as_root,
        );
        let running = layout.join(&layout.current()).join(RELEASE_BINARY);
        Ok(HostProbe {
            work,
            run_as: as_root.then_some(settings.run_as),
            runner: Box::new(SystemRunner),
            attest: Box::new(attest),
            running: Some(running),
        })
    }

    /// A probe that works in `work`, as `run_as`, starting the binary with `runner` once
    /// `attest` has vouched for the tarball. It names no running `kbf`, which
    /// [`HostProbe::with_running`] does.
    pub fn new(
        work: PathBuf,
        run_as: Option<RunAs>,
        runner: Box<dyn Runner>,
        attest: Box<dyn Attest>,
    ) -> HostProbe {
        HostProbe {
            work,
            run_as,
            runner,
            attest,
            running: None,
        }
    }

    /// The same probe, with `binary` (an absolute path) as the `kbf` the host runs now.
    pub fn with_running(mut self, binary: PathBuf) -> HostProbe {
        self.running = Some(binary);
        self
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
    fn meta_log(
        &self,
        commit: &CommitId,
        tarball: &[u8],
        attestation: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        // Nothing of the tarball is unpacked or run until it is attested.
        let attestation = attestation.ok_or_else(|| {
            ProbeFault::Unattested("the bundle carries no attestation for the host's kbf".into())
        })?;
        self.attest
            .verify(commit, tarball, attestation)
            .map_err(|e| ProbeFault::Unattested(format!("the host's kbf is not attested: {e}")))?;
        self.run(commit, tarball).map_err(ProbeFault::Failed)
    }

    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        let fail = |why: String| ProbeFault::Failed(format!("the running kbf failed: {why}"));
        let Some(binary) = self.running.clone() else {
            return Err(fail("this probe names none".to_owned()));
        };
        // The unprivileged user, from `/`: the release is root's and world-readable, and the
        // answer is the build's own, as the staged binary's is.
        let version = self.version(binary, PathBuf::from("/")).map_err(fail)?;
        Ok(version.meta_log)
    }
}

impl HostProbe {
    /// Unpack the (attested) tarball and run it.
    fn run(&self, commit: &CommitId, tarball: &[u8]) -> Result<Option<u64>, String> {
        let io = |e: std::io::Error| format!("probe: {e}");
        let dir = scratch_in(&self.work, "deploy-probe", self.run_as).map_err(io)?;
        let packed = dir.0.join("kbf.tar.gz");
        fs::write(&packed, tarball).map_err(io)?;
        let binary = dir.0.join("kbf");
        crate::archive::unpack_one(&packed, "kbf", &binary, MAX_BINARY, PROBE_TIMEOUT)
            .map_err(|e| format!("probe: the kbf tarball: {e:?}"))?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).map_err(io)?;
        let version = self
            .version(binary, dir.0.clone())
            .map_err(|e| format!("probe: {e}"))?;
        version.check(commit).map_err(|e| format!("probe: {e:?}"))?;
        Ok(version.meta_log)
    }

    /// `program --version --json` in `cwd`, as the probe's user, with an empty environment
    /// and a time limit, read strictly.
    fn version(&self, program: PathBuf, cwd: PathBuf) -> Result<Version, String> {
        let ran = self
            .runner
            .run(&Spawn {
                program,
                args: vec!["--version".into(), "--json".into()],
                env: Vec::new(),
                cwd,
                run_as: self.run_as,
                timeout: PROBE_TIMEOUT,
            })
            .map_err(|e| e.to_string())?;
        if ran.timed_out || ran.code != Some(0) {
            return Err(format!("kbf --version --json ended {:?}", ran.code));
        }
        Version::parse(&ran.stdout).map_err(|e| format!("{e:?}"))
    }
}
