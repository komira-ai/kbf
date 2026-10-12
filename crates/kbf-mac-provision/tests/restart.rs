//! `restart` against a fake Mac: it stops the job, empties the error log while it is
//! stopped and starts it again; it never empties a file a link at the error log's path
//! names (the script's `t_restart` and `t_err_log`).

mod support;

use std::fs;

use kbf_mac_provision::cli::{self, Error};
use kbf_mac_provision::launchd::RestartError;
use support::{ERR_LOG, FakeMac, PLIST, apply, calls, check_keys};

fn restart(mac: &FakeMac) -> Result<String, Error> {
    cli::restart(&mac.profile_file(&mac.profile_text()), mac)
}

/// Catches: a restart that does not empty the error log (the script's mutant "restart
/// does not truncate the error log"), that starts the job before it stops it ("restart
/// starts before it stops"), or empties the log while the job runs.
#[test]
fn restart_stops_empties_the_log_and_starts() {
    let mac = FakeMac::converged("restart");
    mac.write(ERR_LOG, "old panic\n");
    assert_eq!(
        restart(&mac).unwrap(),
        "CHANGE launchd_label: restarted system/org.example.kbf-daemon (error log truncated)\n"
    );
    assert_eq!(fs::read(mac.path(ERR_LOG)).unwrap(), b"");
    let order: Vec<String> = mac
        .log
        .borrow()
        .iter()
        .filter(|c| c.starts_with("launchctl boot") || c.starts_with("write "))
        .map(|c| c.split(' ').take(2).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        order,
        [
            "launchctl bootout",
            &format!("write {ERR_LOG}"),
            "launchctl bootstrap"
        ]
    );
    assert!(
        mac.state
            .borrow()
            .loaded
            .contains("system/org.example.kbf-daemon")
    );
    // The log keeps its owner and mode.
    assert_eq!(check_keys(&mac, Some("launchd_label")).failed, 0);
}

/// Catches: a job that will not load reported as restarted, tried fewer or more than
/// ten times, or without a pause between tries; and apply not saying the job did not
/// restart (the script's `t_restart`).
#[test]
fn a_job_that_will_not_load_fails_restart_and_apply_says_so() {
    let mut mac = FakeMac::converged("restart-fails");
    mac.state_mut().bootstrap_fails = true;
    assert_eq!(
        restart(&mac),
        Err(Error::Restart(RestartError::NotStarted {
            plist: PLIST.into()
        }))
    );
    assert_eq!(calls(&mac, "launchctl bootstrap").len(), 10);
    assert_eq!(calls(&mac, "sleep 1").len(), 9);
    assert_eq!(
        Error::Restart(RestartError::NotStarted {
            plist: PLIST.into()
        })
        .to_string(),
        format!("launchctl bootstrap system {PLIST} failed")
    );
    let t = fs::read_to_string(mac.path(PLIST)).unwrap();
    mac.write(PLIST, &t.replace("Interactive", "Background"));
    let r = apply(&mac);
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text.contains(&format!(
            "CHANGE launchd_label: wrote {PLIST}; the job did not restart: launchctl bootstrap system {PLIST} failed\n"
        )),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("FAIL launchd_label: system/org.example.kbf-daemon is not loaded")
    );
}

/// Catches: a restart that empties a file a link at the error log's path names, as
/// root (the script's mutants "restart empties the error log without checking it", "a
/// hard link at the error log path counts as safe"), that stops the job before it
/// refuses, or an apply that keeps the link ("apply keeps whatever is at the error log
/// path").
#[test]
fn a_link_at_the_error_log_is_never_emptied() {
    for hard in [false, true] {
        let mac = FakeMac::converged(&format!("restart-link-{hard}"));
        mac.write("/etc/precious", "root: precious\n");
        mac.remove(ERR_LOG);
        if hard {
            fs::hard_link(mac.path("/etc/precious"), mac.path(ERR_LOG)).unwrap();
        } else {
            mac.symlink(&mac.path("/etc/precious"), ERR_LOG);
        }
        let mut mac = mac;
        mac.own(ERR_LOG, 480, 480);
        assert_eq!(check_keys(&mac, Some("launchd_label")).failed, 1, "{hard}");
        let refused = restart(&mac);
        assert_eq!(
            refused,
            Err(Error::Restart(RestartError::UnsafeLog {
                role: "_kbf".into()
            })),
            "{hard}"
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            format!(
                "{ERR_LOG} is not a regular file of _kbf with one link; nothing restarted (apply replaces it)"
            )
        );
        assert_eq!(
            calls(&mac, "launchctl boot"),
            Vec::<String>::new(),
            "{hard}"
        );
        let r = apply(&mac);
        assert_eq!(r.failed, 0, "{hard}: {}", r.text);
        assert_eq!(
            fs::read_to_string(mac.path("/etc/precious")).unwrap(),
            "root: precious\n",
            "{hard}"
        );
        assert!(!mac.path(ERR_LOG).is_symlink(), "{hard}");
        assert!(restart(&mac).is_ok(), "{hard}");
    }
}

/// Catches: an error log that could not be emptied reported as a clean restart, or
/// the job left stopped because of it.
#[test]
fn a_log_that_cannot_be_emptied_is_reported_and_the_job_still_starts() {
    let mac = FakeMac::converged("restart-unwritable-log");
    mac.write(ERR_LOG, "old panic\n");
    mac.chmod(ERR_LOG, 0o400);
    let e = restart(&mac).unwrap_err();
    assert!(
        matches!(&e, Error::Restart(RestartError::Truncate(why)) if why.contains(ERR_LOG)),
        "{e:?}"
    );
    assert!(
        e.to_string().starts_with("cannot empty the error log ("),
        "{e}"
    );
    assert!(
        mac.state
            .borrow()
            .loaded
            .contains("system/org.example.kbf-daemon")
    );
}
