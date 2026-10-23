//! The apply job: laying the release out, moving the links, restarting the daemon, and
//! going back when the new release does not come up.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::fx::{Fx, INSTALL, NEW, NEW_VERSION, OLD, Sim, binary, mkdir, mode, write};
use crate::mac::apply::{is_welcomed, plist_xml, run};
use crate::mac::kit;
use crate::mac::layout::DAEMON_LABEL;

const OLD_WELCOME: &str = "\x1b[32m INFO\x1b[0m kbf_daemon::daemon: welcomed server=old\n";

/// Install up to the launch of the apply job; the job's body is run by the test.
fn launched(fx: &Fx, sim: &Sim) {
    write(&fx.host.err_log, 0o644, OLD_WELCOME.as_bytes());
    let answer = fx.handle(sim, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    sim.calls.borrow_mut().clear();
}

fn apply(fx: &Fx, sim: &Sim) -> (u8, String) {
    let code = run(&fx.host, sim);
    (code, fx.status())
}

fn current(fx: &Fx) -> Option<String> {
    kit::links(&fx.host).unwrap().current
}

fn previous(fx: &Fx) -> Option<String> {
    kit::links(&fx.host).unwrap().previous
}

fn launchctl(sim: &Sim) -> Vec<String> {
    sim.lines()
        .into_iter()
        .filter(|l| l.starts_with("/bin/launchctl"))
        .map(|l| l.replace("/bin/launchctl ", ""))
        .collect()
}

/// The release is laid out root's and 0755 beside its VERSION, `previous` takes the release
/// that ran and `current` the new one, the daemon is booted out, waited for, bootstrapped,
/// and the apply is done once the NEW process logged `welcomed`: the old process's line,
/// already in the log, does not count.
#[test]
fn a_good_apply_lays_out_flips_restarts_and_waits_for_welcome() {
    let fx = Fx::new("mac-apply-ok");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.quarantined
        .borrow_mut()
        .insert(fx.host.release(NEW).join("kbf"));
    assert_eq!(apply(&fx, &sim), (0, "done\n".to_owned()));
    let release = fx.host.release(NEW);
    assert_eq!(fs::read(release.join("kbf")).unwrap(), fx.new_binary);
    assert_eq!(
        (
            mode(&release),
            mode(&release.join("kbf")),
            mode(&release.join("VERSION"))
        ),
        (0o755, 0o755, 0o644)
    );
    assert_eq!(
        fs::read_to_string(release.join("VERSION")).unwrap(),
        format!("{NEW_VERSION}\n")
    );
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(NEW), Some(OLD))
    );
    assert!(
        sim.quarantined.borrow().is_empty(),
        "the installed copy was stripped"
    );
    assert_eq!(
        launchctl(&sim),
        [
            format!("print system/{DAEMON_LABEL}"),
            format!("bootout system/{DAEMON_LABEL}"),
            format!("print system/{DAEMON_LABEL}"),
            format!("bootstrap system {}", fx.host.daemon_plist.display()),
        ]
    );
    assert!(!fx.host.stage().exists(), "the stage is cleared");
    assert!(!fx.host.releases().join(format!(".{NEW}.new")).exists());
    assert!(!fx.host.kit.join(".current.new").exists());
    // The result is what a query then reports.
    let mut sim = fx.sim();
    *sim.running_path.borrow_mut() = Some(release.join("kbf"));
    sim.loaded.borrow_mut().insert(DAEMON_LABEL.to_owned());
    let answer = fx.handle(&sim, "query");
    assert_eq!(answer.json()["installed_version"], NEW_VERSION);
    assert_eq!(answer.json()["running_commit"], NEW);
    assert_eq!(answer.json()["apply"], "done");
    let _ = &mut sim;
}

/// `bootout` returns before the service is gone; the daemon is bootstrapped only after
/// `launchctl print` no longer finds it, or the bootstrap fails with an I/O error.
#[test]
fn the_daemon_is_bootstrapped_only_after_it_is_gone() {
    let fx = Fx::new("mac-apply-gone");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.lingering.set(3);
    assert_eq!(apply(&fx, &sim).1, "done\n");
    let lines = launchctl(&sim);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("print")).count(),
        1 + 1 + 3
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("bootstrap")).count(),
        1
    );
    assert!(lines.last().unwrap().starts_with("bootstrap"));
}

/// The apply is done only on the welcomed line of the NEW process, counted from where the
/// log ended before the bootstrap: a log that was cut shorter is read whole, and one that is
/// gone gives nothing.
#[test]
fn only_the_new_processs_welcome_counts() {
    let fx = Fx::new("mac-apply-log");
    let sim = fx.sim();
    launched(&fx, &sim);
    // Rotated by hand while the daemon was down: shorter than the offset.
    write(&fx.host.err_log, 0o644, &OLD_WELCOME.repeat(5).into_bytes());
    let log = fx.host.err_log.clone();
    *sim.on_daemon_bootstrap.borrow_mut() = Some(Box::new(move || {
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&log)
            .unwrap();
    }));
    assert_eq!(apply(&fx, &sim).1, "done\n");
}

/// A new release that does not log `welcomed` is rolled back: the links go back, the old
/// release is bootstrapped, and the status says so.
#[test]
fn a_daemon_that_is_not_welcomed_is_rolled_back() {
    let fx = Fx::new("mac-apply-notwelcome");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.welcome_from.set(2);
    assert_eq!(
        apply(&fx, &sim),
        (1, "failed:not_welcomed:rolled_back\n".to_owned())
    );
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(OLD), None));
    assert_eq!(sim.daemon_bootstraps.get(), 2);
    assert_eq!(sim.daemon_bootouts.get(), 2);
    assert!(sim.loaded.borrow().contains(DAEMON_LABEL));
    assert!(!fx.host.stage().exists());
}

/// A bootstrap that fails: the previous release is bootstrapped again.
#[test]
fn a_bootstrap_that_fails_is_rolled_back() {
    let fx = Fx::new("mac-apply-bootstrap");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.bootstrap_fails.set(1);
    assert_eq!(
        apply(&fx, &sim),
        (1, "failed:bootstrap_failed:rolled_back\n".to_owned())
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(sim.daemon_bootstraps.get(), 2);
    // The new release stays on disk, as written; the old one runs.
    assert!(fx.host.release(NEW).join("kbf").exists());
}

/// When the old release does not come up either, the status says so.
#[test]
fn a_rollback_that_fails_is_said() {
    let fx = Fx::new("mac-apply-norollback");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.welcome.set(false);
    fs::remove_file(&fx.host.err_log).unwrap();
    write(&fx.host.err_log, 0o644, b"");
    assert_eq!(
        apply(&fx, &sim),
        (1, "failed:not_welcomed:rollback_failed\n".to_owned())
    );
    assert_eq!(
        current(&fx).as_deref(),
        Some(OLD),
        "the links are back all the same"
    );

    // The log is gone: nothing can be read from it.
    let fx = Fx::new("mac-apply-nolog");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.welcome.set(false);
    fs::remove_file(&fx.host.err_log).unwrap();
    assert_eq!(apply(&fx, &sim).1, "failed:not_welcomed:rollback_failed\n");

    // The links cannot be put back.
    let fx = Fx::new("mac-apply-norestore");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.welcome_from.set(2);
    let kit = fx.host.kit.clone();
    *sim.on_daemon_bootstrap.borrow_mut() = Some(Box::new(move || {
        fs::set_permissions(&kit, fs::Permissions::from_mode(0o555)).unwrap();
    }));
    assert_eq!(apply(&fx, &sim).1, "failed:not_welcomed:rollback_failed\n");
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A daemon that cannot be booted out stops the apply before its bootstrap, and a launchctl
/// that cannot run does the same.
#[test]
fn a_daemon_that_will_not_go_stops_the_apply() {
    let fx = Fx::new("mac-apply-stuck");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.break_("stuck");
    assert_eq!(
        apply(&fx, &sim),
        (1, "failed:bootout_timeout:rollback_failed\n".to_owned())
    );
    assert_eq!(sim.daemon_bootstraps.get(), 0);
    assert_eq!(current(&fx).as_deref(), Some(OLD));

    let fx = Fx::new("mac-apply-nolaunchctl");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.break_("launchctl");
    assert_eq!(
        apply(&fx, &sim).1,
        "failed:bootout_timeout:rollback_failed\n"
    );
}

/// With no release before it, a failed first apply cannot go back; the links are as they
/// were and the status says there was nothing to go back to.
#[test]
fn a_first_install_that_fails_has_nothing_to_go_back_to() {
    let fx = Fx::new("mac-apply-first");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::remove_file(fx.host.current()).unwrap();
    sim.loaded.borrow_mut().clear();
    sim.bootstrap_fails.set(1);
    assert_eq!(
        apply(&fx, &sim),
        (1, "failed:bootstrap_failed:no_previous\n".to_owned())
    );
    assert_eq!(current(&fx), None);
    assert_eq!(sim.daemon_bootouts.get(), 0, "the daemon was not loaded");
    // And a good first install: no `previous` to move.
    let fx = Fx::new("mac-apply-first-ok");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::remove_file(fx.host.current()).unwrap();
    sim.loaded.borrow_mut().clear();
    assert_eq!(apply(&fx, &sim).1, "done\n");
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(NEW), None));
}

/// Installing the release that is already `current` keeps `previous`; a release already
/// laid out with the same bytes is not written again; with other bytes it is refused.
#[test]
fn a_release_is_written_once() {
    let fx = Fx::new("mac-apply-once");
    let sim = fx.sim();
    launched(&fx, &sim);
    fx.install_release(NEW, "0.0.1+gbbbbbbbbbbbb-1", &fx.new_binary);
    kit::set_link(&fx.host.previous(), Some(OLD)).unwrap();
    let before = fs::metadata(fx.host.release(NEW).join("kbf")).unwrap();
    assert_eq!(apply(&fx, &sim).1, "done\n");
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(NEW), Some(OLD))
    );
    // Same bytes: untouched; the version text is the new one.
    assert_eq!(
        fs::metadata(fx.host.release(NEW).join("kbf"))
            .unwrap()
            .ino(),
        before.ino()
    );
    assert_eq!(
        fs::read_to_string(fx.host.release(NEW).join("VERSION")).unwrap(),
        format!("{NEW_VERSION}\n")
    );

    let fx = Fx::new("mac-apply-differs");
    let sim = fx.sim();
    launched(&fx, &sim);
    let dir = fx.host.release(NEW);
    mkdir(&dir, 0o755);
    write(&dir.join("kbf"), 0o755, &binary("something else"));
    assert_eq!(apply(&fx, &sim), (1, "failed:release_differs\n".to_owned()));
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(sim.daemon_bootouts.get(), 0, "the daemon was not touched");
    assert_eq!(fs::read(dir.join("kbf")).unwrap(), binary("something else"));
}

use std::os::unix::fs::MetadataExt;

/// Failures before the links move leave the old release running and say why.
#[test]
fn a_failure_before_the_flip_changes_nothing_that_runs() {
    // A quarantine attribute that stays on the installed copy.
    let fx = Fx::new("mac-apply-quarantine");
    let sim = fx.sim();
    launched(&fx, &sim);
    sim.quarantined
        .borrow_mut()
        .insert(fx.host.release(NEW).join("kbf"));
    sim.break_("xattr-sticky");
    assert_eq!(apply(&fx, &sim).1, "failed:quarantine\n");
    assert_eq!(
        (current(&fx).as_deref(), sim.daemon_bootouts.get()),
        (Some(OLD), 0)
    );

    // The release cannot be written.
    let fx = Fx::new("mac-apply-nolayout");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::set_permissions(fx.host.releases(), fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(apply(&fx, &sim).1, "failed:layout_failed\n");
    fs::set_permissions(fx.host.releases(), fs::Permissions::from_mode(0o755)).unwrap();

    // The version text cannot be written.
    let fx = Fx::new("mac-apply-noversion");
    let sim = fx.sim();
    launched(&fx, &sim);
    fx.install_release(NEW, NEW_VERSION, &fx.new_binary);
    fs::remove_file(fx.host.release(NEW).join("VERSION")).unwrap();
    mkdir(&fx.host.release(NEW).join("VERSION"), 0o755);
    write(&fx.host.release(NEW).join("VERSION/f"), 0o644, b"");
    assert_eq!(apply(&fx, &sim).1, "failed:layout_failed\n");

    // A release directory others can write is not a place to run from.
    let fx = Fx::new("mac-apply-untrusted");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::set_permissions(fx.host.releases(), fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(apply(&fx, &sim).1, "failed:release_untrusted\n");

    // The links cannot be moved.
    let fx = Fx::new("mac-apply-nolinks");
    let sim = fx.sim();
    launched(&fx, &sim);
    let kit = fx.host.kit.clone();
    *sim.before.borrow_mut() = Some(Box::new(move |cmd| {
        if cmd.program.ends_with("xattr") {
            fs::set_permissions(&kit, fs::Permissions::from_mode(0o555)).unwrap();
        }
    }));
    assert_eq!(apply(&fx, &sim).1, "failed:links_failed\n");
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(sim.daemon_bootouts.get(), 0);
}

/// The plan and the stage are checked again before they are used.
#[test]
fn a_plan_or_stage_that_is_not_what_was_checked_is_refused() {
    let plan_with = |f: &dyn Fn(&mut serde_json::Value)| {
        let fx = Fx::new("mac-apply-plan");
        let sim = fx.sim();
        launched(&fx, &sim);
        let path = fx.host.stage().join("plan.json");
        let mut plan: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        f(&mut plan);
        write(&path, 0o600, plan.to_string().as_bytes());
        let result = apply(&fx, &sim);
        assert_eq!(sim.daemon_bootouts.get(), 0);
        result
    };
    for (why, change) in [
        (
            "a short commit",
            Box::new(|p: &mut serde_json::Value| p["commit"] = "abc".into())
                as Box<dyn Fn(&mut serde_json::Value)>,
        ),
        (
            "a short hash",
            Box::new(|p| p["binary_sha256"] = "abc".into()),
        ),
        (
            "a bad version",
            Box::new(|p| p["version"] = "not a version".into()),
        ),
        ("an extra field", Box::new(|p| p["extra"] = 1.into())),
    ] {
        assert_eq!(
            plan_with(&*change),
            (1, "failed:plan_invalid\n".to_owned()),
            "{why}"
        );
    }
    // No plan at all, or one that is not JSON.
    for text in [None, Some("{")] {
        let fx = Fx::new("mac-apply-noplan");
        let sim = fx.sim();
        launched(&fx, &sim);
        let path = fx.host.stage().join("plan.json");
        match text {
            None => fs::remove_file(&path).unwrap(),
            Some(text) => write(&path, 0o600, text.as_bytes()),
        }
        assert_eq!(apply(&fx, &sim).1, "failed:plan_invalid\n");
    }
    // The staged binary was changed, or is writable by others.
    let fx = Fx::new("mac-apply-changed");
    let sim = fx.sim();
    launched(&fx, &sim);
    write(&fx.host.stage().join("kbf"), 0o755, &binary("swapped"));
    assert_eq!(apply(&fx, &sim).1, "failed:stage_changed\n");
    let fx = Fx::new("mac-apply-writable");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::set_permissions(
        fx.host.stage().join("kbf"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(apply(&fx, &sim).1, "failed:stage_untrusted\n");
    // Links that name no release.
    let fx = Fx::new("mac-apply-links");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::remove_file(fx.host.current()).unwrap();
    mkdir(&fx.host.current(), 0o755);
    assert_eq!(apply(&fx, &sim).1, "failed:links_unreadable\n");
}

/// An apply that is done but whose status cannot be written exits 1.
#[test]
fn an_unwritable_status_is_an_exit_status() {
    let fx = Fx::new("mac-apply-status");
    let sim = fx.sim();
    launched(&fx, &sim);
    fs::remove_file(fx.host.status_file()).unwrap();
    mkdir(&fx.host.status_file(), 0o755);
    assert_eq!(run(&fx.host, &sim), 1);
    assert_eq!(current(&fx).as_deref(), Some(NEW), "it was done");
}

/// The line the daemon writes when the frontend takes it in, and no other.
#[test]
fn the_welcomed_line_is_the_daemons() {
    assert!(is_welcomed(OLD_WELCOME));
    assert!(is_welcomed(
        "x\n INFO kbf_daemon: welcomed server=a interval=1s\n"
    ));
    for not in [
        "",
        " INFO kbf_daemon: session ended server=a reason=x\n",
        " WARN kbf_daemon: was not welcomed\n",
        " INFO kbf_server: welcomed server=a\n",
        " INFO kbf_daemon welcomed\n",
    ] {
        assert!(!is_welcomed(not), "{not:?}");
    }
}

/// The job's plist runs this program once at load, is not kept alive, lives outside
/// `/Library/LaunchDaemons`, and escapes its paths.
#[test]
fn the_job_plist_is_a_one_shot() {
    let fx = Fx::new("mac-apply-plist");
    let mut host = fx.host.clone();
    host.self_exe = "/a&b/<c>/kbf-host-install".into();
    let plist = plist_xml(&host);
    assert!(
        plist.contains("<string>/a&amp;b/&lt;c&gt;/kbf-host-install</string>"),
        "{plist}"
    );
    assert!(
        plist.contains("<key>Label</key>\n\t<string>com.komira.kbf-host-install-apply</string>")
    );
    assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    assert!(plist.contains("<key>KeepAlive</key>\n\t<false/>"));
    assert!(!plist.contains("LaunchDaemons"));
    assert!(plist.starts_with("<?xml"));
}
