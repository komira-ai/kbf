//! A Linux daemon host that runs the `kbf` package: the update is the package manager's.
//!
//! The units of such a host are the packaged ones and run `/usr/bin/kbf`, so a release is not
//! laid out beside the running one and no link moves ([`super::linux`] is for the hand-placed
//! layout). The A/B is two installations of a package file, with the trial recorded first:
//!
//! - **stage** fetches the release's `.deb` or `.rpm` for the version as the unprivileged user
//!   (the release listing, `gh release download`: [`crate::mac::fetch`]) and copies it once,
//!   as root, with its attestation bundle and the release's `SHA256SUMS`, into a directory of
//!   its own. Nothing is believed before every check passes, offline:
//!   1. the file's sha256 is the one `SHA256SUMS` lists for it;
//!   2. `gh attestation verify` accepts the file and its bundle: this repository's
//!      `artifacts.yml` on a GitHub-hosted runner, for the full commit, on the channel's branch
//!      (the same pinned `gh` and trusted root as the hand-placed layout's);
//!   3. the package manager reads the file's own name, version and architecture
//!      (`dpkg-deb --field`, `rpm --query --package`), and they are `kbf`, the version asked for
//!      and this host's CPU.
//!
//!    The file is then kept, root's alone, with its digest;
//! - **switch** installs it on trial: the digest of the kept file is checked again, the
//!   package that runs is read from the package database and a verified copy of it is kept
//!   (fetched and checked the same way when this host holds none), then the trial is written
//!   with its deadline, and only then does the package manager install the new file
//!   (`dpkg --install`, `rpm --upgrade --oldpackage`: the one file, no network, no dependency
//!   resolution) and the configured units restart on it. A unit that does not come up puts
//!   the previous package back at once;
//! - **confirm** keeps the new package and forgets the older files;
//! - **revert**, which the deadline also calls, installs the kept copy of the package that ran
//!   before, clears the trial and restarts the units. A rollback is an installation of an
//!   older version and is never refused.
//!
//! The channels are those of the hand-placed layout (`/etc/kbf/host-install-releases.toml`).

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_host::exec::Exec;
use kbf_host::path::Root;
use kbf_installer::CommitId;
use kbf_installer::attest::{GhCheck, Tools as AttestTools};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::Runner as InstallerRunner;
use kbf_installer::settings::Settings;
use kbf_installer::stage::BUNDLE_CAP;

use super::answer::Code;
use super::backend::{Backend, Fault, Pending};
use super::linux::{
    Tools, channel_fault, config_fault, copy_hashing_mode, fetch_fault, fetch_scratch_in,
    read_capped, signed_by, state_fault,
};
use super::package_format::{Format, Meta, PACKAGE, SumsError, sums_entry};
use super::package_store::{Kept, Record, Store, Trial};
use super::units::restart_and_wait;
use crate::host::Host;
use crate::mac::channel::{MacAllowlist, Source};
use crate::mac::fetch::{Fetcher, Limits};
use crate::mac::release::Origin;
use crate::mac::release::{self, commit_prefix};
use crate::mac::stage::read_token;
use crate::plan::Wait;
use crate::request::{Channel, Unit, Version};
use crate::runner::{Identity, Runner};

/// A Linux host that runs the `kbf` package, as the backend of an update.
pub struct PackageHost {
    pub(crate) format: Format,
    /// The installer's trust files (the pinned `gh`, the trusted root, `installer.conf`) and
    /// the work directory the checks run in.
    pub(crate) layout: Layout,
    /// Who owns what, for the trust files of the check: the host's own.
    pub(crate) trust: Box<dyn Exec>,
    pub(crate) host: Host,
    pub(crate) allowlist: PathBuf,
    pub(crate) units: Vec<Unit>,
    pub(crate) restart_wait: Wait,
    /// The clock the wait is counted on.
    pub(crate) pace: crate::pace::Pace,
    /// `systemctl`, `curl`, `gh release download` and the package manager's tools.
    pub(crate) runner: Box<dyn Runner>,
    /// `gh attestation verify`, the installer's way.
    pub(crate) installer_runner: Box<dyn InstallerRunner>,
    pub(crate) tools: Tools,
    pub(crate) store: Store,
}

fn verify_refused(why: impl std::fmt::Display) -> Fault {
    Fault::refused(Code::VerifyFailed, why.to_string())
}

/// The directory a package is checked in: `work/<commit>.check`, root's and readable by all,
/// with the home of the unprivileged `gh` in it. Removed when dropped.
struct Check {
    dir: PathBuf,
}

impl Check {
    fn make(work: &Path, commit: &str, who: Identity) -> Result<Self, Fault> {
        let stage_failed =
            |e: std::io::Error| Fault::failed(Code::StageFailed, format!("check: {e}"));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(work)
            .map_err(stage_failed)?;
        let dir = work.join(format!("{commit}.check"));
        let _ = fs::remove_dir_all(&dir);
        let made = Self { dir };
        fs::create_dir(&made.dir)
            .and_then(|()| fs::set_permissions(&made.dir, fs::Permissions::from_mode(0o755)))
            .map_err(stage_failed)?;
        let home = made.home();
        fs::create_dir(&home)
            .and_then(|()| fs::set_permissions(&home, fs::Permissions::from_mode(0o700)))
            .and_then(|()| std::os::unix::fs::chown(&home, Some(who.uid), Some(who.gid)))
            .map_err(stage_failed)?;
        Ok(made)
    }

    fn home(&self) -> PathBuf {
        self.dir.join("gh")
    }
}

impl Drop for Check {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A release's package file and what goes with it, copied once: the directory they are in goes
/// when this does.
pub(crate) struct Copies {
    check: Check,
    pub(crate) package: PathBuf,
    bundle: PathBuf,
    sums: String,
    pub(crate) sha256: String,
    name: String,
}

impl Copies {
    /// The directory the copies are in, which goes when they do.
    pub(crate) fn dir(&self) -> &Path {
        &self.check.dir
    }
}

impl PackageHost {
    fn restart_units(&self) -> Result<(), String> {
        for unit in &self.units {
            restart_and_wait(&*self.runner, unit, self.restart_wait, &self.pace)?;
        }
        Ok(())
    }

    /// The version of the package the package database holds, if it holds one.
    fn installed(&self) -> Result<Option<String>, Fault> {
        let exec = self
            .runner
            .run(&self.format.installed())
            .map_err(|e| state_fault(format!("the package database: {e}")))?;
        self.format
            .read_installed(&exec)
            .map_err(|why| state_fault(format!("the package database: {why}")))
    }

    /// Install the package file `file`.
    fn install(&self, file: &Path) -> Result<(), String> {
        match self.runner.run(&self.format.install(file)) {
            Ok(exec) if exec.success() => Ok(()),
            Ok(exec) => Err(format!(
                "installing the package: exit status {:?}: {}",
                exec.code,
                exec.stderr_text().trim()
            )),
            Err(e) => Err(format!("installing the package: {e}")),
        }
    }

    /// What a fetch or a check of the channel `channel` goes by: the host's settings narrowed to
    /// the channel's branch, and the token of its API, if it names one. The pinned `gh` and the
    /// trusted root must be root's, or nothing is believed.
    pub(crate) fn prepare(
        &self,
        channel: &Channel,
    ) -> Result<(Settings, Option<String>, Origin), Fault> {
        let trust = &self.host.trust;
        let allowlist = MacAllowlist::load(trust, &self.allowlist).map_err(config_fault)?;
        let entry = allowlist.channel(trust, channel).map_err(channel_fault)?;
        let Source::Github { token_file } = &entry.source else {
            return Err(Fault::refused(
                Code::ConfigInvalid,
                "a channel of the socket takes its releases from GitHub",
            ));
        };
        let token = match token_file {
            Some(file) => Some(read_token(trust, file).map_err(config_fault)?),
            None => None,
        };
        let settings = Settings::load(&*self.trust, &self.layout)
            .map_err(verify_refused)?
            .narrowed_to(std::slice::from_ref(&entry.source_ref))
            .map_err(|e| Fault::refused(Code::ConfigInvalid, e.to_string()))?;
        let settings = signed_by(settings, &entry.origin);
        GhCheck::pinned(&self.layout, &settings)
            .check_trust(&*self.trust)
            .map_err(verify_refused)?;
        Ok((settings, token, entry.origin))
    }

    /// The release `version` names, kept and verified; its commit. A file this host holds
    /// for that very version is not fetched again.
    fn acquire(
        &self,
        record: &mut Record,
        version: &str,
        channel: &Channel,
    ) -> Result<String, Fault> {
        let prefix = commit_prefix(version).ok_or_else(|| {
            Fault::refused(
                Code::UnknownRelease,
                "the version carries no commit (+g or +, then 12 hex digits)",
            )
        })?;
        let held = record.files.iter().find(|(commit, kept)| {
            commit.starts_with(prefix) && kept.version == version && self.store.has(commit)
        });
        if let Some((commit, _)) = held {
            return Ok(commit.clone());
        }
        let (settings, token, origin) = self.prepare(channel)?;
        let who = Identity {
            uid: settings.run_as.uid,
            gid: settings.run_as.gid,
        };
        let work = self.layout.join(&self.layout.work_dir());
        let scratch = fetch_scratch_in(&work, prefix, who)?;
        let fetched = self.fetch_and_check(
            (&work, &scratch),
            version,
            token.as_deref(),
            (&settings, &origin),
        );
        let _ = fs::remove_dir_all(&scratch);
        let (commit, kept) = fetched?;
        record.files.insert(
            commit.clone(),
            Kept {
                version: version.to_owned(),
                sha256: kept,
                channel: channel.as_str().to_owned(),
            },
        );
        Ok(commit)
    }

    /// Fetch the release of the commit that starts with `prefix` into `scratch`, check what
    /// came, and keep the package. The commit and the digest of the kept file.
    fn fetch_and_check(
        &self,
        (work, scratch): (&Path, &Path),
        version: &str,
        token: Option<&str>,
        (settings, origin): (&Settings, &Origin),
    ) -> Result<(String, String), Fault> {
        let prefix = commit_prefix(version).unwrap_or_default();
        let who = Identity {
            uid: settings.run_as.uid,
            gid: settings.run_as.gid,
        };
        let platform = format!("linux-{}", settings.arch);
        let extension = self.format.asset().extension();
        let home = scratch.join("home");
        let gh = self.layout.join(&self.layout.gh());
        let fetcher = Fetcher {
            origin,
            platform: &platform,
            asset: self.format.asset(),
            runner: &*self.runner,
            curl: &self.tools.curl,
            gh: &gh,
            who,
            home: &home,
            token,
            limits: Limits {
                tool: self.tools.tool,
                download: self.tools.download,
                tarball_max: self.tools.tarball_max,
                bundle_max: BUNDLE_CAP,
            },
        };
        let commit = fetcher.fetch(prefix, scratch).map_err(fetch_fault)?;
        let id =
            CommitId::new(&commit).map_err(|e| Fault::failed(Code::StageFailed, e.to_string()))?;

        let file_name = release::named_asset_on(origin.prefix(), &platform, extension, &commit);
        let bundle_name = release::bundle_of(&file_name);
        let copies = self.copy_in(
            (work, &commit),
            who,
            [
                &scratch.join(&file_name),
                &scratch.join(&bundle_name),
                &scratch.join(release::SUMS_NAME),
            ],
            &file_name,
        )?;
        let meta = self.check_copies(&copies, &id, settings)?;
        if meta.version != version {
            return Err(verify_refused(format!(
                "the package is version {}, not {version}",
                meta.version
            )));
        }
        self.store.keep(
            &commit,
            &copies.package,
            &copies.sha256,
            self.tools.tarball_max,
        )?;
        Ok((commit, copies.sha256))
    }

    /// Copy the package, its bundle and the release's sums (`from`, in that order) once into a
    /// directory of the checks' own, through descriptors; only the copies are looked at from
    /// here on.
    pub(crate) fn copy_in(
        &self,
        (work, commit): (&Path, &str),
        who: Identity,
        from: [&Path; 3],
        file_name: &str,
    ) -> Result<Copies, Fault> {
        let fetch_failed = |name: &str, e: std::io::Error| {
            Fault::refused(Code::FetchFailed, format!("{name}: {e}"))
        };
        let bundle_name = release::bundle_of(file_name);
        let check = Check::make(work, commit, who)?;
        let package = check.dir.join(file_name);
        let bundle = check.dir.join(&bundle_name);
        let copied = copy_hashing_mode(from[0], &package, self.tools.tarball_max, 0o644)
            .map_err(|e| fetch_failed(file_name, e))?;
        copy_hashing_mode(from[1], &bundle, BUNDLE_CAP, 0o644)
            .map_err(|e| fetch_failed(&bundle_name, e))?;
        let sums =
            read_capped(from[2], BUNDLE_CAP).map_err(|e| fetch_failed(release::SUMS_NAME, e))?;
        Ok(Copies {
            check,
            package,
            bundle,
            sums,
            sha256: copied.sha256,
            name: file_name.to_owned(),
        })
    }

    /// Every check a package passes before it is kept or installed, offline, on the copies:
    /// the sums list its digest, `gh attestation verify` accepts it and its bundle for the
    /// commit, and the package manager reads it as `kbf` for this host's CPU. What it says.
    pub(crate) fn check_copies(
        &self,
        copies: &Copies,
        id: &CommitId,
        settings: &Settings,
    ) -> Result<Meta, Fault> {
        // 1. The digest the release lists.
        match sums_entry(&copies.sums, &copies.name) {
            Ok(listed) if listed == copies.sha256 => {}
            Ok(_) => {
                return Err(verify_refused(
                    "the package is not the file SHA256SUMS lists",
                ));
            }
            Err(SumsError::NotListed) => {
                return Err(verify_refused("SHA256SUMS does not list the package"));
            }
            Err(SumsError::Malformed) => return Err(verify_refused("SHA256SUMS is malformed")),
        }
        // 2. The build the release workflow made, for the commit.
        let gh_check = GhCheck::pinned(&self.layout, settings);
        let tools = AttestTools {
            trust: &*self.trust,
            runner: &*self.installer_runner,
        };
        gh_check
            .verify(
                &tools,
                &copies.check.home(),
                &copies.package,
                &copies.bundle,
                id,
            )
            .map_err(verify_refused)?;
        // 3. What the package says it is.
        self.read_meta(&copies.package, settings)
    }

    /// The package manager reads the file's own fields: `kbf`, for this host's CPU.
    fn read_meta(&self, package: &Path, settings: &Settings) -> Result<Meta, Fault> {
        let exec = self
            .runner
            .run(&self.format.inspect(package))
            .map_err(|e| verify_refused(format!("reading the package: {e}")))?;
        if !exec.success() {
            return Err(verify_refused(format!(
                "reading the package: exit status {:?}: {}",
                exec.code,
                exec.stderr_text().trim()
            )));
        }
        let meta = self
            .format
            .read_meta(&exec.stdout_text())
            .map_err(verify_refused)?;
        let arch = self.format.arch_name(settings.arch);
        if meta.name != PACKAGE || meta.arch != arch {
            return Err(verify_refused(format!(
                "the package is {} for {}, not {PACKAGE} for {arch}",
                meta.name, meta.arch
            )));
        }
        Ok(meta)
    }

    /// A verified copy of the package that runs (`version`) is kept: the one this host holds,
    /// or the release fetched and checked the same way as any other. Its commit.
    fn keep_running(
        &self,
        record: &mut Record,
        version: &str,
        channel: &Channel,
    ) -> Result<String, Fault> {
        if let Some(commit) = record.commit_of(version).map(str::to_owned) {
            let kept = &record.files[&commit];
            if self
                .store
                .verified(&commit, kept, self.tools.tarball_max)
                .is_ok()
            {
                return Ok(commit);
            }
            // A copy that is gone or changed is not trusted: it is fetched again.
            record.files.remove(&commit);
        }
        if Version::new(version).is_err() {
            return Err(Fault::refused(
                Code::NoCurrent,
                "the package that runs has a version this program does not read",
            ));
        }
        if commit_prefix(version).is_none() {
            return Err(Fault::refused(
                Code::NoCurrent,
                "the package that runs names no build, so no copy of it can be kept",
            ));
        }
        self.acquire(record, version, channel)
    }

    /// The previous package is installed again, the trial is over, and the units restart on
    /// it. Whether the host is whole again.
    fn put_back(&self) -> Result<(), Fault> {
        let mut record = self.store.load()?;
        let trial = record
            .pending
            .clone()
            .ok_or_else(|| Fault::refused(Code::NothingPending, "no release is on trial"))?;
        let kept =
            record.files.get(&trial.from).cloned().ok_or_else(|| {
                revert_failed("no copy of the package that ran is kept".to_owned())
            })?;
        let path = match self
            .store
            .verified(&trial.from, &kept, self.tools.tarball_max)
        {
            Ok(path) => path,
            Err(_) => {
                // Gone or changed: the same release is fetched and checked again.
                record.files.remove(&trial.from);
                let channel = kept_channel(&kept).map_err(revert_failed)?;
                let commit = self
                    .acquire(&mut record, &kept.version, &channel)
                    .map_err(|f| {
                        revert_failed(format!("fetching the package that ran: {}", f.detail))
                    })?;
                self.store.path(&commit)
            }
        };
        self.install(&path).map_err(revert_failed)?;
        record.pending = None;
        self.store.prune(&mut record, &[trial.from.as_str()]);
        self.store.save(&record)?;
        self.restart_units().map_err(|why| {
            revert_failed(format!(
                "the host is back on the package that ran, but: {why}"
            ))
        })
    }
}

fn revert_failed(why: String) -> Fault {
    Fault::failed(Code::RevertFailed, why)
}

/// The channel a kept package was fetched through.
fn kept_channel(kept: &Kept) -> Result<Channel, String> {
    Channel::new(&kept.channel).map_err(|_| "the kept channel is unreadable".to_owned())
}

impl Backend for PackageHost {
    fn lock_file(&self) -> PathBuf {
        self.host.install_lock.clone()
    }

    fn node_id(&self) -> Option<String> {
        crate::node_id::read(
            &self.host.trust,
            &self.host.node_config,
            &self.host.hostname,
        )
    }

    /// The release the installed package is: its commit when this host kept the package, else
    /// the 12 digits its version carries, else the version itself.
    fn current(&self) -> Result<Option<String>, Fault> {
        let Some(version) = self.installed()? else {
            return Ok(None);
        };
        let record = self.store.load()?;
        Ok(Some(
            record
                .commit_of(&version)
                .or_else(|| commit_prefix(&version))
                .map_or(version.clone(), str::to_owned),
        ))
    }

    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(self.store.load()?.pending.map(|trial| Pending {
            commit: trial.commit,
            from: trial.from,
            deadline_unix: Some(trial.deadline_unix),
            activation: trial.activation,
        }))
    }

    fn has(&self, commit: &str) -> bool {
        self.store.has(commit)
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        let mut record = self.store.load()?;
        let commit = self.acquire(&mut record, version.as_str(), channel)?;
        self.store.save(&record)?;
        Ok(commit)
    }

    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        let mut record = self.store.load()?;
        let target = record.files.get(commit).cloned().ok_or_else(|| {
            Fault::refused(Code::NotStaged, "that package is not kept; install it")
        })?;
        let package = self
            .store
            .verified(commit, &target, self.tools.tarball_max)?;
        let running = self.installed()?.ok_or_else(|| {
            Fault::refused(Code::NoCurrent, "no kbf package is installed to go back to")
        })?;
        if running == target.version {
            return Err(Fault::refused(
                Code::NotStaged,
                "that release is the one that runs",
            ));
        }
        let channel = kept_channel(&target).map_err(state_fault)?;
        let from = self.keep_running(&mut record, &running, &channel)?;
        record.pending = Some(Trial {
            commit: commit.to_owned(),
            from,
            deadline_unix: now.saturating_add(window_secs),
            activation: format!("host-install-{}", &commit[..12]),
        });
        // The trial is on disk before the package manager is asked for anything.
        self.store.save(&record)?;
        let came_up = self.install(&package).and_then(|()| self.restart_units());
        if let Err(why) = came_up {
            // The new package did not come up: the host goes back now, not at the deadline.
            return Err(match self.put_back() {
                Ok(()) => Fault::failed(Code::SwitchFailed, why),
                Err(fault) => Fault::failed(
                    Code::RevertFailed,
                    format!("{why}; going back: {}", fault.detail),
                ),
            });
        }
        Ok(now.saturating_add(window_secs))
    }

    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        let mut record = self.store.load()?;
        record.pending = None;
        self.store.prune(
            &mut record,
            &[pending.commit.as_str(), pending.from.as_str()],
        );
        self.store.save(&record)
    }

    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        self.put_back()
    }
}

/// The host as it is: the real runners and this program's own paths.
pub fn production(units: Vec<Unit>, format: Format, state_dir: &Path) -> PackageHost {
    let root = Root::host();
    let host = Host::production();
    let platform = format!("linux-{}", kbf_installer::binary::Arch::current());
    PackageHost {
        format,
        layout: Layout::new(root.clone(), Platform::current()).with_trust_fallback(),
        trust: Box::new(kbf_host::HostExec::new(root)),
        store: Store::new(
            state_dir.to_path_buf(),
            host.trust.clone(),
            &platform,
            format.asset().extension(),
        ),
        host,
        allowlist: PathBuf::from(super::linux::ALLOWLIST),
        units,
        restart_wait: crate::plan::ACTIVE_WAIT,
        pace: crate::pace::Pace::real(),
        runner: Box::new(crate::runner::SystemRunner),
        installer_runner: Box::new(kbf_installer::run::SystemRunner),
        tools: Tools::production(),
    }
}
