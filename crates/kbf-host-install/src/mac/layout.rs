//! Where everything is on a Mac, and how long the apply waits for it.
//!
//! The kit layout (the one the Mac kit left and the daemon's job runs from):
//!
//! ```text
//! /usr/local/kbf/releases/<commit>/kbf      one file per release, root's
//! /usr/local/kbf/releases/<commit>/VERSION  the version text the release was installed as
//! /usr/local/kbf/current  -> releases/<commit>    relative links
//! /usr/local/kbf/previous -> releases/<commit>
//! /usr/local/kbf/etc/av2_mac.conf           the node's settings (NODE_ID=...)
//! /usr/local/kbf/log/kbf-daemon.err.log     where launchd writes the daemon's stderr
//! /Library/LaunchDaemons/com.komira.kbf-daemon.plist
//! ```

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use crate::pace::Pace;
use crate::trust::Trust;

/// The launchd label of the daemon's job.
pub const DAEMON_LABEL: &str = "com.komira.kbf-daemon";

/// The launchd label of the one-shot job that applies a release.
pub const APPLY_LABEL: &str = "com.komira.kbf-host-install-apply";

/// Where the packaging installs this program on a Mac: the forced command of the ssh key
/// and the one program the sudoers rule names.
pub const SELF_EXE: &str = "/usr/local/libexec/kbf/kbf-host-install";

/// The account the daemon runs as.
pub const DAEMON_USER: &str = "_kbf";

/// The account the attestation check runs as: nobody.
pub const UNPRIVILEGED_USER: &str = "nobody";

/// The directory of a release holds this file: the daemon.
pub const BINARY: &str = "kbf";

/// The file beside it that holds the version text the release was installed as.
pub const VERSION_FILE: &str = "VERSION";

/// How long the apply waits, and how much of each file it takes.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// How long `launchctl bootout` may take to leave the service gone.
    pub gone: Duration,
    /// How long the new daemon has to log that it was welcomed.
    pub welcome: Duration,
    /// How often those two are looked at.
    pub poll: Duration,
    /// How long one external tool may run.
    pub tool: Duration,
    /// How long the download of a release from GitHub may take.
    pub fetch: Duration,
    /// The largest release tarball taken in, in bytes.
    pub tarball_max: u64,
    /// The largest attestation bundle taken in, in bytes.
    pub bundle_max: u64,
    /// The largest binary a tarball may unpack to, in bytes.
    pub binary_max: u64,
}

impl Tuning {
    /// The production values.
    pub const fn production() -> Self {
        Self {
            gone: Duration::from_secs(60),
            welcome: Duration::from_secs(180),
            poll: Duration::from_millis(500),
            tool: Duration::from_secs(120),
            fetch: Duration::from_secs(900),
            tarball_max: 512 * 1024 * 1024,
            bundle_max: 256 * 1024,
            binary_max: 1024 * 1024 * 1024,
        }
    }
}

/// The paths of a Mac and who is trusted to own them.
#[derive(Debug, Clone)]
pub struct MacHost {
    /// The channels an install may use.
    pub(crate) allowlist: PathBuf,
    pub(crate) audit: PathBuf,
    /// Root's private directory: the status file, the lock, the stage.
    pub(crate) work: PathBuf,
    /// The kit tree.
    pub(crate) kit: PathBuf,
    pub(crate) node_conf: PathBuf,
    pub(crate) daemon_plist: PathBuf,
    pub(crate) err_log: PathBuf,
    /// The daemon's own configuration, which the new binary is checked against.
    pub(crate) daemon_config: PathBuf,
    /// The pinned `gh` and Sigstore trusted root the installer package carries.
    pub(crate) gh: PathBuf,
    pub(crate) trusted_root: PathBuf,
    /// The system `curl`, which lists the releases of a GitHub channel.
    pub(crate) curl: PathBuf,
    /// The daemon's working directory (its `HOME`).
    pub(crate) daemon_home: PathBuf,
    /// This program, as the sudoers rule and the apply job name it.
    pub(crate) self_exe: PathBuf,
    /// For the allowlist, the pinned files and the audit log: trusted up to `/`.
    pub(crate) trust: Trust,
    /// For the kit tree: trusted up to the top of the kit.
    pub(crate) kit_trust: Trust,
    pub(crate) tuning: Tuning,
    /// How the waits tell time and pause: the real clock here, a made-up one in tests.
    pub(crate) pace: Rc<Pace>,
}

impl MacHost {
    /// The production paths, trusting root.
    pub fn production() -> Self {
        let kit = PathBuf::from("/usr/local/kbf");
        Self {
            allowlist: PathBuf::from("/usr/local/etc/kbf/host-install.toml"),
            audit: PathBuf::from("/var/log/kbf-host-install.log"),
            work: PathBuf::from("/var/db/kbf-host-install"),
            node_conf: kit.join("etc/av2_mac.conf"),
            daemon_plist: PathBuf::from("/Library/LaunchDaemons/com.komira.kbf-daemon.plist"),
            err_log: kit.join("log/kbf-daemon.err.log"),
            daemon_config: PathBuf::from("/usr/local/etc/kbf/kbf.toml"),
            gh: PathBuf::from("/usr/local/kbf-installer/trust/gh"),
            trusted_root: PathBuf::from("/usr/local/kbf-installer/trust/trusted_root.jsonl"),
            curl: PathBuf::from("/usr/bin/curl"),
            daemon_home: kit.join("work/daemon"),
            self_exe: PathBuf::from(SELF_EXE),
            trust: Trust::root(),
            kit_trust: Trust::root_below(&kit),
            kit,
            tuning: Tuning::production(),
            pace: Rc::new(Pace::real()),
        }
    }

    /// Paths under `dir`, trusting `trust` for everything, with waits of milliseconds and
    /// small limits, for tests.
    #[cfg(test)]
    pub fn for_test(dir: &Path, trust: Trust) -> Self {
        let kit = dir.join("kit");
        Self {
            allowlist: dir.join("host-install.toml"),
            audit: dir.join("audit.log"),
            work: dir.join("work"),
            node_conf: kit.join("etc/av2_mac.conf"),
            daemon_plist: dir.join("daemon.plist"),
            err_log: kit.join("log/kbf-daemon.err.log"),
            daemon_config: dir.join("kbf.toml"),
            gh: dir.join("trust/gh"),
            trusted_root: dir.join("trust/trusted_root.jsonl"),
            curl: dir.join("trust/curl"),
            daemon_home: kit.join("work/daemon"),
            self_exe: PathBuf::from(SELF_EXE),
            kit_trust: trust.clone(),
            trust,
            kit,
            tuning: Tuning {
                gone: Duration::from_millis(60),
                welcome: Duration::from_millis(120),
                poll: Duration::from_millis(2),
                tool: Duration::from_secs(5),
                fetch: Duration::from_secs(7),
                tarball_max: 64 * 1024,
                bundle_max: 4 * 1024,
                binary_max: 16 * 1024,
            },
            pace: Rc::new(crate::pace::fake::Fake::new().pace()),
        }
    }

    pub(crate) fn releases(&self) -> PathBuf {
        self.kit.join("releases")
    }

    pub(crate) fn release(&self, commit: &str) -> PathBuf {
        self.releases().join(commit)
    }

    pub(crate) fn current(&self) -> PathBuf {
        self.kit.join("current")
    }

    pub(crate) fn previous(&self) -> PathBuf {
        self.kit.join("previous")
    }

    pub(crate) fn status_file(&self) -> PathBuf {
        self.work.join("status")
    }

    pub(crate) fn lock_file(&self) -> PathBuf {
        self.work.join("lock")
    }

    /// The marker of a swap on trial, which the socket's switch writes (root's, 0600).
    pub(crate) fn pending_marker(&self) -> PathBuf {
        self.work.join("pending.json")
    }

    pub(crate) fn stage(&self) -> PathBuf {
        self.work.join("stage")
    }

    pub(crate) fn apply_plist(&self) -> PathBuf {
        self.work.join("apply.plist")
    }
}
