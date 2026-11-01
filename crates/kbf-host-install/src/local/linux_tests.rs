//! The Linux backend over a scratch root: the installer's layout, state and verify pipeline,
//! with `curl`, `gh`, `systemctl` and the binary's own report played by the test.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::ScratchHost;
use kbf_installer::CommitId;
use kbf_installer::activate;
use kbf_installer::fixtures::{
    COMMIT, FakeRunner, OTHER, SETTINGS_PATH, elf, file, macho, me, ran, tar_gz, trusted_host_on,
    version_json,
};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::Spawn;
use kbf_installer::state::{self, Locked, State};
use kbf_testkit::dir::TestDir;

use super::answer::{Answer, Code};
use super::backend::{Backend, Fault};
use super::core::Ab;
use super::linux::{Linux, Tools};
use super::request::LocalRequest;
use crate::host::Host;
use crate::plan::Wait;
use crate::request::{Channel, Unit, Version};
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

/// The backend of a rig, lent to the core, which owns the one it is given.
struct Borrowed<'a>(&'a Linux<ScratchHost>);

impl Backend for Borrowed<'_> {
    fn lock_file(&self) -> PathBuf {
        self.0.lock_file()
    }
    fn node_id(&self) -> Option<String> {
        self.0.node_id()
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        self.0.current()
    }
    fn pending(&self) -> Result<Option<super::backend::Pending>, Fault> {
        self.0.pending()
    }
    fn has(&self, commit: &str) -> bool {
        self.0.has(commit)
    }
    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        self.0.stage(version, channel)
    }
    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        self.0.switch(commit, window_secs, now)
    }
    fn confirm(&self, pending: &super::backend::Pending) -> Result<(), Fault> {
        self.0.confirm(pending)
    }
    fn revert(&self, pending: &super::backend::Pending) -> Result<(), Fault> {
        self.0.revert(pending)
    }
    fn kept(&self) -> Result<Vec<kbf_installer::retain::Kept>, Fault> {
        self.0.kept()
    }
    fn retain_file(&self) -> Option<PathBuf> {
        self.0.retain_file()
    }
    fn prune(&self, keep: usize) -> Result<String, Fault> {
        self.0.prune(keep)
    }
}

/// A file of the host's configuration: written, and readable by all but writable by its owner.
fn put(path: &Path, text: impl AsRef<str>) {
    fs::write(path, text.as_ref()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

/// What a build of `commit` prints for `--version --json`, with the protocol numbers this
/// installer understands.
fn what_kbf_says(commit: &str) -> String {
    version_json(commit, "release")
        .replace("\"protocol\":4", "\"protocol\":1")
        .replace("\"min_protocol\":3", "\"min_protocol\":1")
        .replace("\"state_schema\":7", "\"state_schema\":1")
}

/// The release that runs.
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn arch_name() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "x86_64"
    }
}

fn version() -> Version {
    Version::new(&format!("0.1.7+g{}-3", &COMMIT[..12])).unwrap()
}

fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

fn tarball_name(commit: &str) -> String {
    named_tarball("kbf", commit)
}

fn named_tarball(prefix: &str, commit: &str) -> String {
    format!("{prefix}-{commit}-linux-{}.tar.gz", arch_name())
}

fn bundle_name(commit: &str) -> String {
    format!("{}.sigstore.json", tarball_name(commit))
}

/// The binary a release holds: an ELF of this CPU.
fn binary(tail: &str) -> Vec<u8> {
    elf(if arch_name() == "arm64" { 183 } else { 62 }, tail)
}

/// What the network and the service manager do.
#[derive(Default)]
struct Net {
    /// The listing of the releases, one text per page.
    pages: Vec<String>,
    /// The files of each release, by tag and then by asset name.
    assets: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Things that go wrong, by name.
    broken: BTreeSet<&'static str>,
    /// Units that do not restart, units that do not restart the first time, and units that
    /// never become active.
    restart_fails: BTreeSet<String>,
    restart_fails_once: BTreeSet<String>,
    inactive: BTreeSet<String>,
}

/// The programs of the host-install side: `curl`, `gh release download`, `systemctl`.
struct Sim {
    net: Rc<RefCell<Net>>,
    calls: Rc<RefCell<Vec<Cmd>>>,
}

fn exec(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

impl Runner for Sim {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.calls.borrow_mut().push(cmd.clone());
        let mut net = self.net.borrow_mut();
        let name = Path::new(&cmd.program)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        match name.as_str() {
            "systemctl" => {
                let unit = cmd.args.last().unwrap();
                if net.broken.contains("no-systemctl") {
                    return Err(io::Error::other("cannot start systemctl"));
                }
                match cmd.args[0].as_str() {
                    "restart" if net.restart_fails_once.remove(unit) => exec(5, "", "failed once"),
                    "restart" if net.restart_fails.contains(unit) => exec(5, "", "failed"),
                    "is-active" if net.inactive.contains(unit) => exec(3, "inactive", ""),
                    _ => exec(0, "", ""),
                }
            }
            "curl" => {
                if net.broken.contains("list-fails") {
                    return exec(28, "", "timed out");
                }
                if net.broken.contains("list-limited") {
                    return exec(
                        0,
                        "HTTP/2 403 \r\nx-ratelimit-remaining: 0\r\n\r\n{\"message\":\"rate limit\"}",
                        "",
                    );
                }
                let page: usize = cmd
                    .args
                    .last()
                    .unwrap()
                    .rsplit("page=")
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                let body = net.pages.get(page - 1).map_or("[]", String::as_str);
                exec(
                    0,
                    &format!("HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{body}"),
                    "",
                )
            }
            "gh" => {
                if net.broken.contains("download-fails") {
                    return exec(1, "", "error connecting");
                }
                let value_of = |flag: &str| -> Vec<String> {
                    cmd.args
                        .windows(2)
                        .filter(|w| w[0] == flag)
                        .map(|w| w[1].clone())
                        .collect()
                };
                let dir = PathBuf::from(&value_of("--dir")[0]);
                let Some(files) = net.assets.get(&cmd.args[2]) else {
                    return exec(1, "", "release not found");
                };
                for asset in value_of("--pattern") {
                    let Some(bytes) = files.get(&asset) else {
                        return exec(1, "", "no assets match");
                    };
                    let path = dir.join(&asset);
                    let kind = if asset.ends_with(".json") {
                        "bundle"
                    } else if asset == "SHA256SUMS" {
                        "sums"
                    } else {
                        "tarball"
                    };
                    let broken =
                        |what: &str| net.broken.contains(format!("{kind}-{what}").as_str());
                    if broken("is-a-link") {
                        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
                    } else if broken("is-a-directory") {
                        fs::create_dir(&path).unwrap();
                    } else {
                        fs::write(&path, bytes).unwrap();
                        if broken("is-hard-linked") {
                            fs::hard_link(&path, dir.join(format!("{asset}.twin"))).unwrap();
                        }
                    }
                }
                exec(0, "", "")
            }
            other => panic!("unscripted program {other}"),
        }
    }
}

/// A Linux host on a scratch root: release A runs, a channel `stable` offers the release
/// named `COMMIT`.
struct Rig {
    dir: TestDir,
    layout: Layout,
    backend: Linux<ScratchHost>,
    net: Rc<RefCell<Net>>,
    calls: Rc<RefCell<Vec<Cmd>>>,
    installer: Rc<RefCell<Vec<Spawn>>>,
    /// The installer's runner: how `gh attestation verify` ends, and what the binary says.
    gh_exit: Rc<RefCell<i32>>,
    probe: Rc<RefCell<String>>,
    /// The executables of the host's processes, for a prune.
    running: Rc<RefCell<Vec<PathBuf>>>,
}

/// The host's processes, as a test sets them.
struct Running(Rc<RefCell<Vec<PathBuf>>>);

impl kbf_installer::helper::service::Procs for Running {
    fn exe_of(&self, _pid: u32) -> io::Result<PathBuf> {
        Err(io::ErrorKind::NotFound.into())
    }

    fn executables(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self.0.borrow().clone())
    }
}

impl Rig {
    fn new(name: &str) -> Self {
        let exec = ScratchHost::new(&format!("hi-linux-{name}"));
        for sub in [
            "run",
            "var/lib/kbf-installer",
            "var/lib/kbf-installer/releases",
        ] {
            fs::create_dir_all(exec.root.join(sub)).unwrap();
        }
        let layout = Layout::linux(Root::new(&exec.root).unwrap());
        // Release A, running.
        let a_dir = exec.at(&format!("/var/lib/kbf-installer/releases/{A}"));
        fs::create_dir_all(&a_dir).unwrap();
        fs::write(a_dir.join("kbf"), binary("a")).unwrap();
        Locked::acquire(&layout)
            .unwrap()
            .write(&State {
                current: Some(CommitId::new(A).unwrap()),
                ..State::default()
            })
            .unwrap();
        std::os::unix::fs::symlink(A, exec.at("/var/lib/kbf-installer/releases/current")).unwrap();

        let dir = TestDir::under(std::env::temp_dir(), &format!("hi-linux-host-{name}"));
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = fs::metadata(&*dir).unwrap().uid();
        let host = Host::for_test(&dir, Trust::owned_by(uid, &dir));
        put(
            &dir.join("releases.toml"),
            "[channels.stable]\nsource = \"github\"\n",
        );
        put(&host.node_config, "[node]\nid = \"st1\"\n");

        let mut trust = trusted_host_on(Platform::Linux);
        let who = me();
        trust.entries.insert(
            HostPath::new(SETTINGS_PATH).unwrap(),
            Entry::File {
                perms: Perms::new(0o644),
                owner: Owner { uid: 0, gid: 0 },
                bytes: format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", who.uid, who.gid).into_bytes(),
            },
        );

        let net = Rc::new(RefCell::new(Net::default()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let gh_exit = Rc::new(RefCell::new(0));
        let probe = Rc::new(RefCell::new(what_kbf_says(COMMIT)));
        let installer_runner = {
            let (gh_exit, probe) = (Rc::clone(&gh_exit), Rc::clone(&probe));
            FakeRunner::new(move |spawn| {
                if spawn.program.file_name().is_some_and(|n| n == "gh") {
                    Ok(ran(*gh_exit.borrow(), "", "no matching attestation"))
                } else {
                    Ok(ran(0, &probe.borrow(), ""))
                }
            })
        };
        let installer = installer_runner.log();
        let running = Rc::new(RefCell::new(Vec::new()));
        let backend = Linux {
            layout: layout.clone(),
            exec: RefCell::new(exec),
            trust: Box::new(trust),
            host,
            allowlist: dir.join("releases.toml"),
            units: vec![
                Unit::service("kbf-daemon.service").unwrap(),
                Unit::service("kbf-frontend@a.service").unwrap(),
            ],
            restart_wait: Wait {
                timeout: Duration::from_millis(40),
                interval: Duration::from_millis(1),
            },
            pace: crate::pace::fake::Fake::new().pace(),
            runner: Box::new(Sim {
                net: Rc::clone(&net),
                calls: Rc::clone(&calls),
            }),
            installer_runner: Box::new(installer_runner),
            tools: Tools {
                curl: dir.join("curl"),
                tool: Duration::from_secs(5),
                download: Duration::from_secs(5),
                tarball_max: 64 * 1024,
                image_max: 64 * 1024,
            },
            owner: Owner {
                uid: who.uid,
                gid: who.gid,
            },
            scratch_tag: None,
            procs: Box::new(Running(Rc::clone(&running))),
        };
        let rig = Self {
            dir,
            layout,
            backend,
            net,
            calls,
            installer,
            gh_exit,
            probe,
            running,
        };
        rig.offer(COMMIT, &tar_gz(&[file("kbf", &binary("b"))]));
        rig
    }

    /// The channel's GitHub offers release `commit` with this tarball.
    fn offer(&self, commit: &str, tar: &[u8]) {
        self.offer_as("kbf", commit, tar);
    }

    /// [`Rig::offer`] for a release whose files' names start with `prefix`.
    fn offer_as(&self, prefix: &str, commit: &str, tar: &[u8]) {
        let tag = format!("v-{}", &commit[..8]);
        let (tarball, bundle) = (
            named_tarball(prefix, commit),
            format!("{}.sigstore.json", named_tarball(prefix, commit)),
        );
        let assets = format!(
            "{{\"name\":\"{tarball}\",\"size\":{}}},{{\"name\":\"{bundle}\",\"size\":30}}",
            tar.len()
        );
        let release = format!("{{\"tag_name\":\"{tag}\",\"draft\":false,\"assets\":[{assets}]}}");
        let mut net = self.net.borrow_mut();
        net.pages = vec![format!("[{release}]")];
        let files = net.assets.entry(tag).or_default();
        files.insert(tarball, tar.to_vec());
        files.insert(bundle, b"{\"mediaType\":\"bundle\"}\n".to_vec());
    }

    fn root(&self) -> PathBuf {
        self.backend.exec.borrow().root.clone()
    }

    fn releases(&self) -> PathBuf {
        self.root().join("var/lib/kbf-installer/releases")
    }

    fn state(&self) -> State {
        state::read_unlocked(&self.layout).unwrap().unwrap()
    }

    fn link(&self, name: &str) -> Option<String> {
        fs::read_link(self.releases().join(name))
            .ok()
            .map(|t| t.display().to_string())
    }

    fn stage(&self) -> Result<String, Fault> {
        self.backend.stage(&version(), &stable())
    }

    fn systemctl(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.program.ends_with("systemctl"))
            .map(|c| c.args.join(" "))
            .collect()
    }

    /// Nothing of a stage is left: no release directory for `COMMIT`, no scratch, and the
    /// state is as it was.
    fn assert_nothing_installed(&self) {
        assert!(!self.releases().join(COMMIT).exists(), "a release was made");
        let work = self.root().join("var/lib/kbf-installer/work");
        let left: Vec<_> = fs::read_dir(&work)
            .map(|d| d.map(|e| e.unwrap().file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "scratch left behind: {left:?}");
        let state = self.state();
        assert_eq!(
            state.current.as_ref().map(|c| c.to_string()).as_deref(),
            Some(A)
        );
        assert!(state.pending_confirm.is_none());
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The scratch root holds an executable, which a coverage run would scan.
        let _ = fs::remove_dir_all(self.root());
    }
}

fn refused(code: Code) -> impl Fn(&Fault) -> bool {
    move |fault| fault.refused && fault.code == code
}

/// A switch writes the marker with its deadline into the installer's state in the same step as
/// the links move (`previous` takes the release that ran, `current` the new one), restarts each
/// configured unit and waits for it, and answers with the deadline.
#[test]
fn a_switch_puts_the_release_on_trial_and_restarts_the_units() {
    let rig = Rig::new("switch");
    rig.stage().unwrap();
    let deadline = rig.backend.switch(COMMIT, 600, 1_000).unwrap();
    assert_eq!(deadline, 1_600);

    let state = rig.state();
    assert_eq!(state.current.unwrap().to_string(), COMMIT);
    assert_eq!(state.previous.unwrap().to_string(), A);
    let pending = state.pending_confirm.unwrap();
    assert_eq!(pending.deadline_wall, Some(1_600));
    assert_eq!(pending.previous.unwrap().to_string(), A);
    assert_eq!(rig.link("current").as_deref(), Some(COMMIT));
    assert_eq!(rig.link("previous").as_deref(), Some(A));
    assert_eq!(
        rig.systemctl(),
        [
            "restart -- kbf-daemon.service",
            "is-active -- kbf-daemon.service",
            "restart -- kbf-frontend@a.service",
            "is-active -- kbf-frontend@a.service"
        ]
    );
    let pending = rig.backend.pending().unwrap().unwrap();
    assert_eq!(
        (
            pending.commit.as_str(),
            pending.from.as_str(),
            pending.deadline_unix
        ),
        (COMMIT, A, Some(1_600))
    );
    assert_eq!(rig.backend.current().unwrap().as_deref(), Some(COMMIT));
}

/// A unit that will not restart, or does not become active, puts the host back on the release
/// that ran at once, without waiting for the deadline: the links and the state are A's again,
/// the units are restarted on it, and the fault says the switch failed. When the way back fails
/// too the fault says so.
#[test]
fn a_release_that_does_not_come_up_is_put_back_at_once() {
    for (name, ruin) in [("restart", "restart"), ("inactive", "inactive")] {
        let rig = Rig::new(&format!("noup-{name}"));
        rig.stage().unwrap();
        {
            let mut net = rig.net.borrow_mut();
            let set = if ruin == "restart" {
                &mut net.restart_fails
            } else {
                &mut net.inactive
            };
            set.insert("kbf-frontend@a.service".to_owned());
        }
        // The first unit is fine; the second does not come up on either release, so the way
        // back fails too.
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert_eq!(fault.code, Code::RevertFailed, "{name}: {fault:?}");
        assert!(fault.detail.contains("kbf-frontend@a.service"), "{fault:?}");
        let state = rig.state();
        assert_eq!(state.current.unwrap().to_string(), A, "state is A's again");
        assert!(state.pending_confirm.is_none());
        assert_eq!(rig.link("current").as_deref(), Some(A));
    }
}

/// The installer's own refusals come through as failures: a window it does not take, a
/// release that is not laid out, and the release that already runs.
#[test]
fn the_installers_refusals_are_the_switchs_failures() {
    let rig = Rig::new("switch-refused");
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(fault.code, Code::SwitchFailed, "not laid out: {fault:?}");
    rig.stage().unwrap();
    let fault = rig.backend.switch(COMMIT, 5, 1_000).unwrap_err();
    assert_eq!(
        fault.code,
        Code::SwitchFailed,
        "a window under the least: {fault:?}"
    );
    let fault = rig.backend.switch(A, 600, 1_000).unwrap_err();
    assert!(refused(Code::NotStaged)(&fault), "{fault:?}");
    assert!(rig.systemctl().is_empty());
}

/// A confirm clears the marker and keeps the release; a revert puts the links and the state back
/// on the release that ran and restarts the units on it, needing nothing of the release on trial;
/// a revert when the units will not start says so, with the host back.
#[test]
fn a_confirm_keeps_the_release_and_a_revert_goes_back_and_restarts() {
    let rig = Rig::new("confirm-revert");
    rig.stage().unwrap();
    rig.backend.switch(COMMIT, 600, 1_000).unwrap();
    let pending = rig.backend.pending().unwrap().unwrap();
    // A confirm that names another swap is the installer's error, passed on, and keeps the trial.
    let other = super::backend::Pending {
        activation: "someone-else".into(),
        ..pending.clone()
    };
    assert_eq!(
        rig.backend.confirm(&other).unwrap_err().code,
        Code::StateUnavailable
    );
    assert!(rig.backend.pending().unwrap().is_some());
    rig.backend.confirm(&pending).unwrap();
    assert!(rig.backend.pending().unwrap().is_none());
    assert_eq!(rig.state().current.unwrap().to_string(), COMMIT);
    // Again: the installer says it was kept.
    rig.backend.confirm(&pending).unwrap();

    let rig = Rig::new("revert");
    rig.stage().unwrap();
    rig.backend.switch(COMMIT, 600, 1_000).unwrap();
    rig.calls.borrow_mut().clear();
    let pending = rig.backend.pending().unwrap().unwrap();
    rig.backend.revert(&pending).unwrap();
    assert_eq!(rig.state().current.unwrap().to_string(), A);
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(rig.link("current").as_deref(), Some(A));
    assert_eq!(
        rig.systemctl().len(),
        4,
        "both units restarted and waited for"
    );

    let rig = Rig::new("revert-unit");
    rig.stage().unwrap();
    rig.backend.switch(COMMIT, 600, 1_000).unwrap();
    rig.net
        .borrow_mut()
        .restart_fails
        .insert("kbf-daemon.service".to_owned());
    let pending = rig.backend.pending().unwrap().unwrap();
    let fault = rig.backend.revert(&pending).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed);
    assert!(
        fault.detail.contains("back on the release that ran"),
        "{fault:?}"
    );
    assert_eq!(
        rig.state().current.unwrap().to_string(),
        A,
        "the links are back"
    );

    let rig = Rig::new("revert-none");
    let nothing = super::backend::Pending {
        commit: COMMIT.into(),
        from: A.into(),
        deadline_unix: None,
        activation: String::new(),
    };
    let fault = rig.backend.revert(&nothing).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
}

/// A swap armed by another tool has no deadline of ours; the marker is passed on as it is.
#[test]
fn a_trial_armed_with_no_deadline_has_none() {
    let rig = Rig::new("foreign");
    rig.stage().unwrap();
    let mut exec = rig.backend.exec.borrow_mut();
    let order = activate::Order {
        commit: CommitId::new(COMMIT).unwrap(),
        order_id: "7-1".into(),
    };
    activate::activate(
        &mut *exec,
        &rig.layout,
        &order,
        activate::Arm::FromFirstStart { starts: 3 },
        1,
        &mut Vec::new(),
    )
    .unwrap();
    drop(exec);
    let pending = rig.backend.pending().unwrap().unwrap();
    assert_eq!(pending.deadline_unix, None);
    assert_eq!(pending.activation, "7-1");
}

/// What the host reports about itself: its node id from its configuration, the lock it shares
/// with the ssh path, whether a release is laid out, and a host with no state of the installer.
#[test]
fn the_host_reports_its_node_its_lock_and_its_releases() {
    let rig = Rig::new("report");
    assert_eq!(rig.backend.node_id().as_deref(), Some("st1"));
    assert_eq!(rig.backend.lock_file(), rig.dir.join("install.lock"));
    assert!(rig.backend.has(A));
    assert!(!rig.backend.has(COMMIT));
    assert!(!rig.backend.has("not a commit"));

    let bare = Rig::new("bare");
    fs::remove_file(bare.layout.join(&bare.layout.state_file())).unwrap();
    fs::remove_file(bare.releases().join("current")).unwrap();
    assert_eq!(bare.backend.current().unwrap(), None);
    assert_eq!(bare.backend.pending().unwrap(), None);
}

/// A state file the installer cannot read is the host's records being unavailable.
#[test]
fn a_damaged_installer_state_is_unavailable() {
    let rig = Rig::new("damaged");
    let file = rig.layout.join(&rig.layout.state_file());
    fs::write(&file, "not json").unwrap();
    fs::write(rig.layout.join(&rig.layout.state_backup()), "nor this").unwrap();
    assert_eq!(
        rig.backend.current().unwrap_err().code,
        Code::StateUnavailable
    );
    assert_eq!(
        rig.backend.pending().unwrap_err().code,
        Code::StateUnavailable
    );
}

/// The whole update through the core on this backend: a release that is never confirmed is put
/// back by the tick at its deadline (the links, the state and the units), with no request, and
/// the host then tells the update failed by timeout; one that is confirmed stays.
#[test]
fn an_unconfirmed_release_is_reverted_by_the_tick_and_a_confirmed_one_stays() {
    let rig = Rig::new("e2e");
    let now = Rc::new(std::cell::Cell::new(1_000_u64));
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let clock = Rc::clone(&now);
    let ab = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(uid, &rig.dir),
        600,
        Box::new(move || clock.get()),
    );
    let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
    let v = version();
    assert!(matches!(
        ask(&format!("install kbf={} from stable", v.as_str())),
        Answer::Staged { current: false, .. }
    ));
    assert!(matches!(
        ask(&format!("switch kbf={}", v.as_str())),
        Answer::Switched {
            deadline_unix: 1_600,
            ..
        }
    ));
    now.set(1_599);
    assert_eq!(ab.tick(), None);
    now.set(1_600);
    assert!(matches!(ab.tick(), Some(Answer::Reverted { .. })));
    let state = state::read_unlocked(&rig.layout).unwrap().unwrap();
    assert_eq!(state.current.unwrap().to_string(), A);
    let Answer::Queried(view) = ask("query") else {
        panic!("not a query")
    };
    assert_eq!(view.last.unwrap().reason, Some("timeout"));

    // Again, and this time confirmed.
    assert!(matches!(
        ask(&format!("install kbf={} from stable", v.as_str())),
        Answer::Staged { .. }
    ));
    assert!(matches!(
        ask(&format!("switch kbf={}", v.as_str())),
        Answer::Switched { .. }
    ));
    assert!(matches!(
        ask(&format!("confirm kbf={}", v.as_str())),
        Answer::Confirmed { .. }
    ));
    now.set(1_000_000);
    assert_eq!(ab.tick(), None);
    let state = state::read_unlocked(&rig.layout).unwrap().unwrap();
    assert_eq!(state.current.unwrap().to_string(), COMMIT);
}

/// A unit that fails once and works again, as it would on the release that ran, lets the host
/// go back: the fault says the switch failed, and the host is on A, units up.
#[test]
fn a_unit_that_fails_on_b_and_not_on_a_leaves_the_host_on_a() {
    let rig = Rig::new("noup-once");
    rig.stage().unwrap();
    rig.net
        .borrow_mut()
        .restart_fails_once
        .insert("kbf-daemon.service".to_owned());
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(
        (fault.code, fault.refused),
        (Code::SwitchFailed, false),
        "{fault:?}"
    );
    assert!(fault.detail.contains("failed once"), "{fault:?}");
    assert_eq!(rig.state().current.unwrap().to_string(), A);
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(rig.link("current").as_deref(), Some(A));
}

/// A `systemctl` that cannot be run fails the switch, and the way back, which needs it too.
#[test]
fn without_systemctl_a_switch_cannot_go_back_either() {
    let rig = Rig::new("nosystemctl");
    rig.stage().unwrap();
    rig.net.borrow_mut().broken.insert("no-systemctl");
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
    assert!(fault.detail.contains("cannot start systemctl"), "{fault:?}");
    assert_eq!(
        rig.state().current.unwrap().to_string(),
        A,
        "the state is back all the same"
    );
}

/// A release directory whose `kbf` is not a file is not a release that is laid out.
#[test]
fn a_release_directory_without_a_binary_file_is_not_laid_out() {
    let rig = Rig::new("hollow");
    let dir = rig.releases().join(COMMIT);
    fs::create_dir_all(dir.join("kbf")).unwrap();
    assert!(!rig.backend.has(COMMIT));
    fs::remove_dir(dir.join("kbf")).unwrap();
    fs::write(dir.join("kbf"), binary("b")).unwrap();
    assert!(rig.backend.has(COMMIT));
}

/// What the program starts with on a Linux host: the layout of the installer on the real root,
/// the channels file, the programs of a stage, and no lease waited for.
#[test]
fn the_production_host_is_the_installers_layout_and_the_programs_of_a_stage() {
    let host = super::linux::production(vec![Unit::service("kbf-daemon.service").unwrap()]);
    assert_eq!(
        host.allowlist,
        Path::new("/etc/kbf/host-install-releases.toml")
    );
    assert_eq!(
        host.layout.join(&host.layout.releases()),
        Path::new("/var/lib/kbf-installer/releases")
    );
    assert_eq!(host.units.len(), 1);
    assert_eq!(host.owner, Owner { uid: 0, gid: 0 });
    let tools = Tools::production();
    assert_eq!(tools.curl, Path::new("/usr/bin/curl"));
    assert_eq!(tools.tarball_max, 512 * 1024 * 1024);
    assert_eq!(host.tools.download, tools.download);
    use kbf_installer::helper::leases::LeaseSource;
    assert_eq!(super::linux::NoLeases.live(), Ok(0));
}

#[path = "linux_image_tests.rs"]
mod image;

#[path = "linux_retain_tests.rs"]
mod retain;

#[path = "linux_stage_tests.rs"]
mod stage;
