//! A Linux host: the layout, the state and the swap of `kbf-installer`.
//!
//! A host keeps each release in `/var/lib/kbf-installer/releases/<commit>` and two links
//! beside them, `current` and `previous`, which `state.json` names ([`kbf_installer::layout`]).
//! The farm's hosts got theirs from `kbf-deploy host install`, by hand; this backend puts the
//! next one beside it and moves the links the way the installer's own swap does, so a host
//! has one layout and one state file whoever moved it:
//!
//! - **stage**: the release the version names is listed on GitHub and downloaded as the
//!   installer's unprivileged user ([`crate::mac::fetch`], the same code as a Mac's); root
//!   copies the two files once, by descriptor, into the installer's scratch and runs the
//!   installer's verify pipeline over the copy ([`kbf_installer::stage::Pipeline`]): size,
//!   digest, `gh attestation verify` offline (signer `komira-ai/kbf` `artifacts.yml`, hosted
//!   runners, the full commit, the channel's branch), the executable of this CPU, and the
//!   binary's own word on its commit. The release is installed as `releases/<commit>` and
//!   nothing else changes;
//! - **switch**: [`kbf_installer::activate::activate`] writes the pending marker with its
//!   deadline into `state.json`, then moves the links, `previous` first; the configured units
//!   are restarted and waited for. A unit that does not come up puts the host back at once;
//! - **confirm**: [`kbf_installer::activate::confirm`] clears the marker;
//! - **revert**: [`kbf_installer::activate::revert`] writes the state first, then moves the
//!   links back, and the units are restarted on the release that ran.
//!
//! The channels are named in `/etc/kbf/host-install-releases.toml`, in the format of a Mac's
//! allowlist ([`crate::mac::channel`]); the ssh forced command's `host-install.toml` is the
//! apt channels' and is not read.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::exec::Exec;
use kbf_host::path::Root;
use kbf_host::plan::Mode;
use kbf_host::step::Owner;
use kbf_installer::activate::{self, Activated, Arm};
use kbf_installer::attest::{GhCheck, Signer};
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::helper::service::{self, HostProcs, Procs};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::refusal::Refusal as InstallerRefusal;
use kbf_installer::release;
use kbf_installer::retain::{self, Kept};
use kbf_installer::run::Runner as InstallerRunner;
use kbf_installer::settings::Settings;
use kbf_installer::stage::{
    BUNDLE_CAP, Fetched, MAX_BINARY, Order, OrderKind, Partial, Pipeline, Policy, Received,
    Timeouts, Verifier, supported,
};
use kbf_installer::{CommitId, state};
use kbf_update::Locator;
use sha2::{Digest, Sha256};

use super::backend::{Backend, Fault, Pending};
use super::units::restart_and_wait;
use crate::host::Host;
use crate::mac::channel::{ChannelError, MacAllowlist, Source};
use crate::mac::fetch::{AssetKind, FetchError, Fetcher, Limits};
use crate::mac::release::Origin;
use crate::mac::stage::read_token;
use crate::outcome::Refusal;
use crate::plan::Wait;
use crate::request::{Channel, Unit, Version};
use crate::runner::{Identity, Runner};
use crate::trust::{Trust, nofollow};

use super::answer::Code;

/// The channels of the local socket on a Linux host.
pub const ALLOWLIST: &str = "/etc/kbf/host-install-releases.toml";

/// The most a release's tarball may be, in bytes.
pub const TARBALL_MAX: u64 = 512 * 1024 * 1024;

/// The most a frontend image may be, in bytes.
pub const IMAGE_MAX: u64 = 1024 * 1024 * 1024;

/// The programs and the time limits of a stage.
#[derive(Debug, Clone)]
pub struct Tools {
    /// The system `curl`, which lists the releases.
    pub curl: PathBuf,
    /// How long one tool (a listing page) may run.
    pub tool: Duration,
    /// How long the download may take.
    pub download: Duration,
    /// The largest tarball taken in.
    pub tarball_max: u64,
    /// The largest frontend image taken in.
    pub image_max: u64,
}

impl Tools {
    pub fn production() -> Self {
        Self {
            curl: PathBuf::from("/usr/bin/curl"),
            tool: Duration::from_secs(60),
            download: Duration::from_secs(900),
            tarball_max: TARBALL_MAX,
            image_max: IMAGE_MAX,
        }
    }
}

/// No lease is waited for: a stage of the update path moves no link and the updater has
/// drained the node before it switches.
pub(super) struct NoLeases;

impl LeaseSource for NoLeases {
    fn live(&self) -> Result<usize, String> {
        Ok(0)
    }
}

/// A Linux host as the backend of an update.
pub struct Linux<E: Exec> {
    pub(crate) layout: Layout,
    /// The steps that move the links.
    pub(crate) exec: RefCell<E>,
    /// Who owns what, for the trust files of the verify pipeline: the host's own.
    pub(crate) trust: Box<dyn Exec>,
    /// The paths of this program's own files, and who is trusted to own them.
    pub(crate) host: Host,
    pub(crate) allowlist: PathBuf,
    pub(crate) units: Vec<Unit>,
    pub(crate) restart_wait: Wait,
    /// The clock the wait is counted on.
    pub(crate) pace: crate::pace::Pace,
    /// `systemctl`, `curl` and `gh release download`.
    pub(crate) runner: Box<dyn Runner>,
    /// `gh attestation verify` and the probe, the installer's way.
    pub(crate) installer_runner: Box<dyn InstallerRunner>,
    pub(crate) tools: Tools,
    /// Who owns the files of a stage: root, on a host.
    pub(crate) owner: Owner,
    /// The name of the frontend container this helper serves, when it has a configuration
    /// file of its own: it is part of the names of the scratch and check directories, so that
    /// two helpers staging at once never remove each other's download.
    pub(crate) scratch_tag: Option<String>,
    /// Which releases a process executes from, so a prune never removes one.
    pub(crate) procs: Box<dyn Procs>,
}

pub(super) fn commit_id(commit: &str) -> Result<CommitId, Fault> {
    CommitId::new(commit).map_err(|e| Fault::failed(Code::StateUnavailable, e.to_string()))
}

pub(super) fn state_fault(e: impl std::fmt::Display) -> Fault {
    Fault::failed(Code::StateUnavailable, e.to_string())
}

/// The scratch directory `<work>/<prefix>.fetch` (and its `home`) that the unprivileged user
/// `who` fetches into, made new and owned by it; `work` is made, readable by all, if it is not
/// there.
pub(super) fn fetch_scratch_in(work: &Path, prefix: &str, who: Identity) -> Result<PathBuf, Fault> {
    let stage_failed = |e: io::Error| Fault::failed(Code::StageFailed, format!("scratch: {e}"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(work)
        .map_err(stage_failed)?;
    let scratch = work.join(format!("{prefix}.fetch"));
    match fs::remove_dir_all(&scratch) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(stage_failed(e)),
        _ => {}
    }
    for (dir, mode) in [(scratch.clone(), 0o700), (scratch.join("home"), 0o700)] {
        fs::create_dir(&dir)
            .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(mode)))
            .and_then(|()| std::os::unix::fs::chown(&dir, Some(who.uid), Some(who.gid)))
            .map_err(stage_failed)?;
    }
    Ok(scratch)
}

/// The refusal a fetch that found nothing, or could not fetch, is.
pub(super) fn fetch_fault(e: FetchError) -> Fault {
    match e {
        FetchError::Unknown => Fault::refused(Code::UnknownRelease, "no release has that commit"),
        FetchError::Ambiguous => {
            Fault::refused(Code::AmbiguousRelease, "two releases have that commit")
        }
        FetchError::Failed(why) | FetchError::RateLimited(why) => {
            Fault::refused(Code::FetchFailed, why)
        }
    }
}

/// What [`Linux::prepare`] read: everything a fetch and its check need.
pub(super) struct Prepared {
    /// The version's 12 hex digits.
    pub(super) prefix: String,
    /// The token of the channel for the GitHub API, if it names one.
    pub(super) token: Option<String>,
    pub(super) settings: Settings,
    /// The repository, signer workflow and file names of the channel's releases.
    pub(super) origin: Origin,
    /// The unprivileged user the fetch runs as.
    pub(super) who: Identity,
    /// The channel's pinned sums, if it has some.
    pub(super) sums: Option<PathBuf>,
}

/// The refusal of the allowlist or of a channel, as the socket tells it.
pub(super) fn config_fault(refusal: Refusal) -> Fault {
    let code = match refusal {
        Refusal::ChannelNotAllowed => Code::ChannelNotAllowed,
        Refusal::ConfigUnsafe | Refusal::KeyUntrusted => Code::ConfigUnsafe,
        _ => Code::ConfigInvalid,
    };
    Fault::refused(code, format!("the allowlist: {}", refusal.code()))
}

/// The refusal of a channel, as the socket tells it: the words name an untrusted repository.
pub(crate) fn channel_fault(error: ChannelError) -> Fault {
    let code = match error.refusal() {
        Refusal::ChannelNotAllowed => Code::ChannelNotAllowed,
        Refusal::ConfigUnsafe | Refusal::KeyUntrusted => Code::ConfigUnsafe,
        _ => Code::ConfigInvalid,
    };
    Fault::refused(code, error.to_string())
}

/// `settings` with the signers of `origin` as the ones an attestation may name.
pub(crate) fn signed_by(mut settings: Settings, origin: &Origin) -> Settings {
    settings.signers = origin
        .signers()
        .into_iter()
        .map(|(repo, workflow)| Signer {
            repo: repo.to_owned(),
            workflow: workflow.to_owned(),
        })
        .collect();
    settings
}

impl<E: Exec> Linux<E> {
    fn restart_units(&self) -> Result<(), String> {
        for unit in &self.units {
            restart_and_wait(&*self.runner, unit, self.restart_wait, &self.pace)?;
        }
        Ok(())
    }

    /// Put the host back on the release that ran: state, links, restart. Whether the units
    /// are up again.
    fn put_back(&self) -> Result<(), Fault> {
        let mut out = Vec::new();
        let reverted = activate::revert(&mut *self.exec.borrow_mut(), &self.layout, &mut out);
        reverted.map_err(|e| Fault::failed(Code::RevertFailed, e.to_string()))?;
        self.restart_units().map_err(|why| {
            Fault::failed(
                Code::RevertFailed,
                format!("the host is back on the release that ran, but: {why}"),
            )
        })
    }

    /// The scratch directory the unprivileged user fetches into, owned by it.
    pub(super) fn fetch_scratch(&self, prefix: &str, who: Identity) -> Result<PathBuf, Fault> {
        let work = self.layout.join(&self.layout.work_dir());
        fetch_scratch_in(&work, &self.tagged(prefix), who)
    }

    /// `name`, with this helper's tag if it has one: `<name>.<tag>`.
    pub(super) fn tagged(&self, name: &str) -> String {
        match &self.scratch_tag {
            Some(tag) => format!("{name}.{tag}"),
            None => name.to_owned(),
        }
    }

    /// What a fetch needs of the root-owned configuration, read once: the channel's source
    /// and branch, the token for the listing, the settings of the installer and the user the
    /// fetch runs as. Nothing is fetched that cannot be checked: the pinned `gh` and the
    /// trusted root are root's, or this host does not believe what they would say.
    pub(super) fn prepare(&self, version: &Version, channel: &Channel) -> Result<Prepared, Fault> {
        let prefix = crate::mac::release::commit_prefix(version.as_str()).ok_or_else(|| {
            Fault::refused(
                Code::UnknownRelease,
                "the version carries no commit (+g or +, then 12 hex digits)",
            )
        })?;
        let trust: &Trust = &self.host.trust;
        let allowlist = MacAllowlist::load(trust, &self.allowlist).map_err(config_fault)?;
        let channel = allowlist.channel(trust, channel).map_err(channel_fault)?;
        let Source::Github { token_file } = &channel.source else {
            return Err(Fault::refused(
                Code::ConfigInvalid,
                "a channel of the socket takes its releases from GitHub",
            ));
        };
        let token = match token_file {
            Some(file) => Some(read_token(trust, file).map_err(config_fault)?),
            None => None,
        };
        let verify_refused =
            |e: InstallerRefusal| Fault::refused(Code::VerifyFailed, e.to_string());
        let settings = {
            let settings = Settings::load(&*self.trust, &self.layout).map_err(verify_refused)?;
            let settings = settings
                .narrowed_to(std::slice::from_ref(&channel.source_ref))
                .map_err(|e| Fault::refused(Code::ConfigInvalid, e.to_string()))?;
            signed_by(settings, &channel.origin)
        };
        GhCheck::pinned(&self.layout, &settings)
            .check_trust(&*self.trust)
            .map_err(verify_refused)?;
        let who = Identity {
            uid: settings.run_as.uid,
            gid: settings.run_as.gid,
        };
        Ok(Prepared {
            prefix: prefix.to_owned(),
            token,
            settings,
            origin: channel.origin,
            who,
            sums: channel.sums,
        })
    }

    /// Fetch and verify the release; the commit.
    fn fetch_and_verify(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        let prepared = self.prepare(version, channel)?;
        let scratch = self.fetch_scratch(&prepared.prefix, prepared.who)?;
        let result = self.download_and_verify(
            &prepared.prefix,
            prepared.token.as_deref(),
            (&prepared.settings, &prepared.origin),
            prepared.who,
            &scratch,
        );
        let _ = fs::remove_dir_all(&scratch);
        result
    }

    fn download_and_verify(
        &self,
        prefix: &str,
        token: Option<&str>,
        (settings, origin): (&Settings, &Origin),
        who: Identity,
        scratch: &Path,
    ) -> Result<String, Fault> {
        let platform = format!("linux-{}", settings.arch);
        let gh = self.layout.join(&self.layout.gh());
        let home = scratch.join("home");
        let fetcher = Fetcher {
            origin,
            platform: &platform,
            asset: AssetKind::Tarball,
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
        let id = commit_id(&commit)?;
        let partial = Partial::create(&self.layout, &id, self.owner)
            .map_err(|e| Fault::failed(Code::StageFailed, e.to_string()))?;
        let tarball_name = AssetKind::Tarball.name_on(origin.prefix(), &platform, &commit);
        let bundle_name = crate::mac::release::bundle_of(&tarball_name);
        let target = partial.dl().join("kbf.tar.gz");
        let copied = copy_hashing(
            &scratch.join(&tarball_name),
            &target,
            self.tools.tarball_max,
        )
        .map_err(|e| Fault::refused(Code::FetchFailed, format!("{tarball_name}: {e}")))?;
        let bundle = read_capped(&scratch.join(&bundle_name), BUNDLE_CAP)
            .map_err(|e| Fault::refused(Code::FetchFailed, format!("{bundle_name}: {e}")))?;
        let locator = Locator {
            name: "kbf".to_owned(),
            sha256: copied.sha256.clone(),
            size: copied.len,
            attestation: bundle,
        };
        let fetched = Fetched {
            partial,
            files: vec![Received {
                name: "kbf".to_owned(),
                tarball: target,
                bytes: copied.len,
                sha256: copied.sha256,
            }],
        };
        let pipeline = Pipeline {
            host: &self.layout,
            settings,
            policy: policy(self.tools.tarball_max),
            trust: &*self.trust,
            runner: &*self.installer_runner,
            leases: &NoLeases,
            owner: self.owner,
        };
        let order = Order {
            kind: OrderKind::FrontendUpdate,
            commit: &id,
            components: &[locator],
            expected: None,
        };
        pipeline
            .verify(&order, fetched)
            .map_err(|e| Fault::refused(Code::VerifyFailed, e.to_string()))?;
        Ok(commit)
    }
}

/// What the host holds a component to: the contract's timeouts, this build's supported
/// numbers and no operator floor (a rollback is an install of an older release).
fn policy(max_component: u64) -> Policy {
    Policy {
        max_component_bytes: max_component,
        max_binary_bytes: MAX_BINARY,
        min_protocol: 0,
        supported: supported(),
        timeouts: Timeouts::DEFAULT,
    }
}

/// What was copied: its length and sha256.
pub(super) struct Copied {
    pub(super) len: u64,
    pub(super) sha256: String,
}

/// Open the untrusted file `src` without following a symbolic link, and accept it only as a
/// regular file with one link of at most `cap` bytes.
fn open_plain(src: &Path, cap: u64) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(src)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::other("not a regular file with one link"));
    }
    if meta.len() > cap {
        return Err(io::Error::other(format!("larger than {cap} bytes")));
    }
    Ok(file)
}

/// Copy the untrusted file `src` to the new file `dst` (root's, 0600) through the open
/// descriptor, hashing what is copied (see [`open_plain`]). A file that grew since it was
/// looked at is cut at `cap`; the digest then names the cut bytes, which no attestation lists.
pub(super) fn copy_hashing(src: &Path, dst: &Path, cap: u64) -> io::Result<Copied> {
    copy_hashing_mode(src, dst, cap, 0o600)
}

/// [`copy_hashing`] into a file of mode `mode`.
pub(super) fn copy_hashing_mode(src: &Path, dst: &Path, cap: u64, mode: u32) -> io::Result<Copied> {
    let file = open_plain(src, cap)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dst)?;
    // The mode is exactly `mode`, whatever the umask.
    out.set_permissions(fs::Permissions::from_mode(mode))?;
    let mut capped = file.take(cap);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut len = 0_u64;
    loop {
        let n = capped.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hasher.update(&buffer[..n]);
        out.write_all(&buffer[..n])?;
    }
    out.sync_all()?;
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Copied { len, sha256 })
}

/// The text of the untrusted file `src` (see [`open_plain`]).
pub(super) fn read_capped(src: &Path, cap: u64) -> io::Result<String> {
    let mut text = String::new();
    open_plain(src, cap)?.take(cap).read_to_string(&mut text)?;
    Ok(text)
}

impl<E: Exec> Backend for Linux<E> {
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

    fn current(&self) -> Result<Option<String>, Fault> {
        let state = state::read_unlocked(&self.layout).map_err(state_fault)?;
        Ok(state
            .and_then(|state| state.current)
            .map(|commit| commit.to_string()))
    }

    fn pending(&self) -> Result<Option<Pending>, Fault> {
        let state = state::read_unlocked(&self.layout).map_err(state_fault)?;
        Ok(state
            .and_then(|state| state.pending_confirm)
            .map(|p| Pending {
                commit: p.commit.to_string(),
                from: p.previous.map(|c| c.to_string()).unwrap_or_default(),
                deadline_unix: p.deadline_wall,
                activation: p.activation,
            }))
    }

    fn has(&self, commit: &str) -> bool {
        CommitId::new(commit).is_ok_and(|id| {
            matches!(
                self.exec.borrow().stat(&self.layout.release_file(&id, "kbf")),
                Ok(Some(meta)) if meta.kind == kbf_host::exec::Kind::File
            )
        })
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        self.fetch_and_verify(version, channel)
    }

    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        let order = activate::Order {
            commit: commit_id(commit)?,
            order_id: format!("host-install-{}", &commit[..12]),
        };
        let mut out = Vec::new();
        let swapped = activate::activate(
            &mut *self.exec.borrow_mut(),
            &self.layout,
            &order,
            Arm::FromActivation { window_secs },
            now,
            &mut out,
        );
        match swapped.map_err(|e| Fault::failed(Code::SwitchFailed, e.to_string()))? {
            Activated::Swapped | Activated::Repeat => {}
            Activated::AlreadyCurrent => {
                return Err(Fault::refused(
                    Code::NotStaged,
                    "that release is the one that runs",
                ));
            }
        }
        if let Err(why) = self.restart_units() {
            // The release did not come up: the host goes back now, not at the deadline.
            let back = self.put_back();
            return Err(match back {
                Ok(()) => Fault::failed(Code::SwitchFailed, why),
                Err(fault) => Fault::failed(
                    Code::RevertFailed,
                    format!("{why}; going back: {}", fault.detail),
                ),
            });
        }
        Ok(now + window_secs)
    }

    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        let commit = commit_id(&pending.commit)?;
        activate::confirm(&self.layout, &pending.activation, &commit)
            .map(drop)
            .map_err(|e| Fault::failed(Code::StateUnavailable, e.to_string()))
    }

    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        self.put_back()
    }

    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        release::kept(&self.layout).map_err(state_fault)
    }

    fn retain_file(&self) -> Option<PathBuf> {
        Some(retain::path(&self.layout))
    }

    /// The installer's own `gc`, which keeps what `retain.json` names and removes nothing while
    /// a swap is on trial.
    fn prune(&self, keep: usize) -> Result<String, Fault> {
        let in_use = service::in_use(&*self.procs, &self.layout)
            .map_err(|e| state_fault(format!("cannot tell which releases run: {e}")))?;
        let mut out = Vec::new();
        release::gc(
            &mut *self.exec.borrow_mut(),
            &self.layout,
            keep,
            &in_use,
            Mode::Apply,
            &mut out,
        )
        .map_err(state_fault)?;
        Ok(String::from_utf8_lossy(&out).trim_end().replace('\n', "; "))
    }
}

/// The host as it is: the real `exec` and runners, this program's own paths.
pub fn production(units: Vec<Unit>) -> Linux<kbf_host::HostExec> {
    let root = Root::host();
    let layout = Layout::new(root.clone(), Platform::current()).with_trust_fallback();
    Linux {
        layout,
        exec: RefCell::new(kbf_host::HostExec::new(root.clone())),
        trust: Box::new(kbf_host::HostExec::new(root)),
        host: Host::production(),
        allowlist: PathBuf::from(ALLOWLIST),
        units,
        restart_wait: crate::plan::ACTIVE_WAIT,
        pace: crate::pace::Pace::real(),
        runner: Box::new(crate::runner::SystemRunner),
        installer_runner: Box::new(kbf_installer::run::SystemRunner),
        tools: Tools::production(),
        owner: Owner { uid: 0, gid: 0 },
        scratch_tag: None,
        procs: Box::new(HostProcs::new(Platform::current())),
    }
}
