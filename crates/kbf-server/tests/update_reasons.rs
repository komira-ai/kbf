//! The small classifications read off the update module: how loudly each hold and alert is
//! delivered, a release's name, which host rows are open, the policy's defaults and what it
//! refuses, and the commands entering a phase issues.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use std::time::Duration;

use kbf_server::members::MembershipConfig;
use kbf_server::update::{
    Alert, Command, Generation, HoldReason, Holder, HostPhase, HostRecord, OrderId, Policy,
    PolicyError, Release, Severity, Slot,
};
use support::{EPOCH, GENERATION, NOW, ORDER, id, policy, t};

const MIN: u64 = 60;

/// Catches: a hold that leaves the binary healthy, or the node already put back, paging a
/// person as if data availability were at risk (or the other way round). An activate order
/// the helper refused flipped nothing, so it warns; a revert that could not run pages.
///
/// Mutants: `ActivateRefused` moved to the paging group; `RevertRefused` moved to warn.
#[test]
fn every_hold_reason_has_the_severity_its_cause_calls_for() {
    for reason in [
        HoldReason::SecondMemberDown,
        HoldReason::RestartDeadline,
        HoldReason::MemberGone,
        HoldReason::RevertRefused,
    ] {
        assert_eq!(reason.severity(), Severity::Page, "{reason:?}");
    }
    for reason in [
        HoldReason::ActivateStalled,
        HoldReason::HealthLimit,
        HoldReason::RebuildLimit,
        HoldReason::RepairWait,
        HoldReason::ConfirmStalled,
        HoldReason::Reverted,
        HoldReason::StillCordoned,
        HoldReason::ActivateRefused,
    ] {
        assert_eq!(reason.severity(), Severity::Warn, "{reason:?}");
    }
}

/// Catches: an alert for a held run delivered at a fixed severity instead of its reason's.
#[test]
fn a_held_alert_is_as_loud_as_its_reason() {
    assert_eq!(
        Alert::Held(HoldReason::SecondMemberDown).severity(),
        Severity::Page
    );
    assert_eq!(
        Alert::Held(HoldReason::StillCordoned).severity(),
        Severity::Warn
    );
    assert_eq!(Alert::RebuildWaiting.severity(), Severity::Warn);
}

/// Catches: a release that does not give back the name it was made from.
#[test]
fn a_release_keeps_its_name() {
    assert_eq!(Release::new("0123abcd").as_str(), "0123abcd");
}

/// Catches: a host that is in a step, or held, read as open, so it could be started a
/// second time while it holds the slot.
#[test]
fn only_pending_deferred_and_earlier_done_rows_are_open() {
    let row = |phase| HostRecord {
        phase,
        since: t(NOW),
        generation: GENERATION,
        order: ORDER,
        boot_before: None,
    };
    let later = Generation(GENERATION.0 + 1);
    assert!(row(HostPhase::Pending).is_open_in(GENERATION));
    assert!(row(HostPhase::Deferred).is_open_in(GENERATION));
    assert!(!row(HostPhase::Done).is_open_in(GENERATION));
    assert!(row(HostPhase::Done).is_open_in(later));
    for phase in [
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Held {
            reason: HoldReason::RepairWait,
        },
    ] {
        assert!(!row(phase.clone()).is_open_in(later), "{phase:?}");
    }
}

/// Catches: defaults that drift from the numbers of the design (section 2.6 to 2.10) or from
/// the membership config (the time to live and the dwell are
/// `MembershipConfig`'s), a Mac forced by default, a capacity floor nobody asked for, and
/// waits before the restart that outlast the planned-down mark (it lives for the dwell
/// whatever the node does).
#[test]
fn the_default_policy_is_the_designs_and_fits_the_membership_it_reads() {
    let p = policy();
    let m = MembershipConfig::default();
    assert_eq!(p.ttl, m.ttl);
    assert_eq!(p.dwell, m.dwell);
    let secs = Duration::from_secs;
    assert_eq!(p.restart_deadline(), secs(270));
    assert_eq!(p.idle_wait, secs(2 * MIN));
    assert_eq!(p.lease_limit, secs(15 * MIN));
    assert_eq!(p.mac_lease_limit, secs(60 * MIN));
    assert_eq!(p.health_limit, secs(2 * MIN));
    assert_eq!(p.repair_wait, secs(5 * MIN));
    assert_eq!(p.min_serving, 0);
    assert_eq!(p.max_hosts_out, 1);
    assert_eq!(p.mac_force_after, None);
    assert_eq!(p.rebuild_limit, None);
    assert!(p.write_limit * 2 + p.ttl < p.restart_deadline());
    assert_eq!(p.validate(), Ok(()));
}

/// Catches: a policy with no place for daemon-only hosts and Macs accepted, which no host
/// of those kinds could ever be started under. One place is the least; the default is one.
#[test]
fn a_policy_with_no_place_for_hosts_is_refused() {
    let none = Policy {
        max_hosts_out: 0,
        ..policy()
    };
    assert_eq!(none.validate(), Err(PolicyError::NoHostsOut));
    let one = Policy {
        max_hosts_out: 1,
        ..policy()
    };
    assert_eq!(one.validate(), Ok(()));
}

/// Catches: a policy whose waits cannot be kept. Steps 4 and 5 may each wait a write
/// limit while the mark lives only to the restart deadline; a dwell of two time-to-lives
/// or less leaves no deadline at all. Both are refused, and the boundary is exact.
#[test]
fn a_policy_whose_waits_outlast_the_restart_deadline_is_refused() {
    let secs = Duration::from_secs;
    let short = Policy {
        dwell: secs(30),
        ..policy()
    };
    let error = PolicyError::DwellTooShort {
        dwell: secs(30),
        ttl: secs(15),
    };
    assert_eq!(short.validate(), Err(error));

    let at = Policy {
        write_limit: secs(135),
        ..policy()
    };
    let error = PolicyError::WritesOutlastRestartDeadline {
        waits: secs(270),
        deadline: secs(270),
    };
    assert_eq!(at.validate(), Err(error));
    let under = Policy {
        write_limit: secs(134),
        ..policy()
    };
    assert_eq!(under.validate(), Ok(()));
}

/// Catches: a restart re-running a step it already took: only entering
/// `Activating` orders the activation, so a resume re-issues it for that phase and for no
/// other (no host is activated twice), and with the row's own order id, not a new one.
#[test]
fn entry_commands_name_the_activate_order_for_one_phase_only() {
    let phases = [
        HostPhase::Pending,
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Done,
        HostPhase::Held {
            reason: HoldReason::Reverted,
        },
        HostPhase::Deferred,
    ];
    let order = OrderId::new(EPOCH, 77);
    for phase in &phases {
        let record = HostRecord {
            phase: phase.clone(),
            since: t(0),
            generation: GENERATION,
            order,
            boot_before: None,
        };
        let commands = record.entry_commands();
        let activates = commands.contains(&Command::Activate { order });
        assert_eq!(activates, *phase == HostPhase::Activating, "{phase:?}");
        let confirms = commands.contains(&Command::Confirm { order });
        assert_eq!(confirms, *phase == HostPhase::Confirming, "{phase:?}");
        let any = commands
            .iter()
            .any(|c| matches!(c, Command::Activate { .. } | Command::Confirm { .. }));
        assert_eq!(any, activates || confirms, "{phase:?}: another order id");
    }
}

/// Catches: a slot with a holder read as free, or one that holds only a place of the other
/// kind read as free: the slot is empty only with no storage holder and no other holder.
#[test]
fn a_slot_is_empty_only_with_no_holder_of_either_kind() {
    let holder = |name| Holder {
        host: id(name),
        order: ORDER,
    };
    assert!(Slot::default().is_empty());
    let storage = Slot {
        storage: Some(holder("s1")),
        ..Slot::default()
    };
    assert!(!storage.is_empty());
    let hosts = Slot {
        hosts: vec![holder("d1")],
        ..Slot::default()
    };
    assert!(!hosts.is_empty());
}
