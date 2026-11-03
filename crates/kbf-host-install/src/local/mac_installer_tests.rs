//! A Mac on the installer layout, as the `kbf-installer` package and `kbf-mac-provision` lay it
//! out, over a scratch root: no kit (`/usr/local/kbf`) at all, the releases in
//! `/usr/local/kbf-installer/releases`, the state in `/var/db/kbf-installer`, the daemon's
//! launchd job running `releases/current/kbf` as root, a pool-mode `kbf.toml` and the node's
//! key root's alone. `curl`, `gh`, `launchctl`, `lsof`, `xattr`, `codesign` and the binary's own
//! report are played by the test.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::Root;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::ScratchHost;
use kbf_installer::CommitId;
use kbf_installer::fixtures::{COMMIT, FakeRunner, file, macho, me, ran, tar_gz, trusted_host_on};
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::Spawn;
use kbf_installer::state::{self, Locked, State};
use kbf_testkit::dir::TestDir;

use super::answer::{Answer, Code};
use super::backend::Backend;
use super::core::Ab;
use super::linux::{Linux, Tools};
use super::linux_tests::{A, Borrowed, Net, Running, Sim, arch_name, exec, put, what_kbf_says};
use super::request::LocalRequest;
use crate::host::Host;
use crate::plan::Wait;
use crate::request::Version;
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

/// The daemon's launchd job, as `kbf-mac-provision` names it.
const LABEL: &str = "com.komira.kbf-daemon";

/// The pid launchd gives the daemon's process.
const PID: u32 = 4242;

/// A pool-mode `kbf.toml`: a root daemon that runs each action as a lease user.
const KBF_TOML: &str = "mode = \"daemon\"\n[identity]\nnode-id = \"mac-1\"\n[daemon]\n\
driver = [\"native\"]\ndata-dir = \"/Volumes/kbf/data\"\n[macos]\npool-users = 4\n\
pool-first-uid = 600\n";

fn version() -> Version {
    super::linux_tests::version()
}

/// The binary a Mac release holds: a Mach-O of this CPU.
fn binary(tail: &str) -> Vec<u8> {
    macho(
        if arch_name() == "arm64" {
            0x0100_000c
        } else {
            0x0100_0007
        },
        tail,
    )
}

/// What launchd and the process table do: the program the daemon's process runs, set when
/// its job is kickstarted from where `releases/current` points then.
struct Launchd {
    releases: PathBuf,
    /// The program the job's process has mapped, if it runs.
    running: RefCell<Option<PathBuf>>,
    /// The kickstart exits 5, or cannot be started.
    kickstart_fails: Cell<bool>,
    kickstart_unstartable: Cell<bool>,
    /// The commit whose binary never comes up: its process ends at once.
    crashes: RefCell<Option<String>>,
    /// A slow restart: after a kickstart the job's process (a live pid) still runs the
    /// program it ran, and `pending` is what it moves to when the test says (`settle`).
    slow: Cell<bool>,
    pending: RefCell<Option<PathBuf>>,
}

impl Launchd {
    /// The slow restart's process moves to the program it was kickstarted onto.
    fn settle(&self) {
        let next = self.pending.borrow_mut().take();
        *self.running.borrow_mut() = next;
    }
}

/// `curl` and `gh` as the Linux rig plays them, `launchctl` and `lsof` as a Mac's.
struct MacSim {
    sim: Sim,
    launchd: Rc<Launchd>,
}

impl Runner for MacSim {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        let name = Path::new(&cmd.program)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let launchd = &self.launchd;
        if name == "lsof" {
            self.sim.calls.borrow_mut().push(cmd.clone());
        }
        match (name.as_str(), cmd.args.first().map(String::as_str)) {
            ("launchctl", Some("kickstart")) => {
                self.sim.calls.borrow_mut().push(cmd.clone());
                if launchd.kickstart_unstartable.get() {
                    return Err(io::Error::other("cannot start launchctl"));
                }
                if launchd.kickstart_fails.get() {
                    return exec(5, "", "Could not find service");
                }
                let commit = fs::read_link(launchd.releases.join("current")).unwrap();
                let commit = commit.to_string_lossy().into_owned();
                let next = (launchd.crashes.borrow().as_deref() != Some(commit.as_str()))
                    .then(|| launchd.releases.join(&commit).join("kbf"));
                if launchd.slow.replace(false) {
                    *launchd.pending.borrow_mut() = next;
                } else {
                    *launchd.running.borrow_mut() = next;
                }
                exec(0, "", "")
            }
            ("launchctl", Some("print")) => match &*launchd.running.borrow() {
                Some(_) => exec(0, &format!("system/{LABEL} = {{\n\tpid = {PID}\n}}\n"), ""),
                None => exec(
                    0,
                    &format!("system/{LABEL} = {{\n\tstate = waiting\n}}\n"),
                    "",
                ),
            },
            ("lsof", _) => match &*launchd.running.borrow() {
                Some(program) => exec(0, &format!("p{PID}\nn{}\n", program.display()), ""),
                None => exec(1, "", ""),
            },
            _ => self.sim.run(cmd),
        }
    }
}

/// What a lease count answers: the count and its note, or why it cannot be told.
type Count = Result<(usize, Option<String>), String>;

/// A lease count the test sets, with the note a count may give.
#[derive(Clone)]
struct Leases(Rc<RefCell<Count>>);

impl Leases {
    fn say(&self, answer: Count) {
        *self.0.borrow_mut() = answer;
    }
}

impl LeaseSource for Leases {
    fn live(&self) -> Result<usize, String> {
        self.live_noted().map(|(n, _)| n)
    }

    fn live_noted(&self) -> Count {
        self.0.borrow().clone()
    }
}

/// A Mac on the installer layout: release A runs, a channel `stable` offers the release named
/// `COMMIT`, no lease runs.
struct Rig {
    dir: TestDir,
    layout: Layout,
    backend: Linux<ScratchHost>,
    net: Rc<RefCell<Net>>,
    calls: Rc<RefCell<Vec<Cmd>>>,
    installer: Rc<RefCell<Vec<Spawn>>>,
    launchd: Rc<Launchd>,
    leases: Leases,
    logged: Rc<RefCell<Vec<String>>>,
    /// The clock the restart's wait is counted on, moved only by its sleeps.
    clock: crate::pace::fake::Fake,
    /// The node's key: root's alone on a root daemon.
    key: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let exec = ScratchHost::new(&format!("hi-macinst-{name}"));
        let layout = Layout::new(Root::new(&exec.root).unwrap(), Platform::MacOs);
        let releases = exec.at("/usr/local/kbf-installer/releases");
        for sub in [
            "/var/run",
            "/var/db/kbf-installer",
            "/usr/local/etc/kbf",
            "/var/db/kbf",
        ] {
            fs::create_dir_all(exec.at(sub)).unwrap();
        }
        fs::create_dir_all(releases.join(A)).unwrap();
        fs::write(releases.join(A).join("kbf"), binary("a")).unwrap();
        Locked::acquire(&layout)
            .unwrap()
            .write(&State {
                current: Some(CommitId::new(A).unwrap()),
                ..State::default()
            })
            .unwrap();
        std::os::unix::fs::symlink(A, releases.join("current")).unwrap();
        put(&exec.at("/usr/local/etc/kbf/kbf.toml"), KBF_TOML);
        let key = exec.at("/var/db/kbf/node.key");
        fs::write(&key, "-----BEGIN PRIVATE KEY-----\n").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            !exec.at("/usr/local/kbf").exists(),
            "a provisioned Mac has no kit"
        );

        let dir = TestDir::under(std::env::temp_dir(), &format!("hi-macinst-host-{name}"));
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = fs::metadata(&*dir).unwrap().uid();
        let mut host = Host::for_test(&dir, Trust::owned_by(uid, &dir));
        // The node's configuration, where the test's trust reaches it.
        host.node_config = dir.join("kbf.toml");
        put(&host.node_config, KBF_TOML);
        put(
            &dir.join("host-install.toml"),
            "[channels.stable]\nsource = \"github\"\n",
        );

        let mut trust = trusted_host_on(Platform::MacOs);
        let who = me();
        trust.entries.insert(
            layout.settings(),
            Entry::File {
                perms: Perms::new(0o644),
                owner: Owner { uid: 0, gid: 0 },
                bytes: format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", who.uid, who.gid).into_bytes(),
            },
        );
        let installer_runner = FakeRunner::new(|spawn| {
            let program = spawn.program.file_name().unwrap().to_string_lossy();
            let first = spawn.args.first().map(|a| a.to_string_lossy().into_owned());
            Ok(match (program.as_ref(), first.as_deref()) {
                ("gh", _) => ran(0, "", ""),
                ("xattr", _) => ran(1, "", "xattr: No such xattr: com.apple.quarantine"),
                ("codesign", Some("--verify")) => ran(0, "", ""),
                ("codesign", _) => ran(
                    0,
                    "",
                    "CodeDirectory v=20400 size=1 flags=0x10002(adhoc,runtime) hashes=1+2\n",
                ),
                _ => ran(0, &what_kbf_says(COMMIT), ""),
            })
        });
        let installer = installer_runner.log();
        let net = Rc::new(RefCell::new(Net::default()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let launchd = Rc::new(Launchd {
            releases: releases.clone(),
            running: RefCell::new(Some(releases.join(A).join("kbf"))),
            kickstart_fails: Cell::new(false),
            kickstart_unstartable: Cell::new(false),
            crashes: RefCell::new(None),
            slow: Cell::new(false),
            pending: RefCell::new(None),
        });
        let leases = Leases(Rc::new(RefCell::new(Ok((0, None)))));
        let clock = crate::pace::fake::Fake::new();
        let logged = Rc::new(RefCell::new(Vec::new()));
        let backend = Linux {
            layout: layout.clone(),
            exec: RefCell::new(exec),
            trust: Box::new(trust),
            host,
            allowlist: dir.join("host-install.toml"),
            units: Vec::new(),
            launchd: Some(LABEL.to_owned()),
            leases: Box::new(leases.clone()),
            log: {
                let logged = Rc::clone(&logged);
                Box::new(move |line| logged.borrow_mut().push(line.to_owned()))
            },
            restart_wait: Wait {
                timeout: Duration::from_millis(40),
                interval: Duration::from_millis(1),
            },
            pace: clock.pace(),
            runner: Box::new(MacSim {
                sim: Sim {
                    net: Rc::clone(&net),
                    calls: Rc::clone(&calls),
                },
                launchd: Rc::clone(&launchd),
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
            procs: Box::new(Running(Rc::default())),
        };
        let rig = Self {
            dir,
            layout,
            backend,
            net,
            calls,
            installer,
            launchd,
            leases,
            logged,
            clock,
            key,
        };
        rig.offer(COMMIT);
        rig
    }

    /// The channel's GitHub offers the Mac release `commit`.
    fn offer(&self, commit: &str) {
        let tar = tar_gz(&[file("kbf", &binary("b"))]);
        let tag = format!("v-{}", &commit[..8]);
        let tarball = format!("kbf-{commit}-darwin-{}.tar.gz", arch_name());
        let bundle = format!("{tarball}.sigstore.json");
        let assets = format!(
            "{{\"name\":\"{tarball}\",\"size\":{}}},{{\"name\":\"{bundle}\",\"size\":30}}",
            tar.len()
        );
        let mut net = self.net.borrow_mut();
        net.pages = vec![format!(
            "[{{\"tag_name\":\"{tag}\",\"draft\":false,\"assets\":[{assets}]}}]"
        )];
        let files = net.assets.entry(tag).or_default();
        files.insert(tarball, tar);
        files.insert(bundle, b"{\"mediaType\":\"bundle\"}\n".to_vec());
    }

    fn ab(&self) -> Ab<Borrowed<'_>> {
        Ab::new(
            Borrowed(&self.backend),
            self.dir.join("installer-state.json"),
            Trust::owned_by(me().uid, &self.dir),
            600,
            Box::new(|| 1_000),
        )
    }

    fn root(&self) -> PathBuf {
        self.backend.exec.borrow().root.clone()
    }

    fn releases(&self) -> PathBuf {
        self.launchd.releases.clone()
    }

    fn current_link(&self) -> String {
        fs::read_link(self.releases().join("current"))
            .unwrap()
            .display()
            .to_string()
    }

    fn state(&self) -> State {
        state::read_unlocked(&self.layout).unwrap().unwrap()
    }

    fn kickstarts(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.args == ["kickstart", "-k", &format!("system/{LABEL}")])
            .count()
    }

    /// The host runs A, nothing is on trial, the job was not touched.
    #[track_caller]
    fn untouched(&self) {
        assert_eq!(self.current_link(), A);
        let state = self.state();
        assert_eq!(state.current.unwrap().to_string(), A);
        assert!(state.pending_confirm.is_none());
        assert_eq!(self.kickstarts(), 0);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The scratch root holds an executable, which a coverage run would scan.
        let _ = fs::remove_dir_all(self.root());
    }
}

fn ask(ab: &Ab<Borrowed<'_>>, line: &str) -> Answer {
    ab.handle(&LocalRequest::parse(line).unwrap())
}

/// Catches a provisioned Mac that cannot be updated through the socket: with no kit on the
/// host, the release is fetched as the Mac's (`darwin-<arch>`), checked and laid out in the
/// installer's releases; a switch with a live lease is refused `leases_live` and changes
/// nothing; the same switch once the leases are over moves the links, kickstarts the
/// daemon's job and is on trial until the confirm, after which the new release stays. The
/// node's id is the one `kbf.toml` names.
#[test]
fn a_provisioned_mac_is_updated_on_the_installer_layout() {
    let rig = Rig::new("update");
    let ab = rig.ab();
    let Answer::Queried(view) = ask(&ab, "query") else {
        panic!("not a query")
    };
    assert_eq!(view.node_id.as_deref(), Some("mac-1"));
    assert_eq!(view.commit.as_deref(), Some(A));
    assert!(matches!(
        ask(
            &ab,
            &format!("install kbf={} from stable", version().as_str())
        ),
        Answer::Staged { current: false, .. }
    ));
    assert_eq!(
        fs::read(rig.releases().join(COMMIT).join("kbf")).unwrap(),
        binary("b")
    );

    rig.leases.say(Ok((1, None)));
    let switch = format!("switch kbf={}", version().as_str());
    assert_eq!(
        ask(&ab, &switch),
        Answer::Refused(
            Code::LeasesLive,
            "1 leases are live; not switching".to_owned()
        )
    );
    rig.untouched();

    rig.leases.say(Ok((0, None)));
    assert!(matches!(
        ask(&ab, &switch),
        Answer::Switched {
            deadline_unix: 1_600,
            ..
        }
    ));
    assert_eq!(rig.current_link(), COMMIT);
    assert_eq!(rig.kickstarts(), 1);
    assert_eq!(
        *rig.launchd.running.borrow(),
        Some(rig.releases().join(COMMIT).join("kbf"))
    );
    assert!(rig.state().pending_confirm.is_some());
    assert!(matches!(
        ask(&ab, &format!("confirm kbf={}", version().as_str())),
        Answer::Confirmed { .. }
    ));
    let state = rig.state();
    assert_eq!(state.current.unwrap().to_string(), COMMIT);
    assert!(state.pending_confirm.is_none());
}

/// Catches a stage that runs the new binary against the node's configuration as another
/// account: on a root daemon in pool mode, whose key is root's alone (0600), the stage asks
/// the binary only for its own word on its commit, as the installer's unprivileged user,
/// with an empty environment and no `--config`; nothing is run as `_kbf`, and no command
/// names `kbf.toml` or the key.
#[test]
fn a_pool_mode_stage_reads_no_node_key_and_runs_nothing_as_the_daemon() {
    let rig = Rig::new("pool-stage");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    assert_eq!(fs::metadata(&rig.key).unwrap().mode() & 0o777, 0o600);
    let installer = rig.installer.borrow();
    let probes: Vec<&Spawn> = installer
        .iter()
        .filter(|s| s.program.file_name().is_some_and(|n| n == "kbf"))
        .collect();
    assert_eq!(probes.len(), 1, "{installer:?}");
    let probe = probes[0];
    assert_eq!(
        probe
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["--version", "--json"]
    );
    assert!(probe.env.is_empty());
    let who = me();
    assert_eq!(
        probe.run_as.map(|r| (r.uid, r.gid)),
        Some((who.uid, who.gid))
    );
    let named = |text: &str| {
        text.contains("kbf.toml") || text.contains("node.key") || text.contains("_kbf")
    };
    for spawn in installer.iter() {
        let line = format!("{} {:?}", spawn.program.display(), spawn.args);
        assert!(!named(&line), "{line}");
    }
    for cmd in rig.calls.borrow().iter() {
        let line = format!("{} {}", cmd.program, cmd.args.join(" "));
        assert!(!named(&line), "{line}");
        assert!(!cmd.program.ends_with("dscl"), "{line}");
    }
}

/// Catches a Mac left on a release that does not come up: when the daemon's job does not
/// run the new release after the kickstart, the switch fails, the host goes back to A at once
/// (the state and the link), and the job is kickstarted again onto A, which runs.
#[test]
fn a_release_whose_daemon_does_not_run_is_put_back_at_once() {
    let rig = Rig::new("crash");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    *rig.launchd.crashes.borrow_mut() = Some(COMMIT.to_owned());
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(fault.code, Code::SwitchFailed, "{fault:?}");
    let program = rig.releases().join(COMMIT).join("kbf");
    assert_eq!(
        fault.detail,
        format!(
            "system/{LABEL} did not run {} 0 seconds after the restart",
            program.display()
        )
    );
    assert_eq!(rig.current_link(), A);
    assert_eq!(rig.state().current.unwrap().to_string(), A);
    assert_eq!(rig.kickstarts(), 2);
    assert_eq!(
        *rig.launchd.running.borrow(),
        Some(rig.releases().join(A).join("kbf"))
    );
}

/// Catches a kickstart that failed read as a restart: a `launchctl` that exits non-zero, or
/// cannot be started, fails the switch with its words, and the host is put back.
#[test]
fn a_kickstart_that_fails_fails_the_switch_and_says_why() {
    let rig = Rig::new("kickfail");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.launchd.kickstart_fails.set(true);
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
    assert_eq!(
        fault.detail,
        format!(
            "launchctl kickstart system/{LABEL}: exit status Some(5): Could not find service; \
             going back: the host is back on the release that ran, but: launchctl kickstart \
             system/{LABEL}: exit status Some(5): Could not find service"
        )
    );
    assert_eq!(rig.current_link(), A);

    let rig = Rig::new("kickgone");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.launchd.kickstart_unstartable.set(true);
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert!(
        fault.detail.starts_with(&format!(
            "launchctl kickstart system/{LABEL}: cannot start launchctl"
        )),
        "{fault:?}"
    );
}

/// Catches lease directories that a stopped daemon left being let through silently: when the
/// count lets the switch through on that judgment, the switch is made and its note is in the
/// log, whole; and a count that cannot be made refuses the switch `state_unavailable` with
/// the reason, changing nothing.
#[test]
fn a_judged_lease_count_is_logged_and_one_that_cannot_be_made_refuses() {
    let rig = Rig::new("noted");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    let note = "/Volumes/kbf/data/leases/lease-3-7 remains and no kbf daemon is running; the \
                daemon's start-up sweep removes it";
    rig.leases.say(Ok((0, Some(note.to_owned()))));
    rig.backend.switch(COMMIT, 600, 1_000).unwrap();
    assert_eq!(*rig.logged.borrow(), [format!("leases: {note}")]);

    let rig = Rig::new("unread");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.leases
        .say(Err("/Volumes/kbf/data/leases is a link".to_owned()));
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert!(fault.refused);
    assert_eq!(fault.code, Code::StateUnavailable);
    assert_eq!(
        fault.detail,
        "cannot tell whether leases are live: /Volumes/kbf/data/leases is a link"
    );
    rig.untouched();
    assert!(rig.logged.borrow().is_empty());
}

/// Catches an operator's `activate` on the installer layout that leaves the old release
/// running: the links move to the kept release named, and the daemon's job is kickstarted
/// onto it.
#[test]
fn an_activate_kickstarts_the_job_onto_the_release_named() {
    let rig = Rig::new("activate");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.backend.activate(COMMIT).unwrap();
    assert_eq!(rig.current_link(), COMMIT);
    assert_eq!(rig.kickstarts(), 1);
    *rig.launchd.crashes.borrow_mut() = Some(A.to_owned());
    let fault = rig.backend.activate(A).unwrap_err();
    assert_eq!(fault.code, Code::RestartFailed, "{fault:?}");
    assert_eq!(rig.current_link(), A);
}

/// The `lsof` asks the restart made, one per look at the job's process.
fn lsof_looks(rig: &Rig) -> usize {
    rig.calls
        .borrow()
        .iter()
        .filter(|c| c.program.ends_with("lsof"))
        .count()
}

/// Catches a restart taken as done while the job's live process still runs the release it
/// left: after the kickstart the job keeps a pid whose program is A's binary for three
/// sleeps of the injected clock; the switch waits, looking each interval, and is made only
/// once the process runs the new release (four looks, three sleeps of the interval), with
/// the host on the new release and nothing put back.
#[test]
fn a_job_still_running_the_old_release_is_waited_for_until_it_runs_the_new_one() {
    let rig = Rig::new("slow");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.launchd.slow.set(true);
    let launchd = Rc::clone(&rig.launchd);
    rig.clock.on_sleep(move |n| {
        if n == 3 {
            launchd.settle();
        }
    });
    let looks = lsof_looks(&rig);
    assert_eq!(rig.backend.switch(COMMIT, 600, 1_000).unwrap(), 1_600);
    assert_eq!(lsof_looks(&rig) - looks, 4);
    assert_eq!(rig.clock.sleeps(), vec![Duration::from_millis(1); 3]);
    assert_eq!(rig.current_link(), COMMIT);
    assert_eq!(rig.kickstarts(), 1);
    assert_eq!(
        *rig.launchd.running.borrow(),
        Some(rig.releases().join(COMMIT).join("kbf"))
    );
}

/// Catches a job that never leaves the old release taken as up, or one waited for past the
/// wait: the job's live process runs A's binary however long the switch looks; when the
/// injected clock reaches the wait (40 ms in sleeps of 1 ms) the switch fails naming the
/// program it waited for, the host is put back on A (state and link), and the job,
/// kickstarted again, runs A.
#[test]
fn a_job_that_keeps_running_the_old_release_fails_the_switch_and_goes_back() {
    let rig = Rig::new("stuck");
    rig.backend
        .stage(&version(), &super::linux_tests::stable())
        .unwrap();
    rig.launchd.slow.set(true);
    let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
    assert_eq!(fault.code, Code::SwitchFailed, "{fault:?}");
    assert_eq!(
        fault.detail,
        format!(
            "system/{LABEL} did not run {} 0 seconds after the restart",
            rig.releases().join(COMMIT).join("kbf").display()
        )
    );
    assert_eq!(rig.clock.sleeps(), vec![Duration::from_millis(1); 40]);
    assert_eq!(rig.current_link(), A);
    assert_eq!(rig.state().current.unwrap().to_string(), A);
    assert_eq!(rig.kickstarts(), 2);
    assert_eq!(
        *rig.launchd.running.borrow(),
        Some(rig.releases().join(A).join("kbf"))
    );
}
