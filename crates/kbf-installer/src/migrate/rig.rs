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
use crate::state::{Locked, State};
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

    pub fn binary(&self) -> &'static str {
        self.roots().binary()
    }

    pub fn new_state_dir(&self) -> PathBuf {
        self.at(self.roots().new_state.as_str().trim_start_matches('/'))
    }

    pub fn new_tree(&self) -> PathBuf {
        self.at(self.roots().new_tree.as_str().trim_start_matches('/'))
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
        self.write_old_state(&State {
            current: Some(CommitId::new(A).expect("commit")),
            previous: Some(CommitId::new(B).expect("commit")),
            ..State::default()
        });
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

    pub fn write_old_state(&self, state: &State) {
        Locked::acquire(&self.layout)
            .expect("lock")
            .write(state)
            .expect("state");
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
                    out.insert(
                        name,
                        fs::read_to_string(&path).unwrap_or_else(|_| "<binary>".to_owned()),
                    );
                }
            }
        }
        let base = self.at(path);
        let mut out = BTreeMap::new();
        walk(&base, &base, &mut out);
        out
    }

    pub fn json(&self, path: &Path) -> serde_json::Value {
        serde_json::from_slice(
            &fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        )
        .expect("json")
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
