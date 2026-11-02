//! The command line of `activate`: what it takes, who may run it, and what it prints, audits
//! and exits with.

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::ExitCode;

use kbf_retain::{CommitId, Kept};
use kbf_testkit::dir::TestDir;

use super::activate_cli::{Command, LINUX_HOLD, MAC_HOLD, Place, main_with, run};
use super::answer::{Answer, Code};
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::request::Target;
use crate::request::{Channel, Version};
use crate::trust::Trust;

fn args(line: &str) -> Vec<String> {
    line.split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The target, the flags in any order and the units are read; each flag but `--restart-unit`
/// once, `kbf=` once and required; a unit is a kbf unit.
///
/// Catches: a flag taken twice silently; a command with no release; a unit outside kbf's.
#[test]
fn the_command_line_is_read() {
    let command = Command::parse(&args(
        "--force-lock kbf=bbbbbbbbbbbb --restart-unit kbf-daemon.service --pin \
         --restart-unit kbf-frontend@a.service",
    ))
    .unwrap();
    assert!(matches!(&command.target, Target::Commit(c) if c == "bbbbbbbbbbbb"));
    assert!(command.how.pin && command.how.force_lock);
    let units: Vec<&str> = command.units.iter().map(|u| u.as_str()).collect();
    assert_eq!(units, ["kbf-daemon.service", "kbf-frontend@a.service"]);
    let plain = Command::parse(&args("kbf=0.1.7+gbbbbbbbbbbbb-3")).unwrap();
    assert!(!plain.how.pin && !plain.how.force_lock && plain.units.is_empty());

    for (line, why) in [
        ("", "kbf=<commit|version> is required"),
        ("--pin", "kbf=<commit|version> is required"),
        ("kbf=bbbbbbbbbbbb --pin --pin", "--pin is given twice"),
        (
            "kbf=bbbbbbbbbbbb --force-lock --force-lock",
            "--force-lock is given twice",
        ),
        ("kbf=bbbbbbbbbbbb kbf=cccccccccccc", "kbf= is given twice"),
        (
            "kbf=bbbbbbbbbbbb --restart-unit",
            "--restart-unit needs a value",
        ),
        (
            "kbf=bbbbbbbbbbbb --restart-unit sshd.service",
            "--restart-unit \"sshd.service\" is not a kbf unit",
        ),
        ("kbf=bbbbbbbbbbbb --yes", "unknown argument \"--yes\""),
        (
            "kbf=BBBB",
            "\"BBBB\" is neither a commit (12 to 40 hex digits) nor a version",
        ),
    ] {
        assert_eq!(Command::parse(&args(line)).unwrap_err(), why, "{line:?}");
    }
}

/// Only root runs it, and arguments that do not parse are refused first; the platform's run
/// is asked only for root with a sound command, and its status is the exit status.
///
/// Catches: an activate run for an account that is not root.
#[test]
fn only_root_with_a_sound_command_reaches_the_run() {
    let ran = Cell::new(0);
    let go = |_: &Command| {
        ran.set(ran.get() + 1);
        7
    };
    assert_eq!(main_with(&args("--pin"), 0, &go), ExitCode::from(2));
    assert_eq!(
        main_with(&args("kbf=bbbbbbbbbbbb"), 1000, &go),
        ExitCode::from(2)
    );
    assert_eq!(ran.get(), 0);
    assert_eq!(
        main_with(&args("kbf=bbbbbbbbbbbb"), 0, &go),
        ExitCode::from(7)
    );
    assert_eq!(ran.get(), 1);
    assert_eq!(LINUX_HOLD, "/var/lib/kbf-updater/pin.json");
    assert_eq!(MAC_HOLD, "/var/db/kbf-updater/pin.json");
}

/// A host with release A current and B kept, which swaps as it is asked.
#[derive(Clone)]
struct Two {
    lock: PathBuf,
    fault: Option<Fault>,
}

impl Backend for Two {
    fn lock_file(&self) -> PathBuf {
        self.lock.clone()
    }
    fn node_id(&self) -> Option<String> {
        None
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        Ok(Some("a".repeat(40)))
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(None)
    }
    fn has(&self, _commit: &str) -> bool {
        true
    }
    fn stage(&self, _version: &Version, _channel: &Channel) -> Result<String, Fault> {
        Err(Fault::refused(Code::UnknownRelease, "-"))
    }
    fn switch(&self, _commit: &str, _window_secs: u64, _now: u64) -> Result<u64, Fault> {
        Err(Fault::refused(Code::NotStaged, "-"))
    }
    fn confirm(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        Ok(())
    }
    fn activate(&self, _commit: &str) -> Result<(), Fault> {
        self.fault.clone().map_or(Ok(()), Err)
    }
    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        Ok(vec![
            Kept {
                commit: CommitId::new(&"b".repeat(40)).unwrap(),
                version: Some("0.2.0+gbbbbbbbbbbbb-1".to_owned()),
                why: Vec::new(),
                version_left_out: false,
            },
            Kept {
                commit: CommitId::new(&"a".repeat(40)).unwrap(),
                version: None,
                why: vec!["current", "pinned"],
                version_left_out: false,
            },
        ])
    }
}

/// What a run prints and exits with: the answer's line on stdout; on stderr the detail and,
/// for a release not kept, each kept release with its version and why it stays; 0 for done, 2
/// for refused, 3 for failed. An audit log that cannot be opened runs nothing, fails, and says
/// so on both streams.
///
/// Catches: a refusal whose listing does not reach the operator; an activate run with no audit
/// log; a failure that exits 0.
#[test]
fn a_run_prints_the_answer_and_the_kept_releases_and_exits_by_it() {
    let dir = TestDir::under(std::env::temp_dir(), "acli-run");
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let uid = fs::metadata(&*dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &dir);
    let audit = dir.join("audit.log");
    let place = Place {
        trust: &trust,
        audit: &audit,
    };
    let ab = |fault: Option<Fault>| {
        Ab::new(
            Two {
                lock: dir.join("lock"),
                fault,
            },
            dir.join("state.json"),
            trust.clone(),
            600,
            Box::new(|| 7),
        )
    };
    let go = |ab: &Ab<Two>, line: &str| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            &|target, how| ab.activate(target, how),
            &Command::parse(&args(line)).unwrap(),
            &place,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    };
    let (code, out, err) = go(&ab(None), "kbf=bbbbbbbbbbbb");
    assert_eq!(code, 0);
    assert!(out.starts_with("{\"result\":\"activated\""), "{out}");
    assert_eq!(err, "");

    let (code, out, err) = go(&ab(None), "kbf=cccccccccccc");
    assert_eq!(code, 2);
    assert!(
        out.starts_with("{\"result\":\"refused\",\"reason\":\"not_kept\""),
        "{out}"
    );
    assert_eq!(
        err,
        format!(
            "kbf-host-install: activate: \"cccccccccccc\" names no release this host keeps\n  \
             kept: {b} 0.2.0+gbbbbbbbbbbbb-1\n  kept: {a} - (current, pinned)\n",
            a = "a".repeat(40),
            b = "b".repeat(40)
        )
    );

    let failing = ab(Some(Fault::failed(Code::RestartFailed, "down")));
    let (code, out, err) = go(&failing, "kbf=bbbbbbbbbbbb");
    assert_eq!(code, 3);
    assert_eq!(
        out,
        "{\"result\":\"failed\",\"reason\":\"restart_failed\"}\n"
    );
    assert_eq!(err, "kbf-host-install: activate: down\n");
    let lines = fs::read_to_string(&audit).unwrap();
    assert_eq!(lines.lines().count(), 3);

    fs::remove_file(&audit).unwrap();
    fs::create_dir(&audit).unwrap();
    let (code, out, err) = go(&ab(None), "kbf=bbbbbbbbbbbb");
    assert_eq!(code, 3);
    assert_eq!(
        out,
        "{\"result\":\"failed\",\"reason\":\"audit_unavailable\"}\n"
    );
    assert!(
        err.starts_with("kbf-host-install: audit log unavailable: "),
        "{err}"
    );
    assert!(err.contains("cannot be opened"), "{err}");
    assert!(
        !matches!(
            ab(None).activate(&Target::parse("bbbbbbbbbbbb").unwrap(), Default::default()),
            Answer::Failed(..)
        ),
        "the host itself was fine"
    );
}

/// The program as root runs it on this platform: arguments that do not parse are refused before
/// the account is asked, and on a Linux host a command with no `--restart-unit` is refused
/// before any of the host's files is read.
///
/// Catches: a Linux activate that restarts no unit.
#[cfg(target_os = "linux")]
#[test]
fn the_program_refuses_before_it_reads_the_host() {
    assert_eq!(super::activate_cli::main(&args("--pin")), ExitCode::from(2));
    assert_eq!(
        main_with(
            &args("kbf=bbbbbbbbbbbb"),
            0,
            &super::activate_cli::production
        ),
        ExitCode::from(2)
    );
}

/// `unpin` takes no argument and is root's: an argument, or another account, is refused (exit
/// 2) before anything runs; root with none runs it and exits with its status.
///
/// Catches: an unpin run by the agent's account; an argument taken for something it is not.
#[test]
fn only_root_with_no_argument_reaches_the_unpin() {
    use super::unpin_cli;
    let ran = std::cell::Cell::new(0);
    let go = || {
        ran.set(ran.get() + 1);
        0
    };
    assert_eq!(
        unpin_cli::main_with(&args("kbf=bbbbbbbbbbbb"), 0, &go),
        ExitCode::from(2)
    );
    assert_eq!(unpin_cli::main_with(&[], 1000, &go), ExitCode::from(2));
    assert_eq!(ran.get(), 0);
    assert_eq!(unpin_cli::main_with(&[], 0, &go), ExitCode::from(0));
    assert_eq!(ran.get(), 1);
}
