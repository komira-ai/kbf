//! The verify pipeline: the fetched bytes of a release become `releases/<commit>/` only
//! after every check of docs/design/kbf-installer.md section 5 has passed, in its order.
//! Nothing here fetches (the fetch child hands over [`Fetched`] files) and nothing here
//! activates (the cycle moves links after the stage returns).
//!
//! The order is the safety property; each step happens only if every one before it passed,
//! and nothing is unpacked or run before step 4:
//!
//! 0. The trust list (`gh`, the trusted root, and every directory above them) is root's
//!    alone ([`crate::trust`]).
//! 1. **Count**: each component's file is exactly the size the order names, within the
//!    host's cap. The order and the fetch child only claim a size; the descriptor opened
//!    here is asked (`fstat`: a regular file, root's, one link).
//! 2. **Digest**: the sha256 of the file read here equals the order's, and the fetch
//!    child's own count of the stream equals both (the order's digest is only a locator).
//! 3. **Attestation**, offline, by the pinned `gh` ([`crate::attest`]): the repository,
//!    the signer workflow, the OIDC issuer, one of the allowed source refs and the order's
//!    commit as `--source-digest`. `gh` lists the file's sha256 among the attestation's
//!    subjects itself. It runs unprivileged with an empty environment, a scratch home and
//!    no network.
//! 4. **Shape** ([`crate::archive`]): gzip and tar holding exactly one regular file named
//!    for the component (no link, device, directory or path), a decompressed size under
//!    the cap, nothing after the tar's end and nothing after the gzip member; unpacked by
//!    root; its header is the executable this host runs; on macOS no quarantine attribute
//!    and a valid code signature ([`crate::darwin`]).
//! 5. **The binary names itself**: `<name> --version --json`, run as the unprivileged
//!    user in an empty directory with an empty environment, must print at most 4 KiB of
//!    exactly the fields a build reports, name the order's commit and carry the facts the
//!    order expected (platform, protocol, min_protocol, state_schema, profile). They must
//!    meet the operator's floor and be understood by this installer. An activation that
//!    carries no expected facts is refused; a `frontend_update` may leave them out, and
//!    then only the commit and the release profile are checked.
//! 6. **Leases**: root observes ([`crate::helper::leases`]) that none is live.
//! 7. **Install**: `VERIFIED` is written into the directory, which is renamed to
//!    `releases/<commit>/`, the parent is synced, and then the state's `staged` names it.
//!    A directory already there is the same release (the same `VERIFIED` marker and the
//!    same bytes) or a refusal; a release is never replaced.
//!
//! Every external step is bounded: `gh` 120 s, unpacking 120 s, the probe 10 s
//! ([`Timeouts`]); a program is killed with its whole process group. The scratch
//! directories (`work/<commit>.partial/` and `work/<commit>.check/`) are root's and go
//! whatever the outcome.
//!
//! A refusal carries the contract's stable word ([`Refusal::word`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::exec::Exec;
use kbf_host::step::Owner;
use kbf_update::{Facts, Locator};
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::archive::unpack_one;
use crate::attest::{GhCheck, SourceRef, Tools};
use crate::binary::{Arch, BinaryKind, Version};
use crate::darwin;
use crate::helper::fetcher::InstalledRelease;
use crate::helper::leases::LeaseSource;
use crate::layout::{Layout, Platform};
use crate::refusal::{ArchiveFault, FactsFault, Refusal, TrustFault, VersionFault, WHAT_PROBE};
use crate::run::{Runner, Spawn};
use crate::settings::Settings;
use crate::state::{Locked, StateError};

/// The marker written into a release directory once every check has passed.
pub const VERIFIED: &str = "VERIFIED";

/// The most of an attestation bundle that is taken (the order record's own cap).
pub const BUNDLE_CAP: u64 = 256 * 1024;

/// The most an unpacked binary may be.
pub const MAX_BINARY: u64 = 1 << 30;

/// What kind of order the release is verified for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    /// A node's `activate`: the expected facts are required.
    Activate,
    /// The frontend's own update: the facts may be left out (`expected` is `None`). Its
    /// order record is a locator and carries none, so only the commit and the release
    /// profile are checked.
    FrontendUpdate,
}

/// The claims of the order record, which are the stage's input and never its trust.
#[derive(Clone, Copy, Debug)]
pub struct Order<'a> {
    pub kind: OrderKind,
    pub commit: &'a CommitId,
    /// The components the platform needs: name, sha256, size, attestation bundle.
    pub components: &'a [Locator],
    pub expected: Option<&'a Facts>,
}

/// One component the fetch child brought in: the file the root parent wrote in
/// [`Partial::dl`] while it hashed the stream.
#[derive(Clone, Debug)]
pub struct Received {
    pub name: String,
    pub tarball: PathBuf,
    /// How many bytes the parent read from the child.
    pub bytes: u64,
    /// The sha256 of those bytes, as the parent computed it while reading.
    pub sha256: String,
}

/// What the fetch step hands over: its scratch directory and the files in it.
#[derive(Debug)]
pub struct Fetched {
    pub partial: Partial,
    pub files: Vec<Received>,
}

/// `work/<commit>.partial/`: root's, 0755, with `dl/` (0700) for the downloaded tarballs.
/// The release is built in it and the directory itself is renamed into place, so what it
/// holds when that happens is the release and nothing else. Removed when dropped (a
/// directory that was renamed away is no longer there to remove).
#[derive(Debug)]
pub struct Partial {
    dir: PathBuf,
}

impl Partial {
    /// Make the directory and `dl/` in it, owned by `owner`. A directory already there
    /// is a crashed stage's, which `run` sweeps at its start: it is refused, not reused.
    pub fn create(host: &Layout, commit: &CommitId, owner: Owner) -> Result<Partial, Refusal> {
        let dir = work_path(host, commit, "partial")?;
        // Only a directory made here is removed on drop: a refused stale one is not ours.
        make_dir(&dir, 0o755, owner)?;
        let made = Partial { dir };
        make_dir(&made.dl(), 0o700, owner)?;
        Ok(made)
    }

    /// The download directory.
    pub fn dl(&self) -> PathBuf {
        self.dir.join("dl")
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// `work/<commit>.<suffix>`, with `work/` made when it is missing.
fn work_path(host: &Layout, commit: &CommitId, suffix: &str) -> Result<PathBuf, Refusal> {
    let work = host.join(&host.work_dir());
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o755);
    builder.create(&work).map_err(work_err(&work))?;
    Ok(work.join(format!("{commit}.{suffix}")))
}

fn make_dir(dir: &Path, mode: u32, owner: Owner) -> Result<(), Refusal> {
    fs::create_dir(dir).map_err(work_err(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).map_err(work_err(dir))?;
    std::os::unix::fs::chown(dir, Some(owner.uid), Some(owner.gid)).map_err(work_err(dir))
}

/// The refusal for an `io::Error` from using `path` in a scratch directory.
fn work_err(path: &Path) -> impl FnOnce(io::Error) -> Refusal {
    let path = path.to_owned();
    move |source| Refusal::Work { path, source }
}

/// How long each external step may take before it is killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub gh: Duration,
    pub extract: Duration,
    pub probe: Duration,
}

impl Timeouts {
    /// The contract's: `gh` 120 s, unpacking 120 s, the probe 10 s.
    pub const DEFAULT: Timeouts = Timeouts {
        gh: Duration::from_secs(120),
        extract: Duration::from_secs(120),
        probe: Duration::from_secs(10),
    };
}

/// The newest of each version number this installer understands: a release that declares
/// more is refused (`installer_too_old`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supported {
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
}

/// What the host decides about how a release is verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The most a component's tarball may be (`max_component_bytes`).
    pub max_component_bytes: u64,
    /// The most an unpacked binary, and what the tarball decompresses to, may be.
    pub max_binary_bytes: u64,
    /// The operator's floor: a release whose `protocol` is lower is refused. 0 for none.
    pub min_protocol: u64,
    pub supported: Supported,
    pub timeouts: Timeouts,
}

/// A release that passed every check and is installed in `releases/<commit>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub commit: CommitId,
    /// The binaries installed, in the order's order.
    pub components: Vec<String>,
    /// The allowed ref the attestation named.
    pub source_ref: SourceRef,
    /// What the (first) binary declared.
    pub version: Version,
    /// `linux-x86_64`, `linux-arm64` or `darwin-arm64`, from the binary's header.
    pub platform: String,
    /// The directory was already there with the same release in it.
    pub already_installed: bool,
}

impl Staged {
    /// What the cycle compares with the order's expected facts before it moves a link.
    pub fn installed(&self) -> InstalledRelease {
        let small = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        InstalledRelease {
            commit: self.commit.clone(),
            platform: self.platform.clone(),
            protocol: small(self.version.protocol),
            min_protocol: small(self.version.min_protocol),
            state_schema: small(self.version.state_schema),
            profile: self.version.profile.clone(),
        }
    }
}

/// Verifies what was fetched and installs it, or says why not. Nothing but scratch has
/// changed when it refuses.
pub trait Verifier {
    fn verify(&self, order: &Order<'_>, fetched: Fetched) -> Result<Staged, Refusal>;
}

/// Everything the real pipeline needs from the host.
pub struct Pipeline<'a> {
    pub host: &'a Layout,
    pub settings: &'a Settings,
    pub policy: Policy,
    /// Says who owns the trust list and reads it; the host's own on a host.
    pub trust: &'a dyn Exec,
    pub runner: &'a dyn Runner,
    pub leases: &'a dyn LeaseSource,
    /// Who owns the release: root on a host (a scratch test can chown only to itself).
    pub owner: Owner,
}

impl Verifier for Pipeline<'_> {
    fn verify(&self, order: &Order<'_>, fetched: Fetched) -> Result<Staged, Refusal> {
        // Fail closed before any work: an activation has to say what it expects.
        if order.kind == OrderKind::Activate && order.expected.is_none() {
            return Err(Refusal::Facts {
                component: String::new(),
                fault: FactsFault::Missing,
            });
        }
        let gh = self.gh();
        gh.check_trust(self.trust)?;
        let files = pair(order, &fetched.files)?;

        for (locator, received) in &files {
            self.count_and_digest(locator, received)?;
        }
        let scratch = Scratch::make(self.host, order.commit, self.settings, self.owner)?;
        let mut source_ref = None;
        for (locator, received) in &files {
            let verified = self.attest(&gh, &scratch, locator, received, order.commit)?;
            source_ref.get_or_insert(verified);
        }

        let want = BinaryKind::wanted(self.host.platform(), self.settings.arch);
        let mut version = None;
        for (locator, received) in &files {
            let said = self.unpack_and_probe(
                &fetched.partial,
                &scratch,
                &received.tarball,
                &locator.name,
                want,
                order,
            )?;
            version.get_or_insert(said);
        }
        let (version, platform) = version.expect("an order names a component");

        match self.leases.live() {
            Ok(0) => {}
            Ok(n) => return Err(Refusal::Lease(format!("{n} leases are live"))),
            Err(why) => {
                return Err(Refusal::Lease(format!(
                    "cannot tell whether any is live: {why}"
                )));
            }
        }

        let names: Vec<String> = order.components.iter().map(|c| c.name.clone()).collect();
        let already_installed = self.install(fetched.partial, order.commit, &names)?;
        Ok(Staged {
            commit: order.commit.clone(),
            components: names,
            source_ref: source_ref.expect("an order names a component"),
            version,
            platform,
            already_installed,
        })
    }
}

/// Each ordered component with the file received for it. The sets must be the same, with
/// no name twice: the order says what the platform needs, and nothing else is taken.
fn pair<'a>(
    order: &'a Order<'_>,
    received: &'a [Received],
) -> Result<Vec<(&'a Locator, &'a Received)>, Refusal> {
    let mut pairs = Vec::new();
    for locator in order.components {
        let mut found = received.iter().filter(|r| r.name == locator.name);
        match (found.next(), found.next()) {
            (Some(r), None) => pairs.push((locator, r)),
            (None, _) => {
                return Err(Refusal::Components(format!(
                    "{} was not fetched",
                    locator.name
                )));
            }
            (Some(_), Some(_)) => {
                return Err(Refusal::Components(format!(
                    "{} was fetched twice",
                    locator.name
                )));
            }
        }
    }
    if received.len() != pairs.len() {
        return Err(Refusal::Components(
            "a component was fetched that the order does not name".to_owned(),
        ));
    }
    Ok(pairs)
}

/// The scratch directory of the checks: `work/<commit>.check/`, root's, with `gh/` (the
/// unprivileged user's home for `gh`, 0700) and `empty/` (the directory the probe runs
/// in), and the links and bundles the unprivileged user reads. Removed when dropped.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn make(
        host: &Layout,
        commit: &CommitId,
        settings: &Settings,
        owner: Owner,
    ) -> Result<Scratch, Refusal> {
        let dir = work_path(host, commit, "check")?;
        // A check directory left by a crash is ours alone to remove.
        let _ = fs::remove_dir_all(&dir);
        let made = Scratch { dir };
        make_dir(&made.dir, 0o755, owner)?;
        make_dir(&made.empty(), 0o755, owner)?;
        let gh = made.gh();
        make_dir(&gh, 0o700, owner)?;
        std::os::unix::fs::chown(&gh, Some(settings.run_as.uid), Some(settings.run_as.gid))
            .map_err(work_err(&gh))?;
        Ok(made)
    }

    fn gh(&self) -> PathBuf {
        self.dir.join("gh")
    }

    fn empty(&self) -> PathBuf {
        self.dir.join("empty")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Pipeline<'_> {
    fn gh(&self) -> GhCheck {
        let mut gh = GhCheck::pinned(self.host, self.settings);
        gh.timeout = self.policy.timeouts.gh;
        gh
    }

    /// Steps 1 and 2 for one component.
    fn count_and_digest(&self, locator: &Locator, received: &Received) -> Result<(), Refusal> {
        let max = self.policy.max_component_bytes;
        let size = |got: u64| Refusal::Size {
            name: locator.name.clone(),
            want: locator.size,
            got,
            max,
        };
        if locator.size == 0 || locator.size > max {
            return Err(size(locator.size));
        }
        let file = open_owned(&received.tarball, self.owner)?;
        let len = file.metadata().map_err(work_err(&received.tarball))?.len();
        if len != locator.size {
            return Err(size(len));
        }
        if received.bytes != locator.size {
            return Err(size(received.bytes));
        }
        let mismatch = || Refusal::HashMismatch {
            name: locator.name.clone(),
        };
        if received.sha256 != locator.sha256 {
            return Err(mismatch());
        }
        let got = sha256_of(file).map_err(work_err(&received.tarball))?;
        if got != locator.sha256 {
            return Err(mismatch());
        }
        Ok(())
    }

    /// Step 3 for one component: a readable link to the tarball and the bundle in the
    /// scratch directory (the unprivileged `gh` cannot enter `dl/`), then `gh`.
    fn attest(
        &self,
        gh: &GhCheck,
        scratch: &Scratch,
        locator: &Locator,
        received: &Received,
        commit: &CommitId,
    ) -> Result<SourceRef, Refusal> {
        let bundle_bytes = locator.attestation.as_bytes();
        if bundle_bytes.len() as u64 > BUNDLE_CAP {
            return Err(Refusal::Size {
                name: format!("{} attestation", locator.name),
                want: BUNDLE_CAP,
                got: bundle_bytes.len() as u64,
                max: BUNDLE_CAP,
            });
        }
        let bundle = scratch.dir.join(format!("{}.bundle.jsonl", locator.name));
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&bundle)
            .map_err(work_err(&bundle))?;
        out.write_all(bundle_bytes).map_err(work_err(&bundle))?;
        drop(out);

        let link = scratch.dir.join(format!("{}.tar.gz", locator.name));
        let file = open_owned(&received.tarball, self.owner)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(work_err(&received.tarball))?;
        fs::hard_link(&received.tarball, &link).map_err(work_err(&link))?;

        let tools = Tools {
            trust: self.trust,
            runner: self.runner,
        };
        gh.verify(&tools, &scratch.gh(), &link, &bundle, commit)
            .map(|verified| verified.source_ref)
    }

    /// Steps 4 and 5 for one component: unpack it into the partial directory, check it is
    /// the executable this host runs, and have it name itself.
    fn unpack_and_probe(
        &self,
        partial: &Partial,
        scratch: &Scratch,
        tarball: &Path,
        name: &str,
        want: BinaryKind,
        order: &Order<'_>,
    ) -> Result<(Version, String), Refusal> {
        let binary = partial.path().join(name);
        unpack_one(
            tarball,
            name,
            &binary,
            self.policy.max_binary_bytes,
            self.policy.timeouts.extract,
        )
        .map_err(|fault: ArchiveFault| Refusal::Archive {
            component: name.to_owned(),
            fault,
        })?;
        // Whatever the umask said, the unprivileged user must be able to run it.
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .map_err(work_err(&binary))?;
        let mut header = Vec::new();
        File::open(&binary)
            .and_then(|f| f.take(32).read_to_end(&mut header))
            .map_err(work_err(&binary))?;
        let found = BinaryKind::of(&header);
        if found != want {
            return Err(Refusal::WrongPlatform {
                component: name.to_owned(),
                found,
                want,
            });
        }
        if self.host.platform() == Platform::MacOs {
            let timeout = self.policy.timeouts.gh;
            if darwin::quarantined(self.runner, &binary, timeout)? {
                return Err(Refusal::Quarantined {
                    name: name.to_owned(),
                });
            }
            darwin::check_signature(self.runner, name, &binary, timeout)?;
        }
        let said = self.probe(&binary, name, &scratch.empty())?;
        said.check(order.commit).map_err(|fault| Refusal::Version {
            component: name.to_owned(),
            fault,
        })?;
        let platform = platform_of(self.host.platform(), self.settings.arch);
        self.facts(name, &said, &platform, order)?;
        Ok((said, platform))
    }

    /// `<binary> --version --json`, as the unprivileged user, strictly read.
    fn probe(&self, binary: &Path, name: &str, cwd: &Path) -> Result<Version, Refusal> {
        let spawn = Spawn {
            program: binary.to_owned(),
            args: vec!["--version".into(), "--json".into()],
            env: Vec::new(),
            cwd: cwd.to_owned(),
            run_as: Some(self.settings.run_as),
            timeout: self.policy.timeouts.probe,
        };
        let ran = self.runner.run(&spawn).map_err(|source| Refusal::Tool {
            what: WHAT_PROBE,
            source,
        })?;
        if ran.timed_out {
            return Err(Refusal::TimedOut { what: WHAT_PROBE });
        }
        let version = |fault| Refusal::Version {
            component: name.to_owned(),
            fault,
        };
        if ran.code != Some(0) {
            return Err(version(VersionFault::Failed(ran.code)));
        }
        Version::parse(&ran.stdout).map_err(version)
    }

    /// The facts the binary declared against the order's, the operator's floor and what
    /// this installer understands, in that order.
    fn facts(
        &self,
        name: &str,
        said: &Version,
        platform: &str,
        order: &Order<'_>,
    ) -> Result<(), Refusal> {
        let differs = |fault| Refusal::Facts {
            component: name.to_owned(),
            fault,
        };
        // A build that reports its platform must agree with its own header.
        if let Some(declared) = &said.platform
            && declared != platform
        {
            return Err(differs(FactsFault::Differs {
                field: "platform (declared, against the binary's header)",
                want: platform.to_owned(),
                got: declared.clone(),
            }));
        }
        if let Some(e) = order.expected {
            let checks: [(&'static str, String, String); 5] = [
                ("platform", e.platform.clone(), platform.to_owned()),
                (
                    "protocol",
                    e.protocol.to_string(),
                    said.protocol.to_string(),
                ),
                (
                    "min_protocol",
                    e.min_protocol.to_string(),
                    said.min_protocol.to_string(),
                ),
                (
                    "state_schema",
                    e.state_schema.to_string(),
                    said.state_schema.to_string(),
                ),
                ("profile", e.profile.clone(), said.profile.clone()),
            ];
            for (field, want, got) in checks {
                if want != got {
                    return Err(differs(FactsFault::Differs { field, want, got }));
                }
            }
        }
        if said.protocol < self.policy.min_protocol {
            return Err(Refusal::Floor {
                component: name.to_owned(),
                what: format!(
                    "protocol {} is below the floor {}",
                    said.protocol, self.policy.min_protocol
                ),
            });
        }
        let s = self.policy.supported;
        let newer = [
            ("protocol", said.protocol, s.protocol),
            ("min_protocol", said.min_protocol, s.min_protocol),
            ("state_schema", said.state_schema, s.state_schema),
        ];
        for (field, got, most) in newer {
            if got > most {
                return Err(Refusal::InstallerTooOld {
                    component: name.to_owned(),
                    what: format!("{field} {got}; this installer understands up to {most}"),
                });
            }
        }
        Ok(())
    }

    /// Step 7. Returns whether the same release was already in place.
    fn install(
        &self,
        partial: Partial,
        commit: &CommitId,
        names: &[String],
    ) -> Result<bool, Refusal> {
        let dir = partial.path().to_owned();
        let owner = self.owner;
        let marker = dir.join(VERIFIED);
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&marker)
            .map_err(work_err(&marker))?;
        out.write_all(format!("{commit}\n").as_bytes())
            .and_then(|()| out.sync_all())
            .map_err(work_err(&marker))?;
        drop(out);
        // What the fetch left in `dl/` is not part of the release.
        let dl = partial.dl();
        fs::remove_dir_all(&dl).map_err(work_err(&dl))?;
        let mut entries: Vec<&str> = names.iter().map(String::as_str).collect();
        entries.push(VERIFIED);
        for entry in entries {
            let path = dir.join(entry);
            std::os::unix::fs::chown(&path, Some(owner.uid), Some(owner.gid))
                .map_err(work_err(&path))?;
        }
        sync_dir(&dir)?;

        let target = self.host.join(&self.host.release_dir(commit));
        let releases = target
            .parent()
            .expect("a release directory has a parent")
            .to_owned();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755);
        builder.create(&releases).map_err(work_err(&releases))?;

        let already = match fs::symlink_metadata(&target) {
            Ok(_) => {
                self.same_release(&target, &dir, names)?;
                true
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::rename(&dir, &target).map_err(work_err(&target))?;
                sync_dir(&releases)?;
                false
            }
            Err(e) => return Err(work_err(&target)(e)),
        };

        // `gc` keeps a staged release; the lock is the one every state change is under.
        let failed = |e: StateError| Refusal::Install(e.into());
        let lock = Locked::acquire(self.host).map_err(failed)?;
        let mut state = lock.read().map_err(failed)?.unwrap_or_default();
        state.staged = Some(commit.clone());
        lock.write(&state).map_err(failed)?;
        Ok(already)
    }

    /// The release directory already on the host is this release: a plain directory with
    /// the marker and the same bytes for each component. Anything else is refused, since
    /// a release is never replaced.
    fn same_release(&self, target: &Path, new: &Path, names: &[String]) -> Result<(), Refusal> {
        let differs = |path: &Path| Refusal::Differs {
            path: path.display().to_string(),
        };
        let is = |path: &Path, plain: fn(&fs::Metadata) -> bool| {
            fs::symlink_metadata(path).is_ok_and(|m| plain(&m))
        };
        if !is(target, fs::Metadata::is_dir) {
            return Err(differs(target));
        }
        if !is(&target.join(VERIFIED), fs::Metadata::is_file) {
            return Err(differs(&target.join(VERIFIED)));
        }
        for name in names {
            let old = target.join(name);
            let same = is(&old, fs::Metadata::is_file)
                && file_sha256(&old).ok() == file_sha256(&new.join(name)).ok();
            if !same {
                return Err(differs(&old));
            }
        }
        Ok(())
    }
}

/// The platform an order spells for a host of `host` on `arch`. The header was already
/// checked to be the executable this host runs ([`BinaryKind::wanted`]), so the header
/// and the host agree on it.
pub(crate) fn platform_of(host: Platform, arch: Arch) -> String {
    let os = match host {
        Platform::Linux => "linux",
        Platform::MacOs => "darwin",
    };
    format!("{os}-{arch}")
}

/// Open `path` without following a link and accept it only as a regular file owned by
/// `owner` with one link: what the fetch parent made, and nothing swapped in its place.
fn open_owned(path: &Path, owner: Owner) -> Result<File, Refusal> {
    let untrusted = |fault| Refusal::Untrusted {
        path: path.to_owned(),
        fault,
    };
    let unreadable = |e| untrusted(TrustFault::Unreadable(e));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(untrusted(TrustFault::Kind {
            found: "non-regular file",
            want: "file",
        }));
    }
    if meta.uid() != owner.uid {
        return Err(untrusted(TrustFault::NotRoot { uid: meta.uid() }));
    }
    if meta.nlink() != 1 {
        return Err(untrusted(TrustFault::Link));
    }
    Ok(file)
}

fn sha256_of(mut file: File) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(&buf[..n]);
    }
}

fn file_sha256(path: &Path) -> io::Result<String> {
    sha256_of(File::open(path)?)
}

/// Sync a directory, so a rename or a new entry in it survives a power cut.
fn sync_dir(dir: &Path) -> Result<(), Refusal> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(work_err(dir))
}

#[cfg(test)]
#[path = "stage_tests.rs"]
mod tests;
