//! A fresh host (nothing installed) and the announcement of each install to the health port.

use std::sync::{Arc, Mutex};

use crate::api::{Phase, RollState};
use crate::fakes::Event;
use crate::ports::{Installed, PortError};
use crate::updater_restart_tests::{TARGET, record, restart};
use crate::updater_tests::Rig;

use Phase::{Done, Installing, Pending};

/// Catches a host with nothing installed being taken for an error, or for a host that is
/// older than the target: its install gets no answer, so the roll asks (a query), the host
/// says "not installed" (a null `installed_version`), and the roll installs on it. Then it
/// checks the host's health and the roll succeeds. Before `NotInstalled` that answer was a
/// query error and the roll waited out the 120 s of the query bound.
#[test]
fn a_fresh_host_is_installed_then_checked_and_the_roll_succeeds() {
    let rig = Rig::new("fresh-after-silence");
    rig.inst.script_install(
        "fe",
        vec![Err(PortError::unreachable("ssh: connection reset"))],
        Ok(()),
    );
    rig.inst
        .script_query("fe", vec![Ok(Installed::NotInstalled)]);
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(roll.hosts[0].attempts, 2);
    assert_eq!(
        &rig.log.touching("fe")[..4],
        &[
            Event::Install("fe".into(), TARGET.into()),
            Event::Query("fe".into()),
            Event::Install("fe".into(), TARGET.into()),
            Event::Health("fe".into()),
        ]
    );
    // The query bound was not waited out.
    assert!(rig.elapsed() < std::time::Duration::from_secs(120));
}

/// The same after a restart in `installing`: the host reports nothing installed, so the
/// install is run once more (the attempt counter says two), the health is checked, and the
/// roll goes on to the end.
#[test]
fn a_restart_over_a_fresh_host_installs_on_it() {
    let rig = Rig::new("fresh-after-restart");
    rig.inst.set_installed("st1", Installed::NotInstalled);
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
        &[
            Event::Query("st1".into()),
            Event::Install("st1".into(), TARGET.into()),
            Event::Health("st1".into())
        ]
    );
    assert_eq!(roll.hosts[1].attempts, 2);
}

/// Catches the install cap being skipped for a fresh host: after two installs that left
/// nothing installed the roll is interrupted for an operator, with a text that says the host
/// has no kbf, and a third install is not made.
#[test]
fn a_host_that_stays_uninstalled_is_for_an_operator() {
    let rig = Rig::new("fresh-forever");
    rig.inst
        .script_query("st1", vec![Ok(Installed::NotInstalled)]);
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 2), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    assert!(rig.log.installs().is_empty());
    let detail = roll.hosts[1].detail.clone().expect("detail");
    assert!(
        detail.contains("st1 has no kbf installed and not 2.0 after 2 install attempts"),
        "{detail}"
    );
}

/// Catches the repeat after a silent install being lost for a fresh host: the install made
/// for a host that reports nothing installed gets no answer, so the host is asked again, is
/// still bare, and gets its second (last) install, which lands.
#[test]
fn a_silent_install_on_a_fresh_host_is_repeated_once() {
    let rig = Rig::new("fresh-silent");
    rig.inst.set_installed("st1", Installed::NotInstalled);
    rig.inst.script_install(
        "st1",
        vec![Err(PortError::unreachable("ssh: connection reset"))],
        Ok(()),
    );
    rig.inst.script_query(
        "st1",
        vec![Ok(Installed::NotInstalled), Ok(Installed::NotInstalled)],
    );
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 0), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(roll.hosts[1].attempts, 2);
    assert_eq!(
        &rig.log.touching("st1")[..5],
        &[
            Event::Query("st1".into()),
            Event::Install("st1".into(), TARGET.into()),
            Event::Query("st1".into()),
            Event::Install("st1".into(), TARGET.into()),
            Event::Health("st1".into())
        ]
    );
}

/// Catches an adapter that is not told when an install begins: the health port hears of every
/// install call, before the call (the adapter notes the time to tell the swap of this install
/// from an earlier one), including the repeat after a silent answer.
#[test]
fn the_health_port_hears_of_every_install_before_it_begins() {
    let rig = Rig::new("announce");
    rig.inst.script_install(
        "fe",
        vec![Err(PortError::unreachable("ssh: connection reset"))],
        Ok(()),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (sink, health) = (Arc::clone(&seen), rig.health.clone());
    rig.inst.on_install(move |h| {
        sink.lock()
            .expect("lock")
            .push((h.alias.clone(), health.installing_calls().len()));
    });
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Done);
    let seen = seen.lock().expect("lock").clone();
    assert_eq!(
        seen,
        [
            ("fe".to_owned(), 1),
            ("fe".to_owned(), 2),
            ("st1".to_owned(), 3),
            ("dm".to_owned(), 4),
            ("st2".to_owned(), 5),
        ]
    );
    assert_eq!(
        rig.health.installing_calls(),
        ["fe", "fe", "st1", "dm", "st2"]
    );
}

/// Catches the health port not being told that a host is done: it hears of each host once it
/// reached `Done`, in roll order, and of no host that did not.
#[test]
fn the_health_port_hears_of_each_host_that_is_done() {
    let rig = Rig::new("settled");
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Done);
    assert_eq!(rig.health.settled_calls(), ["fe", "st1", "dm", "st2"]);
}
