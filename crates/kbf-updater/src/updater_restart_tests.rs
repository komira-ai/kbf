//! Restart, durability and corruption: a roll found on disk in each phase, the record's
//! write order, a crash at every write of a roll, and a `roll.json` that cannot be trusted.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::Updater;
use crate::api::{ErrorCode, Failure, HostRoll, Phase, Roll, RollState};
use crate::fakes::Event;
use crate::ports::{Installed, PortError};
use crate::record::{Durable, FORMAT, Record, RollStore, StoreError, roll_id};
use crate::updater_tests::{ORDER, Rig, all};

pub(crate) const OLD: &str = "1.0";
pub(crate) const TARGET: &str = "2.0";

/// A record of `all("2.0", "run-1")` with the given phases (one per host, in plan order).
pub(crate) fn record(phases: [(Phase, u32); 4], state: RollState) -> Record {
    let failed = phases.iter().position(|(p, _)| *p == Phase::Failed);
    Record {
        format: FORMAT,
        seq: 1,
        request: all(TARGET, "run-1"),
        roll: Roll {
            id: roll_id(1),
            state,
            hosts: ORDER
                .iter()
                .zip(phases)
                .map(|(alias, (phase, attempts))| HostRoll {
                    alias: (*alias).to_owned(),
                    phase,
                    attempts,
                    detail: None,
                })
                .collect(),
            failure: failed.map(|i| Failure {
                alias: ORDER[i].to_owned(),
                code: ErrorCode::InstallFailed,
                detail: "no".to_owned(),
            }),
        },
        resumes: 0,
        abort_requested: false,
    }
}

use Phase::{Checking, Done, Failed, Installing, Pending, WaitingHealthy};

/// Put `rec` on the rig's disk, as a process that stopped there would have left it, and open
/// a new updater over it with the hosts as `installed` says.
pub(crate) fn restart(rig: &Rig, rec: &Record) -> Updater {
    rig.store().save(rec).expect("save");
    rig.log.clear();
    rig.open()
}

fn phases_of(roll: &Roll) -> Vec<Phase> {
    roll.hosts.iter().map(|h| h.phase).collect()
}

fn ev(kind: &str, alias: &str) -> Event {
    match kind {
        "ck" => Event::Check(alias.to_owned()),
        "in" => Event::Install(alias.to_owned(), TARGET.to_owned()),
        "hl" => Event::Health(alias.to_owned()),
        _ => Event::Query(alias.to_owned()),
    }
}

/// Catches a restart that redoes finished work or skips the host in hand: with the frontend
/// done and `st1` still pending, the roll continues at `st1` and the frontend is not asked
/// about again; the resume is counted once.
#[test]
fn a_restart_with_a_pending_host_continues_at_it() {
    let rig = Rig::new("r-pending");
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert!(rig.log.touching("fe").is_empty());
    assert_eq!(rig.log.installs(), ["st1", "dm", "st2"]);
    assert_eq!(rig.store().load().expect("l").expect("r").resumes, 1);
}

/// Catches a restart in `checking` skipping the check: safe-to-stop is asked again (the
/// cluster may have changed while the updater was down) and only then is the host installed.
#[test]
fn a_restart_in_checking_asks_safe_to_stop_again() {
    let rig = Rig::new("r-checking");
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Checking, 0), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    u.drive("roll-1").expect("drive");
    assert_eq!(
        &rig.log.touching("st1")[..3],
        &[ev("ck", "st1"), ev("in", "st1"), ev("hl", "st1")]
    );
}

/// Catches a blind reinstall after a restart in `installing`: the host is asked first, and
/// when it already has the target there is no install, the attempt counter is unchanged, and
/// the roll goes on to wait for health.
#[test]
fn installing_with_the_target_already_there_is_not_installed_again() {
    let rig = Rig::new("r-installing-target");
    rig.inst
        .set_installed("st1", Installed::Version(TARGET.into()));
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(
        &rig.log.touching("st1")[..2],
        &[ev("qy", "st1"), ev("hl", "st1")]
    );
    assert_eq!(roll.hosts[1].attempts, 1);
    assert_eq!(rig.log.installs(), ["dm", "st2"]);
}

/// Catches a restart that trusts "installing" meant "installed", and one that never repeats an
/// install: the host still reports the old version, so the install is run once more and the
/// attempt counter says two.
#[test]
fn installing_with_the_old_version_still_there_installs_once_more() {
    let rig = Rig::new("r-installing-old");
    rig.inst
        .set_installed("st1", Installed::Version(OLD.into()));
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(
        &rig.log.touching("st1")[..3],
        &[ev("qy", "st1"), ev("in", "st1"), ev("hl", "st1")]
    );
    assert_eq!(roll.hosts[1].attempts, 2);
}

/// Catches a repeat install that fails being swallowed: the rerun's failure fails the roll at
/// that host with the install's code.
#[test]
fn a_failed_repeat_install_fails_the_roll() {
    let rig = Rig::new("r-installing-fails");
    rig.inst
        .script_install("st1", vec![], Err(PortError("refused: older".into())));
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.failure.expect("f").code, ErrorCode::InstallFailed);
    assert!(rig.log.touching("dm").is_empty());
}

/// Catches a restart that installs on top of a running install: "in progress" is waited out
/// (polling the query, never installing), and when the target appears the roll goes on.
#[test]
fn an_install_in_progress_is_waited_for_not_repeated() {
    let rig = Rig::new("r-in-progress");
    rig.inst.script_query(
        "st1",
        vec![
            Ok(Installed::InProgress),
            Ok(Installed::InProgress),
            Ok(Installed::Version(TARGET.into())),
        ],
    );
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert!(!rig.log.installs().contains(&"st1".to_owned()));
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Catches an install "in progress" for ever holding the updater: past the cap the roll is
/// interrupted, not failed, and nothing is installed.
#[test]
fn an_install_in_progress_for_ever_interrupts_the_roll() {
    let rig = Rig::new("r-in-progress-forever");
    rig.inst.set_installed("st1", Installed::InProgress);
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    assert_eq!(rig.elapsed(), Duration::from_secs(900));
    assert!(
        roll.hosts[1]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("still in progress"))
    );
}

fn silent_for(rig: &Rig, errors: usize, then: Installed) {
    let mut answers: Vec<_> = (0..errors)
        .map(|_| Err(PortError::unreachable("ssh: timed out")))
        .collect();
    answers.push(Ok(then));
    rig.inst.script_query("st1", answers);
}

/// Catches the 120 s rule drifting by a poll either way: 24 unanswered queries (the last at
/// 115 s) still resume when the 25th answers at 120 s; the 25th unanswered one, at 120 s,
/// interrupts. The interrupted roll names the host, installs nothing and touches nothing
/// after it, and is on disk as interrupted.
#[test]
fn no_answer_for_120_seconds_interrupts_the_roll() {
    let rig = Rig::new("r-silent-ok");
    silent_for(&rig, 24, Installed::Version(TARGET.into()));
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    assert_eq!(u.drive("roll-1").expect("drive").state, RollState::Done);

    let rig = Rig::new("r-silent-interrupted");
    silent_for(&rig, 25, Installed::Version(TARGET.into()));
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    assert_eq!(roll.failure, None);
    assert_eq!(rig.elapsed(), Duration::from_secs(120));
    assert_eq!(phases_of(&roll), [Done, Installing, Pending, Pending]);
    assert!(
        roll.hosts[1]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("ssh: timed out"))
    );
    assert!(rig.log.installs().is_empty());
    for later in ["dm", "st2"] {
        assert!(rig.log.touching(later).is_empty());
    }
    assert_eq!(
        rig.store().load().expect("l").expect("r").roll.state,
        RollState::Interrupted
    );
}

/// Catches silence being counted from the wrong moment: an "in progress" answer is an answer,
/// so errors after it get a fresh 120 s, not the time since the restart.
#[test]
fn an_answer_resets_the_silence_clock() {
    let rig = Rig::new("r-answer-resets");
    let mut script = vec![];
    for _ in 0..20 {
        script.push(Err(PortError("t".into())));
    }
    script.push(Ok(Installed::InProgress));
    for _ in 0..20 {
        script.push(Err(PortError("t".into())));
    }
    script.push(Ok(Installed::Version(TARGET.into())));
    rig.inst.script_query("st1", script);
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    assert_eq!(u.drive("roll-1").expect("drive").state, RollState::Done);
}

/// Catches an interrupted roll not blocking: new rolls are 409 `roll_active` naming it, it
/// is not driven, and only an operator abort frees the updater.
#[test]
fn an_interrupted_roll_blocks_new_rolls_until_aborted() {
    let rig = Rig::new("r-interrupted-blocks");
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Interrupted,
        ),
    );
    let err = u.start(&all(TARGET, "run-2")).expect_err("blocked");
    assert_eq!(
        (err.code, err.roll_id.as_deref()),
        (ErrorCode::RollActive, Some("roll-1"))
    );
    assert_eq!(
        u.drive("roll-1").expect("as is").state,
        RollState::Interrupted
    );
    assert!(rig.log.events().is_empty());
    assert_eq!(u.abort("roll-1").expect("abort").state, RollState::Aborted);
    // And it survives yet another restart as aborted, not running.
    let again = rig.open();
    assert_eq!(
        again.current().expect("live").expect("c").state,
        RollState::Aborted
    );
    assert!(again.start(&all(TARGET, "run-2")).is_ok());
}

/// Catches a restart in `waiting_healthy` installing again: the install is over, so only the
/// health wait resumes (with a fresh bound).
#[test]
fn a_restart_while_waiting_healthy_only_waits() {
    let rig = Rig::new("r-waiting");
    rig.health.script(
        "st1",
        vec![Ok(crate::ports::Health::Waiting("x".into()))],
        Ok(crate::ports::Health::Healthy),
    );
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (WaitingHealthy, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.touching("st1"), [ev("hl", "st1"), ev("hl", "st1")]);
}

/// Catches a restart resuming past a failed host: a failed roll is returned as it is and no
/// port is called; and a new roll may start (a failed roll is over).
#[test]
fn a_failed_roll_is_never_resumed() {
    let rig = Rig::new("r-failed");
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Failed, 1), (Pending, 0), (Pending, 0)],
            RollState::Failed,
        ),
    );
    let roll = u.drive("roll-1").expect("as is");
    assert_eq!(roll.state, RollState::Failed);
    assert_eq!(phases_of(&roll), [Done, Failed, Pending, Pending]);
    assert!(rig.log.events().is_empty());
    assert_eq!(u.get("roll-1").expect("get"), roll);
}

/// Catches the resume counter: a fresh roll has none, each restart that resumes adds one, and
/// a restart that finds the roll already over adds none.
#[test]
fn each_restart_is_counted() {
    let rig = Rig::new("r-counter");
    let u = rig.open();
    rig.roll(&u);
    assert_eq!(rig.store().load().expect("l").expect("r").resumes, 0);

    let mut rec = record(
        [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Running,
    );
    rec.resumes = 3;
    let u = restart(&rig, &rec);
    u.drive("roll-1").expect("drive");
    assert_eq!(rig.store().load().expect("l").expect("r").resumes, 4);
    let again = rig.open();
    again.drive("roll-1").expect("as is");
    assert_eq!(rig.store().load().expect("l").expect("r").resumes, 4);
}

/// Counts saves, and fails every one after the first `limit`: a crash between two writes of a
/// roll, as far as the next process can tell (the temp file is left behind, as a kill would).
#[derive(Debug)]
struct Crash {
    saves: Arc<AtomicUsize>,
    limit: usize,
}

impl Durable for Crash {
    fn file(&self, _file: &File) -> std::io::Result<()> {
        if self.saves.fetch_add(1, Ordering::SeqCst) >= self.limit {
            return Err(std::io::Error::other("killed"));
        }
        Ok(())
    }

    fn dir(&self, _dir: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

fn crashing(rig: &Rig, limit: usize) -> (Updater, Arc<AtomicUsize>) {
    let saves = Arc::new(AtomicUsize::new(0));
    let store = RollStore::with_sync(
        &rig.dir,
        Box::new(Crash {
            saves: Arc::clone(&saves),
            limit,
        }),
    );
    (rig.open_with(store), saves)
}

/// Catches every ordering and idempotence defect of a restart at once by killing the updater
/// at each write of a whole roll (start, each phase change of each host, the final state) and
/// resuming from what is on disk with the hosts as the crash left them: the roll must finish
/// `done`, every host must be installed exactly once (a crash before the install is made up,
/// one after it is not repeated, because the host is asked first), hosts must be installed in
/// plan order, and the process's memory must equal its disk at the moment of the crash.
#[test]
fn a_crash_at_every_write_of_a_roll_is_resumed_without_a_repeat_or_a_skip() {
    let probe = Rig::new("crash-count");
    let (u, saves) = crashing(&probe, usize::MAX);
    probe.roll(&u);
    let total = saves.load(Ordering::SeqCst);
    assert_eq!(
        total,
        1 + 3 + 3 * 4 + 1,
        "start, fe x3, three nodes x4, done"
    );

    for limit in 0..total {
        let rig = Rig::new(&format!("crash-{limit}"));
        let (u, _) = crashing(&rig, limit);
        let outcome = u
            .start(&all(TARGET, "run-1"))
            .and_then(|s| u.drive(&s.roll.id));
        let err = outcome.expect_err("the crash stops the roll");
        assert_eq!(err.code, ErrorCode::StateLost, "limit {limit}");
        // Memory never ran ahead of disk.
        let on_disk = rig.store().load().expect("load").map(|r| r.roll);
        assert_eq!(u.memory(), on_disk, "limit {limit}");

        let u = rig.open();
        let roll = match u.current().expect("live") {
            Some(r) => u.drive(&r.id).expect("resume"),
            None => rig.roll(&u),
        };
        assert_eq!(roll.state, RollState::Done, "limit {limit}");
        assert!(
            roll.hosts.iter().all(|h| h.phase == Phase::Done),
            "limit {limit}"
        );
        assert_eq!(
            rig.log.installs(),
            ORDER,
            "limit {limit}: once each, in plan order"
        );
    }
}

/// Records what is on disk each time a sync is called.
#[derive(Debug)]
struct Observe {
    dir: std::path::PathBuf,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Durable for Observe {
    fn file(&self, file: &File) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode() & 0o777;
        let temp = fs::read_to_string(self.dir.join(".roll.json.tmp")).unwrap_or_default();
        let live = fs::read_to_string(self.dir.join("roll.json")).ok();
        let note = format!(
            "file mode={mode:o} temp_written={} live_is_temp={}",
            !temp.is_empty(),
            live.as_deref() == Some(temp.as_str())
        );
        self.seen.lock().expect("seen").push(note);
        Ok(())
    }

    fn dir(&self, dir: &Path) -> std::io::Result<()> {
        let temp_gone = !dir.join(".roll.json.tmp").exists();
        let live = dir.join("roll.json").exists();
        self.seen
            .lock()
            .expect("seen")
            .push(format!("dir temp_gone={temp_gone} live={live}"));
        Ok(())
    }
}

/// Catches a record that is not written the durable way: the temp file is complete, private
/// (0600) and flushed before the rename (the live file is still the old one when the file
/// sync runs), and the directory is flushed after it (the temp is gone and the live file
/// is there); a missing or reordered sync shows here, because a crash cannot be staged.
#[test]
fn a_save_is_temp_fsync_rename_dir_fsync_in_that_order_with_mode_0600() {
    let rig = Rig::new("durable-order");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = RollStore::with_sync(
        &rig.dir,
        Box::new(Observe {
            dir: rig.dir.clone(),
            seen: Arc::clone(&seen),
        }),
    );
    store
        .save(&record(
            [(Pending, 0), (Pending, 0), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ))
        .expect("1");
    store
        .save(&record(
            [(Installing, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ))
        .expect("2");
    assert_eq!(
        *seen.lock().expect("seen"),
        [
            "file mode=600 temp_written=true live_is_temp=false",
            "dir temp_gone=true live=true",
            "file mode=600 temp_written=true live_is_temp=false",
            "dir temp_gone=true live=true",
        ]
    );
    assert_eq!(
        store.load().expect("l").expect("r").roll.hosts[0].phase,
        Installing
    );
}

/// What a corrupt-record test expects of the open.
fn refused(rig: &Rig, why: &str) -> StoreError {
    let before = rig.log.events().len();
    let err = Updater::open(
        crate::updater_tests::inventory(),
        rig.store(),
        rig.ports(),
        crate::Timing::default(),
    )
    .err()
    .unwrap_or_else(|| panic!("opened over a record that is {why}"));
    assert_eq!(rig.log.events().len(), before, "{why}: a port was called");
    err
}

/// Catches an updater that starts over a roll.json it cannot read (fail closed): garbage,
/// an empty file, a record cut off in the middle, a field it does not know, a version the
/// wire rule refuses, another format, and a directory where the file should be. Each is a
/// refusal to start, with no port called, and with the right kind of error.
#[test]
fn a_corrupt_or_partial_roll_json_refuses_to_start() {
    let rig = Rig::new("corrupt");
    let good = serde_json::to_string_pretty(&record(
        [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
        RollState::Running,
    ))
    .expect("json");
    let live = rig.dir.join("roll.json");
    let cases: Vec<(&str, String)> = vec![
        ("garbage", "\u{0}\u{1}not json".into()),
        ("empty", String::new()),
        ("truncated", good[..good.len() / 2].to_owned()),
        ("a trailing half-write", format!("{good}{{")),
        (
            "an unknown field",
            good.replacen("\"seq\"", "\"extra\": 1, \"seq\"", 1),
        ),
        ("a bad version", good.replace("\"2.0\"", "\"-2.0\"")),
        (
            "another format",
            good.replace("\"format\": 1", "\"format\": 2"),
        ),
        ("json null", "null".into()),
    ];
    for (why, text) in cases {
        fs::write(&live, &text).expect("write");
        let err = refused(&rig, why);
        match (why, &err) {
            ("another format", StoreError::Inconsistent(m)) => assert!(m.contains("format 2")),
            ("another format", other) => panic!("{why}: {other}"),
            (_, StoreError::Corrupt(_)) => {}
            (_, other) => panic!("{why}: wrong error {other}"),
        }
    }
    fs::remove_file(&live).expect("rm");
    fs::create_dir(&live).expect("dir");
    assert!(matches!(refused(&rig, "a directory"), StoreError::Io(_)));
}

/// Catches a record that parses but lies, which would send a roll to the wrong host: a later
/// host in flight while an earlier one is pending, two hosts in flight, a done roll with
/// work left, a failed one without a failure or a failed host, a failure on a roll that is
/// not failed, a bad id, and no hosts. Each names what is wrong.
#[test]
fn a_record_that_contradicts_itself_refuses_to_start() {
    let rig = Rig::new("inconsistent");
    let run = RollState::Running;
    let mut bad_id = record([(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)], run);
    bad_id.roll.id = "roll-7".into();
    let mut no_hosts = record([(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)], run);
    no_hosts.roll.hosts.clear();
    let mut failure_on_running = record([(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)], run);
    failure_on_running.roll.failure = Some(Failure {
        alias: "st1".into(),
        code: ErrorCode::InstallFailed,
        detail: "x".into(),
    });
    let mut failed_without_failure = record(
        [(Done, 1), (Failed, 1), (Pending, 0), (Pending, 0)],
        RollState::Failed,
    );
    failed_without_failure.roll.failure = None;
    let mut failed_without_host = record(
        [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Failed,
    );
    failed_without_host.roll.failure = Some(Failure {
        alias: "st1".into(),
        code: ErrorCode::InstallFailed,
        detail: "x".into(),
    });
    let mut failed_host_no_failure =
        record([(Done, 1), (Failed, 1), (Pending, 0), (Pending, 0)], run);
    failed_host_no_failure.roll.failure = None;
    let cases: Vec<(&str, Record, &str)> = vec![
        (
            "failed, no failed host",
            failed_without_host,
            "failed without a failure and a failed host",
        ),
        (
            "failed host, no failure",
            failed_host_no_failure,
            "not failed",
        ),
        (
            "later host ahead",
            record(
                [(Pending, 0), (Installing, 1), (Pending, 0), (Pending, 0)],
                run,
            ),
            "after an unfinished host",
        ),
        (
            "two in flight",
            record(
                [(Done, 1), (Installing, 1), (Installing, 1), (Pending, 0)],
                run,
            ),
            "after an unfinished host",
        ),
        (
            "done after failed",
            record(
                [(Done, 1), (Failed, 1), (Done, 1), (Pending, 0)],
                RollState::Failed,
            ),
            "after an unfinished host",
        ),
        (
            "done with work left",
            record(
                [(Done, 1), (Done, 1), (Pending, 0), (Pending, 0)],
                RollState::Done,
            ),
            "done and a host is not",
        ),
        (
            "failed, no failure",
            failed_without_failure,
            "failed without a failure",
        ),
        (
            "failed host, running roll",
            record([(Done, 1), (Failed, 1), (Pending, 0), (Pending, 0)], run),
            "not failed",
        ),
        (
            "failure on a running roll",
            failure_on_running,
            "not failed",
        ),
        ("a different id", bad_id, "roll id roll-7"),
        ("no hosts", no_hosts, "no hosts"),
    ];
    for (why, rec, want) in cases {
        // Write the record bypassing no check: `save` does not validate, `load` does.
        rig.store().save(&rec).expect("save");
        match refused(&rig, why) {
            StoreError::Inconsistent(m) => assert!(m.contains(want), "{why}: {m}"),
            other => panic!("{why}: wrong error {other}"),
        }
    }
}

/// Catches a stale temp file (a kill between the temp write and the rename) breaking the next
/// start or the next write: the live file is what counts, and the next save replaces the temp.
#[test]
fn a_leftover_temp_file_is_ignored_and_overwritten() {
    let rig = Rig::new("temp-leftover");
    let rec = record(
        [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Running,
    );
    rig.store().save(&rec).expect("save");
    fs::write(rig.dir.join(".roll.json.tmp"), "{ half a rec").expect("tmp");
    let u = rig.open();
    assert_eq!(u.current().expect("live").expect("c").hosts[0].phase, Done);
    assert_eq!(u.drive("roll-1").expect("drive").state, RollState::Done);
    assert!(rig.store().load().expect("l").is_some());
}

/// Catches a missing roll.json being an error: a first start has no record and is fine.
#[test]
fn no_roll_json_is_a_first_start() {
    let rig = Rig::new("first-start");
    let u = rig.open();
    assert_eq!(u.current().expect("live"), None);
    assert!(rig.store().load().expect("load").is_none());
}

/// Catches a roll that goes on when the inventory no longer has the host the record names
/// (the file was edited between runs): it fails at that host with `unknown_alias` before any
/// call, and the hosts after it are untouched.
#[test]
fn a_host_that_left_the_inventory_fails_the_roll_before_it_is_touched() {
    let rig = Rig::new("r-host-gone");
    let mut rec = record(
        [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Running,
    );
    rec.roll.hosts[1].alias = "gone".into();
    let u = restart(&rig, &rec);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    let f = roll.failure.expect("failure");
    assert_eq!(
        (f.code, f.alias.as_str()),
        (ErrorCode::UnknownAlias, "gone")
    );
    assert!(rig.log.events().is_empty());
}

/// Catches a resume whose first write fails leaving the updater believing a thread still
/// drives (every later `drive` would then be refused as busy): the failure is `state_lost`,
/// the same on the next try, and the roll on disk is untouched with no port called.
#[test]
fn a_resume_that_cannot_record_itself_is_state_lost_and_leaves_the_roll_alone() {
    let rig = Rig::new("r-resume-unwritable");
    let rec = record(
        [(Done, 1), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Running,
    );
    rig.store().save(&rec).expect("save");
    let (u, _) = crashing(&rig, 0);
    for _ in 0..2 {
        assert_eq!(
            u.drive("roll-1").expect_err("lost").code,
            ErrorCode::StateLost
        );
        // Not left believing a thread drives (a reload is refused while one does).
        u.reload().expect("reload");
    }
    assert!(rig.log.events().is_empty());
    assert_eq!(rig.store().load().expect("l").expect("r"), rec);
}
