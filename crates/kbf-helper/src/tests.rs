//! The helper's passes on a scratch host: the real installer verbs over a real file tree,
//! with the one thing a test cannot run (the attestation check of a stage) played by a
//! closure, and the service restart recorded as a step.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::{Root, Step};
use kbf_installer::activate;
use kbf_installer::attest::SourceRef;
use kbf_installer::layout::Layout;
use kbf_installer::refusal::{InboxFault, Refusal};
use kbf_installer::stage::Staged;
use kbf_installer::state::{Locked, State};
use kbf_installer::testkit::{A, B, c};

use crate::cycle::{Config, Helper};
use crate::restart::Restart;

const UNIT: &str = "kbf.service";
const T0: u64 = 1_800_000_000;

struct Rig {
    helper: Helper<ScratchHost>,
    root: PathBuf,
    layout: Layout,
    /// The restarts the start-up made (the links were missing and were made).
    base: usize,
}

fn order_json(id: u64, verb: &str, commit: &str) -> String {
    format!(
        "{{\"order_id\":{id},\"epoch\":1,\"verb\":\"{verb}\",\"commit\":\"{commit}\",\"window_s\":600}}"
    )
}

impl Rig {
    /// A host running release A, with release B installed beside it, an empty inbox, and
    /// the service restart recorded.
    fn new(name: &str) -> Rig {
        let mut host = ScratchHost::new(&format!("helper-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/lib/kbf-deploy",
            "var/lib/kbf-updater/inbox/orders",
            "usr/local/kbf/releases",
        ] {
            fs::create_dir_all(root.join(sub)).expect("seed");
        }
        let dir = root.join("usr/local/kbf/releases").join(A);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        host.answer = Box::new(|_| Ok(reply(0, "", "")));
        let layout = Layout::linux(Root::new(&root).expect("absolute"));
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(A)),
                ..State::default()
            })
            .expect("state");
        let cfg = Config {
            update_dir: kbf_host::HostPath::new("/var/lib/kbf-updater").expect("path"),
            restart: Restart::Systemd(UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        };
        let mut helper = Helper::new(layout.clone(), host, cfg);
        helper.recover().expect("recover");
        let mut rig = Rig {
            helper,
            root,
            layout,
            base: 0,
        };
        rig.cycle(T0 - 100);
        rig.base = rig.restarts();
        rig
    }

    fn orders(&self) -> PathBuf {
        self.root.join("var/lib/kbf-updater/inbox/orders")
    }

    fn put_order(&self, id: u64, body: &str) {
        fs::write(self.orders().join(format!("{id:020}.json")), body).expect("order");
    }

    fn status(&self) -> serde_json::Value {
        let bytes = fs::read(self.root.join("var/lib/kbf-updater/status.json")).expect("status");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn state(&self) -> State {
        kbf_installer::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    fn restarts(&self) -> usize {
        self.helper
            .exec()
            .log
            .iter()
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", UNIT]))
            .count()
            - self.base
    }

    fn cycle(&mut self, now: u64) {
        let mut never = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
            panic!("no offer is expected")
        };
        self.helper.cycle(now, &mut never).expect("a pass");
    }

    /// An inbox entry for `commit`, as the node's client names it.
    fn put_offer(&self, generation: u64, commit: &str) -> String {
        let name = format!("{generation:020}-{commit}");
        fs::create_dir_all(self.root.join("var/lib/kbf-updater/inbox").join(&name)).expect("entry");
        name
    }
}

fn staged(commit: &str) -> Staged {
    Staged {
        commit: c(commit),
        components: vec!["kbf"],
        source_ref: SourceRef::new("refs/heads/main").expect("ref"),
        protocol: 7,
        min_protocol: 6,
        state_schema: 3,
        version: "0.1.0".to_owned(),
        log: String::new(),
    }
}

/// A staged release B: the stage's own work is the installer's (its tests); here it marks
/// B staged in the state, as `release::stage` does.
fn stage_b(
    layout: Layout,
) -> impl FnMut(&mut ScratchHost, &std::path::Path) -> Result<Staged, Refusal> {
    move |_, _| {
        let dir = layout.join(&kbf_installer::layout::release_dir(&c(B)));
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        let lock = Locked::acquire(&layout).expect("lock");
        let mut state = lock.read().expect("reads").unwrap_or_default();
        state.staged = Some(c(B));
        lock.write(&state).expect("writes");
        Ok(staged(B))
    }
}

/// Catches an offer staged more than once: a refused entry would otherwise be fetched,
/// verified and refused again every few seconds, and an accepted one re-installed.
///
/// Mutant seen red: `handled_entry` not recorded (the stager runs on every pass).
#[test]
fn the_newest_entry_is_staged_once_and_a_refusal_is_reported_not_retried() {
    let mut rig = Rig::new("stage-once");
    let older = rig.put_offer(1, A);
    let newest = rig.put_offer(2, B);
    assert_ne!(older, newest);
    let mut calls = Vec::new();
    let mut inner = stage_b(rig.layout.clone());
    let mut stager = |exec: &mut ScratchHost, entry: &std::path::Path| {
        calls.push(
            entry
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
        );
        inner(exec, entry)
    };
    rig.helper.cycle(T0, &mut stager).expect("pass");
    rig.helper.cycle(T0 + 3, &mut stager).expect("pass");
    assert_eq!(
        calls,
        std::slice::from_ref(&newest),
        "only the newest, and only once"
    );
    assert_eq!(rig.status()["state"], "staged");
    assert_eq!(rig.status()["staged_commit"], B);
    assert_eq!(rig.status()["staged"]["protocol"], 7);

    let mut refuse = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
        Err(Refusal::Inbox {
            name: "offer.json".to_owned(),
            fault: InboxFault::Missing,
        })
    };
    let newer = format!("{:020}-{}", 3, "d".repeat(40));
    fs::create_dir(rig.root.join("var/lib/kbf-updater/inbox").join(&newer)).expect("entry");
    rig.helper.cycle(T0 + 6, &mut refuse).expect("pass");
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("inbox")
    );
    // The same entry again: not looked at (a refuse that panicked would show it).
    let mut again = |_: &mut ScratchHost, _: &std::path::Path| -> Result<Staged, Refusal> {
        panic!("a refused entry is not staged again")
    };
    rig.helper.cycle(T0 + 9, &mut again).expect("pass");
}

/// Catches the swap being skipped, the restart being skipped or repeated, and an order
/// acted on twice: the activate order moves the links, restarts the unit once, reports a
/// swap on trial, and the same order file put back (a replay) does nothing.
///
/// Mutants seen red: no restart step; the acknowledged id not recorded (the replay
/// restarts again); the order file not removed.
#[test]
fn an_activate_order_swaps_restarts_once_and_is_not_replayed() {
    let mut rig = Rig::new("activate");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    let links = |rig: &Rig, name: &str| {
        fs::read_link(rig.root.join("usr/local/kbf").join(name)).expect("link")
    };
    assert_eq!(
        links(&rig, "current"),
        PathBuf::from(format!("releases/{B}"))
    );
    assert_eq!(
        links(&rig, "previous"),
        PathBuf::from(format!("releases/{A}"))
    );
    assert_eq!(rig.restarts(), 1);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert_eq!(rig.status()["installed_commit"], B);
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert!(
        !rig.orders().join(format!("{:020}.json", 5)).exists(),
        "taken"
    );
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.restarts(), 1, "a replayed order restarts nothing");
    assert!(
        !rig.orders().join(format!("{:020}.json", 5)).exists(),
        "removed unread"
    );
}

/// Catches a confirm that does not keep the swap, and one that names the wrong swap: the
/// confirm order carries the commit, the helper finds the activation's own id.
///
/// Mutant seen red: the confirm passing the confirm order's id instead of the pending
/// swap's (NotPending-style refusal).
#[test]
fn a_confirm_order_keeps_the_swap_and_a_revert_order_goes_back() {
    let mut rig = Rig::new("confirm");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    rig.put_order(6, &order_json(6, "confirm", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.status()["state"], "confirmed");
    assert_eq!(rig.restarts(), 1);

    rig.put_order(7, &order_json(7, "revert", A));
    rig.cycle(T0 + 9);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.restarts(), 2, "a revert restarts the unit");
    assert_eq!(rig.status()["state"], "reverted");
}

/// Catches a swap left on trial forever: the Linux boot check starts the window at the
/// first start; the pass after the window reverts and restarts the unit.
///
/// Mutant seen red: `expire` not called in the pass.
#[test]
fn a_swap_not_confirmed_in_its_window_is_reverted() {
    let mut rig = Rig::new("expire");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    // The first start of B (the unit's ExecStartPre) starts the window.
    let layout = rig.layout.clone();
    let mut out = Vec::new();
    let decision = activate::boot_check(rig.helper.exec_mut(), &layout, T0 + 10, 600, &mut out)
        .expect("checks");
    assert!(matches!(decision, activate::Boot::Start { .. }));
    rig.cycle(T0 + 500);
    assert_eq!(rig.state().current, Some(c(B)), "inside the window");
    rig.cycle(T0 + 10 + 601);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.status()["state"], "reverted");
    assert_eq!(rig.restarts(), 2);
}

/// Catches a restart lost on the way: when the restart fails, the owed restart stays in
/// `helper.json` and the next pass retries, and the node is told why in the meantime.
///
/// Mutant seen red: `restart_due` cleared before the restart is tried.
#[test]
fn a_failed_restart_is_owed_until_it_succeeds() {
    let mut rig = Rig::new("restart-owed");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    // Fail the first restart only.
    let tries = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = tries.clone();
    rig.helper_exec_mut().answer = Box::new(move |_| {
        seen.set(seen.get() + 1);
        if seen.get() == 1 {
            Ok(reply(1, "", "unit not found"))
        } else {
            Ok(reply(0, "", ""))
        }
    });
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(
        rig.status()["refusal"]
            .as_str()
            .expect("text")
            .contains("restart failed")
    );
    rig.cycle(T0 + 6);
    assert_eq!(tries.get(), 2, "retried on the next pass, once");
    rig.cycle(T0 + 9);
    assert_eq!(tries.get(), 2, "and not again");
}

impl Rig {
    fn helper_exec_mut(&mut self) -> &mut ScratchHost {
        self.helper.exec_mut()
    }
}

/// Catches a floor moved by garbage: a file that is not an order is refused, removed and
/// does not acknowledge anything, so the real order after it (a lower id) still runs. A
/// link in the orders directory is removed as a link; what it points at is untouched.
///
/// Mutants seen red: acknowledging an unparsable file's id (the real order is then
/// ignored as old); removing through the link's target (the secret goes).
#[test]
fn a_file_that_is_not_an_order_moves_nothing() {
    let mut rig = Rig::new("garbage");
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0, &mut stager).expect("stage");
    let secret = rig.root.join("secret");
    fs::write(&secret, b"not yours").expect("secret");
    rig.put_order(900, "not json");
    symlink(&secret, rig.orders().join(format!("{:020}.json", 901))).expect("link");
    rig.put_order(902, &order_json(903, "activate", B));
    rig.put_order(903, &order_json(903, "reboot", B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.status()["acked_order_id"], 0);
    assert_eq!(fs::read(&secret).expect("kept"), b"not yours");
    assert_eq!(
        fs::read_dir(rig.orders()).expect("dir").count(),
        0,
        "all removed"
    );
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.restarts(), 1);
}

/// Catches the node never reading the helper: what the helper writes is what `kbf-update`
/// reads, in every state the helper reports.
#[test]
fn status_json_is_read_by_the_node_client() {
    let mut rig = Rig::new("status-contract");
    let status = rig.root.join("var/lib/kbf-updater/status.json");
    let read = || kbf_update::helper::read(&status);
    rig.cycle(T0);
    let kbf_update::helper::Helper::Reported(idle) = read() else {
        panic!("reported")
    };
    assert_eq!(idle.state, kbf_update::helper::HelperState::Idle);
    assert_eq!(idle.installed.as_deref(), Some(A));
    rig.put_offer(1, B);
    let mut stager = stage_b(rig.layout.clone());
    rig.helper.cycle(T0 + 3, &mut stager).expect("stage");
    let kbf_update::helper::Helper::Reported(staged) = read() else {
        panic!("reported")
    };
    assert_eq!(staged.state, kbf_update::helper::HelperState::Staged);
    assert_eq!(staged.staged.as_deref(), Some(B));
    assert_eq!(staged.staged_facts.expect("facts").protocol, 7);
    rig.put_order(5, &order_json(5, "activate", B));
    rig.cycle(T0 + 6);
    let kbf_update::helper::Helper::Reported(on_trial) = read() else {
        panic!("reported")
    };
    assert_eq!(
        on_trial.state,
        kbf_update::helper::HelperState::ActivatedUnconfirmed
    );
    assert_eq!(on_trial.acked_order_id, 5);
    assert!(on_trial.holds(B) && on_trial.holds(A));
}

/// Catches a restart that runs the wrong program or the wrong unit: argv is fixed by the
/// helper's own flag, an absolute program, and nothing else (a unit name with a space or
/// an option-looking prefix stays one argument); a host with no restart runs nothing.
///
/// Mutant seen red: the step returned as `None` (also `a_failed_restart_is_owed...`).
#[test]
fn the_restart_is_one_fixed_command() {
    let program = |r: Restart| match r.step() {
        Some(Step::Run(cmd)) => (cmd.program.as_str().to_owned(), cmd.args),
        other => panic!("not a command: {other:?}"),
    };
    assert_eq!(
        program(Restart::Systemd("kbf.service".to_owned())),
        (
            "/usr/bin/systemctl".to_owned(),
            vec!["restart".to_owned(), "kbf.service".to_owned()]
        )
    );
    assert_eq!(
        program(Restart::Launchd("com.komira.kbf-daemon".to_owned())),
        (
            "/bin/launchctl".to_owned(),
            vec![
                "kickstart".to_owned(),
                "-k".to_owned(),
                "system/com.komira.kbf-daemon".to_owned()
            ]
        )
    );
    assert!(Restart::Off.step().is_none());
}
