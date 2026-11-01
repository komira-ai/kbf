use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, Outcome};

use crate::converge::{Convergence, RoleCounts, State, converge, converge_service, role_counts};
use crate::services::Services;
use crate::testutil::{beat, desired, fleet, last, secs, unhealthy, updating};

const NOW: u64 = 1_000;

fn read(d: Option<(&str, u32, u32)>, hosts: &[(&str, HostRole, Heartbeat)]) -> Convergence {
    let d = d.map(|(v, s, f)| desired(v, s, f));
    converge(d.as_ref(), &fleet(secs(NOW), hosts), secs(NOW))
}

fn role<'a>(c: &'a Convergence, name: &str) -> &'a RoleCounts {
    c.roles.iter().find(|r| r.role == name).expect("a role")
}

/// Catches: a farm where every live host runs the desired version, healthy and idle, not
/// reading converged; and the counts per role not being the hosts' own.
#[test]
fn a_farm_all_at_the_desired_version_has_converged() {
    let c = read(
        Some(("2.0", 1, 1)),
        &[
            ("fe1", HostRole::FRONTEND, beat("2.0")),
            ("st1", HostRole::STORAGE, beat("2.0")),
            ("st2", HostRole::STORAGE, beat("2.0")),
            ("d1", HostRole::DAEMON, beat("2.0")),
        ],
    );
    assert_eq!(c.state, State::Converged);
    assert_eq!(c.desired.as_ref().expect("desired").as_str(), "2.0");
    assert!(!c.quiet);
    let names: Vec<&str> = c.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(names, ["frontend", "storage", "daemon"]);
    assert_eq!(
        *role(&c, "storage"),
        RoleCounts {
            role: "storage".to_owned(),
            at_desired: 2,
            updating: 0,
            failed: 0,
            down: 0
        }
    );
}

/// Catches: a host still behind, or one mid-update, being passed over: each reads
/// in_progress and is counted where it stands.
#[test]
fn a_host_behind_or_updating_keeps_the_farm_in_progress() {
    let behind = read(
        Some(("2.0", 0, 0)),
        &[
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("d2", HostRole::DAEMON, beat("1.0")),
        ],
    );
    assert_eq!(behind.state, State::InProgress);
    assert_eq!(role(&behind, "daemon").at_desired, 1);
    let mid = read(
        Some(("2.0", 0, 0)),
        &[
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("d2", HostRole::DAEMON, updating(beat("1.0"), "2.0")),
        ],
    );
    assert_eq!(mid.state, State::InProgress);
    assert_eq!(role(&mid, "daemon").updating, 1);
    // A host that says it is updating while already on the version is not converged either.
    let odd = read(
        Some(("2.0", 0, 0)),
        &[("d1", HostRole::DAEMON, updating(beat("2.0"), "2.0"))],
    );
    assert_eq!(odd.state, State::InProgress);
}

/// Catches: a failed update to the desired version not blocking, and a failed update to
/// another version blocking: only the first is blocked, and it wins over a roll that is
/// still moving elsewhere.
#[test]
fn a_failed_update_to_the_desired_version_blocks_and_to_an_older_one_does_not() {
    let blocked = read(
        Some(("2.0", 0, 0)),
        &[
            (
                "d1",
                HostRole::DAEMON,
                last(beat("1.0"), "2.0", Outcome::Failed),
            ),
            ("d2", HostRole::DAEMON, updating(beat("1.0"), "2.0")),
        ],
    );
    assert_eq!(blocked.state, State::Blocked);
    let d = role(&blocked, "daemon");
    assert_eq!((d.failed, d.updating, d.at_desired), (1, 1, 0));
    let older = read(
        Some(("2.0", 0, 0)),
        &[
            (
                "d1",
                HostRole::DAEMON,
                last(beat("1.0"), "1.5", Outcome::Failed),
            ),
            ("d2", HostRole::DAEMON, beat("2.0")),
            (
                "d3",
                HostRole::DAEMON,
                last(beat("1.0"), "1.5", Outcome::Succeeded),
            ),
        ],
    );
    assert_eq!(older.state, State::InProgress);
    assert_eq!(role(&older, "daemon").failed, 0);
    // A failure to the desired version of a host that has since come to run it, or that
    // succeeded, is no failure.
    let healed = read(
        Some(("2.0", 0, 0)),
        &[
            (
                "d1",
                HostRole::DAEMON,
                last(beat("2.0"), "2.0", Outcome::Failed),
            ),
            (
                "d2",
                HostRole::DAEMON,
                last(beat("2.0"), "2.0", Outcome::Succeeded),
            ),
        ],
    );
    assert_eq!(healed.state, State::Converged);
    assert_eq!(role(&healed, "daemon").failed, 0);
    // A failed host that is trying again is updating, not failed.
    let retry = read(
        Some(("2.0", 0, 0)),
        &[(
            "d1",
            HostRole::DAEMON,
            updating(last(beat("1.0"), "2.0", Outcome::Failed), "2.0"),
        )],
    );
    assert_eq!(retry.state, State::InProgress);
    assert_eq!(role(&retry, "daemon").failed, 0);
}

/// Catches: a silent host counted as at the desired version or as live: a host that last
/// said it ran the desired version but has not heartbeated for the down limit is `down`
/// and nowhere else, a silent failed host does not block, and a farm with only silent hosts
/// is not converged.
#[test]
fn a_silent_host_is_down_and_nothing_else() {
    let mut f = fleet(
        secs(NOW),
        &[
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("d2", HostRole::DAEMON, beat("2.0")),
        ],
    );
    f.record(
        crate::testutil::id("d3"),
        HostRole::DAEMON,
        last(beat("1.0"), "2.0", Outcome::Failed),
        secs(NOW - 100),
    );
    f.record(
        crate::testutil::id("d4"),
        HostRole::DAEMON,
        beat("2.0"),
        secs(NOW - 100),
    );
    let d = desired("2.0", 0, 0);
    let c = converge(Some(&d), &f, secs(NOW));
    assert_eq!(
        *role(&c, "daemon"),
        RoleCounts {
            role: "daemon".to_owned(),
            at_desired: 2,
            updating: 0,
            failed: 0,
            down: 2
        }
    );
    assert_eq!(c.state, State::Converged);
    let alone = read(None, &[]);
    assert_eq!(alone.state, State::InProgress);
    let silent_only = converge(
        Some(&d),
        &fleet(secs(0), &[("d1", HostRole::DAEMON, beat("2.0"))]),
        secs(NOW),
    );
    assert_eq!(silent_only.state, State::InProgress);
}

/// Catches: a coordinator that has just restarted reporting converged from what little it
/// has heard: inside the quiet start, with no host, or with fewer hosts up than the
/// minimums, the farm is in progress; with the minimums met it converges.
#[test]
fn a_restarted_coordinator_does_not_report_a_vacuous_convergence() {
    let hosts = [("st1", HostRole::STORAGE, beat("2.0"))];
    let d = desired("2.0", 1, 0);
    let f = fleet(secs(10), &hosts);
    let quiet = converge(Some(&d), &f, secs(20));
    assert!(quiet.quiet);
    assert_eq!(quiet.state, State::InProgress);
    assert_eq!(read(Some(("2.0", 0, 0)), &[]).state, State::InProgress);
    let short = read(Some(("2.0", 2, 0)), &hosts);
    assert_eq!(short.state, State::InProgress);
    let no_frontend = read(Some(("2.0", 1, 1)), &hosts);
    assert_eq!(no_frontend.state, State::InProgress);
    assert_eq!(read(Some(("2.0", 1, 0)), &hosts).state, State::Converged);
    // A storage host at the version but unhealthy is not up.
    let sick = read(
        Some(("2.0", 1, 0)),
        &[("st1", HostRole::STORAGE, unhealthy(beat("2.0")))],
    );
    assert_eq!(sick.state, State::InProgress);
}

const TWO_DAEMONS: &str = "[[service]]\nname = \"agent\"\n[service.roles.daemon]\nmin_up = 2\n";

/// A fleet whose hosts all heartbeated `service` at second 990: `kbf` hosts are not in it.
fn fleet_of(service: &str, hosts: &[(&str, HostRole, Heartbeat)]) -> crate::fleet::Fleet {
    let mut f = fleet(secs(990), &[("kbf1", HostRole::STORAGE, beat("9.0"))]);
    for (name, role, b) in hosts {
        f.record_in(
            service,
            crate::testutil::id(name),
            role.clone(),
            b.clone(),
            secs(990),
        );
    }
    f
}

/// Behaviour: a service's convergence reads its own hosts and its own desired state, never
/// kbf's: a kbf host at another version does not count, the role counts are the service's,
/// the service's required hosts up come from its role definition, and a failed update to
/// the service's desired version blocks it.
#[test]
fn a_service_converges_on_its_own_hosts_and_its_own_minimums() {
    let services = Services::parse(TWO_DAEMONS).expect("services");
    let def = services.get("agent").expect("agent");
    let d = desired("3.0", 0, 0);
    let one = fleet_of("agent", &[("d1", HostRole::DAEMON, beat("3.0"))]);
    let short = converge_service(def, Some(&d), &one, secs(NOW));
    assert_eq!(short.state, State::InProgress, "two daemons must be up");
    assert_eq!(short.desired.as_ref().expect("desired").as_str(), "3.0");
    assert_eq!(role(&short, "daemon").at_desired, 1);
    assert_eq!(short.roles.len(), 1, "the kbf storage host is not counted");
    let two = fleet_of(
        "agent",
        &[
            ("d1", HostRole::DAEMON, beat("3.0")),
            ("d2", HostRole::DAEMON, beat("3.0")),
        ],
    );
    assert_eq!(
        converge_service(def, Some(&d), &two, secs(NOW)).state,
        State::Converged
    );
    let stuck = fleet_of(
        "agent",
        &[
            ("d1", HostRole::DAEMON, beat("3.0")),
            (
                "d2",
                HostRole::DAEMON,
                last(beat("2.0"), "3.0", Outcome::Failed),
            ),
        ],
    );
    assert_eq!(
        converge_service(def, Some(&d), &stuck, secs(NOW)).state,
        State::Blocked
    );
    let none = converge_service(def, None, &two, secs(NOW));
    assert_eq!((none.desired, none.state), (None, State::InProgress));
    assert!(role_counts(def, None, &two, secs(NOW)).is_empty());
}
