//! A scratch host with the old layout, the services played by a recorder, and the
//! programs the migration runs played by a fake.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use kbf_host::Root;
use kbf_host::testkit::{ScratchHost, reply};

use super::floor::{FrontendFloor, NoFrontend, Seed, Source};
use super::*;
use crate::fixtures::{FakeRunner, ran, version_json};
use crate::helper::memory::Memory;
use crate::layout::Platform;
use crate::run::Spawn;
use crate::testkit::{A, B, C};

/// The umask a host's root has. Every test of this module makes directories and files that
/// the rules judge, so it is set once for the test binary (a developer's own is often 002,
/// which makes group-writable directories the rules refuse).
pub fn root_umask() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    });
}

pub fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn unit(name: &str) -> Unit {
    Unit::new(name).expect("a unit name")
}

pub const OLD_UNIT: &str = "[Service]\nExecStart=/usr/local/kbf/current/kbf daemon\n";
pub const HELPER_UNIT: &str =
    "[Service]\nExecStart=/usr/bin/kbf-installer run --unit kbf-daemon.service\n";
pub const PLIST: &str = "<dict>\n<key>ProgramArguments</key>\n<array>\n<string>/usr/local/kbf/current/kbf-daemon</string>\n</array>\n</dict>\n";

type Verify = Box<dyn Fn(&CommitId, &Version) -> Result<(), String>>;

/// A verifier a test plays.
pub struct FnVerifier(pub Verify);

impl Verifier for FnVerifier {
    fn install_verified(&self, claimed: &CommitId, probed: &Version) -> Result<(), String> {
        (self.0)(claimed, probed)
    }
}

pub struct Rig {
    pub host: ScratchHost,
    pub layout: Layout,
    pub platform: Platform,
    /// The units that are running, by file name.
    pub active: Rc<RefCell<BTreeSet<String>>>,
    /// Each command that changed a service, with whether `releases/current` existed when
    /// it ran.
    pub events: Rc<RefCell<Vec<String>>>,
    /// Make the next `start` fail.
    pub fail_start: Rc<Cell<bool>>,
}

pub fn put(path: &Path, bytes: &str, mode: u32) {
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, bytes).expect("file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("mode");
}

impl Rig {
    pub fn linux(name: &str) -> Rig {
        Rig::new(name, Platform::Linux)
    }

    pub fn mac(name: &str) -> Rig {
        Rig::new(name, Platform::MacOs)
    }

    fn new(name: &str, platform: Platform) -> Rig {
        let rig = Rig::bare(name, platform);
        rig.build_old();
        rig
    }

    /// A scratch host with the services played and nothing on it but `run/`.
    fn bare(name: &str, platform: Platform) -> Rig {
        root_umask();
        let mut host = ScratchHost::new(&format!("migrate-{name}"));
        host.euid = me();
        let root = host.root.clone();
        let layout = Layout::new(Root::new(&root).expect("absolute"), platform);
        let active = Rc::new(RefCell::new(BTreeSet::new()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let fail_start = Rc::new(Cell::new(false));
        host.answer = Box::new({
            let (active, events, fail_start, root) = (
                active.clone(),
                events.clone(),
                fail_start.clone(),
                root.clone(),
            );
            move |cmd| {
                let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
                let current = root.join(match platform {
                    Platform::Linux => "var/lib/kbf-installer/releases/current",
                    Platform::MacOs => "usr/local/kbf-installer/releases/current",
                });
                let links = current.symlink_metadata().is_ok();
                let record =
                    |what: String| events.borrow_mut().push(format!("{what} links={links}"));
                // systemctl names the unit file; launchctl a domain target or a plist path.
                let (verb, unit): (&str, String) = match (cmd.program.as_str(), args.as_slice()) {
                    ("/usr/bin/systemctl", ["daemon-reload"]) => ("daemon-reload", String::new()),
                    ("/usr/bin/systemctl", ["is-active", "--quiet", "--", u]) => {
                        ("is-active", (*u).to_owned())
                    }
                    ("/usr/bin/systemctl", [verb, "--", u]) => (verb, (*u).to_owned()),
                    ("/bin/launchctl", ["bootstrap", "system", plist]) => (
                        "start",
                        plist.rsplit('/').next().expect("a name").to_owned(),
                    ),
                    ("/bin/launchctl", [verb, domain]) => (
                        match *verb {
                            "print" => "is-active",
                            "bootout" => "stop",
                            other => other,
                        },
                        format!("{}.plist", domain.trim_start_matches("system/")),
                    ),
                    other => panic!("a command the rig does not know: {other:?}"),
                };
                match verb {
                    "is-active" => Ok(reply(
                        i32::from(!active.borrow().contains(&unit)) * 3,
                        "",
                        "",
                    )),
                    "stop" => {
                        active.borrow_mut().remove(&unit);
                        record(format!("stop {unit}"));
                        Ok(reply(0, "", ""))
                    }
                    "start" if fail_start.get() => Err(io::Error::other("planted: start failed")),
                    "start" => {
                        active.borrow_mut().insert(unit.clone());
                        record(format!("start {unit}"));
                        Ok(reply(0, "", ""))
                    }
                    other => {
                        record(format!("{other} {unit}").trim_end().to_owned());
                        Ok(reply(0, "", ""))
                    }
                }
            }
        });
        for dir in ["run", "var/run"] {
            fs::create_dir_all(host.root.join(dir)).expect("dir");
        }
        Rig {
            host,
            layout,
            platform,
            active,
            events,
            fail_start,
        }
    }

    pub fn at(&self, path: &str) -> PathBuf {
        self.host.root.join(path)
    }

    pub fn roots(&self) -> Roots {
        Roots::new(self.platform)
    }

    fn under(&self, path: &kbf_host::HostPath) -> PathBuf {
        self.at(path.as_str().trim_start_matches('/'))
    }

    pub fn binary(&self) -> &'static str {
        self.roots().binary()
    }

    pub fn new_state_dir(&self) -> PathBuf {
        self.under(&self.layout.state_dir())
    }

    pub fn new_tree(&self) -> PathBuf {
        self.under(&self.layout.install_root())
    }

    /// The old layout: releases `A` (current) and `B` (previous), their links, the state
    /// the old tools left, the old helper's memory, and the units.
    fn build_old(&self) {
        let roots = self.roots();
        let host = |p: &kbf_host::HostPath| self.at(p.as_str().trim_start_matches('/'));
        let old = host(&roots.old_tree);
        for (commit, body) in [(A, "binary of A"), (B, "binary of B")] {
            let dir = old.join("releases").join(commit);
            put(&dir.join(self.binary()), body, 0o755);
            put(&dir.join("manifest.json"), "{}", 0o644);
            put(&dir.join("units/kbf-daemon.service"), OLD_UNIT, 0o644);
        }
        std::os::unix::fs::symlink(format!("releases/{A}"), old.join("current")).expect("link");
        std::os::unix::fs::symlink(format!("releases/{B}"), old.join("previous")).expect("link");
        fs::create_dir_all(host(&roots.old_state)).expect("state dir");
        self.write_old_state(Some(A), Some(B), None, None);
        self.write_memory(&Memory {
            acked_epoch: 3,
            acked_order_id: 42,
            ..Memory::default()
        });
        let units = host(&roots.unit_dir);
        match self.platform {
            Platform::Linux => {
                put(&units.join("kbf-daemon.service"), OLD_UNIT, 0o644);
                put(&units.join("kbf-helper.service"), HELPER_UNIT, 0o644);
                self.active
                    .borrow_mut()
                    .insert("kbf-daemon.service".to_owned());
                self.active
                    .borrow_mut()
                    .insert("kbf-helper.service".to_owned());
            }
            Platform::MacOs => {
                put(&units.join("com.komira.kbf-daemon.plist"), PLIST, 0o644);
                put(&units.join("com.komira.kbf-helper.plist"), PLIST, 0o644);
                self.active
                    .borrow_mut()
                    .insert("com.komira.kbf-daemon.plist".to_owned());
                self.active
                    .borrow_mut()
                    .insert("com.komira.kbf-helper.plist".to_owned());
            }
        }
    }

    /// The old tools' `state.json` (schema 1): the releases the links name, a release staged,
    /// and the commit of a swap on trial.
    pub fn write_old_state(
        &self,
        current: Option<&str>,
        previous: Option<&str>,
        staged: Option<&str>,
        pending: Option<&str>,
    ) {
        let dir = self.under(&self.roots().old_state);
        let pending = pending.map(|commit| {
            serde_json::json!({
                "commit": commit, "previous": previous, "deadline": null,
                "order_id": null, "starts_left": 3,
            })
        });
        let state = serde_json::json!({
            "schema": 1, "current": current, "previous": previous, "staged": staged,
            "pending_confirm": pending, "order_floor": 0,
        });
        put(&dir.join("state.json"), &state.to_string(), 0o600);
    }

    pub fn write_memory(&self, memory: &Memory) {
        let dir = self.at(self.roots().old_state.as_str().trim_start_matches('/'));
        put(
            &dir.join("helper.json"),
            &serde_json::to_string(memory).expect("json"),
            0o600,
        );
    }

    pub fn daemon(&self) -> Unit {
        match self.platform {
            Platform::Linux => unit("kbf-daemon.service"),
            Platform::MacOs => unit("com.komira.kbf-daemon.plist"),
        }
    }

    pub fn helper(&self) -> Unit {
        match self.platform {
            Platform::Linux => unit("kbf-helper.service"),
            Platform::MacOs => unit("com.komira.kbf-helper.plist"),
        }
    }

    pub fn migration(&self) -> Migration {
        Migration {
            layout: self.layout.clone(),
            roots: self.roots(),
            run_as: RunAs {
                uid: 65534,
                gid: 65534,
            },
            owner_uid: me(),
            units: vec![self.daemon()],
            helper_unit: Some(self.helper()),
            installer_unit: Some(match self.platform {
                Platform::Linux => unit("kbf-installer.service"),
                Platform::MacOs => unit("com.komira.kbf-installer.plist"),
            }),
            seed: Seed {
                source: Source::Url,
                min_generation: Some(17),
            },
        }
    }

    /// Every file under `path`, by relative name, with its content, a link's target, or `dir`.
    pub fn snapshot(&self, path: &str) -> BTreeMap<String, String> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let path = entry.expect("entry").path();
                let name = path
                    .strip_prefix(base)
                    .expect("below")
                    .display()
                    .to_string();
                let meta = path.symlink_metadata().expect("meta");
                if meta.is_symlink() {
                    out.insert(
                        name,
                        format!("-> {}", fs::read_link(&path).expect("link").display()),
                    );
                } else if meta.is_dir() {
                    out.insert(name, "dir".to_owned());
                    walk(base, &path, out);
                } else {
                    let bytes = fs::read(&path).unwrap_or_default();
                    out.insert(name, String::from_utf8_lossy(&bytes).into_owned());
                }
            }
        }
        let base = self.at(path);
        let mut out = BTreeMap::new();
        walk(&base, &base, &mut out);
        out
    }

    pub fn json(&self, path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).expect("a file")).expect("json")
    }
}

/// A runner that answers `--version --json` for the build in the directory the program is
/// in, as `commit`'s release build, and records every spawn.
pub fn runner() -> FakeRunner {
    FakeRunner::new(|spawn: &Spawn| {
        let commit = spawn
            .program
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .expect("a release directory")
            .to_owned();
        Ok(ran(0, &version_json(&commit, "release"), ""))
    })
}

pub fn run_migrate(
    rig: &mut Rig,
    verifier: &dyn Verifier,
    mode: Mode,
) -> Result<Report, MigrateError> {
    run_with(rig, &runner(), verifier, &NoFrontend, mode)
}

pub fn run_with(
    rig: &mut Rig,
    runner: &dyn Runner,
    verifier: &dyn Verifier,
    frontend: &dyn FrontendFloor,
    mode: Mode,
) -> Result<Report, MigrateError> {
    let m = rig.migration();
    migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner,
            verifier,
            frontend,
        },
        mode,
    )
}

pub fn commit(id: &str) -> CommitId {
    CommitId::new(id).expect("a commit")
}

pub const OTHER: &str = C;

// The test farm's hosts, from the files under `testdata/testfarm/`: the units are what
// `kbf-deploy linux render-units --mode` renders with `KBF_BIN_DIR=/usr/local/kbf/current`
// (a test in kbf-deploy keeps them equal to the kit's output), the state and the helper's
// memory are what the dev installer and the helper of `feature/pull-updates` wrote.

pub const FARM_X: &str = "6ed9c1a07f5b4e2d8c3a91f04b7d65e2a8c1f937";
pub const FARM_Y: &str = "b1e4a7d2c9f0385e6a4d1b7c2f9e0834a5d6c1b2";
pub const FARM_DAEMON_UNIT: &str =
    include_str!("../../testdata/testfarm/linux/etc/systemd/system/kbf-daemon.service");
pub const FARM_SD_UNIT: &str =
    include_str!("../../testdata/testfarm/linux/etc/systemd/system/kbf-storage-and-daemon.service");
pub const FARM_HELPER_UNIT: &str =
    include_str!("../../testdata/testfarm/linux/etc/systemd/system/kbf-helper.service");
pub const FARM_STATE: &str =
    include_str!("../../testdata/testfarm/linux/var/lib/kbf-deploy/state.json");
pub const FARM_HELPER_JSON: &str =
    include_str!("../../testdata/testfarm/linux/var/lib/kbf-deploy/helper.json");
pub const FARM_PLIST: &str =
    include_str!("../../testdata/testfarm/macos/org.example.kbf-daemon.plist");

/// What the farm's release `commit` holds as its binary in these tests: bytes that name the
/// build (the version probe is played by the rig's runner, which answers by directory).
pub fn farm_binary(commit: &str) -> String {
    format!("test farm kbf {commit}\n")
}

impl Rig {
    /// A Linux test-farm host: releases `FARM_X` (current) and `FARM_Y` (previous) as
    /// `kbf-deploy host install` leaves them (the binary and `units/`), the state and the
    /// helper's memory as the deployed builds wrote them, and the daemon, the
    /// storage-and-daemon and the helper units, all running.
    pub fn testfarm_linux(name: &str) -> Rig {
        let rig = Rig::bare(&format!("testfarm-linux-{name}"), Platform::Linux);
        rig.farm_releases("kbf");
        for (name, text) in [
            ("kbf-daemon.service", FARM_DAEMON_UNIT),
            ("kbf-storage-and-daemon.service", FARM_SD_UNIT),
            ("kbf-helper.service", FARM_HELPER_UNIT),
        ] {
            put(&rig.at("etc/systemd/system").join(name), text, 0o644);
            rig.active.borrow_mut().insert(name.to_owned());
        }
        put(&rig.at("var/lib/kbf-deploy/state.json"), FARM_STATE, 0o600);
        put(
            &rig.at("var/lib/kbf-deploy/helper.json"),
            FARM_HELPER_JSON,
            0o600,
        );
        rig
    }

    /// A macOS test-farm host: the same releases with `kbf-daemon`, and the daemon's
    /// LaunchDaemon plist as `kbf-mac-provision` renders it.
    pub fn testfarm_mac() -> Rig {
        let rig = Rig::bare("testfarm-mac", Platform::MacOs);
        rig.farm_releases("kbf-daemon");
        put(
            &rig.at("Library/LaunchDaemons/org.example.kbf-daemon.plist"),
            FARM_PLIST,
            0o644,
        );
        rig.active
            .borrow_mut()
            .insert("org.example.kbf-daemon.plist".to_owned());
        put(&rig.at("var/db/kbf-deploy/state.json"), FARM_STATE, 0o600);
        put(
            &rig.at("var/db/kbf-deploy/helper.json"),
            FARM_HELPER_JSON,
            0o600,
        );
        rig
    }

    fn farm_releases(&self, binary: &str) {
        let old = self.at("usr/local/kbf");
        for commit in [FARM_X, FARM_Y] {
            let dir = old.join("releases").join(commit);
            put(&dir.join(binary), &farm_binary(commit), 0o755);
            if binary == "kbf" {
                put(
                    &dir.join("units/kbf-daemon.service"),
                    FARM_DAEMON_UNIT,
                    0o644,
                );
            }
        }
        std::os::unix::fs::symlink(format!("releases/{FARM_X}"), old.join("current"))
            .expect("link");
        std::os::unix::fs::symlink(format!("releases/{FARM_Y}"), old.join("previous"))
            .expect("link");
    }
}
