//! The updater's own update: stage beside, switch, restart; a binary that exits at start is
//! replaced by the previous one.

mod support;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_step_runner::Finished;
use kbf_step_runner::selfupdate::{
    FollowOn, Layout, START_LIMIT_BURST, fallback_unit, follow_on, main_dropin,
};
use support::Scratch;
use update_hook::{Class, Phase, Verdict};

fn binary(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    path
}

fn done(verdict: Verdict) -> Finished {
    Finished {
        verdict,
        reason: None,
    }
}

fn ok() -> Finished {
    done(Verdict::Succeeded {
        promotion_pending: false,
    })
}

/// Catches a staged binary that is not whole, not executable, or that moves a link: staging puts
/// the file in `versions/` with mode 0755 and changes neither `current` nor `previous`; a version
/// that is not a plain file name is refused.
#[test]
fn staging_adds_a_version_and_moves_no_link() {
    let s = Scratch::new("su-stage");
    let layout = Layout::new(&s.path().join("bin"));
    let src = binary(s.path(), "new", "#!new\n");
    layout.stage("1.2.3", &src).unwrap();
    let staged = layout.dir().join("versions/1.2.3");
    assert_eq!(fs::read_to_string(&staged).unwrap(), "#!new\n");
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(layout.current().unwrap(), None);
    assert_eq!(layout.previous().unwrap(), None);
    assert!(!layout.dir().join("versions/1.2.3.tmp").exists());
    for bad in ["", ".hidden", "a/b", "a b", "../x", "é"] {
        let err = layout.stage(bad, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
    }
    assert!(layout.stage("1.2.4", &s.path().join("absent")).is_err());
}

/// Catches a switch that loses the way back: `previous` becomes what `current` was, a switch to
/// the version already current changes nothing (so a repeat after a crash keeps the real previous
/// one), and a version that was never staged is refused.
#[test]
fn a_switch_remembers_the_previous_binary_and_is_idempotent() {
    let s = Scratch::new("su-switch");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v1".into()), None)
    );
    layout.switch("v2").unwrap();
    layout.switch("v2").unwrap();
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v2".into()), Some("v1".into()))
    );
    assert_eq!(fs::read_to_string(layout.current_path()).unwrap(), "b");
    assert_eq!(fs::read_to_string(layout.previous_path()).unwrap(), "a");
    assert!(layout.switch("v3").is_err());
    assert_eq!(
        layout.switch("../v1").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

/// Catches a flip back that does nothing, or flips twice: with no previous binary it reports
/// `false`; after a switch it points `current` at the previous one once.
#[test]
fn flipping_back_restores_the_previous_binary_once() {
    let s = Scratch::new("su-flip");
    let layout = Layout::new(&s.path().join("bin"));
    assert!(!layout.flip_back().unwrap());
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    layout.switch("v1").unwrap();
    layout.switch("v2").unwrap();
    assert!(layout.flip_back().unwrap());
    assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));
    assert!(!layout.flip_back().unwrap());
}

/// Catches a link move that trips on what a crash left: a temporary link from an earlier attempt
/// is replaced, and one that cannot be removed is an error rather than a silent skip; a `current`
/// that is not a link is an error too.
#[test]
fn leftovers_of_a_crash_are_replaced_and_odd_files_refused() {
    let s = Scratch::new("su-leftover");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("v1", &binary(s.path(), "a", "a")).unwrap();
    fs::write(layout.dir().join("current.tmp"), "stale").unwrap();
    layout.switch("v1").unwrap();
    assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));

    layout.stage("v2", &binary(s.path(), "b", "b")).unwrap();
    fs::create_dir(layout.dir().join("previous.tmp")).unwrap();
    assert!(layout.switch("v2").is_err());

    let other = Layout::new(&s.path().join("odd"));
    fs::create_dir_all(other.dir()).unwrap();
    fs::write(other.current_path(), "not a link").unwrap();
    assert!(other.current().is_err());
}

/// Catches a follow-on that restarts when it should not or not when it should: only a succeeded
/// update that is not already this version stages, switches and asks for the restart; a repeat
/// after a crash gives the same state; a failed update, or the new binary started, does nothing.
#[test]
fn the_follow_on_restarts_only_after_a_success_that_changes_the_binary() {
    let s = Scratch::new("su-follow");
    let layout = Layout::new(&s.path().join("bin"));
    let v1 = binary(s.path(), "a", "a");
    let v2 = binary(s.path(), "b", "b");
    layout.stage("v1", &v1).unwrap();
    layout.switch("v1").unwrap();

    for verdict in [
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release,
        },
        Verdict::Deferred {
            retry_after_secs: None,
        },
        Verdict::Halt,
    ] {
        assert_eq!(
            follow_on(&done(verdict), &layout, "v1", "v2", &v2).unwrap(),
            FollowOn::NotNeeded
        );
        assert_eq!(layout.current().unwrap(), Some("v1".to_owned()));
    }
    assert_eq!(
        follow_on(&ok(), &layout, "v2", "v2", &v2).unwrap(),
        FollowOn::NotNeeded
    );
    assert!(!layout.dir().join("versions/v2").exists());

    assert_eq!(
        follow_on(&ok(), &layout, "v1", "v2", &v2).unwrap(),
        FollowOn::Restart
    );
    assert_eq!(
        follow_on(&ok(), &layout, "v1", "v2", &v2).unwrap(),
        FollowOn::Restart
    );
    assert_eq!(
        (layout.current().unwrap(), layout.previous().unwrap()),
        (Some("v2".into()), Some("v1".into()))
    );
    let pending = done(Verdict::Succeeded {
        promotion_pending: true,
    });
    assert_eq!(
        follow_on(&pending, &layout, "v2", "v2", &v2).unwrap(),
        FollowOn::NotNeeded
    );
    assert!(follow_on(&ok(), &layout, "v2", "v3", &s.path().join("absent")).is_err());
}

fn system_binary(name: &str) -> PathBuf {
    ["/bin", "/usr/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.exists())
        .expect("a system binary")
}

/// Catches a fallback that leaves the host with a broken updater: a binary that exits at
/// start fails `START_LIMIT_BURST` starts of the link the unit runs; the flip the fallback unit
/// makes puts the previous binary back, and the next start succeeds.
#[test]
fn a_binary_that_exits_at_start_is_replaced_by_the_previous() {
    let s = Scratch::new("su-exits-at-start");
    let layout = Layout::new(&s.path().join("bin"));
    layout.stage("good", &system_binary("true")).unwrap();
    layout.switch("good").unwrap();
    let ran = Command::new(layout.current_path()).status().unwrap();
    assert!(ran.success());

    let new = system_binary("false");
    assert_eq!(
        follow_on(&ok(), &layout, "good", "bad", &new).unwrap(),
        FollowOn::Restart
    );
    for _ in 0..START_LIMIT_BURST {
        let started = Command::new(layout.current_path()).status().unwrap();
        assert!(!started.success());
    }
    assert!(layout.flip_back().unwrap());
    assert!(
        Command::new(layout.current_path())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(layout.current().unwrap(), Some("good".to_owned()));
}

/// Catches the unit texts drifting from what the fallback needs: the drop-in restarts the unit and
/// names the fallback after the burst; the fallback runs the previous binary's verb, then starts
/// the main unit again.
#[test]
fn the_units_run_the_fallback_after_the_burst() {
    assert_eq!(
        main_dropin("updater-fallback.service", START_LIMIT_BURST),
        "[Unit]\nOnFailure=updater-fallback.service\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nRestart=always\nRestartSec=1\n"
    );
    let layout = Layout::new(Path::new("/usr/libexec/kbf-updater"));
    assert_eq!(
        fallback_unit(&layout, "kbf-updater.service"),
        "[Unit]\nDescription=Put the previous updater binary back after the new one failed to start\n\n[Service]\nType=oneshot\nExecStart=/usr/libexec/kbf-updater/previous self-update-fallback --dir /usr/libexec/kbf-updater\nExecStartPost=/usr/bin/systemctl reset-failed kbf-updater.service\nExecStartPost=/usr/bin/systemctl start --no-block kbf-updater.service\n"
    );
}
