//! The delegated subtree's steps against a fake cgroup filesystem that keeps the
//! kernel's rules the steps depend on:
//!
//! - a cgroup's `cgroup.controllers` is its parent's `cgroup.subtree_control` (the
//!   delegated cgroup's is what the test says systemd gave it);
//! - enabling a controller a cgroup does not offer is ENOENT;
//! - enabling controllers in a non-root cgroup that holds a process is EBUSY, and so is
//!   moving a process into a cgroup that enables controllers (no internal processes);
//! - a process is in one cgroup: moving it removes it from the one it was in, and
//!   moving a process that has ended is ESRCH;
//! - `memory.max` and `memory.min` exist only where the parent enables `memory`, and
//!   not at the root;
//!   `cpu.max` only where the parent enables `cpu`, and not at the root;
//!   `cpuset.cpus.effective` only where the parent enables `cpuset`, and at the root;
//! - making a cgroup in one this process may not write is EACCES.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

#[derive(Default)]
struct Node {
    /// What the cgroup offers when not its parent's subtree (the root, and the cgroup
    /// systemd delegated).
    offered: Option<String>,
    enabled: Vec<String>,
    procs: Vec<u32>,
    files: BTreeMap<String, String>,
    read_only: bool,
}

#[derive(Default)]
struct Fake {
    nodes: RefCell<BTreeMap<String, Node>>,
    /// Every change, in order: `mkdir <cg>`, `<cg>/<file> <- <value>`.
    log: RefCell<Vec<String>>,
    /// Processes that have ended.
    ended: Vec<u32>,
    /// A process that appears in this cgroup on the first attempt to enable controllers
    /// in it (forked between the listing and the write).
    late: RefCell<Option<(String, u32)>>,
    /// The mount has no cgroup v2 root (`cgroup.controllers`).
    v1_mount: bool,
    /// Reads and writes of `<cg>/<file>` that fail with EACCES, as a file the
    /// daemon's user may not open would.
    denied: Vec<String>,
    /// Whether [`Fake::late`] comes back after each move: a cgroup that never empties.
    respawn: bool,
    /// An exiting thread: the cgroup stays populated while `cgroup.procs` lists nothing
    /// (the kernel skips a task that has begun exit when it moves a process, and the
    /// task leaves its cgroup only at the end of exit). Enabling controllers in the
    /// cgroup is EBUSY this many more times, then works.
    exiting: RefCell<Option<(String, u32)>>,
    /// Every pause asked for; none is waited.
    pauses: RefCell<Vec<Duration>>,
}

fn parent(cgroup: &str) -> &str {
    match cgroup.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((p, _)) => p,
    }
}

fn errno(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

impl Fake {
    /// A host whose root offers `cpuset cpu io memory pids`, with `delegated` made
    /// (its ancestors too) and offering `offered`, holding processes `procs`.
    fn host(delegated: &str, offered: &str, procs: &[u32]) -> Self {
        let fake = Self::default();
        {
            let mut nodes = fake.nodes.borrow_mut();
            nodes.insert(
                "/".into(),
                Node {
                    offered: Some("cpuset cpu io memory pids".into()),
                    enabled: vec![
                        "cpuset".into(),
                        "cpu".into(),
                        "memory".into(),
                        "pids".into(),
                    ],
                    ..Node::default()
                },
            );
            let mut at = String::new();
            for part in delegated.trim_start_matches('/').split('/') {
                at = format!("{at}/{part}");
                nodes.insert(at.clone(), Node::default());
            }
            let node = nodes.get_mut(delegated).expect("made");
            node.offered = Some(offered.into());
            node.procs = procs.to_vec();
        }
        fake
    }

    fn controllers(&self, cgroup: &str) -> Vec<String> {
        let nodes = self.nodes.borrow();
        let node = &nodes[cgroup];
        match &node.offered {
            Some(o) => o.split_whitespace().map(str::to_owned).collect(),
            None => nodes[parent(cgroup)].enabled.clone(),
        }
    }

    fn exists(&self, cgroup: &str) -> bool {
        self.nodes.borrow().contains_key(cgroup)
    }

    fn file_shown(&self, cgroup: &str, file: &str) -> bool {
        match file {
            "memory.max" | "memory.min" => {
                cgroup != "/" && self.controllers(cgroup).iter().any(|c| c == "memory")
            }
            "cpu.max" => cgroup != "/" && self.controllers(cgroup).iter().any(|c| c == "cpu"),
            "cpuset.cpus.effective" => {
                cgroup == "/" || self.controllers(cgroup).iter().any(|c| c == "cpuset")
            }
            _ => false,
        }
    }

    fn set(&self, cgroup: &str, file: &str, value: &str) {
        let mut nodes = self.nodes.borrow_mut();
        let node = nodes.get_mut(cgroup).expect("cgroup");
        node.files.insert(file.into(), value.into());
    }

    fn procs(&self, cgroup: &str) -> Vec<u32> {
        self.nodes.borrow()[cgroup].procs.clone()
    }

    fn enabled(&self, cgroup: &str) -> Vec<String> {
        self.nodes.borrow()[cgroup].enabled.clone()
    }

    fn check_denied(&self, cgroup: &str, file: &str) -> io::Result<()> {
        if self.denied.iter().any(|d| *d == format!("{cgroup}/{file}")) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl CgroupFs for Fake {
    fn read(&self, cgroup: &str, file: &str) -> io::Result<String> {
        self.check_denied(cgroup, file)?;
        if !self.exists(cgroup) {
            return Err(io::ErrorKind::NotFound.into());
        }
        if self.v1_mount && cgroup == "/" && file == "cgroup.controllers" {
            return Err(io::ErrorKind::NotFound.into());
        }
        let nodes = self.nodes.borrow();
        let node = &nodes[cgroup];
        match file {
            "cgroup.controllers" => {
                drop(nodes);
                Ok(format!("{}\n", self.controllers(cgroup).join(" ")))
            }
            "cgroup.subtree_control" => Ok(format!("{}\n", node.enabled.join(" "))),
            "cgroup.procs" => Ok(node.procs.iter().map(|p| format!("{p}\n")).collect()),
            _ => {
                let value = node.files.get(file).cloned();
                drop(nodes);
                if !self.file_shown(cgroup, file) {
                    return Err(io::ErrorKind::NotFound.into());
                }
                Ok(value.unwrap_or_else(|| match file {
                    "memory.max" => "max\n".into(),
                    "memory.min" => "0\n".into(),
                    "cpu.max" => "max 100000\n".into(),
                    _ => String::new(),
                }))
            }
        }
    }

    fn write(&self, cgroup: &str, file: &str, value: &str) -> io::Result<()> {
        self.check_denied(cgroup, file)?;
        if !self.exists(cgroup) {
            return Err(io::ErrorKind::NotFound.into());
        }
        match file {
            "cgroup.procs" => {
                let pid: u32 = value.trim().parse().expect("a pid");
                if self.ended.contains(&pid) {
                    // It ends now, after the listing that named it.
                    for node in self.nodes.borrow_mut().values_mut() {
                        node.procs.retain(|p| *p != pid);
                    }
                    return Err(errno(rustix::io::Errno::SRCH));
                }
                if cgroup != "/" && !self.enabled(cgroup).is_empty() {
                    return Err(errno(rustix::io::Errno::BUSY));
                }
                let mut nodes = self.nodes.borrow_mut();
                for node in nodes.values_mut() {
                    node.procs.retain(|p| *p != pid);
                }
                nodes.get_mut(cgroup).expect("cgroup").procs.push(pid);
            }
            "cgroup.subtree_control" => {
                let respawn = self.respawn;
                let late = self.late.borrow_mut().take_if(|(at, _)| at == cgroup);
                if respawn {
                    self.late.borrow_mut().clone_from(&late);
                }
                if let Some((at, pid)) = late {
                    self.nodes
                        .borrow_mut()
                        .get_mut(&at)
                        .expect("cgroup")
                        .procs
                        .push(pid);
                }
                let exiting = match self.exiting.borrow_mut().as_mut() {
                    Some((at, left)) if at == cgroup && *left > 0 => {
                        *left -= 1;
                        true
                    }
                    _ => false,
                };
                if exiting || (cgroup != "/" && !self.procs(cgroup).is_empty()) {
                    return Err(errno(rustix::io::Errno::BUSY));
                }
                let offered = self.controllers(cgroup);
                let mut nodes = self.nodes.borrow_mut();
                let node = nodes.get_mut(cgroup).expect("cgroup");
                for word in value.split_whitespace() {
                    let c = word.strip_prefix('+').expect("only enabling");
                    if !offered.iter().any(|o| o == c) {
                        return Err(errno(rustix::io::Errno::NOENT));
                    }
                    if !node.enabled.iter().any(|e| e == c) {
                        node.enabled.push(c.into());
                    }
                }
            }
            _ => {
                if !self.file_shown(cgroup, file) {
                    return Err(io::ErrorKind::NotFound.into());
                }
                self.set(cgroup, file, value);
            }
        }
        self.log
            .borrow_mut()
            .push(format!("{cgroup}/{file} <- {value}"));
        Ok(())
    }

    fn mkdir(&self, cgroup: &str) -> io::Result<()> {
        if self.exists(cgroup) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let up = parent(cgroup);
        if !self.exists(up) {
            return Err(io::ErrorKind::NotFound.into());
        }
        self.writable(up)?;
        self.nodes
            .borrow_mut()
            .insert(cgroup.into(), Node::default());
        self.log.borrow_mut().push(format!("mkdir {cgroup}"));
        Ok(())
    }

    fn writable(&self, cgroup: &str) -> io::Result<()> {
        if self.nodes.borrow()[cgroup].read_only {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }

    fn pause(&self, how_long: Duration) {
        self.pauses.borrow_mut().push(how_long);
    }

    fn path(&self, cgroup: &str, file: &str) -> PathBuf {
        Mount(Path::new("/sys/fs/cgroup")).path(cgroup, file)
    }
}

const UNIT: &str = "/system.slice/kbf-daemon.service";
const ACTIONS_CG: &str = "/system.slice/kbf-daemon.service/actions";
const SUPERVISOR_CG: &str = "/system.slice/kbf-daemon.service/supervisor";
const SELF: &str = "0::/system.slice/kbf-daemon.service\n";
/// The `memory.min` the tests ask for on `supervisor/`: 256 MiB.
const MIN: u64 = 256 << 20;

/// Catches the steps out of the kernel's order (enabling controllers before the
/// processes left the delegated cgroup is EBUSY on a real host; enabling them in
/// `actions/` before its parent offers them is ENOENT), a process left behind in the
/// delegated cgroup, `actions/` without `memory` (no lease cgroup could set
/// `memory.max`), `--supervisor-memory-min-mib` not reaching `supervisor/memory.min`
/// (builds could then reclaim the daemon's memory), and `--actions-memory-max-gib`
/// not reaching `actions/memory.max`. The unit's own `memory.min` is the kernel's 0, so
/// it caps the leaf's protection and is named.
#[test]
fn a_fresh_unit_gets_its_leaf_its_controllers_and_its_actions_cgroup() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41, 42]);
    let got = delegate_in(&fake, SELF, MIN, Some(3 << 30)).expect("delegated");
    assert_eq!(
        got,
        Delegation {
            root: UNIT.into(),
            actions: ACTIONS_CG.into(),
            memory_min_capped: Some((UNIT.into(), 0)),
        }
    );
    assert_eq!(
        fake.log(),
        [
            format!("mkdir {SUPERVISOR_CG}"),
            format!("{SUPERVISOR_CG}/cgroup.procs <- 41"),
            format!("{SUPERVISOR_CG}/cgroup.procs <- 42"),
            format!("{UNIT}/cgroup.subtree_control <- +cpu +memory +pids"),
            format!("{SUPERVISOR_CG}/memory.min <- 268435456"),
            format!("mkdir {ACTIONS_CG}"),
            format!("{ACTIONS_CG}/cgroup.subtree_control <- +cpu +memory +pids"),
            format!("{ACTIONS_CG}/memory.max <- 3221225472"),
        ]
    );
    assert_eq!(fake.procs(UNIT), Vec::<u32>::new());
    assert_eq!(fake.procs(SUPERVISOR_CG), [41, 42]);
    assert_eq!(fake.enabled(ACTIONS_CG), ["cpu", "memory", "pids"]);
    assert_eq!(
        fake.read(ACTIONS_CG, "memory.max").expect("memory.max"),
        "3221225472"
    );
    assert_eq!(
        fake.read(SUPERVISOR_CG, "memory.min").expect("memory.min"),
        "268435456"
    );
}

/// Catches the leaf's protection reported as whole when an ancestor's `memory.min`
/// (the unit's or the slice's `MemoryMin=`) caps it: the nearest lower one is named,
/// `max` counts as unlimited, the root cgroup is passed, an ancestor that protects
/// enough or shows no `memory.min` is not named, and 0 asks for nothing and is never
/// capped. Then a write the daemon may not make stops the start, naming the file.
#[test]
fn the_daemons_protection_is_checked_against_its_ancestors() {
    let fake = Fake::host(UNIT, "cpu memory pids", &[41]);
    fake.set(UNIT, "memory.min", "max\n");
    fake.set("/system.slice", "memory.min", "134217728\n");
    let got = delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(
        got.memory_min_capped,
        Some(("/system.slice".into(), 128 << 20))
    );

    fake.set("/system.slice", "memory.min", "1073741824\n");
    fake.set(UNIT, "memory.min", "268435456\n");
    let got = delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(got.memory_min_capped, None);

    fake.set(UNIT, "memory.min", "0\n");
    let got = delegate_in(&fake, SELF, 0, None).expect("delegated");
    assert_eq!(got.memory_min_capped, None);
    assert_eq!(
        fake.read(SUPERVISOR_CG, "memory.min").expect("memory.min"),
        "0"
    );

    // An ancestor that shows no memory.min (its parent does not enable memory) is
    // passed, not read as 0.
    let hidden = Fake::host(UNIT, "cpu memory pids", &[41]);
    hidden
        .nodes
        .borrow_mut()
        .get_mut("/")
        .expect("root")
        .enabled
        .retain(|c| c != "memory");
    hidden.set(UNIT, "memory.min", "268435456\n");
    let got = delegate_in(&hidden, SELF, MIN, None).expect("delegated");
    assert_eq!(got.memory_min_capped, None);
    assert!(
        hidden.read("/system.slice", "memory.min").is_err(),
        "the premise: the slice shows none"
    );

    fake.set(UNIT, "memory.min", "lots");
    let err = delegate_in(&fake, SELF, MIN, None)
        .expect_err("garbage")
        .to_string();
    assert!(err.contains("parse") && err.contains("memory.min"), "{err}");

    let mut denied = Fake::host(UNIT, "cpu memory pids", &[41]);
    denied.denied = vec![format!("{SUPERVISOR_CG}/memory.min")];
    let err = delegate_in(&denied, SELF, MIN, None)
        .expect_err("denied")
        .to_string();
    assert!(
        err.contains(&format!("write /sys/fs/cgroup{SUPERVISOR_CG}/memory.min")),
        "{err}"
    );
}

/// Catches a daemon started again in the same unit (now in `supervisor/`) that takes
/// the leaf for its delegated cgroup (it would nest `supervisor/supervisor`, and enable
/// controllers in a cgroup it is in, EBUSY), or that fails on cgroups already made.
/// Without a memory cap, `memory.max` is left alone.
#[test]
fn a_second_start_from_the_leaf_finds_its_work_done() {
    let fake = Fake::host(UNIT, "cpu memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("first");
    let again = format!("0::{SUPERVISOR_CG}\n");
    let got = delegate_in(&fake, &again, MIN, None).expect("second");
    assert_eq!(got.actions, ACTIONS_CG);
    assert!(!fake.exists(&format!("{SUPERVISOR_CG}/supervisor")));
    assert!(
        !fake.log().iter().any(|l| l.contains("memory.max")),
        "{:?}",
        fake.log()
    );
    assert_eq!(fake.read(ACTIONS_CG, "memory.max").expect("shown"), "max\n");
}

/// Catches a process forked between the listing and the enable left behind (the
/// enable is EBUSY: the daemon must list and move again), and a process that ended
/// after the listing (ESRCH) failing the start.
#[test]
fn processes_that_come_and_go_during_the_move_are_handled() {
    let mut fake = Fake::host(UNIT, "cpu memory pids", &[41, 42]);
    fake.ended = vec![42];
    *fake.late.borrow_mut() = Some((UNIT.into(), 43));
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(fake.procs(SUPERVISOR_CG), [41, 43]);
    assert_eq!(fake.procs(UNIT), Vec::<u32>::new());
}

/// Catches the refusals going missing or losing their fix: a cgroup v1 host (no `0::`
/// line), a hybrid one (no cgroup v2 at the mount), a daemon in the root cgroup, a unit
/// without `Delegate=yes` (no `memory` offered), and a cgroup the daemon may not write.
/// None of them may have changed anything.
#[test]
fn hosts_and_units_that_cannot_delegate_are_refused_with_the_fix() {
    let v1 = "12:memory:/system.slice/kbf-daemon.service\n1:name=systemd:/system.slice\n";
    let refused = |fake: &Fake, text: &str| {
        let err = delegate_in(fake, text, MIN, Some(1 << 30))
            .expect_err("refused")
            .to_string();
        assert!(fake.log().is_empty(), "{err}: changed {:?}", fake.log());
        err
    };
    let fake = Fake::host(UNIT, "cpu memory pids", &[41]);
    let err = refused(&fake, v1);
    assert!(
        err.contains("cgroup v1") && err.contains("systemd.unified_cgroup_hierarchy=1"),
        "{err}"
    );

    let mut hybrid = Fake::host(UNIT, "cpu memory pids", &[41]);
    hybrid.v1_mount = true;
    let err = refused(&hybrid, SELF);
    assert!(
        err.contains("/sys/fs/cgroup has no cgroup.controllers"),
        "{err}"
    );
    assert!(err.contains("systemd.unified_cgroup_hierarchy=1"), "{err}");

    let err = refused(&fake, "0::/\n");
    assert!(
        err.contains("root cgroup") && err.contains("Delegate=yes"),
        "{err}"
    );
    let err = refused(&fake, "0::/supervisor\n");
    assert!(err.contains("root cgroup"), "{err}");
    let err = refused(&fake, "0::\n");
    assert!(err.contains("root cgroup"), "{err}");

    let mut unreadable = Fake::host(UNIT, "cpu memory pids", &[41]);
    unreadable.denied = vec!["//cgroup.controllers".into()];
    let err = refused(&unreadable, SELF);
    assert!(
        err.contains("read /sys/fs/cgroup/cgroup.controllers"),
        "{err}"
    );

    let undelegated = Fake::host(UNIT, "cpu pids", &[41]);
    let err = refused(&undelegated, SELF);
    assert!(
        err.contains(&format!("cgroup {UNIT} does not offer memory")),
        "{err}"
    );
    assert!(err.contains("Delegate=yes"), "{err}");

    let read_only = Fake::host(UNIT, "cpu memory pids", &[41]);
    read_only
        .nodes
        .borrow_mut()
        .get_mut(UNIT)
        .expect("unit")
        .read_only = true;
    let err = refused(&read_only, SELF);
    assert!(
        err.contains(&format!("make cgroup /sys/fs/cgroup{SUPERVISOR_CG}")),
        "{err}"
    );
    assert!(
        err.contains("may not write its cgroup") && err.contains("User="),
        "{err}"
    );
}

/// Catches the set-up giving up on a cgroup that is still populated by a thread that
/// has begun to exit: `cgroup.procs` lists nothing, the kernel answers EBUSY until the
/// thread has left, and that takes longer than a fixed number of immediate tries (a
/// test binary's second test thread ends as the daemon's set-up starts).
#[test]
fn a_cgroup_held_by_an_exiting_thread_is_waited_for() {
    let fake = Fake::host(UNIT, "cpu memory pids", &[41]);
    *fake.exiting.borrow_mut() = Some((UNIT.into(), 25));
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(fake.procs(UNIT), Vec::<u32>::new());
    assert!(!fake.enabled(UNIT).is_empty(), "{:?}", fake.log());
    // One pause per refusal, none before the first try.
    assert_eq!(fake.pauses.borrow().len(), 25);
}

/// Catches the real mount's pause being a different call than the fake's: it returns
/// (a zero pause), so the set-up on a real host waits through the same method.
#[test]
fn the_mounts_pause_returns() {
    Mount(Path::new("/sys/fs/cgroup")).pause(Duration::ZERO);
}

/// Catches a failed move or enable read as done (leases would then fail making their
/// cgroups), and a cgroup that never empties retried forever: each is an error naming
/// the file, after a bounded number of tries.
#[test]
fn a_move_or_enable_that_fails_stops_the_start() {
    let mut denied = Fake::host(UNIT, "cpu memory pids", &[41]);
    denied.denied = vec![format!("{SUPERVISOR_CG}/cgroup.procs")];
    let err = delegate_in(&denied, SELF, MIN, None)
        .expect_err("denied")
        .to_string();
    assert!(
        err.contains("move a process into") && err.contains("User="),
        "{err}"
    );

    let mut denied = Fake::host(UNIT, "cpu memory pids", &[41]);
    denied.denied = vec![format!("{UNIT}/cgroup.subtree_control")];
    let err = delegate_in(&denied, SELF, MIN, None)
        .expect_err("denied")
        .to_string();
    assert!(err.contains("enable controllers in"), "{err}");

    let mut busy = Fake::host(UNIT, "cpu memory pids", &[41]);
    busy.respawn = true;
    *busy.late.borrow_mut() = Some((UNIT.into(), 43));
    let err = delegate_in(&busy, SELF, MIN, None)
        .expect_err("busy")
        .to_string();
    assert!(
        err.contains(&format!("enable controllers in /sys/fs/cgroup{UNIT}")),
        "{err}"
    );
    // Every refusal was followed by a pause and a new listing and move (the process
    // that appeared is moved on the next try), until the pauses add up to the bound.
    let pauses = busy.pauses.borrow();
    let moves = busy.log().iter().filter(|l| l.ends_with("<- 43")).count();
    assert_eq!(moves, pauses.len(), "{:?}", busy.log());
    assert!(pauses.iter().sum::<Duration>() >= MOVE_PAUSE_TOTAL);
    assert!(pauses.windows(2).all(|w| w[0] <= w[1]));
    assert!(pauses.iter().all(|p| *p <= MOVE_PAUSE_MAX));
}

/// Catches `--cgroup-parent` taken without a check (every lease would fail making its
/// cgroup): one that does not enable `memory` for its children, or that this process
/// may not write, is refused with the fix; a good one gets its `memory.max` and
/// nothing else.
#[test]
fn an_actions_cgroup_given_on_the_command_line_is_checked() {
    let fake = Fake::host(UNIT, "cpu memory pids", &[]);
    fake.nodes
        .borrow_mut()
        .insert(ACTIONS_CG.into(), Node::default());
    fake.write(UNIT, "cgroup.subtree_control", "+cpu +pids")
        .expect("enable");
    fake.write(ACTIONS_CG, "cgroup.subtree_control", "+cpu +pids")
        .expect("enable");
    fake.log.borrow_mut().clear();
    let err = adopt_in(&fake, ACTIONS_CG, None)
        .expect_err("no memory")
        .to_string();
    assert!(
        err.contains(&format!("cgroup {ACTIONS_CG} does not enable memory")),
        "{err}"
    );
    assert!(err.contains("leave --cgroup-parent out"), "{err}");

    fake.write(UNIT, "cgroup.subtree_control", "+memory")
        .expect("enable");
    fake.write(ACTIONS_CG, "cgroup.subtree_control", "+memory")
        .expect("enable");
    fake.nodes
        .borrow_mut()
        .get_mut(ACTIONS_CG)
        .expect("actions")
        .read_only = true;
    let err = adopt_in(&fake, ACTIONS_CG, None)
        .expect_err("read-only")
        .to_string();
    assert!(
        err.contains("make lease cgroups in") && err.contains("User="),
        "{err}"
    );

    fake.nodes
        .borrow_mut()
        .get_mut(ACTIONS_CG)
        .expect("actions")
        .read_only = false;
    fake.log.borrow_mut().clear();
    adopt_in(&fake, ACTIONS_CG, Some(2 << 30)).expect("adopted");
    assert_eq!(
        fake.log(),
        [format!("{ACTIONS_CG}/memory.max <- 2147483648")]
    );
    fake.log.borrow_mut().clear();
    adopt_in(&fake, ACTIONS_CG, None).expect("adopted");
    assert!(fake.log().is_empty(), "{:?}", fake.log());
}

/// Catches capacity read from MemTotal alone (a host that also runs storage would be
/// overbooked), a lower `memory.max` above `actions/` (the unit's `MemoryMax=`)
/// ignored, `max` read as a number, and the CPUs not taken from the nearest
/// `cpuset.cpus.effective`.
#[test]
fn capacity_is_the_lowest_memory_max_and_the_nearest_cpuset() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(
        capacity_in(&fake, ACTIONS_CG).expect("read"),
        Capacity::default()
    );

    fake.set("/", "cpuset.cpus.effective", "0-87\n");
    fake.write(ACTIONS_CG, "memory.max", "68719476736")
        .expect("cap");
    assert_eq!(
        capacity_in(&fake, ACTIONS_CG).expect("read"),
        Capacity {
            cpus: Some(88),
            memory_bytes: Some(64 << 30)
        }
    );
    fake.set(UNIT, "memory.max", "34359738368\n");
    fake.set("/system.slice", "cpuset.cpus.effective", "0-3,8,10-11\n");
    assert_eq!(
        capacity_in(&fake, ACTIONS_CG).expect("read"),
        Capacity {
            cpus: Some(7),
            memory_bytes: Some(32 << 30)
        }
    );
    fake.set(UNIT, "memory.max", "lots");
    let err = capacity_in(&fake, ACTIONS_CG)
        .expect_err("garbage")
        .to_string();
    assert!(err.contains("memory.max"), "{err}");
    fake.set(UNIT, "memory.max", "max");
    fake.set("/system.slice", "cpuset.cpus.effective", "0-");
    let err = capacity_in(&fake, ACTIONS_CG)
        .expect_err("garbage")
        .to_string();
    assert!(
        err.contains("parse") && err.contains("cpuset.cpus.effective"),
        "{err}"
    );

    let mut unreadable = Fake::host(UNIT, "cpu memory pids", &[41]);
    delegate_in(&unreadable, SELF, MIN, None).expect("delegated");
    unreadable.denied = vec![format!("{ACTIONS_CG}/memory.max")];
    let err = capacity_in(&unreadable, ACTIONS_CG)
        .expect_err("unreadable")
        .to_string();
    assert!(err.contains("read") && err.contains("memory.max"), "{err}");
}

/// Catches a CPU list miscounted: ranges are inclusive, and a malformed list is an
/// error, not a count.
#[test]
fn cpu_lists_are_counted() {
    assert_eq!(count_cpus("0"), Some(1));
    assert_eq!(count_cpus("0-3,8,10-11"), Some(7));
    assert_eq!(count_cpus("3-1"), None);
    assert_eq!(count_cpus("a"), None);
}

/// Catches `--reserve-cpus` not reaching the kernel: `actions/cpu.max` is the leases'
/// CPUs in the kernel's `<quota> <period>` form (72 CPUs is 7200000 of 100000, a quota
/// of CPUs and not of microseconds), written to `actions/` and nowhere else, never a
/// quota of 0 (a cgroup that never runs), and a failed write names the file.
#[test]
fn the_leases_cpus_are_written_to_the_actions_cgroup() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    let before = fake.log().len();
    limit_cpus_in(&fake, ACTIONS_CG, 72).expect("limited");
    assert_eq!(
        fake.log()[before..],
        [format!("{ACTIONS_CG}/cpu.max <- 7200000 100000")]
    );
    limit_cpus_in(&fake, ACTIONS_CG, 0).expect("limited");
    assert_eq!(
        read(&fake, ACTIONS_CG, "cpu.max").expect("read"),
        "100000 100000"
    );

    // A file the daemon may not write is an error that names it.
    let mut denied = Fake::host(UNIT, "cpu memory pids", &[41]);
    delegate_in(&denied, SELF, MIN, None).expect("delegated");
    denied.denied = vec![format!("{ACTIONS_CG}/cpu.max")];
    let err = limit_cpus_in(&denied, ACTIONS_CG, 4)
        .expect_err("denied")
        .to_string();
    assert!(err.contains("write") && err.contains("cpu.max"), "{err}");
}

/// Catches the capacity ignoring `cpu.max`: the lowest quota on the path counts (a
/// unit's `CPUQuota=` as well as ours), `max` is no limit, a quota is rounded down but
/// is at least 1 CPU, a quota above the cpuset does not raise it, and junk is an error.
#[test]
fn capacity_counts_the_lowest_cpu_quota() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    fake.set("/", "cpuset.cpus.effective", "0-87\n");
    limit_cpus_in(&fake, ACTIONS_CG, 72).expect("limited");
    let cpus = |fake: &Fake| capacity_in(fake, ACTIONS_CG).expect("read").cpus;
    assert_eq!(cpus(&fake), Some(72));
    // The unit's own CPUQuota=, lower: it counts.
    fake.set(UNIT, "cpu.max", "3500000 100000\n");
    assert_eq!(cpus(&fake), Some(35));
    // Less than a CPU is one; max is no limit above.
    fake.set(UNIT, "cpu.max", "max 100000\n");
    fake.set(ACTIONS_CG, "cpu.max", "50000 100000\n");
    assert_eq!(cpus(&fake), Some(1));
    // A quota above the cpuset does not raise it.
    fake.set(ACTIONS_CG, "cpu.max", "9000000 100000\n");
    fake.set("/system.slice", "cpuset.cpus.effective", "0-3\n");
    assert_eq!(cpus(&fake), Some(4));
    // Only a quota: no cpuset anywhere.
    let only = Fake::host(UNIT, "cpu memory pids", &[41]);
    delegate_in(&only, SELF, MIN, None).expect("delegated");
    assert_eq!(cpus(&only), None);
    limit_cpus_in(&only, ACTIONS_CG, 6).expect("limited");
    assert_eq!(cpus(&only), Some(6));
    only.set(ACTIONS_CG, "cpu.max", "lots 100000");
    let err = capacity_in(&only, ACTIONS_CG)
        .expect_err("junk")
        .to_string();
    assert!(err.contains("cpu.max"), "{err}");
}

/// Catches a quota read as CPUs without its period, a period of 0 dividing, and `max`
/// read as a number.
#[test]
fn a_cpu_max_is_counted_in_cpus() {
    assert_eq!(quota_cpus("max 100000"), Ok(None));
    assert_eq!(quota_cpus("max"), Ok(None));
    assert_eq!(quota_cpus("200000 100000"), Ok(Some(2)));
    assert_eq!(quota_cpus("400000 200000"), Ok(Some(2)));
    assert_eq!(quota_cpus("250000 100000"), Ok(Some(2)));
    assert_eq!(quota_cpus("1000 100000"), Ok(Some(1)));
    assert_eq!(quota_cpus("300000"), Ok(Some(3)));
    assert!(quota_cpus("100000 0").is_err());
    assert!(quota_cpus("x 100000").is_err());
}

/// Catches `--lease-placement=numa-node` enabling `cpuset` in the wrong order or place
/// (in `actions/` before its parent is ENOENT on a real host; in the parent alone leaves
/// the lease cgroups without cpuset files), writing what is already enabled, and
/// reporting the CPUs `actions/` may use (what the placer restricts the nodes to).
#[test]
fn cpuset_is_enabled_in_the_parent_then_in_actions_and_the_cpus_are_returned() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    fake.set(ACTIONS_CG, "cpuset.cpus.effective", "0-21,44-65\n");
    let before = fake.log().len();
    let cpus = enable_cpuset_in(&fake, ACTIONS_CG).expect("enabled");
    assert_eq!(cpus.expect("cpus").to_string(), "0-21,44-65");
    assert_eq!(
        fake.log()[before..],
        [
            format!("{UNIT}/cgroup.subtree_control <- +cpuset"),
            format!("{ACTIONS_CG}/cgroup.subtree_control <- +cpuset"),
        ]
    );
    assert_eq!(
        fake.enabled(ACTIONS_CG),
        ["cpu", "memory", "pids", "cpuset"]
    );
    // Again: nothing left to write.
    let before = fake.log().len();
    enable_cpuset_in(&fake, ACTIONS_CG).expect("enabled");
    assert_eq!(fake.log().len(), before);
}

/// Catches a unit that is not offered `cpuset` (a user manager without the drop-in)
/// accepted, and the fix missing from the message; nothing is written.
#[test]
fn a_unit_without_cpuset_is_refused_with_the_fix() {
    let fake = Fake::host(UNIT, "cpu memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    let before = fake.log().len();
    let error = enable_cpuset_in(&fake, ACTIONS_CG).expect_err("refused");
    let text = error.to_string();
    assert!(
        text.contains(UNIT) && text.contains("does not offer cpuset") && text.contains("Delegate="),
        "{text}"
    );
    assert_eq!(fake.log().len(), before);
}

/// Catches a `cpuset.cpus.effective` that is empty taken for a list of no CPU, and one
/// that does not parse taken for none: the first says nothing is known (the placer then
/// keeps every node), the second fails with the file's path.
#[test]
fn an_empty_or_unreadable_effective_cpuset_is_told_apart() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    delegate_in(&fake, SELF, MIN, None).expect("delegated");
    assert_eq!(enable_cpuset_in(&fake, ACTIONS_CG).expect("enabled"), None);
    fake.set(ACTIONS_CG, "cpuset.cpus.effective", "0-x\n");
    let error = enable_cpuset_in(&fake, ACTIONS_CG).expect_err("unparsed");
    assert!(
        error.to_string().contains("cpuset.cpus.effective"),
        "{error}"
    );
}

/// Catches an `actions` cgroup made directly under the root (`--cgroup-parent=/actions`)
/// failing on its parent: the root is the parent, already enabling `cpuset`.
#[test]
fn an_actions_cgroup_under_the_root_has_the_root_as_its_parent() {
    let fake = Fake::host(UNIT, "cpuset cpu io memory pids", &[41]);
    fake.mkdir("/actions").expect("made");
    enable_cpuset_in(&fake, "/actions").expect("enabled");
    assert_eq!(
        fake.log(),
        [
            "mkdir /actions",
            "/actions/cgroup.subtree_control <- +cpuset"
        ]
    );
}
