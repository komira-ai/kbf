//! `install` as the root process answers it: every check before the apply job is launched,
//! and the launch.

use std::fs;

use super::fx::{Fx, INSTALL, NEW, NEW_VERSION, OLD, OLD_VERSION, binary, mkdir, mode, write};
use crate::mac::layout::{APPLY_LABEL, DAEMON_LABEL};
use crate::mac::status::{self, ApplyState};

fn reason(answer: &super::fx::Answer) -> String {
    answer.json()["reason"].as_str().unwrap().to_owned()
}

/// A good request is checked end to end and the apply job is launched: the answer is
/// `apply_started` with the commit, the status is `running`, the plan and the job's plist are
/// written, nothing of the kit changed yet, and the audit line says so.
#[test]
fn an_install_launches_the_apply_job() {
    let fx = Fx::new("mac-install-ok");
    let sim = fx.sim();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(
        answer.json(),
        serde_json::json!({"result":"apply_started","version":NEW_VERSION,"commit":NEW,"apply":"running"})
    );
    assert_eq!(fx.status(), "running\n");
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.host.stage().join("plan.json")).unwrap())
            .unwrap();
    assert_eq!(plan["commit"], NEW);
    assert_eq!(plan["version"], NEW_VERSION);
    assert_eq!(plan["binary_sha256"].as_str().unwrap().len(), 64);
    // The links as they were are in the plan.
    assert_eq!(plan["before_current"], OLD);
    assert!(plan["before_previous"].is_null());
    assert_eq!(mode(&fx.host.stage().join("plan.json")), 0o600);
    assert_eq!(mode(&fx.host.stage()), 0o700);
    assert_eq!(mode(&fx.host.work), 0o711);
    let plist = fs::read_to_string(fx.host.apply_plist()).unwrap();
    assert!(plist.contains(&format!("<string>{APPLY_LABEL}</string>")));
    assert!(plist.contains(&format!("<string>{}</string>", fx.host.self_exe.display())));
    assert!(plist.contains("<string>--apply</string>"));
    assert!(plist.contains(&format!("<string>{}</string>", fx.host.stage().display())));
    assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    assert!(plist.contains("<key>KeepAlive</key>\n\t<false/>"));
    assert_eq!(mode(&fx.host.apply_plist()), 0o644);
    assert_eq!(
        sim.lines()
            .iter()
            .filter(|l| l.starts_with("/bin/launchctl"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            format!("/bin/launchctl print system/{APPLY_LABEL}"),
            format!(
                "/bin/launchctl bootstrap system {}",
                fx.host.apply_plist().display()
            ),
        ]
    );
    assert_eq!(
        fx.link(&fx.host.current()).as_deref(),
        Some(format!("releases/{OLD}").as_str())
    );
    assert!(
        !fx.host.release(NEW).exists(),
        "the apply job lays the release out, not this"
    );
    assert_eq!(sim.daemon_bootouts.get(), 0);
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["verb"], "install");
    assert_eq!(audit[0]["version"], NEW_VERSION);
    assert_eq!(audit[0]["channel"], "stable");
    assert_eq!(audit[0]["outcome"], "apply_started");
}

/// A finished apply job stays loaded in launchd; it is unloaded, and gone, before the next
/// is started.
#[test]
fn the_last_apply_job_is_unloaded_before_the_next_is_started() {
    let fx = Fx::new("mac-install-stale");
    let sim = fx.sim();
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    mkdir(&fx.host.work, 0o711);
    status::write(&fx.host, &ApplyState::Done).unwrap();
    assert_eq!(fx.handle(&sim, INSTALL).code, 0);
    let launchctl: Vec<String> = sim
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("/bin/launchctl"))
        .collect();
    assert_eq!(
        launchctl[..2],
        [
            format!("/bin/launchctl print system/{APPLY_LABEL}"),
            format!("/bin/launchctl bootout system/{APPLY_LABEL}"),
        ]
    );
    assert!(launchctl[2].starts_with("/bin/launchctl print"));
    assert!(launchctl.last().unwrap().contains("bootstrap system"));
    // One that cannot be unloaded stops the install.
    let sim = fx.sim();
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    sim.break_("stuck-apply");
    status::write(&fx.host, &ApplyState::Done).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "install_failed".into()));
    assert!(answer.err.contains("cannot be unloaded"), "{}", answer.err);
    assert_eq!(fx.status(), "failed:launch_failed\n");
}

/// Whatever goes wrong while launching ends in `failed:launch_failed` in the status, an
/// install failure, and no job.
#[test]
fn a_job_that_cannot_be_launched_is_a_failure_with_a_status() {
    let fx = Fx::new("mac-install-launch");
    let sim = fx.sim();
    sim.break_("bootstrap-apply");
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "install_failed".into()));
    assert!(
        answer
            .err
            .contains("launchctl bootstrap: exit status Some(5)"),
        "{}",
        answer.err
    );
    assert_eq!(fx.status(), "failed:launch_failed\n");
    assert!(!sim.loaded.borrow().contains(APPLY_LABEL));
    // The plan could not be written (something is in its place).
    let sim = fx.sim();
    let stage = fx.host.stage();
    *sim.before.borrow_mut() = Some(Box::new(move |cmd| {
        if cmd.program.ends_with("/kbf") {
            write(&stage.join("plan.json"), 0o600, b"");
        }
    }));
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(reason(&answer), "install_failed");
    assert!(
        answer.err.contains("cannot record the plan"),
        "{}",
        answer.err
    );
    // The job's plist could not be written.
    let sim = fx.sim();
    fs::remove_file(fx.host.apply_plist()).unwrap();
    mkdir(&fx.host.apply_plist(), 0o755);
    let answer = fx.handle(&sim, INSTALL);
    assert!(
        answer.err.contains("cannot write the job"),
        "{}",
        answer.err
    );
    fs::remove_dir(fx.host.apply_plist()).unwrap();
    // The status could not be written.
    let sim = fx.sim();
    fs::remove_file(fx.host.status_file()).unwrap();
    mkdir(&fx.host.status_file(), 0o755);
    let answer = fx.handle(&sim, INSTALL);
    assert!(
        answer.err.contains("cannot record the plan"),
        "{}",
        answer.err
    );
}

/// A request outside the grammar, or no request, is refused and recorded, escaped.
#[test]
fn a_request_outside_the_grammar_is_refused_and_audited() {
    let fx = Fx::new("mac-install-bad");
    let sim = fx.sim();
    let answer = fx.handle(&sim, "install kbf=1 from stable ");
    assert_eq!((answer.code, reason(&answer)), (2, "bad_command".into()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        crate::mac::handle_root(&fx.host, &sim, None, &mut out, &mut err),
        2
    );
    assert_eq!(sim.calls.borrow().len(), 0);
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0]["command"], "install kbf=1 from stable ");
    assert_eq!(audit[0]["reason"], "bad_command");
    assert_eq!(audit[1].get("command"), None);
}

#[test]
fn a_channel_the_allowlist_does_not_name_is_refused() {
    let fx = Fx::new("mac-install-channel");
    let sim = fx.sim();
    let answer = fx.handle(&sim, "install kbf=0.1.7+gbbbbbbbbbbbb-3 from beta");
    assert_eq!(
        (answer.code, reason(&answer)),
        (2, "channel_not_allowed".into())
    );
    fs::remove_file(&fx.host.allowlist).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (2, "config_invalid".into()));
    assert_eq!(sim.calls.borrow().len(), 0);
    assert!(!fx.host.work.exists(), "a refused request touches nothing");
}

/// A channel of a repository other than kbf's is refused by the ssh path as
/// `channel_not_allowed` with the words on stderr, unless the allowlist lists the
/// repository; nothing runs and nothing is touched.
#[test]
fn a_channel_of_an_untrusted_repository_is_refused_with_its_reason() {
    let fx = Fx::new("mac-install-untrusted");
    let sim = fx.sim();
    let text = "[channels.stable]\nsource = \"github\"\nrepository = \"owner/other\"\nsigner_workflow = \"owner/other/.github/workflows/release.yml\"\n";
    write(&fx.host.allowlist, 0o644, text.as_bytes());
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(
        (answer.code, reason(&answer)),
        (2, "channel_not_allowed".into())
    );
    assert!(
        answer.err.contains("owner/other") && answer.err.contains("trusted_workflows"),
        "{}",
        answer.err
    );
    assert_eq!(sim.calls.borrow().len(), 0);
    assert!(!fx.host.work.exists(), "a refused request touches nothing");
}

/// An install is not started when the audit log cannot be opened.
#[test]
fn no_install_goes_unrecorded() {
    let fx = Fx::new("mac-install-audit");
    let sim = fx.sim();
    mkdir(&fx.host.audit, 0o755);
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(
        (answer.code, reason(&answer)),
        (3, "audit_unavailable".into())
    );
    assert!(
        answer.err.contains("audit log unavailable"),
        "{}",
        answer.err
    );
    assert_eq!(sim.calls.borrow().len(), 0);
    assert_eq!(fx.status(), "");
    // A query is answered all the same.
    assert_eq!(fx.handle(&sim, "query").code, 0);
}

/// One install at a time: a running apply job, or another install in its critical section,
/// is `busy`; a `running` status whose job is gone is not.
#[test]
fn one_install_at_a_time() {
    let fx = Fx::new("mac-install-busy");
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o711);
    status::write(&fx.host, &ApplyState::Running).unwrap();
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    sim.apply_running.set(true);
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "busy".into()));
    assert_eq!(fx.status(), "running\n");
    assert!(!fx.host.stage().exists());

    // Another install holds the lock.
    let sim = fx.sim();
    status::write(&fx.host, &ApplyState::Done).unwrap();
    let held = crate::host::take_lock(&fx.host.lock_file()).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "busy".into()));
    drop(held);
    crate::locks::wait_lockable(&fx.host.lock_file());

    // A lost apply does not block the next.
    let sim = fx.sim();
    status::write(&fx.host, &ApplyState::Running).unwrap();
    assert_eq!(fx.handle(&sim, INSTALL).code, 0);
}

/// An install that comes while the apply job is loaded and not yet started is `busy`, and
/// leaves the stage the job is about to read.
#[test]
fn an_install_is_busy_while_the_apply_job_is_starting() {
    let fx = Fx::new("mac-install-spawning");
    let sim = fx.sim();
    assert_eq!(fx.handle(&sim, INSTALL).code, 0);
    sim.apply_spawning.set(true);
    let plan = fs::read(fx.host.stage().join("plan.json")).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "busy".into()));
    assert_eq!(fs::read(fx.host.stage().join("plan.json")).unwrap(), plan);
}

/// The program that runs as root must be root's, in directories closed to others; an
/// install through any other is refused before anything is read or staged.
#[test]
fn an_install_is_refused_when_the_program_is_not_trusted() {
    let fx = Fx::new("mac-install-selfexe");
    let sim = fx.sim();
    let dir = fx.host.self_exe.parent().unwrap().to_owned();
    fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (2, "config_unsafe".into()));
    assert!(!fx.host.stage().exists());
    assert_eq!(sim.count("/bin/launchctl"), 0);
    fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    fs::set_permissions(
        &fx.host.self_exe,
        std::os::unix::fs::PermissionsExt::from_mode(0o775),
    )
    .unwrap();
    assert_eq!(fx.handle(&sim, INSTALL).code, 2);
}

/// The work directory is made 0711 when missing, and must be root's.
#[test]
fn the_work_directory_is_root_only() {
    let fx = Fx::new("mac-install-work");
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o777);
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (2, "config_unsafe".into()));
    fs::remove_dir(&fx.host.work).unwrap();
    let mut host = fx.host.clone();
    host.work = fx.dir.join("missing/work");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::mac::handle_root(&host, &sim, Some(INSTALL), &mut out, &mut err);
    assert_eq!(code, 3);
    assert!(String::from_utf8(err).unwrap().contains("work directory"));
}

/// A host without the kit is not a state to install into.
#[test]
fn a_host_without_the_kit_is_not_installed_into() {
    let fx = Fx::new("mac-install-nokit");
    let sim = fx.sim();
    fs::remove_file(&fx.host.daemon_plist).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(
        (answer.code, reason(&answer)),
        (3, "package_state_unclean".into())
    );
    assert!(answer.err.contains("daemon job"), "{}", answer.err);
    write(&fx.host.daemon_plist, 0o644, b"<plist/>");
    fs::set_permissions(
        fx.host.releases(),
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(reason(&answer), "package_state_unclean");
    assert!(answer.err.contains("releases"), "{}", answer.err);
    fs::set_permissions(
        fx.host.releases(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink("elsewhere", fx.host.current()).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(reason(&answer), "package_state_unclean");
    assert!(
        answer.err.contains("does not name a release"),
        "{}",
        answer.err
    );
    assert_eq!(fx.status(), "");
}

/// A host with no `current` link yet (a first install) has no version to compare.
#[test]
fn a_host_with_no_current_release_installs() {
    let fx = Fx::new("mac-install-first");
    let sim = fx.sim();
    fs::remove_file(fx.host.current()).unwrap();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.json()["result"], "apply_started", "{}", answer.err);
}

/// The same version is done only when the daemon runs it; a release that was laid out and
/// did not come up is installed again.
#[test]
fn the_same_version_is_a_no_op_only_when_it_runs() {
    let fx = Fx::new("mac-install-same");
    fx.install_release(NEW, NEW_VERSION, &fx.new_binary);
    let sim = fx.sim();
    *sim.running_path.borrow_mut() = Some(fx.host.release(NEW).join("kbf"));
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.code, 0);
    assert_eq!(
        answer.json(),
        serde_json::json!({"result":"already_installed","installed_version":NEW_VERSION,"unit_state":"not_run"})
    );
    assert!(
        sim.lines()
            .iter()
            .all(|l| l.starts_with("/bin/launchctl print") || l.starts_with("/usr/sbin/lsof"))
    );
    assert!(!fx.host.stage().exists());

    // The daemon still runs the other release.
    let sim = fx.sim();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.json()["result"], "apply_started", "{}", answer.err);
    // The daemon's program is not known.
    let sim = fx.sim();
    *sim.running_path.borrow_mut() = None;
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.json()["result"], "apply_started", "{}", answer.err);
}

/// A rollback is an install of an older version: the older release is installed like any
/// other, and no version is refused for being older.
#[test]
fn an_older_version_is_installed_like_a_newer_one() {
    let fx = Fx::new("mac-install-older");
    fx.install_release(NEW, NEW_VERSION, &fx.new_binary);
    fx.offer_release(OLD, &binary("old"));
    let sim = fx.sim();
    *sim.running_path.borrow_mut() = Some(fx.host.release(NEW).join("kbf"));
    let answer = fx.handle(&sim, &format!("install kbf={OLD_VERSION} from stable"));
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(answer.json()["result"], "apply_started");
    assert_eq!(answer.json()["commit"], OLD);
    // Older than anything: only a release the sums list is wanted.
    let answer = fx.handle(&sim, "install kbf=0.0.1+gaaaaaaaaaaaa-1 from stable");
    assert_eq!(answer.json()["result"], "apply_started");
}

/// A channel with no sums installs through the whole verb: the release is the tarball in the
/// directory that the attestation accepts, an older version installs like a newer one, a
/// version no tarball matches is refused with nothing staged, and one the attestation
/// refuses starts nothing.
#[test]
fn a_channel_without_sums_installs_what_the_attestation_accepts() {
    let fx = Fx::new("mac-install-nosums");
    write(
        &fx.host.allowlist,
        0o644,
        format!(
            "[channels.stable]\ndir = \"{}\"\n",
            fx.channel_dir.display()
        )
        .as_bytes(),
    );
    let sim = fx.sim();
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(answer.json()["commit"], NEW);
    assert_eq!(fx.status(), "running\n");
    // Older, and installed: the same path.
    let fx_old = Fx::new("mac-install-nosums-older");
    write(
        &fx_old.host.allowlist,
        0o644,
        format!(
            "[channels.stable]\ndir = \"{}\"\n",
            fx_old.channel_dir.display()
        )
        .as_bytes(),
    );
    fx_old.offer_release(OLD, &binary("old"));
    fx_old.install_release(NEW, NEW_VERSION, &fx_old.new_binary);
    let sim = fx_old.sim();
    *sim.running_path.borrow_mut() = Some(fx_old.host.release(NEW).join("kbf"));
    let answer = fx_old.handle(&sim, &format!("install kbf={OLD_VERSION} from stable"));
    assert_eq!(answer.json()["commit"], OLD, "{}", answer.err);
    // No tarball starts with the digits: nothing is staged.
    let fx = Fx::new("mac-install-nosums-unknown");
    write(
        &fx.host.allowlist,
        0o644,
        format!(
            "[channels.stable]\ndir = \"{}\"\n",
            fx.channel_dir.display()
        )
        .as_bytes(),
    );
    let sim = fx.sim();
    let answer = fx.handle(&sim, "install kbf=0.1.7+gcccccccccccc-3 from stable");
    assert_eq!((answer.code, reason(&answer)), (3, "verify_failed".into()));
    assert!(answer.err.contains("unknown release"), "{}", answer.err);
    assert!(!fx.host.stage().exists());
    // The attestation refuses.
    sim.break_("gh");
    let answer = fx.handle(&sim, INSTALL);
    assert!(answer.err.contains("not attested"), "{}", answer.err);
    assert_eq!(fx.status(), "");
}

/// A release that fails a check starts nothing and leaves no status, no job and no stage.
#[test]
fn a_release_that_fails_a_check_starts_nothing() {
    let fx = Fx::new("mac-install-refused");
    let sim = fx.sim();
    sim.break_("gh");
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!((answer.code, reason(&answer)), (3, "verify_failed".into()));
    assert!(answer.err.contains("not attested"), "{}", answer.err);
    assert_eq!(fx.status(), "");
    assert!(!fx.host.stage().exists());
    assert!(!sim.loaded.borrow().contains(APPLY_LABEL));
    assert!(sim.loaded.borrow().contains(DAEMON_LABEL));
    assert_eq!(fx.audit_lines()[0]["reason"], "verify_failed");
}
