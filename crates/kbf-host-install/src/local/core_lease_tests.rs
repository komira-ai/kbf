//! A switch the backend refused because leases still run (`leases_live`) is not an end of the
//! update: the last end the host tells stays as it was, before and after a restart, where any
//! other failed switch is recorded.

use super::answer::{Answer, Code, LastView};
use super::backend::Fault;
use super::core_tests::{C, Fx, VB, VC};

fn live() -> Fault {
    Fault::refused(Code::LeasesLive, "1 leases are live; not switching")
}

/// The last end `fx` tells, through the running process and through a restarted one.
fn last(fx: &Fx) -> (Option<LastView>, Option<LastView>) {
    let restarted = fx.restarted();
    let Answer::Queried(view) = restarted.handle(&super::request::LocalRequest::Query) else {
        panic!("not a query")
    };
    (fx.query().last, view.last)
}

/// Catches a live-lease refusal that clears or rewrites the host's last end: after an
/// update that succeeded (VB confirmed) and after one that failed (VB reverted), a
/// `leases_live` refusal of the next switch leaves the last end exactly as it was, read by
/// the process and by a restarted one.
#[test]
fn a_live_lease_refusal_keeps_the_last_end_as_it_was() {
    let succeeded = Fx::new("core-lease-ok");
    assert!(matches!(succeeded.install(VB), Answer::Staged { .. }));
    assert!(matches!(
        succeeded.ask(&format!("switch kbf={VB}")),
        Answer::Switched { .. }
    ));
    assert!(matches!(
        succeeded.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { .. }
    ));
    let failed = Fx::new("core-lease-failed");
    assert!(matches!(failed.install(VB), Answer::Staged { .. }));
    assert!(matches!(
        failed.ask(&format!("switch kbf={VB}")),
        Answer::Switched { .. }
    ));
    assert!(matches!(failed.ask("revert"), Answer::Reverted { .. }));

    for fx in [&succeeded, &failed] {
        let before = last(fx);
        assert!(before.0.is_some() && before.0 == before.1, "{before:?}");
        fx.world.borrow_mut().lays = Some(C.to_owned());
        assert!(matches!(fx.install(VC), Answer::Staged { .. }));
        fx.world.borrow_mut().switch_fault = Some(live());
        assert_eq!(
            fx.ask(&format!("switch kbf={VC}")),
            Answer::Refused(
                Code::LeasesLive,
                "1 leases are live; not switching".to_owned()
            )
        );
        assert_eq!(last(fx), before);
    }
}

/// Catches the exception spreading to other refusals: a switch the backend refused for
/// another reason is still recorded as the target's failed switch.
#[test]
fn another_refused_switch_is_still_recorded() {
    let fx = Fx::new("core-lease-other");
    assert!(matches!(fx.install(VB), Answer::Staged { .. }));
    fx.world.borrow_mut().switch_fault =
        Some(Fault::refused(Code::StateUnavailable, "cannot tell"));
    assert!(matches!(
        fx.ask(&format!("switch kbf={VB}")),
        Answer::Refused(Code::StateUnavailable, _)
    ));
    let (now, restarted) = last(&fx);
    let now = now.expect("a last end");
    assert_eq!(now.target, VB);
    assert_eq!(now.reason, Some("switch_failed"));
    assert_eq!(restarted, Some(now));
}
