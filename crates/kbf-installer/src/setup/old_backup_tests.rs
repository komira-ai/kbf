//! Tests of how `setup` removes what an earlier build's setup left of the daily database
//! backup (a timer and the drop-in of its service). The helpers are `tests.rs`'s.

use super::*;

const TIMER: &str = "kbf-backup.timer";
const DROPIN_DIR: &str = "/etc/systemd/system/kbf-backup.service.d";
const DROPIN: &str = "/etc/systemd/system/kbf-backup.service.d/10-service-user.conf";

fn mentions_backup(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .map(ToString::to_string)
        .filter(|s| s.contains("kbf-backup"))
        .collect()
}

/// Catches setup leaving the retired backup behind on a host an earlier build set up: its timer
/// would keep starting a `kbf-installer backup` verb that no longer exists, and its drop-in
/// would shadow nothing. The timer is disabled and stopped, then the drop-in and its
/// directory go, then systemd reloads; a second run on the result plans none of it, and a host
/// that never had the backup gets no `kbf-backup` step at all.
///
/// Mutants to plant: the `disable` left out (red: no disable); the reload left out (red:
/// no reload); the directory left out (red: the directory stays); the removal made
/// unconditional (red: steps on the clean host).
#[test]
fn a_host_set_up_with_the_backup_timer_loses_the_timer_and_its_dropin() {
    let mut os = FakeOs::linux();
    os.enabled.insert(TIMER.to_owned());
    let mut exec = released(host(), Platform::Linux);
    exec.entries.insert(
        hp(DROPIN_DIR),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    put(
        &mut exec,
        DROPIN,
        0o644,
        ROOT,
        "[Service]\nUser=kbf-server\n",
    );

    let said = go_ok(&os, &mut exec, &raw(Mode::Frontend));

    let steps = mentions_backup(&exec);
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(
        steps[0].contains("disable") && steps[0].contains("--now") && steps[0].contains(TIMER),
        "{steps:?}"
    );
    assert_eq!(steps[1], format!("remove {DROPIN}"));
    assert_eq!(steps[2], format!("remove {DROPIN_DIR}"));
    assert!(!exec.entries.contains_key(&hp(DROPIN)));
    assert!(!exec.entries.contains_key(&hp(DROPIN_DIR)));
    let at = |needle: &str| {
        exec.log
            .iter()
            .position(|s| s.to_string().contains(needle))
            .unwrap_or_else(|| panic!("no step with {needle}"))
    };
    assert!(at("disable") < at(&format!("remove {DROPIN}")));
    assert!(at(&format!("remove {DROPIN_DIR}")) < at("daemon-reload"));
    assert!(said.contains("kbf-backup.timer"), "{said}");

    // The host after: the timer is no longer enabled, and the run plans nothing of it.
    os.enabled.remove(TIMER);
    exec.log.clear();
    let again = go_ok(&os, &mut exec, &raw(Mode::Frontend));
    assert!(
        mentions_backup(&exec).is_empty(),
        "{:?}",
        mentions_backup(&exec)
    );
    assert!(!cmds(&exec).iter().any(|c| c.contains("daemon-reload")));
    assert!(!again.contains("kbf-backup"), "{again}");
}

/// Catches a fresh host getting backup steps or a reload it does not need.
///
/// Mutant to plant: the removal made unconditional (red: a step names kbf-backup).
#[test]
fn a_host_that_never_had_the_backup_timer_gets_no_step_for_it() {
    let mut exec = released(host(), Platform::Linux);
    let said = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    assert!(
        mentions_backup(&exec).is_empty(),
        "{:?}",
        mentions_backup(&exec)
    );
    assert!(!cmds(&exec).iter().any(|c| c.contains("daemon-reload")));
    assert!(!said.contains("kbf-backup"), "{said}");
}

/// Catches the cleanup keyed to the timer alone: a host whose timer was already disabled by
/// hand still has the drop-in (or just its empty directory) an earlier setup made, and the
/// drop-in would go on giving the retired backup unit a user. Each leftover is removed and
/// reported without a `disable` step, and systemd reloads.
///
/// Mutants to plant: the removal made conditional on the timer being enabled (red: the file or
/// the directory stays); the `disable` made unconditional (red: a disable step on a host whose
/// timer is already off).
#[test]
fn a_leftover_dropin_or_directory_is_removed_when_the_timer_is_already_off() {
    let dir = || Entry::Dir {
        perms: Perms::new(0o755),
        owner: ROOT,
    };
    let mut exec = released(host(), Platform::Linux);
    exec.entries.insert(hp(DROPIN_DIR), dir());
    put(
        &mut exec,
        DROPIN,
        0o644,
        ROOT,
        "[Service]\nUser=kbf-server\n",
    );
    let said = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    let steps = mentions_backup(&exec);
    assert_eq!(
        steps,
        [format!("remove {DROPIN}"), format!("remove {DROPIN_DIR}")]
    );
    assert!(said.contains("kbf-backup.timer"), "{said}");
    assert!(cmds(&exec).iter().any(|c| c.contains("daemon-reload")));

    let mut exec = released(host(), Platform::Linux);
    exec.entries.insert(hp(DROPIN_DIR), dir());
    let said = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    assert_eq!(mentions_backup(&exec), [format!("remove {DROPIN_DIR}")]);
    assert!(said.contains("kbf-backup.timer"), "{said}");
    assert!(!exec.entries.contains_key(&hp(DROPIN_DIR)));
}
