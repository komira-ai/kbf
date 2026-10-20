//! Adoption at start, on the real gate and real (stand-in) children.

use std::net::TcpListener;
use std::os::fd::OwnedFd;

use kbf_proxy::config::{ProxyConfig, UpstreamTable};
use kbf_proxy::slot::Bind;
use kbf_proxy::{Proxy, ProxyRunner};

use super::*;
use crate::proxy::boot::Progress;
use crate::proxy::child_rig::{Cleanup, alive, rig, wait_for};
use crate::proxy::fakes::plan;
use crate::proxy::gate_real_tests::proxy_table;
use crate::proxy::model::{Intent, Mode, Phase, Readiness, RoleInfo};
use crate::proxy::ports::Recorder as _;
use crate::testkit::{A, B, c};
use std::cell::{Cell, RefCell};
use std::time::Duration;

fn gate() -> (ProxyGate, ProxyRunner) {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
        "supervisor-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let config = ProxyConfig::from_toml_str(&proxy_table(&name, "")).expect("config");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(listener)).expect("proxy");
    (ProxyGate::new(handle), runner)
}

fn state_dir(rig: &crate::proxy::child_rig::Rig) -> PathBuf {
    rig.dir.parent().expect("under the host").to_owned()
}

fn bound_slot(gate: &ProxyGate) -> Option<kbf_proxy::slot::Slot> {
    match gate.handle.slots().bind() {
        Bind::Bound(permit) => Some(permit.slot()),
        _ => None,
    }
}

/// The restarted proxy finds the child its predecessor recorded, still the recorded process,
/// and routes to it: the startup hold ends into that slot and a stream binds there.
#[test]
fn a_live_recorded_child_is_adopted_and_the_startup_hold_ends_into_its_slot() {
    let rig = rig("supervisor-adopt");
    let ident = rig
        .child
        .spawn(Slot::B, &c(B), Start::Active)
        .expect("spawn");
    let after = rig.restarted();
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    assert!(
        matches!(gate.handle.slots().bind(), Bind::Held { .. }),
        "held at first"
    );
    assert_eq!(
        adopt_children(&gate, &after, &state_dir(&rig), &env.boot()),
        Startup::Adopted { active: Slot::B }
    );
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::B));
    assert!(!gate.handle.snapshot().holding);
}

/// Catches adoption of a child that is gone or no longer the recorded process: the hold is
/// left in force, with the reason.
#[test]
fn a_recorded_child_that_is_gone_is_not_adopted_and_the_hold_stays() {
    let rig = rig("supervisor-gone");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    rig.child.terminate(&ident).expect("signal");
    wait_for("exit", || !alive(ident.pid));
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("adopted a dead child");
    };
    assert!(why.contains("no frontend child is running"), "{why}");
    assert!(why.contains("no-installed-release"), "{why}");
    assert!(
        gate.handle.snapshot().holding,
        "the startup hold is untouched"
    );
    assert_eq!(gate.active(), None);
}

/// Catches routing to a child of an update nobody is resuming: an update in flight, or a
/// record that cannot be read, leaves the hold even when a child lives.
#[test]
fn an_update_in_flight_or_a_corrupt_record_leaves_the_hold_with_a_live_child() {
    let rig = rig("supervisor-inflight");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let state = state_dir(&rig);
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);

    let intent = Intent::begin(&plan(Mode::Frontend), Some(ident.clone()), 1);
    assert_eq!(intent.phase, Phase::Staged);
    FileRecorder::new(&state).record(&intent).expect("record");
    let Startup::Held(why) = adopt_children(&gate, &after, &state, &env.boot()) else {
        panic!("adopted under an update");
    };
    assert!(
        why.contains("update is in flight") && why.contains("Staged"),
        "{why}"
    );

    std::fs::write(state.join("update.json"), b"{not json").expect("corrupt");
    let Startup::Held(why) = adopt_children(&gate, &after, &state, &env.boot()) else {
        panic!("adopted with a corrupt record");
    };
    assert!(why.contains("corrupt"), "{why}");
    assert!(gate.handle.snapshot().holding);

    // A finished update is history, not an update in flight.
    let mut done = intent;
    done.phase = Phase::Done;
    FileRecorder::new(&state).record(&done).expect("record");
    assert_eq!(
        adopt_children(&gate, &after, &state, &env.boot()),
        Startup::Adopted { active: Slot::A }
    );
}

/// Catches two live children being treated as one: with no update in flight and both slots
/// running, nobody knows which holds the generation, so nothing is routed.
#[test]
fn two_live_children_with_no_update_leave_the_hold() {
    let rig = rig("supervisor-two");
    let a = rig.child.spawn(Slot::A, &c(A), Start::Active).expect("a");
    let b = rig.child.spawn(Slot::B, &c(B), Start::Active).expect("b");
    let _stop = Cleanup(&rig.child, vec![a, b]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("picked one of two");
    };
    assert!(why.contains("both slots"), "{why}");
    assert_eq!(gate.active(), None);
}

/// The command line of each slot and start, word for word, at base port 18000: the slot's
/// four loopback ports (the proxy's table: REAPI, worker, operator, admin), no `--instance`,
/// and `--await-takeover` on a candidate only. (`tests/proxy_child_argv.rs` parses these
/// same words with the `kbf` binary's own parser.)
#[test]
fn a_frontend_is_started_on_its_slots_ports_and_a_candidate_awaits_the_takeover() {
    let table = UpstreamTable { base_port: 18_000 };
    let argv = frontend_argv(Path::new("/run/kbf-proxy/proxy-auth"), table);
    let words = |slot, start| -> Vec<String> {
        argv(slot, start)
            .into_iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect()
    };
    let expect = |base: u16, extra: &[&str]| -> Vec<String> {
        let mut want = vec![
            "--mode=frontend".to_owned(),
            format!("--listen=127.0.0.1:{base}"),
            format!("--worker-listen=127.0.0.1:{}", base + 1),
            format!("--api-listen=127.0.0.1:{}", base + 2),
            format!("--admin-listen=127.0.0.1:{}", base + 3),
            "--proxy-auth-file=/run/kbf-proxy/proxy-auth".to_owned(),
        ];
        want.extend(extra.iter().map(|w| (*w).to_owned()));
        want
    };
    assert_eq!(words(Slot::A, Start::Active), expect(18_000, &[]));
    assert_eq!(
        words(Slot::A, Start::Candidate),
        expect(18_000, &["--await-takeover"])
    );
    assert_eq!(words(Slot::B, Start::Active), expect(18_010, &[]));
    assert_eq!(
        words(Slot::B, Start::Candidate),
        expect(18_010, &["--await-takeover"])
    );
    assert_eq!(
        children_dir(Path::new("/var/lib/kbf-proxy")),
        PathBuf::from("/var/lib/kbf-proxy/children")
    );
}

/// Catches an adoption that fails at the gate (the startup hold is gone: ended already, or
/// held by someone else) being reported as adopted.
#[test]
fn an_adoption_the_gate_refuses_is_reported_as_held() {
    let rig = rig("supervisor-refused");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    let _taken = gate.handle.startup_hold().expect("the startup hold");
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("reported adopted without the hold");
    };
    assert!(why.contains("startup hold is gone"), "{why}");
}

/// Admin calls that answer from a script: the role, when `/readyz` turns true, and how many
/// takeovers fail first.
struct ScriptAdmin<'a> {
    role: RoleInfo,
    /// The role call fails (the child never answers it).
    role_err: bool,
    ready_after: u32,
    polls: Cell<u32>,
    takeovers: Cell<u32>,
    takeover_fails: Cell<u32>,
    /// Whether the startup hold is in force, read at each `/readyz` poll.
    holding: Box<dyn Fn() -> bool + 'a>,
    held_at_poll: RefCell<Vec<bool>>,
}

impl ChildAdmin for ScriptAdmin<'_> {
    fn role(&self, _slot: Slot) -> Result<RoleInfo, PortError> {
        if self.role_err {
            return Err(PortError("admin port closed".into()));
        }
        Ok(self.role)
    }

    fn takeover(&self, _slot: Slot, _limit: Duration) -> Result<(), PortError> {
        self.takeovers.set(self.takeovers.get() + 1);
        if self.takeover_fails.get() > 0 {
            self.takeover_fails.set(self.takeover_fails.get() - 1);
            return Err(PortError("refused".into()));
        }
        Ok(())
    }

    fn ready(&self, _slot: Slot) -> Result<Readiness, PortError> {
        self.polls.set(self.polls.get() + 1);
        self.held_at_poll.borrow_mut().push((self.holding)());
        Ok(Readiness {
            readyz: self.polls.get() > self.ready_after,
            health: false,
            capabilities: false,
        })
    }

    fn quiesce(&self, _: Slot) -> Result<(), PortError> {
        unreachable!("the boot never quiesces")
    }

    fn stand_down(&self, _: Slot) -> Result<(), PortError> {
        unreachable!("the boot never stands down")
    }

    fn resume(&self, _: Slot) -> Result<(), PortError> {
        unreachable!("the boot never resumes")
    }

    fn open_counts(&self, _: Slot) -> Result<crate::proxy::model::OpenCounts, PortError> {
        unreachable!("the boot never counts")
    }

    fn post_desired(&self, _: Slot, _: &CommitId) -> Result<(), PortError> {
        unreachable!("the boot never posts a desired release")
    }
}

/// Time that moves only when the boot sleeps, plus a real millisecond so a process that
/// should exit has the time to; `on_wait` runs at each sleep of two seconds, the wait
/// between tries.
struct FastClock<'a> {
    now: Cell<Duration>,
    on_wait: Box<dyn Fn() + 'a>,
    /// Set: the next sleep says the proxy is shutting down.
    halt: Cell<bool>,
}

impl Clock for FastClock<'_> {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn sleep(&self, d: Duration) -> Result<(), crate::proxy::model::Halt> {
        if self.halt.get() {
            return Err(crate::proxy::model::Halt::Interrupted);
        }
        let wait = d == Duration::from_secs(2);
        // A stopped process needs a moment to be gone before the next try starts.
        std::thread::sleep(Duration::from_millis(if wait { 200 } else { 1 }));
        self.now.set(self.now.get() + d);
        if wait {
            (self.on_wait)();
        }
        Ok(())
    }
}

#[derive(Default)]
struct Notes(RefCell<Vec<Progress>>);

impl Report for Notes {
    fn progress(&self, progress: &Progress, _detail: Option<&str>) {
        self.0.borrow_mut().push(*progress);
    }
}

struct Env<'a> {
    admin: ScriptAdmin<'a>,
    clock: FastClock<'a>,
    notes: Notes,
    status: PathBuf,
}

impl<'a> Env<'a> {
    fn new(rig: &crate::proxy::child_rig::Rig, gate: &'a ProxyGate) -> Env<'a> {
        Env {
            admin: ScriptAdmin {
                role: RoleInfo {
                    holder: true,
                    state: RoleState::Active,
                },
                role_err: false,
                ready_after: 0,
                polls: Cell::new(0),
                takeovers: Cell::new(0),
                takeover_fails: Cell::new(0),
                holding: Box::new(|| gate.handle.snapshot().holding),
                held_at_poll: RefCell::new(Vec::new()),
            },
            clock: FastClock {
                now: Cell::new(Duration::ZERO),
                on_wait: Box::new(|| {}),
                halt: Cell::new(false),
            },
            notes: Notes::default(),
            status: state_dir(rig).join("status.json"),
        }
    }

    fn boot(&self) -> Boot<'_> {
        Boot {
            admin: &self.admin,
            clock: &self.clock,
            migrator: &NoMigration,
            report: &self.notes,
            status: &self.status,
            timing: Timing {
                ready_within: Duration::from_secs(5),
                ..Timing::frontend()
            },
            retry: Retry {
                attempts: 3,
                first_wait: Duration::from_secs(2),
                max_wait: Duration::from_secs(4),
            },
        }
    }

    /// `status.json` says `commit` is installed.
    fn install(&self, commit: &str) {
        std::fs::write(
            &self.status,
            format!("{{\"installed_commit\":\"{commit}\"}}\n"),
        )
        .expect("status.json");
    }
}

/// The marker the installer writes into a release it installed.
fn verify(rig: &crate::proxy::child_rig::Rig, commit: &str) {
    let dir = rig
        .child
        .layout
        .join(&rig.child.layout.release_dir(&c(commit)));
    std::fs::write(dir.join(crate::stage::VERIFIED), format!("{commit}\n")).expect("VERIFIED");
}

fn reasons(env: &Env<'_>) -> Vec<&'static str> {
    env.notes
        .0
        .borrow()
        .iter()
        .filter_map(|p| p.reason)
        .collect()
}

/// Catches a host that comes up with no child staying dark: after a reboot nothing runs,
/// `status.json` names the installed release, and that release is started in slot A (a
/// process of its own, recorded), made to take the generation, and the streams are
/// released into it.
#[test]
fn after_a_reboot_the_installed_release_is_started_and_the_streams_are_released_into_it() {
    let rig = rig("supervisor-cold");
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    env.install(A);
    verify(&rig, A);
    let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot());
    let recorded = rig
        .child
        .recorded(Slot::A)
        .expect("the process is recorded");
    let _stop = Cleanup(&rig.child, vec![recorded.ident.clone()]);
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_eq!(
        recorded.commit,
        c(A),
        "the installed release, not `current`"
    );
    assert_eq!(env.admin.takeovers.get(), 1);
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::A));
    assert!(!gate.handle.snapshot().holding);
    assert_eq!(reasons(&env), Vec::<&str>::new());
}

/// Catches streams let through before the child is ready: every poll of `/readyz` that says
/// "not yet" finds the startup hold in force, and no stream binds until the child is ready
/// and has taken the generation.
#[test]
fn the_streams_stay_held_until_the_child_is_ready() {
    let rig = rig("supervisor-cold-hold");
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.ready_after = 3;
    env.install(A);
    verify(&rig, A);
    let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot());
    let recorded = rig.child.recorded(Slot::A).expect("recorded");
    let _stop = Cleanup(&rig.child, vec![recorded.ident]);
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_eq!(env.admin.polls.get(), 4);
    assert!(
        env.admin.held_at_poll.borrow().iter().all(|held| *held),
        "held at every poll: {:?}",
        env.admin.held_at_poll.borrow()
    );
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::A));
}

/// Catches starting a release the installer did not name or did not install: a commit that
/// has no directory (the hand-placed host whose `status.json` names another), and a
/// directory without `VERIFIED`. Nothing is spawned, no record is written, the streams stay
/// held, and the report says why in a fixed word with the operator's fix.
#[test]
fn a_release_that_is_missing_or_not_verified_is_refused_and_nothing_starts() {
    let rig = rig("supervisor-cold-refuse");
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    // The rig has releases A and B, neither verified: a commit with no directory.
    env.install(crate::testkit::C);
    let Startup::Held(why) = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot())
    else {
        panic!("started a release with no directory");
    };
    assert!(why.contains("binary-missing"), "{why}");
    assert!(
        !why.contains(crate::testkit::C),
        "no value in the reason: {why}"
    );
    env.install(A);
    let Startup::Held(why) = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot())
    else {
        panic!("started an unverified release");
    };
    assert!(why.contains("release-not-verified"), "{why}");
    assert_eq!(rig.child.recorded(Slot::A), None, "nothing was spawned");
    assert_eq!(env.admin.polls.get(), 0);
    assert!(gate.handle.snapshot().holding);
    assert!(
        env.notes
            .0
            .borrow()
            .iter()
            .any(|p| p.state == "gave-up" && p.fix.is_some_and(|f| f.contains("VERIFIED")))
    );
    assert_eq!(gate.active(), None);
}

/// Catches a cold start that gives up on the first failure or never gives up: the first
/// takeover is refused, the process is stopped, the boot waits and starts a second process
/// that serves; the first was SIGTERMed, not left running next to it.
#[test]
fn a_refused_takeover_is_retried_with_a_new_process_that_then_serves() {
    let rig = rig("supervisor-cold-retry");
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    env.admin.takeover_fails.set(1);
    env.install(A);
    verify(&rig, A);
    let first = std::rc::Rc::new(RefCell::new(None));
    let seen = std::rc::Rc::clone(&first);
    let child = &rig.child;
    let clock = FastClock {
        now: Cell::new(Duration::ZERO),
        on_wait: Box::new(move || {
            // The wait between the tries: the first process must be gone by now.
            *seen.borrow_mut() = child_pid_record(child);
        }),
        halt: Cell::new(false),
    };
    let boot = Boot {
        clock: &clock,
        ..env.boot()
    };
    let outcome = adopt_children(&gate, child, &state_dir(&rig), &boot);
    let second = rig.child.recorded(Slot::A).expect("recorded");
    let _stop = Cleanup(&rig.child, vec![second.ident.clone()]);
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_eq!(env.admin.takeovers.get(), 2);
    let first = first.borrow().expect("a first process was started");
    assert_ne!(
        first, second.ident.pid,
        "a new process, not the first again"
    );
    wait_for("the first process is stopped", || !alive(first));
    assert_eq!(
        env.notes
            .0
            .borrow()
            .iter()
            .map(|p| p.state)
            .collect::<Vec<_>>(),
        ["starting", "retrying", "starting", "serving"]
    );
}

fn child_pid_record(child: &ProcChild) -> Option<u32> {
    child.recorded(Slot::A).map(|r| r.ident.pid)
}

/// Catches a boot with no end: every try is refused, the third is the last, and the proxy
/// then holds with the last word; the streams were never released.
#[test]
fn a_boot_that_cannot_start_gives_up_after_the_cap_and_holds() {
    let rig = rig("supervisor-cold-giveup");
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    env.admin.takeover_fails.set(99);
    env.install(A);
    verify(&rig, A);
    let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot());
    let ident = rig.child.recorded(Slot::A).map(|r| r.ident);
    let _stop = Cleanup(&rig.child, ident.into_iter().collect());
    let Startup::Held(why) = outcome else {
        panic!("served after every takeover was refused");
    };
    assert!(why.contains("takeover-failed"), "{why}");
    assert_eq!(env.admin.takeovers.get(), 3);
    assert_eq!(reasons(&env), ["takeover-failed"; 3]);
    assert!(gate.handle.snapshot().holding);
    assert_eq!(gate.active(), None);
}

/// Catches a shutdown in the middle of the cold start being reported as a failure to start:
/// the proxy told to stop while it waits between two tries holds with the shutdown as the
/// reason (not an installed release that "did not start"), and no stream was released.
#[test]
fn a_proxy_told_to_stop_between_two_tries_holds_and_says_it_is_shutting_down() {
    let rig = rig("supervisor-cold-stopped");
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    env.admin.takeover_fails.set(99);
    env.clock.halt.set(true);
    env.install(A);
    verify(&rig, A);
    let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &env.boot());
    let ident = rig.child.recorded(Slot::A).map(|r| r.ident);
    let _stop = Cleanup(&rig.child, ident.into_iter().collect());
    assert_eq!(outcome, Startup::Held("the proxy is shutting down".into()));
    assert_eq!(env.admin.takeovers.get(), 1, "stopped after the first try");
    assert!(gate.handle.snapshot().holding);
    assert_eq!(gate.active(), None);
}

/// Catches a proxy restart that starts a second child: a candidate its predecessor started
/// and never made take the generation is adopted (same pid, no new spawn) and takes it.
#[test]
fn a_candidate_a_previous_proxy_started_is_adopted_and_takes_the_generation() {
    let rig = rig("supervisor-cold-candidate");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Candidate)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.role = RoleInfo {
        holder: false,
        state: RoleState::Candidate,
    };
    let outcome = adopt_children(&gate, &after, &state_dir(&rig), &env.boot());
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_eq!(env.admin.takeovers.get(), 1);
    assert_eq!(
        after.recorded(Slot::A).map(|r| r.ident),
        Some(ident),
        "the process the predecessor recorded, not a second one"
    );
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::A));
}

/// Catches adopting a pid that was reused: the record names a live process whose start time
/// differs, so it is not ours. It is not signalled, the cold start proceeds and the record
/// then names the new process.
#[test]
fn a_record_whose_pid_was_reused_is_discarded_and_the_cold_start_goes_on() {
    let rig = rig("supervisor-cold-stale");
    let stranger = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop_stranger = Cleanup(&rig.child, vec![stranger.clone()]);
    let path = rig.dir.join("a.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("json");
    record["ident"]["start_time"] = (stranger.start_time + 1).into();
    std::fs::write(&path, serde_json::to_vec(&record).expect("json")).expect("rewrite");
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let env = Env::new(&rig, &gate);
    env.install(A);
    verify(&rig, A);
    let outcome = adopt_children(&gate, &after, &state_dir(&rig), &env.boot());
    let fresh = after.recorded(Slot::A).expect("a new record").ident;
    let _stop_fresh = Cleanup(&after, vec![fresh.clone()]);
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_ne!(fresh.pid, stranger.pid, "a process of the boot's own");
    assert!(
        alive(stranger.pid),
        "the process that is not ours is not signalled"
    );
    assert_eq!(env.admin.takeovers.get(), 1);
}

/// Catches a release check that cannot fail: the real source (not a fake) refuses a binary the
/// proxy's user cannot execute and accepts one it can. A binary with execute bits for the group
/// only, or for the owner only, is judged by the bits that apply to the user, not by any bit.
#[test]
fn the_real_source_runs_only_a_binary_the_proxys_user_can_execute() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = rig("supervisor-source");
    verify(&rig, A);
    let status = state_dir(&rig).join("status.json");
    let source = HostSource {
        status: &status,
        child: &rig.child,
    };
    let program = rig.child.program(&c(A));
    // The rig's binary is a link to the test executable: followed, and executable.
    assert_eq!(source.runnable(&c(A)), Ok(()));
    std::fs::remove_file(&program).expect("rm link");
    std::fs::write(&program, b"not run\n").expect("write");
    let mode = |bits: u32| {
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(bits)).expect("mode");
    };
    mode(0o644);
    assert_eq!(source.runnable(&c(A)), Err(Reason::BinaryNotExecutable));
    mode(0o755);
    assert_eq!(source.runnable(&c(A)), Ok(()));
    mode(0o700);
    assert_eq!(source.runnable(&c(A)), Ok(()), "the owner is the user");
    // Only the group may execute: the owner (this user) may not. Root runs anything with an
    // execute bit.
    mode(0o070);
    let root = rustix::process::geteuid().is_root();
    assert_eq!(
        source.runnable(&c(A)),
        root.then_some(()).ok_or(Reason::BinaryNotExecutable)
    );
    mode(0o755);
    std::fs::remove_file(&program).expect("rm");
    assert_eq!(source.runnable(&c(A)), Err(Reason::BinaryMissing));
}

/// Catches an adopted candidate whose startup hold is not kept by the gate: while the
/// recorded candidate is awaited, the gate holds the guard (nobody else can take it) and the
/// streams stay held at every `/readyz` poll; when it never comes up the gate still holds it.
#[test]
fn the_startup_hold_stays_in_force_while_an_adopted_candidate_is_awaited() {
    let rig = rig("supervisor-adopt-hold");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Candidate)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.role = RoleInfo {
        holder: false,
        state: RoleState::Candidate,
    };
    env.admin.ready_after = 2;
    env.admin.holding =
        Box::new(|| gate.handle.startup_hold().is_none() && gate.handle.snapshot().holding);
    let outcome = adopt_children(&gate, &after, &state_dir(&rig), &env.boot());
    assert_eq!(outcome, Startup::Adopted { active: Slot::A });
    assert_eq!(env.admin.polls.get(), 3);
    assert_eq!(
        *env.admin.held_at_poll.borrow(),
        [true, true, true],
        "the gate held the startup hold at every poll"
    );
    assert_eq!(bound_slot(&gate), Some(kbf_proxy::slot::Slot::A));
}

/// Catches an adoption of a child nobody could ask: a live recorded child whose role never
/// answers is not routed to, and the reason names the slot; the startup hold stays in force.
#[test]
fn a_live_child_whose_role_cannot_be_read_is_not_adopted_and_the_hold_stays() {
    let rig = rig("supervisor-role-err");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.role_err = true;
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("adopted a child whose role is unknown");
    };
    assert!(
        why.contains("the live child in slot A cannot be adopted"),
        "{why}"
    );
    assert!(gate.handle.snapshot().holding);
    assert_eq!(gate.active(), None);
}

/// Catches a candidate taking the generation without the startup hold: with the hold gone (taken
/// by someone else) the recorded candidate is left alone, no takeover is asked for, and the
/// reason is returned.
#[test]
fn a_recorded_candidate_is_left_alone_when_the_startup_hold_is_gone() {
    let rig = rig("supervisor-cand-nohold");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Candidate)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.role = RoleInfo {
        holder: false,
        state: RoleState::Candidate,
    };
    let _taken = gate.handle.startup_hold().expect("the startup hold");
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("took the generation without the hold");
    };
    assert!(why.contains("startup hold is gone"), "{why}");
    assert_eq!(env.admin.takeovers.get(), 0);
    assert_eq!(gate.active(), None);
}

/// Catches a refused takeover reported as adopted: the recorded candidate that is refused the
/// generation is left running ("the process is left as it is"), the hold stays and nothing is
/// routed.
#[test]
fn a_recorded_candidate_refused_the_generation_is_left_as_it_is_and_the_hold_stays() {
    let rig = rig("supervisor-cand-refused");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Candidate)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let (gate, _runner) = gate();
    let mut env = Env::new(&rig, &gate);
    env.admin.role = RoleInfo {
        holder: false,
        state: RoleState::Candidate,
    };
    env.admin.takeover_fails.set(1);
    let Startup::Held(why) = adopt_children(&gate, &after, &state_dir(&rig), &env.boot()) else {
        panic!("reported adopted after a refused takeover");
    };
    assert!(
        why.contains("the recorded candidate in slot A did not come up")
            && why.contains("takeover: refused")
            && why.contains("left as it is"),
        "{why}"
    );
    assert_eq!(env.admin.takeovers.get(), 1);
    assert!(alive(ident.pid), "the process is not signalled");
    assert!(gate.handle.snapshot().holding);
    assert_eq!(gate.active(), None);
}

/// Catches a secret that cannot be sent being swallowed: a header value with a non-ASCII byte is
/// refused with the reason, and the proxy's real secret (64 hex digits) is accepted.
#[test]
fn a_secret_the_client_cannot_send_is_refused_with_the_reason() {
    let why = secret_headers(&[("x-secret", &[0xff_u8][..])]).expect_err("non-ASCII");
    assert!(
        why.contains("the proxy secret is not a header value"),
        "{why}"
    );
    assert!(secret_headers(&[("x-secret", &b"abcdef"[..])]).is_ok());
}

/// What a fixture operator port received: the first request's head, as text.
fn serve_once(listener: TcpListener) -> std::thread::JoinHandle<String> {
    use std::io::{Read as _, Write as _};
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).expect("read") == 0 {
                break;
            }
            head.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .expect("answer");
        String::from_utf8_lossy(&head).into_owned()
    })
}

/// Catches an admin client wired to the wrong place: the desired-release offer of each slot goes
/// to that slot's operator port, with the proxy's secret and the operator token read from the
/// file at the call (a missing file is the error of the token, not of the network).
#[test]
fn the_child_admin_offers_a_release_to_the_operator_port_of_its_slot() {
    use crate::proxy::admin::AdminError;
    let (gate, _runner) = gate();
    let token_file = kbf_host::testkit::scratch("supervisor-child-admin").join("operator.token");
    let offset = UpstreamTable { base_port: 0 }.port(kbf_proxy::slot::Slot::A, Target::Operator);
    let stride =
        UpstreamTable { base_port: 0 }.port(kbf_proxy::slot::Slot::B, Target::Operator) - offset;

    let table = UpstreamTable { base_port: 20_000 };
    let missing = child_admin(&gate.handle, table, token_file.clone()).expect("admin");
    assert!(
        matches!(
            missing.try_post_desired(Slot::A, &c(A)),
            Err(AdminError::Token(std::io::ErrorKind::NotFound))
        ),
        "no token file"
    );

    std::fs::write(&token_file, "operator-token\n").expect("token");
    for (slot, extra) in [(Slot::A, 0), (Slot::B, stride)] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let table = UpstreamTable {
            base_port: port - offset - extra,
        };
        let admin = child_admin(&gate.handle, table, token_file.clone()).expect("admin");
        let seen = serve_once(listener);
        admin.try_post_desired(slot, &c(A)).expect("offered");
        let head = seen.join().expect("fixture");
        assert!(head.starts_with("POST /v1/update/desired "), "{head}");
        assert!(
            head.contains("authorization: Bearer operator-token\r\n"),
            "{head}"
        );
        for (name, value) in gate.handle.admin_headers().iter() {
            let line = format!("{}: {}\r\n", name.as_str(), value.to_str().expect("ASCII"));
            assert!(head.contains(&line), "{line:?} in {head}");
        }
    }
}

/// Migration on the cold start: the database a test cannot reach, with secrets in its URL.
mod migration {
    use kbf_state::postgres::PostgresConfig;
    use kbf_state::postgres::schema::{self, Outcome};

    use super::*;
    use crate::fixtures::{FakeRunner, ran, version_json};
    use crate::proxy::root_orders::StatusFile;

    const URL: &str = "postgres://kbf_marker_user:hunter2_marker@127.0.0.1:1/kbf_marker_db";
    const SECRETS: [&str; 4] = [
        "hunter2_marker",
        "kbf_marker_user",
        "kbf_marker_db",
        "127.0.0.1",
    ];

    fn unreachable_db() -> Database {
        Database::new(
            PostgresConfig::parse(URL).expect("parses"),
            schema::embedded(),
        )
    }

    /// What `commit --version --json` prints for a release that needs schema `schema`.
    fn declares(commit: &str, schema: u32) -> String {
        version_json(commit, "release")
            .replace("\"state_schema\":7", &format!("\"state_schema\":{schema}"))
    }

    fn says(stdout: String) -> FakeRunner {
        FakeRunner::new(move |_| Ok(ran(0, &stdout, "")))
    }

    /// One progress report: its state, its reason word and the port's own text.
    type Line = (&'static str, Option<&'static str>, Option<String>);

    /// Every progress report with the port's own text, as the log gets it.
    #[derive(Default)]
    struct Told(RefCell<Vec<Line>>);

    impl Report for Told {
        fn progress(&self, progress: &Progress, detail: Option<&str>) {
            self.0
                .borrow_mut()
                .push((progress.state, progress.reason, detail.map(str::to_owned)));
        }
    }

    fn nothing_applied(schema: u32) -> Result<Outcome, kbf_state::postgres::schema::SchemaError> {
        Ok(Outcome {
            from: None,
            to: schema,
            ran: Vec::new(),
        })
    }

    /// Catches a verb whose migrator is not the database's: the constructor the verb calls
    /// must build a `DbMigrator`, which asks the database (here unreachable, so `database_error`),
    /// not a hook that says yes. For a release `status.json` calls staged it reads the schema
    /// there; for the installed one it runs the release's own program (here a stand-in that
    /// refuses `--version`), which is `schema_unknown`. No part of the URL is in either.
    ///
    /// Mutants seen red: `host_migrator` returning a hook that migrates nothing (both `Ok`);
    /// the installed probe not given (`not_staged`).
    #[test]
    fn the_verbs_migrator_asks_the_database_and_the_release() {
        let rig = rig("supervisor-host-migrator");
        let status = state_dir(&rig).join("status.json");
        let facts = StatusFile::new(&status);
        let m = host_migrator(unreachable_db(), &facts, &rig.child);
        std::fs::write(
            &status,
            format!(
                "{{\"installed_commit\":\"{A}\",\"staged_commit\":\"{A}\",\"staged\":{{\"protocol\":1,\"min_protocol\":1,\"state_schema\":1}}}}\n"
            ),
        )
        .expect("status.json");
        let staged = m.migrate_forward(&c(A)).unwrap_err().0;
        assert!(staged.starts_with("database_error: "), "{staged}");
        std::fs::write(&status, format!("{{\"installed_commit\":\"{A}\"}}\n")).expect("status");
        let installed = m.migrate_forward(&c(A)).unwrap_err().0;
        assert!(installed.starts_with("schema_unknown: "), "{installed}");
        for text in [staged, installed] {
            for secret in SECRETS {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }

    /// Catches a cold start that goes on after a migration refusal: the database is down, so
    /// every attempt fails as `migration-failed` with the refusal's word in the log, no process
    /// is started or recorded, the child's admin port is never asked, the streams stay held, and
    /// the operator gets a fix. Neither the held reason nor the log carries the URL.
    ///
    /// Mutants seen red: the refusal ignored by the boot (a process was recorded); the fix of
    /// `migration-failed` removed.
    #[test]
    fn a_refused_migration_holds_with_its_reason_and_starts_nothing() {
        let rig = rig("supervisor-cold-migration-refused");
        let (gate, _runner) = gate();
        let env = Env::new(&rig, &gate);
        env.install(A);
        verify(&rig, A);
        let runner = says(declares(A, 1));
        let facts = StatusFile::new(&env.status);
        let m = host_migrator_with(&runner, unreachable_db(), &facts, &rig.child);
        let told = Told::default();
        let boot = Boot {
            migrator: &m,
            report: &told,
            ..env.boot()
        };
        let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &boot);
        // Whatever a defect started is stopped when the test ends.
        let _stop = Cleanup(
            &rig.child,
            rig.child
                .recorded(Slot::A)
                .map(|r| r.ident)
                .into_iter()
                .collect(),
        );
        let Startup::Held(why) = outcome else {
            panic!("started on a database that is down");
        };
        assert!(why.contains("migration-failed"), "{why}");
        assert_eq!(rig.child.recorded(Slot::A), None, "a process was started");
        assert_eq!(env.admin.polls.get(), 0);
        assert!(gate.handle.snapshot().holding);
        assert_eq!(gate.active(), None);
        let told = told.0.borrow();
        let failed: Vec<_> = told
            .iter()
            .filter(|(_, reason, _)| reason.is_some())
            .collect();
        assert_eq!(failed.len(), 3, "{told:?}");
        for (_, reason, detail) in failed {
            assert_eq!(*reason, Some("migration-failed"));
            let detail = detail.as_deref().expect("the port's text");
            assert!(detail.starts_with("database_error: "), "{detail}");
            for secret in SECRETS {
                assert!(
                    !detail.contains(secret) && !why.contains(secret),
                    "{secret}"
                );
            }
        }
        assert!(
            Reason::MigrationFailed
                .fix()
                .is_some_and(|fix| fix.contains("database_busy") && fix.contains("restart")),
        );
    }

    /// Catches a migration that runs after the process or not on the release that starts: the
    /// installed release asks its own binary for its schema, the migration runs with it and the
    /// bound, and at that moment no process is recorded; afterwards the release runs and serves.
    ///
    /// Mutants seen red: the migrator called after `spawn` (a process was already recorded);
    /// the schema taken from the installer's newest instead of the release's.
    #[test]
    fn the_installed_release_is_migrated_for_its_own_schema_before_it_is_spawned() {
        let rig = rig("supervisor-cold-migrates-first");
        let (gate, _runner) = gate();
        let env = Env::new(&rig, &gate);
        env.install(A);
        verify(&rig, A);
        let runner = says(declares(A, 5));
        let facts = StatusFile::new(&env.status);
        let seen = RefCell::new(Vec::new());
        let m = host_migrator_with(&runner, unreachable_db(), &facts, &rig.child).with_apply(
            Box::new(|_, schema, wait| {
                seen.borrow_mut()
                    .push((schema, wait, rig.child.recorded(Slot::A).is_some()));
                nothing_applied(schema)
            }),
        );
        let boot = Boot {
            migrator: &m,
            ..env.boot()
        };
        let outcome = adopt_children(&gate, &rig.child, &state_dir(&rig), &boot);
        let recorded = rig.child.recorded(Slot::A).expect("started after");
        let _stop = Cleanup(&rig.child, vec![recorded.ident]);
        assert_eq!(outcome, Startup::Adopted { active: Slot::A });
        assert_eq!(*seen.borrow(), [(5, Duration::from_secs(30), false)]);
        assert_eq!(runner.calls().len(), 1);
        assert!(runner.calls()[0].program.ends_with("kbf-act"));
    }

    /// Real server, own database: a cold start against a fresh database creates the schema
    /// (kbf opens on it), and the next cold start finds it done and changes nothing. The
    /// database is created for this test alone, so no other test process shares its schema.
    /// No part of the URL is in anything the log is given.
    ///
    /// Mutants seen red: the hook migrating nothing (no schema; kbf's open refused);
    /// the `schema_version` row written again on the second start (the row changed).
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_cold_start_creates_the_schema_on_a_fresh_database_and_the_next_changes_nothing() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let url = std::env::var("KBF_TEST_POSTGRES_URL").expect("KBF_TEST_POSTGRES_URL");

        /// A database created for one test and dropped with it.
        struct Own {
            name: String,
            url: String,
        }

        impl Own {
            fn new(url: &str) -> Own {
                let name = format!(
                    "kbf_cold_{}_{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                );
                let mut admin = postgres::Client::connect(url, postgres::NoTls).expect("connects");
                admin
                    .batch_execute(&format!("CREATE DATABASE {name}"))
                    .expect("creates");
                let (base, _) = url.rsplit_once('/').expect("a database in the URL");
                Own {
                    url: format!("{base}/{name}"),
                    name,
                }
            }
        }

        impl Drop for Own {
            fn drop(&mut self) {
                let base = self.url.rsplit_once('/').map_or("", |(base, _)| base);
                if let Ok(mut admin) =
                    postgres::Client::connect(&format!("{base}/postgres"), postgres::NoTls)
                {
                    let _ = admin.batch_execute(&format!(
                        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                        self.name
                    ));
                }
            }
        }

        let own = Own::new(&url);
        let config = PostgresConfig::parse(&own.url).expect("parses");
        let mut client = postgres::Client::connect(&own.url, postgres::NoTls).expect("connects");
        let rows = |client: &mut postgres::Client| -> Vec<String> {
            client
                .query(
                    "SELECT version::text || ' ' || applied_at::text FROM kbf_state.schema_version ORDER BY version",
                    &[],
                )
                .expect("the version table")
                .iter()
                .map(|r| r.get::<_, String>(0))
                .collect()
        };
        let absent = client
            .query_one("SELECT to_regclass('kbf_state.schema_version')::text", &[])
            .expect("catalog");
        assert_eq!(
            absent.get::<_, Option<String>>(0),
            None,
            "the database is fresh"
        );

        let rig = rig("supervisor-cold-real-database");
        let runner = says(declares(A, 1));
        let facts = StatusFile::new(state_dir(&rig).join("status.json"));
        let m = host_migrator_with(
            &runner,
            Database::new(config.clone(), schema::embedded()),
            &facts,
            &rig.child,
        );
        let told = Told::default();
        let start = |child: &ProcChild| {
            let (gate, _runner) = gate();
            let env = Env::new(&rig, &gate);
            env.install(A);
            verify(&rig, A);
            let boot = Boot {
                migrator: &m,
                report: &told,
                ..env.boot()
            };
            let outcome = adopt_children(&gate, child, &state_dir(&rig), &boot);
            (outcome, child.recorded(Slot::A).map(|r| r.ident))
        };

        let (outcome, started) = start(&rig.child);
        let first = started.expect("the release started");
        let _stop = Cleanup(&rig.child, vec![first.clone()]);
        assert_eq!(outcome, Startup::Adopted { active: Slot::A });
        assert_eq!(schema::current(&config), Ok(1));
        kbf_state::postgres::PostgresStore::open(&config).expect("kbf opens on what was made");
        let made = rows(&mut client);
        assert_eq!(made.len(), 1, "{made:?}");

        // The host restarts: nothing runs, the next start finds the schema in place.
        rig.child.terminate(&first).expect("signal");
        wait_for("exit", || !alive(first.pid));
        let after = rig.restarted();
        let (outcome, started) = start(&after);
        let second = started.expect("started again");
        let _stop = Cleanup(&after, vec![second.clone()]);
        assert_eq!(outcome, Startup::Adopted { active: Slot::A });
        assert_ne!(first, second, "a new process");
        assert_eq!(
            rows(&mut client),
            made,
            "the second start changed the schema"
        );
        for (_, _, detail) in told.0.borrow().iter() {
            for secret in own.url.split(['/', '@', ':']).filter(|part| part.len() > 4) {
                assert!(
                    detail.as_deref().is_none_or(|d| !d.contains(secret)),
                    "{secret} in {detail:?}"
                );
            }
        }
    }
}
