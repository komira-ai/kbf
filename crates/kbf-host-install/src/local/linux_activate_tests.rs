//! `activate` on a Linux host: the installer's layout over a scratch root, the units played by
//! the test's `systemctl`, the release source cut off.

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use kbf_installer::CommitId;
use kbf_installer::retain::{self, Retained};

use super::{A, Ab, Answer, Borrowed, Rig, Trust, binary};
use crate::host::take_lock;
use crate::local::activate_cli::{self, Command};
use crate::local::backend::Backend;
use crate::local::core::activate::{How, LOCK_TAKES};
use crate::local::production::LinuxEnv;
use crate::local::request::Target;
use crate::locks;

/// The release kept beside A, and its version.
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const VB: &str = "0.1.6+gbbbbbbbbbbbb-1";
/// A process id no process has: past the largest a kernel gives.
const GONE: u32 = i32::MAX as u32;
/// The clock of the core: every record names this time.
const NOW: u64 = 1_000;

type Notes = Rc<RefCell<Vec<String>>>;

fn id(commit: &str) -> CommitId {
    CommitId::new(commit).unwrap()
}

/// Set the time the release directory `commit` was laid out, in seconds after an epoch of the
/// test's own: the listing is newest first.
fn age(rig: &Rig, commit: &str, at: u64) {
    fs::File::open(rig.releases().join(commit))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + at))
        .unwrap();
}

/// Lay release `commit` out beside A, newer than it, with `version` recorded for it.
fn keep(rig: &Rig, commit: &str, version: Option<&str>, at: u64) {
    let dir = rig.releases().join(commit);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("kbf"), binary(commit)).unwrap();
    age(rig, A, 1);
    age(rig, commit, at);
    if let Some(version) = version {
        Retained::update(&retain::path(&rig.layout), |r| {
            r.versions.insert(id(commit), version.to_owned());
        })
        .unwrap();
    }
}

/// The release source is unreachable: every listing and download fails.
fn cut_off(rig: &Rig) {
    let mut net = rig.net.borrow_mut();
    net.broken.insert("list-fails");
    net.broken.insert("download-fails");
}

/// The agent's state directory, where the hold file goes.
fn agent_dir(rig: &Rig) -> std::path::PathBuf {
    let dir = rig.dir.join("agent");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn core(rig: &Rig) -> (Ab<Borrowed<'_>>, Notes) {
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let notes: Notes = Rc::default();
    let told = Rc::clone(&notes);
    let ab = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(uid, &rig.dir),
        600,
        Box::new(|| NOW),
    )
    .keeping(2, Box::new(move |t| told.borrow_mut().push(t.to_owned())))
    .holding(agent_dir(rig).join("pin.json"));
    (ab, notes)
}

fn target(text: &str) -> Target {
    Target::parse(text).unwrap()
}

const PIN: How = How {
    pin: true,
    force_lock: false,
};
const FORCE: How = How {
    pin: false,
    force_lock: true,
};

fn current_and_previous(rig: &Rig) -> (Option<String>, Option<String>) {
    let state = rig.state();
    (
        state.current.map(|c| c.to_string()),
        state.previous.map(|c| c.to_string()),
    )
}

fn activated(commit: &str, version: Option<&str>, from: &str) -> Answer {
    Answer::Activated {
        commit: CommitId::new(commit).unwrap(),
        version: version.map(str::to_owned),
        from: Some(CommitId::new(from).unwrap()),
        pinned: false,
        lock_taken: false,
    }
}

/// `answer`, an [`Answer::Activated`], with its pin and its take of the lock set.
fn flagged(answer: Answer, pin: bool, taken: bool) -> Answer {
    match answer {
        Answer::Activated {
            commit,
            version,
            from,
            ..
        } => Answer::Activated {
            commit,
            version,
            from,
            pinned: pin,
            lock_taken: taken,
        },
        other => other,
    }
}

/// With the release source unreachable, `activate` by version swaps to the kept release B for
/// good: the state names B current, A previous and nothing on trial, the links follow, every
/// unit is restarted on B, and `query` tells the swap as the last update. Nothing is listed or
/// downloaded.
///
/// Catches: an activate that fetches; one that arms a trial; `previous` not the release left; a
/// unit not restarted.
#[test]
fn activate_swaps_to_a_kept_release_with_the_release_source_unreachable() {
    let rig = Rig::new("act-swap");
    keep(&rig, B, Some(VB), 2);
    cut_off(&rig);
    let (ab, _) = core(&rig);
    assert_eq!(
        ab.activate(&target(VB), How::default()),
        activated(B, Some(VB), A)
    );
    assert_eq!(
        current_and_previous(&rig),
        (Some(B.to_owned()), Some(A.to_owned()))
    );
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(rig.link("current").as_deref(), Some(B));
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
    assert!(
        rig.calls
            .borrow()
            .iter()
            .all(|c| c.program.ends_with("systemctl")),
        "nothing but systemctl ran"
    );
    let Answer::Queried(view) = ab.handle(&super::LocalRequest::Query) else {
        panic!("a query");
    };
    assert_eq!(view.commit.as_deref(), Some(B));
    assert_eq!(view.version.as_deref(), Some(VB));
    let last = view.last.unwrap();
    assert_eq!((last.target.as_str(), last.outcome), (VB, "succeeded"));
}

/// A release the host does not keep is refused with the releases it keeps, in the listing's
/// format and newest first; a name that fits two kept releases is refused the same way. Nothing
/// changes: the state, the links, the units.
///
/// Catches: a refusal without the listing; an activate that fetches what it does not keep;
/// the first of two matches taken.
#[test]
fn activate_refuses_a_release_not_kept_and_lists_the_kept_ones() {
    let rig = Rig::new("act-unkept");
    keep(&rig, B, Some(VB), 2);
    cut_off(&rig);
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target("cccccccccccc"), How::default());
    assert_eq!(
        answer.json_line(),
        format!(
            "{{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":[\
             {{\"commit\":\"{B}\",\"version\":\"{VB}\"}},\
             {{\"commit\":\"{A}\",\"version\":null,\"why\":[\"current\"]}}]}}"
        )
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        "\"cccccccccccc\" names no release this host keeps"
    );

    let twin = format!("{}{}", &B[..12], "e".repeat(28));
    keep(&rig, &twin, None, 3);
    let answer = ab.activate(&target(&B[..12]), How::default());
    assert!(
        answer
            .json_line()
            .starts_with("{\"result\":\"refused\",\"reason\":\"ambiguous_release\",\"releases\":["),
        "{}",
        answer.json_line()
    );
    assert_eq!(
        current_and_previous(&rig),
        (Some(A.to_owned()), None),
        "nothing moved"
    );
    assert!(rig.systemctl().is_empty());
    assert!(!rig.dir.join(LOCK_TAKES).exists());
}

/// The swap executes nothing of the release it leaves: no step of the layout runs a program, no
/// program is asked about anything, the installer's runner starts nothing, and the one program
/// run (`systemctl`) names nothing under `releases/<A>`. The release left is as it was.
///
/// Catches: a swap that asks the release being left anything (its version, a check) by running
/// it.
#[test]
fn activate_executes_nothing_under_the_release_it_leaves() {
    let rig = Rig::new("act-noexec");
    keep(&rig, B, Some(VB), 2);
    cut_off(&rig);
    let left = rig.releases().join(A);
    let before = fs::read(left.join("kbf")).unwrap();
    let (ab, _) = core(&rig);
    assert_eq!(
        ab.activate(&target(B), How::default()),
        activated(B, Some(VB), A)
    );
    let exec = rig.backend.exec.borrow();
    assert!(
        !exec.log.iter().any(|s| matches!(
            s,
            kbf_host::step::Step::Run(_) | kbf_host::step::Step::Capture { .. }
        )),
        "a step ran a program: {:?}",
        exec.log
    );
    assert!(exec.queries.borrow().is_empty(), "a program was asked");
    assert!(rig.installer.borrow().is_empty(), "the installer ran");
    let left_text = left.display().to_string();
    for call in rig.calls.borrow().iter() {
        assert!(call.program.ends_with("systemctl"), "{call:?}");
        assert!(
            !call.program.contains(&left_text) && !call.args.iter().any(|a| a.contains(A)),
            "{call:?} names the release left"
        );
    }
    assert_eq!(fs::read(left.join("kbf")).unwrap(), before);
}

/// A live holder of the install lock is refused and named, and nothing changes; a holder whose
/// process is gone is named so, with what takes its lock; a holder that wrote no id is named so.
///
/// Catches: an activate that waits for the lock or runs beside its holder; a refusal that does
/// not say who holds it.
#[test]
fn activate_refuses_a_live_lock_holder_and_names_it() {
    let rig = Rig::new("act-busy");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    let path = rig.backend.lock_file();
    let held = take_lock(&path).unwrap();
    let answer = ab.activate(&target(B), How::default());
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"refused\",\"reason\":\"busy\"}"
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        format!(
            "the install lock {} is held by pid {}, which is alive: let it end or stop it, or \
             take it from a stuck holder with --force-lock",
            path.display(),
            std::process::id()
        )
    );
    drop(held);
    locks::wait_lockable(&path);

    let holder = hold_as(&path, "");
    let answer = ab.activate(&target(B), How::default());
    assert_eq!(
        answer.log_detail().unwrap(),
        format!(
            "the install lock {} is held by a process that wrote no id: take it with --force-lock",
            path.display()
        )
    );
    drop(holder);
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(rig.systemctl().is_empty());
}

/// The lock file at `path` held by an open file of the test's, whose content is `pid`.
fn hold_as(path: &std::path::Path, pid: &str) -> fs::File {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.try_lock().unwrap();
    file.write_all(pid.as_bytes()).unwrap();
    file
}

/// A dead holder's lock is refused without `--force-lock`, saying it is gone; with it, the lock
/// is taken (the lock file made anew, so the old holder keeps only the file it had), the take is
/// appended to the record beside the state (root's, 0600) before the swap, and the swap is
/// done. A stuck holder that is alive is taken from the same way, and recorded as alive.
///
/// Catches: `--force-lock` that does not take a held lock; a take that is not recorded; a take
/// recorded with the wrong holder.
#[test]
fn force_lock_takes_a_dead_holders_lock_and_records_the_take() {
    let rig = Rig::new("act-force");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    let path = rig.backend.lock_file();
    let dead = hold_as(&path, &format!("{GONE}\n"));
    let answer = ab.activate(&target(VB), How::default());
    assert_eq!(
        answer.log_detail().unwrap(),
        format!(
            "the install lock {} is held by pid {GONE}, which is gone: take it with --force-lock",
            path.display()
        )
    );
    let old_inode = dead.metadata().unwrap().ino();

    let answer = ab.activate(&target(VB), FORCE);
    assert_eq!(answer, flagged(activated(B, Some(VB), A), false, true));
    let record = rig.dir.join(LOCK_TAKES);
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        format!(
            "{{\"at_unix\":{NOW},\"by\":\"activate\",\"target\":\"{VB}\",\"holder_pid\":{GONE},\
             \"holder_alive\":false}}\n"
        )
    );
    assert_eq!(fs::metadata(&record).unwrap().mode() & 0o7777, 0o600);
    assert_ne!(
        fs::metadata(&path).unwrap().ino(),
        old_inode,
        "a new lock file"
    );
    // Another open of the old file (through the descriptor's name) finds it still locked.
    let old = fs::File::open(format!("/proc/self/fd/{}", dead.as_raw_fd())).unwrap();
    assert!(old.try_lock().is_err(), "the old holder keeps its own file");
    assert!(!locks::flock_held(&path), "the activate let its lock go");

    // A stuck holder, alive, is taken from the same way.
    let stuck = take_lock(&path).unwrap();
    let answer = ab.activate(&target(A), FORCE);
    assert!(
        matches!(
            answer,
            Answer::Activated {
                lock_taken: true,
                ..
            }
        ),
        "{answer:?}"
    );
    drop(stuck);
    let lines = fs::read_to_string(&record).unwrap();
    assert_eq!(lines.lines().count(), 2);
    assert!(
        lines.lines().nth(1).unwrap().ends_with(&format!(
            "\"target\":\"{A}\",\"holder_pid\":{},\"holder_alive\":true}}",
            std::process::id()
        )),
        "{lines}"
    );

    // A lock no one holds is taken as usual, and no take is recorded.
    let answer = ab.activate(&target(VB), FORCE);
    assert_eq!(answer, activated(B, Some(VB), A));
    assert_eq!(fs::read_to_string(&record).unwrap(), lines);
}

/// `--pin` writes the hold file in the agent's directory (the release and its version, the
/// time and the verb; 0644) through whatever the directory's owner put in the way, and pins the
/// release in `retain.json`, so a prune with newer releases keeps it.
///
/// Catches: a pin that writes no hold file, or writes through a link the agent's account made;
/// a pinned release that a prune removes.
#[test]
fn pin_writes_the_hold_file_and_the_pinned_release_survives_a_prune() {
    let rig = Rig::new("act-pin");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    let agent = agent_dir(&rig);
    let elsewhere = rig.dir.join("elsewhere");
    fs::write(&elsewhere, "not the agent's").unwrap();
    std::os::unix::fs::symlink(&elsewhere, agent.join("pin.json")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, agent.join(".pin.json.new")).unwrap();
    assert_eq!(
        ab.activate(&target(VB), PIN),
        flagged(activated(B, Some(VB), A), true, false)
    );
    let hold = agent.join("pin.json");
    assert_eq!(
        fs::read_to_string(&hold).unwrap(),
        format!(
            "{{\"commit\":\"{B}\",\"version\":\"{VB}\",\"at_unix\":{NOW},\"by\":\"activate\"}}"
        )
    );
    let meta = fs::symlink_metadata(&hold).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
    assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "not the agent's");
    assert!(!agent.join(".pin.json.new").exists());
    let retained = Retained::read(&retain::path(&rig.layout)).unwrap();
    assert_eq!(retained.pinned.into_iter().collect::<Vec<_>>(), [id(B)]);

    // Back on A, with four newer releases: a prune keeping two keeps B, which is pinned.
    assert!(matches!(
        ab.activate(&target(A), How::default()),
        Answer::Activated { .. }
    ));
    for n in 0..4 {
        keep(&rig, &format!("{n}{}", "f".repeat(39)), None, 10 + n);
    }
    rig.backend.prune(2).unwrap();
    assert!(
        rig.releases().join(B).join("kbf").exists(),
        "the pin holds B"
    );
    assert!(!rig.releases().join(format!("0{}", "f".repeat(39))).exists());
}

/// An activate during a trial ends it: the release on trial becomes `previous`, nothing is on
/// trial, and a tick past the old deadline puts nothing back.
///
/// Catches: a trial marker that outlives the activate, whose deadline then reverts it.
#[test]
fn activate_ends_a_trial_and_no_deadline_reverts_it() {
    let rig = Rig::new("act-trial");
    keep(&rig, B, Some(VB), 2);
    let deadline = rig.backend.switch(B, 600, 100).unwrap();
    assert!(rig.state().pending_confirm.is_some());
    let ab = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(fs::metadata(&*rig.dir).unwrap().uid(), &rig.dir),
        600,
        Box::new(move || deadline + 1),
    );
    assert_eq!(
        ab.activate(&target(A), How::default()),
        Answer::Activated {
            commit: CommitId::new(A).unwrap(),
            version: None,
            from: Some(CommitId::new(B).unwrap()),
            pinned: false,
            lock_taken: false,
        }
    );
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(
        current_and_previous(&rig),
        (Some(A.to_owned()), Some(B.to_owned()))
    );
    assert_eq!(ab.tick(), None, "nothing to put back");
    assert_eq!(rig.link("current").as_deref(), Some(A));
}

/// Units that do not come up on the release named leave the host on it, failed with
/// `restart_failed` (and pinned, when asked): there is no going back to the release left, and a
/// unit after the one that failed is restarted all the same. A kept directory with no
/// binary is refused as not kept; links the layout cannot move fail with `state_unavailable`.
///
/// Catches: a failed restart answered by starting the release left; a swap to a directory with
/// nothing to run.
#[test]
fn what_the_host_cannot_do_is_told_and_a_failed_restart_does_not_go_back() {
    let rig = Rig::new("act-noup");
    keep(&rig, B, Some(VB), 2);
    rig.net
        .borrow_mut()
        .restart_fails
        .insert("kbf-daemon.service".to_owned());
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target(B), PIN);
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"failed\",\"reason\":\"restart_failed\"}"
    );
    assert!(
        rig.dir.join("agent/pin.json").exists(),
        "the release the links name is pinned"
    );
    assert!(
        answer
            .log_detail()
            .unwrap()
            .starts_with("the host runs bbbbbbbbbbbb now, but: systemctl restart"),
        "{answer:?}"
    );
    assert_eq!(
        current_and_previous(&rig),
        (Some(B.to_owned()), Some(A.to_owned()))
    );
    // The unit after the one that failed is restarted all the same (on B): none goes on
    // running the release left, and none is started on it.
    assert_eq!(
        rig.systemctl(),
        [
            "restart -- kbf-daemon.service",
            "restart -- kbf-frontend@a.service",
            "is-active -- kbf-frontend@a.service"
        ]
    );
    assert_eq!(rig.link("current").as_deref(), Some(B));

    let rig = Rig::new("act-nobinary");
    keep(&rig, B, None, 2);
    fs::remove_file(rig.releases().join(B).join("kbf")).unwrap();
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target(B), PIN);
    assert_eq!(
        answer.json_line(),
        format!(
            "{{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":[\
             {{\"commit\":\"{B}\",\"version\":null}},\
             {{\"commit\":\"{A}\",\"version\":null,\"why\":[\"current\"]}}]}}"
        )
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        format!("\"{B}\" names {B}, whose binary this host cannot run")
    );
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(!rig.dir.join("agent/pin.json").exists(), "nothing pinned");

    let rig = Rig::new("act-nolinks");
    keep(&rig, B, None, 2);
    let previous = rig.releases().join("previous");
    fs::create_dir(&previous).unwrap();
    fs::write(previous.join("x"), "in the way").unwrap();
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target(B), How::default());
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"failed\",\"reason\":\"state_unavailable\"}"
    );
    assert!(rig.systemctl().is_empty());
}

/// The command as root runs it on a Linux host: the units are required; a state directory that
/// cannot be made fails; otherwise it answers, exits 0, 2 or 3, and appends its audit line,
/// which says the release, the one left, the pin and the take of the lock.
///
/// Catches: an activate that is not audited; an audit line that does not say the lock was
/// taken.
#[test]
fn the_command_audits_what_it_did_on_a_linux_host() {
    let rig = Rig::new("act-cli");
    keep(&rig, B, Some(VB), 2);
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &rig.dir);
    let env = LinuxEnv {
        passwd: rig.dir.join("passwd"),
        state_dir: rig.dir.join("state"),
        state_name: "state.json".to_owned(),
        audit: rig.dir.join("audit.log"),
    };
    let hold = agent_dir(&rig).join("pin.json");
    let command = |line: &str| {
        let args: Vec<String> = line.split(' ').map(str::to_owned).collect();
        Command::parse(&args).unwrap()
    };
    let run = |line: &str| {
        activate_cli::on_linux(
            &command(line),
            || Borrowed(&rig.backend),
            &env,
            &trust,
            hold.clone(),
        )
    };
    assert_eq!(run(&format!("kbf={VB}")), 2, "no --restart-unit");
    assert!(!env.audit.exists());

    let _dead = hold_as(&rig.backend.lock_file(), &format!("{GONE}\n"));
    let line = format!("kbf={VB} --pin --force-lock --restart-unit kbf-daemon.service");
    assert_eq!(run(&line), 0);
    assert!(env.state_dir.join(LOCK_TAKES).exists());
    assert!(hold.exists());
    assert_eq!(run("kbf=cccccccccccc --restart-unit kbf-daemon.service"), 2);
    let audit: Vec<serde_json::Value> = fs::read_to_string(&env.audit)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let fields = |n: usize| {
        let mut entry = audit[n].clone();
        entry.as_object_mut().unwrap().remove("time");
        entry
    };
    assert_eq!(
        fields(0),
        serde_json::json!({
            "verb": "activate", "version": VB, "channel": null, "outcome": "activated",
            "reason": null, "result": format!("{B} from {A}; pinned; install lock taken"),
        })
    );
    assert_eq!(
        fields(1),
        serde_json::json!({
            "verb": "activate", "version": "cccccccccccc", "channel": null,
            "outcome": "refused", "reason": "not_kept",
        })
    );

    // A state directory that cannot be made is a failure, before anything is asked.
    let blocked = LinuxEnv {
        state_dir: rig.dir.join("passwd"),
        ..env
    };
    fs::write(&blocked.state_dir, "a file").unwrap();
    fs::set_permissions(&blocked.state_dir, fs::Permissions::from_mode(0o644)).unwrap();
    let code = activate_cli::on_linux(
        &command("kbf=cccccccccccc --restart-unit kbf-daemon.service"),
        || Borrowed(&rig.backend),
        &blocked,
        &trust,
        hold.clone(),
    );
    assert_eq!(code, 3);
}

#[path = "linux_activate_more_tests.rs"]
mod more;
