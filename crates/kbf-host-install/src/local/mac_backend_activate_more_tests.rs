//! `activate` on a Mac, continued: a kept release that cannot run, the apply job seen by launchd
//! alone, links that cannot be put back, the daemon's job failing in each way, the release that
//! runs or is on trial named again, what a pin protects, and a run that was interrupted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::super::{Shared, current, previous, rig};
use super::{
    Answer, Code, Fx, How, NEW, NEW_VERSION, OLD, Retained, Sim, activated, core, keep, mkdir,
    retain, target, write,
};
use crate::local::backend::Backend;
use crate::local::mac_backend::{MacBackend, not_selected_for_test};
use crate::mac::apply::{self, NotSelected};
use crate::mac::kit::{self, Links};
use crate::mac::layout::APPLY_LABEL;
use crate::mac::status::{self, ApplyState};

const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const VC: &str = "0.1.8+gcccccccccccc-1";

const PIN: How = How {
    pin: true,
    force_lock: false,
};

/// Nothing changed: the links as the kit had them, no marker made or removed, the daemon not
/// touched, no hold file.
fn unchanged(fx: &Fx, sim: &Sim) {
    assert_eq!((current(fx).as_deref(), previous(fx)), (Some(OLD), None));
    assert_eq!(sim.daemon_bootouts.get() + sim.daemon_bootstraps.get(), 0);
    assert!(!fx.dir.join("agent/pin.json").exists());
}

/// A kept release whose binary is missing, writable by others, or a link to another release's
/// binary is refused (`not_kept`, with the listing) before anything changes: the daemon is not
/// stopped for a release it cannot start.
///
/// Catches: a swap to a release directory with nothing root may run in it.
#[test]
fn a_kept_release_that_cannot_run_is_refused_before_anything_moves() {
    for how in ["missing", "untrusted", "redirected"] {
        let (fx, sim, _) = rig(&format!("mbam-{how}"));
        keep(&fx, NEW, NEW_VERSION, 2);
        let binary = fx.host.release(NEW).join("kbf");
        match how {
            "missing" => fs::remove_file(&binary).unwrap(),
            "untrusted" => fs::set_permissions(&binary, fs::Permissions::from_mode(0o777)).unwrap(),
            _ => {
                fs::remove_file(&binary).unwrap();
                std::os::unix::fs::symlink(fx.host.release(OLD).join("kbf"), &binary).unwrap();
            }
        }
        let answer = core(&fx, &sim).activate(&target(NEW), PIN);
        assert!(
            matches!(&answer, Answer::Unresolved { code: Code::NotKept, releases, .. }
                if releases.len() == 2),
            "{how}: {answer:?}"
        );
        unchanged(&fx, &sim);
    }
}

/// The ssh path's apply job holds no install lock, so launchd is asked about it whatever the
/// status file says: a job that runs with no status, with a status that is not one, or with a
/// stale `done`; a `launchctl` that cannot be run; a query that fails with another status than
/// "no such service", or is killed by a signal: all refuse the activate (busy), and nothing
/// changes. The fixture's "no such service" status is the one the check trusts.
///
/// Catches: an activate beside a running apply because its status was missing or stale.
#[test]
fn a_running_apply_job_is_found_by_launchd_whatever_its_status_says() {
    for case in [
        "no-status",
        "not-a-status",
        "stale-done",
        "no-launchd",
        "print-apply-fails",
        "print-apply-killed",
    ] {
        let (fx, sim, _) = rig(&format!("mbam-{case}"));
        keep(&fx, NEW, NEW_VERSION, 2);
        match case {
            "no-launchd" => sim.break_("launchctl"),
            "print-apply-fails" => sim.break_("print-apply-fails"),
            "print-apply-killed" => sim.break_("print-apply-killed"),
            _ => {
                sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
                sim.apply_running.set(true);
            }
        }
        match case {
            "not-a-status" => write(&fx.host.status_file(), 0o600, b"garbage\n"),
            "stale-done" => status::write(&fx.host, &ApplyState::Done).unwrap(),
            _ => {}
        }
        let answer = core(&fx, &sim).activate(&target(NEW), How::default());
        assert!(
            matches!(&answer, Answer::Refused(Code::Busy, _)),
            "{case}: {answer:?}"
        );
        unchanged(&fx, &sim);
    }
    assert_eq!(
        include_str!("../../tests/fixtures/mac/print_missing.code").trim(),
        status::PRINT_NO_SERVICE.to_string()
    );
}

/// When what follows the move of the links fails and the links cannot be put back either, the
/// failure says so (`revert_failed`, with what the links name); put back, it is the step's own
/// failure. Each way a select can fail is the fault it should be.
///
/// Catches: a failure that says the links are as they were when they are not.
#[test]
fn links_that_cannot_be_put_back_are_told_as_they_are() {
    let (fx, sim, _) = rig("mbam-stuck");
    keep(&fx, NEW, NEW_VERSION, 2);
    let before = kit::links(&fx.host).unwrap();
    let kit_dir = fx.host.kit.clone();
    let not = apply::select(&fx.host, &*sim, NEW, &before, || {
        fs::set_permissions(&kit_dir, fs::Permissions::from_mode(0o555)).unwrap();
        Err("pending.json: refused".to_owned())
    })
    .unwrap_err();
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        not,
        NotSelected::AfterLinks {
            why: "pending.json: refused".to_owned(),
            restored: false
        }
    );
    assert_eq!(current(&fx).as_deref(), Some(NEW), "the links name NEW");
    assert_eq!(sim.daemon_bootouts.get(), 0);
    let fault = not_selected_for_test(&fx.host, NEW, not);
    assert_eq!(fault.code, Code::RevertFailed);
    assert_eq!(
        fault.detail,
        format!(
            "pending.json: refused, and the links could not be put back: they are {:?}",
            Links {
                current: Some(NEW.to_owned()),
                previous: Some(OLD.to_owned())
            }
        )
    );
    let fault = not_selected_for_test(&fx.host, NEW, NotSelected::Links { restored: false });
    assert_eq!(fault.code, Code::RevertFailed);
    assert!(
        fault
            .detail
            .starts_with("links_failed, and the links could not be put back")
    );
    let fault = not_selected_for_test(&fx.host, NEW, NotSelected::Links { restored: true });
    assert_eq!(
        (fault.code, fault.detail.as_str()),
        (Code::SwitchFailed, "links_failed")
    );
    let back = NotSelected::AfterLinks {
        why: "x".to_owned(),
        restored: true,
    };
    let fault = not_selected_for_test(&fx.host, NEW, back);
    assert_eq!(
        (fault.code, fault.detail.as_str()),
        (Code::StateUnavailable, "x")
    );
    // Links that cannot be read are told as such.
    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink("/elsewhere", fx.host.current()).unwrap();
    let fault = not_selected_for_test(&fx.host, NEW, NotSelected::Links { restored: false });
    assert!(
        fault.detail.ends_with("does not name a release"),
        "{fault:?}"
    );
}

/// Each way the daemon's job fails after the links moved (a bootout that does not end, a
/// bootstrap that fails, a daemon that runs another release than it was started on) is
/// `restart_failed`: the kit names the release, a trial's marker is gone, and the release left
/// is not started again.
///
/// Catches: a launchd failure answered by starting the release being left.
#[test]
fn each_failure_of_the_daemons_job_is_restart_failed_on_the_release_named() {
    for (case, reason) in [
        ("stuck", "bootout_timeout"),
        ("bootstrap", "bootstrap_failed"),
        ("runs-stale", "wrong_release"),
    ] {
        let (fx, sim, _) = rig(&format!("mbam-{case}"));
        keep(&fx, NEW, NEW_VERSION, 2);
        write(
            &fx.host.pending_marker(),
            0o600,
            format!(
                "{{\"commit\":\"{OLD}\",\"from\":\"{NEW}\",\"from_previous\":null,\"deadline_unix\":5}}"
            )
            .as_bytes(),
        );
        match case {
            "stuck" => sim.break_("stuck"),
            "bootstrap" => sim.bootstrap_fails.set(99),
            _ => sim.break_("runs-stale"),
        }
        let answer = core(&fx, &sim).activate(&target(NEW), How::default());
        assert_eq!(
            answer.log_detail().unwrap(),
            format!("the kit runs bbbbbbbbbbbb now, but: {reason}"),
            "{case}"
        );
        assert_eq!(current(&fx).as_deref(), Some(NEW), "{case}");
        assert!(!fx.host.pending_marker().exists(), "{case}");
        assert!(
            sim.daemon_bootstraps.get() <= 1,
            "{case}: OLD is not started"
        );
    }
}

/// The release that runs, named again, restarts the daemon and keeps `previous`; a kit with no
/// `current` takes the release named with no `previous`; after a `restart_failed`, the same
/// command asked again restarts the daemon and keeps `previous` the release left first; the
/// release on trial, named, ends its trial (the marker goes) and keeps the release before it as
/// `previous`.
///
/// Catches: the Mac swap's own links code setting `previous` to the release itself, or needing a
/// `current` to swap from.
#[test]
fn the_release_that_runs_or_is_on_trial_named_again_keeps_previous() {
    let (fx, sim, _) = rig("mbam-same");
    let answer = core(&fx, &sim).activate(&target(OLD), How::default());
    assert!(matches!(&answer, Answer::Activated { from: Some(f), .. } if f.as_str() == OLD));
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(OLD), None));
    assert_eq!(sim.daemon_bootstraps.get(), 1);

    let (fx, sim, _) = rig("mbam-nocurrent");
    keep(&fx, NEW, NEW_VERSION, 2);
    fs::remove_file(fx.host.current()).unwrap();
    let answer = core(&fx, &sim).activate(&target(NEW), How::default());
    assert!(
        matches!(&answer, Answer::Activated { from: None, .. }),
        "{answer:?}"
    );
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(NEW), None));

    let (fx, sim, _) = rig("mbam-retry");
    keep(&fx, NEW, NEW_VERSION, 2);
    sim.welcome.set(false);
    let ab = core(&fx, &sim);
    assert!(matches!(
        ab.activate(&target(NEW), How::default()),
        Answer::Failed(Code::RestartFailed, _)
    ));
    sim.welcome.set(true);
    assert_eq!(
        ab.activate(&target(NEW), How::default()),
        activated(NEW, NEW_VERSION, NEW, false, false)
    );
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(NEW), Some(OLD))
    );

    let (fx, sim, _) = rig("mbam-ontrial");
    keep(&fx, NEW, NEW_VERSION, 2);
    let backend = MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(&sim))));
    backend.switch(NEW, 600, 1).unwrap();
    let answer = core(&fx, &sim).activate(&target(NEW), PIN);
    assert_eq!(answer, activated(NEW, NEW_VERSION, NEW, true, false));
    assert!(backend.pending().unwrap().is_none(), "the trial is over");
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(NEW), Some(OLD))
    );
}

/// A pinned release that is neither current nor previous, and older than the releases a prune
/// keeps, stays because of the pin alone; an unpinned one as old is removed.
///
/// Catches: a Mac pin that a prune ignores once the release has left the links.
#[test]
fn a_pinned_release_off_the_links_survives_a_mac_prune_by_its_pin_alone() {
    let (fx, sim, _) = rig("mbam-pinonly");
    keep(&fx, NEW, NEW_VERSION, 2);
    keep(&fx, C, VC, 3);
    let ab = core(&fx, &sim);
    assert!(matches!(
        ab.activate(&target(NEW), PIN),
        Answer::Activated { .. }
    ));
    assert!(matches!(
        ab.activate(&target(C), How::default()),
        Answer::Activated { .. }
    ));
    assert!(matches!(
        ab.activate(&target(OLD), How::default()),
        Answer::Activated { .. }
    ));
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(OLD), Some(C))
    );
    let old = format!("0{}", "e".repeat(39));
    keep(&fx, &old, "0.0.1+g0eeeeeeeeeee-1", 1);
    keep(&fx, NEW, NEW_VERSION, 1);
    for n in 1..5u64 {
        keep(
            &fx,
            &format!("{n}{}", "f".repeat(39)),
            "0.0.2+gffffffffffff-1",
            10 + n,
        );
    }
    retain::prune(&fx.host, 2).unwrap();
    assert!(
        fx.host.release(NEW).join("kbf").exists(),
        "the pin holds NEW"
    );
    assert!(
        !fx.host.release(&old).exists(),
        "an unpinned one as old is removed"
    );
    assert!(
        Retained::read(&retain::file(&fx.host))
            .unwrap()
            .pinned
            .len()
            == 1
    );
}

/// A run cut short after the links moved and before the trial's marker went: the marker stays,
/// and the first tick past its deadline puts back the release that ran before the trial, as for
/// any trial (this window is one `unlink`). A run cut short after the marker went and before the
/// daemon restarted: the same command asked again restarts the daemon on the release, and
/// `previous` stays the release left.
///
/// Catches: an interrupted activate the same command cannot finish.
#[test]
fn an_interrupted_mac_activate_is_put_back_or_finished_by_asking_again() {
    let (fx, sim, _) = rig("mbam-cut1");
    keep(&fx, NEW, NEW_VERSION, 2);
    keep(&fx, C, VC, 3);
    // A trial of NEW from OLD, then an activate of C cut short after its links moved.
    write(
        &fx.host.pending_marker(),
        0o600,
        format!(
            "{{\"commit\":\"{NEW}\",\"from\":\"{OLD}\",\"from_previous\":null,\"deadline_unix\":5}}"
        )
        .as_bytes(),
    );
    kit::set_link(&fx.host.previous(), Some(NEW)).unwrap();
    kit::set_link(&fx.host.current(), Some(C)).unwrap();
    let ab = core(&fx, &sim);
    let reverted = ab.tick();
    assert!(
        matches!(reverted, Some(Answer::Reverted { .. })),
        "{reverted:?}"
    );
    assert_eq!(
        current(&fx).as_deref(),
        Some(OLD),
        "the trial's deadline put OLD back"
    );

    let (fx, sim, _) = rig("mbam-cut2");
    keep(&fx, NEW, NEW_VERSION, 2);
    kit::set_link(&fx.host.previous(), Some(OLD)).unwrap();
    kit::set_link(&fx.host.current(), Some(NEW)).unwrap();
    let ab = core(&fx, &sim);
    assert!(matches!(
        ab.activate(&target(NEW), How::default()),
        Answer::Activated { .. }
    ));
    assert_eq!(sim.daemon_bootstraps.get(), 1, "restarted on NEW");
    assert_eq!(previous(&fx).as_deref(), Some(OLD));
    mkdir(&fx.dir.join("unused"), 0o755);
}

/// An apply job launchd still has but lists as ended (it exited) runs no more: the activate goes
/// ahead.
///
/// Catches: a check so closed that a finished job blocks every activate until it is unloaded.
#[test]
fn an_apply_job_listed_as_ended_does_not_hold_the_activate() {
    let (fx, sim, _) = rig("mbam-ended");
    keep(&fx, NEW, NEW_VERSION, 2);
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    status::write(&fx.host, &ApplyState::Done).unwrap();
    assert!(matches!(
        core(&fx, &sim).activate(&target(NEW), How::default()),
        Answer::Activated { .. }
    ));
}
