//! What a confirmed release on a package host asks of systemd: new files of this program, the
//! agent and their units are on disk, and the running ones are asked to restart on them.

use std::rc::Rc;

use super::answer::Answer;
use super::package_format::Format;
use super::package_tests::{Rig, both, new_version};
use super::request::LocalRequest;

const AGENT: &str = "kbf-updater-agent.service";
const HELPER: &str = "kbf-host-local.service";

/// The reloads and the no-block restarts that were asked of systemd, in order.
fn queued(rig: &Rig) -> Vec<String> {
    rig.events()
        .into_iter()
        .filter(|e| e == "reload" || e.starts_with("restart-no-block:"))
        .collect()
}

/// The argument vectors of the `systemctl` calls that reload or restart without blocking.
fn queued_argv(rig: &Rig) -> Vec<Vec<String>> {
    rig.calls
        .borrow()
        .iter()
        .filter(|c| c.program.ends_with("systemctl"))
        .filter(|c| c.args[0] == "daemon-reload" || c.args.iter().any(|a| a == "--no-block"))
        .map(|c| c.args.clone())
        .collect()
}

/// Nothing is asked of systemd for the agent or this program through the install, the restart
/// of the release's own units and a trial that is running; the confirm asks for a reload, then
/// for a restart of the agent, then of this program, each without waiting.
#[test]
fn a_confirm_reloads_systemd_then_queues_a_restart_of_the_agent_and_then_of_the_helper() {
    both(|format| {
        let rig = Rig::new("confirm-queue", format);
        let ab = rig.ab(Rc::new(std::cell::Cell::new(1_000_u64)));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let v = new_version();
        ask(&format!("install kbf={v} from stable"));
        assert!(matches!(
            ask(&format!("switch kbf={v}")),
            Answer::Switched { .. }
        ));
        assert!(rig.record().pending.is_some(), "the release is on trial");
        assert_eq!(queued(&rig), Vec::<String>::new());

        assert!(matches!(
            ask(&format!("confirm kbf={v}")),
            Answer::Confirmed { .. }
        ));
        assert_eq!(
            queued(&rig),
            [
                "reload".to_owned(),
                format!("restart-no-block:{AGENT}"),
                format!("restart-no-block:{HELPER}"),
            ]
        );
        let argv = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            queued_argv(&rig),
            [
                argv(&["daemon-reload"]),
                argv(&["restart", "--no-block", "--", AGENT]),
                argv(&["restart", "--no-block", "--", HELPER]),
            ]
        );
        // The confirm was kept before systemd was asked: the record is final.
        assert_eq!(rig.record().pending, None);
    });
}

/// A trial that goes back asks systemd for nothing of the agent or this program: a revert on
/// request, a revert at the deadline, and a switch whose installation failed.
#[test]
fn a_trial_that_goes_back_queues_no_reload_and_no_restart_of_the_agent_or_the_helper() {
    both(|format| {
        let v = new_version();

        let rig = Rig::new("confirm-revert", format);
        let ab = rig.ab(Rc::new(std::cell::Cell::new(1_000_u64)));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        assert!(matches!(ask("revert"), Answer::Reverted { .. }));
        assert_eq!(queued(&rig), Vec::<String>::new());

        let rig = Rig::new("confirm-deadline", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(Rc::clone(&now));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        now.set(1_600);
        assert!(matches!(ab.tick(), Some(Answer::Reverted { .. })));
        assert_eq!(queued(&rig), Vec::<String>::new());

        let rig = Rig::new("confirm-install-fails", format);
        rig.net.borrow_mut().install_fails = 1;
        let ab = rig.ab(Rc::new(std::cell::Cell::new(1_000_u64)));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        ask(&format!("install kbf={v} from stable"));
        assert!(matches!(
            ask(&format!("switch kbf={v}")),
            Answer::Failed(..)
        ));
        assert_eq!(queued(&rig), Vec::<String>::new());
    });
}

/// A request that does not work is logged and does not undo the confirm, and one that fails
/// does not stop the next: a reload that fails, an agent restart that cannot be started and a
/// helper restart that exits with a failure all leave the release confirmed.
#[test]
fn a_request_to_systemd_that_fails_does_not_undo_the_confirm_or_stop_the_next() {
    for (reload_fails, agent_errors, helper_fails) in [
        (1, false, false),
        (0, true, false),
        (0, false, true),
        (1, true, true),
    ] {
        let rig = Rig::new("confirm-failing", Format::Deb);
        {
            let mut net = rig.net.borrow_mut();
            net.reload_fails = reload_fails;
            if agent_errors {
                net.no_block_errors.insert(AGENT.to_owned());
            }
            if helper_fails {
                net.no_block_fails.insert(HELPER.to_owned());
            }
        }
        let ab = rig.ab(Rc::new(std::cell::Cell::new(1_000_u64)));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let v = new_version();
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        assert!(matches!(
            ask(&format!("confirm kbf={v}")),
            Answer::Confirmed { .. }
        ));
        assert_eq!(
            queued(&rig),
            [
                "reload".to_owned(),
                format!("restart-no-block:{AGENT}"),
                format!("restart-no-block:{HELPER}"),
            ]
        );
        assert_eq!(rig.record().pending, None);
        assert_eq!(rig.installed(), Some(v));
    }
}

/// A confirm that was answered and lost, asked again, finds the release kept and answers the
/// same without asking systemd for anything more.
#[test]
fn a_confirm_asked_again_queues_nothing_more() {
    let rig = Rig::new("confirm-again", Format::Deb);
    let ab = rig.ab(Rc::new(std::cell::Cell::new(1_000_u64)));
    let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
    let v = new_version();
    ask(&format!("install kbf={v} from stable"));
    ask(&format!("switch kbf={v}"));
    ask(&format!("confirm kbf={v}"));
    let once = queued(&rig);
    assert_eq!(once.len(), 3);
    assert!(matches!(
        ask(&format!("confirm kbf={v}")),
        Answer::Confirmed { .. }
    ));
    assert_eq!(queued(&rig), once);
}
