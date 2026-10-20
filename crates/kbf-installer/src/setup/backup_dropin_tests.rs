//! Tests of the `kbf-backup.service` drop-in `setup` writes, and of its reload under a timer
//! that is enabled already. The
//! helpers are `tests.rs`'s.

use super::*;
use crate::setup::plan::BACKUP_USER_DROPIN;

/// Catches a drop-in that changes under a timer that is enabled already (a re-run after a
/// mode change from frontend to all-in-one): systemd keeps the old unit until a reload, so
/// the service would run as the old user. A fresh enable needs no reload of its own (`enable`
/// reloads), and a re-run that changes nothing needs none.
///
/// Mutants to plant: the reload left out (red: no reload); the reload made unconditional (red:
/// a reload with nothing changed, and on the fresh enable).
#[test]
fn a_changed_backup_dropin_under_an_enabled_timer_is_reloaded() {
    let reloads = |exec: &FakeExec| {
        cmds(exec)
            .iter()
            .filter(|c| c.contains("daemon-reload"))
            .count()
    };
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-backup.timer".to_owned());
    let mut exec = released(host(), Platform::Linux);
    put(
        &mut exec,
        BACKUP_USER_DROPIN,
        0o644,
        Owner { uid: 0, gid: 0 },
        "[Service]\nUser=kbf-server\n",
    );
    go_ok(&os, &mut exec, &raw(Mode::AllInOne));
    assert!(text(&exec, BACKUP_USER_DROPIN).contains("User=kbf\n"));
    assert_eq!(reloads(&exec), 1, "{:?}", cmds(&exec));
    let wrote = write_index(&exec, BACKUP_USER_DROPIN).expect("written");
    let reload = exec
        .log
        .iter()
        .position(|s| s.to_string().contains("daemon-reload"))
        .expect("reloaded");
    assert!(wrote < reload);

    // Nothing changed: no reload.
    exec.log.clear();
    go_ok(&os, &mut exec, &raw(Mode::AllInOne));
    assert_eq!(reloads(&exec), 0, "{:?}", cmds(&exec));

    // A timer that is not enabled yet is enabled, which reloads by itself.
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    assert_eq!(reloads(&exec), 0, "{:?}", cmds(&exec));
}

/// Catches the backup unit's `User=` coming from anywhere but the mode's service user: a
/// frontend host renders the frontend's user (`kbf-server`), an all-in-one host its own
/// (`kbf`), root owns the drop-in (0644) and writes it before the timer is enabled, and a
/// host that keeps no state in Postgres gets none.
///
/// Mutants seen red: the state directory hardcoded to the frontend's (all-in-one red); the user hardcoded in `backup_user_dropin`'s caller; the drop-in written
/// after the enable; the drop-in written on a daemon host.
#[test]
fn the_backup_unit_runs_as_the_modes_service_user() {
    for (mode, user) in [(Mode::Frontend, "kbf-server"), (Mode::AllInOne, "kbf")] {
        let mut exec = released(host(), Platform::Linux);
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        let body = text(&exec, BACKUP_USER_DROPIN);
        assert!(
            body.contains(&format!("[Service]\nUser={user}\n")),
            "{mode:?}: {body}"
        );
        // The state directory is the mode's own unit's: all-in-one's is /var/lib/kbf, not the
        // frontend's, and the verb is told so (its default is the frontend's).
        assert!(
            body.contains(&format!(
                "StateDirectory=\nStateDirectory={user}\nExecStart=\nExecStart=/usr/bin/kbf-installer --apply backup --dir /var/lib/{user}/backups\n"
            )),
            "{mode:?}: {body}"
        );
        assert_eq!(body.matches("User=").count(), 1, "{body}");
        let (perms, owner) = file_meta(&exec, BACKUP_USER_DROPIN);
        assert_eq!(perms, 0o644);
        assert_eq!(owner, Owner { uid: 0, gid: 0 });
        let wrote = write_index(&exec, BACKUP_USER_DROPIN).expect("written");
        let enabled = exec
            .log
            .iter()
            .position(|s| {
                s.to_string().contains("enable") && s.to_string().contains("kbf-backup.timer")
            })
            .expect("timer enabled");
        assert!(wrote < enabled, "{mode:?}");
    }
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert!(write_index(&exec, BACKUP_USER_DROPIN).is_none());
}
