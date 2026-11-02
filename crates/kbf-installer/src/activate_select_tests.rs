//! [`select`]: the operator's swap to a release the host keeps, with no trial.

use super::*;

/// A swap to a kept release writes the state first and moves both links: `current` is the
/// release named, `previous` the release that ran, nothing is on trial and a `staged` that
/// names the release is cleared (one that names another release stays).
///
/// Catches: `previous` not moved to the release left; a trial armed by the swap; `staged`
/// left naming the release that now runs, or cleared when it names another.
#[test]
fn select_makes_a_kept_release_current_for_good_and_the_one_left_previous() {
    for (staged, want) in [(B, None), (C, Some(c(C)))] {
        let (mut exec, layout) = host(&format!("select-{staged}"));
        running(&exec, &layout, A, &[B, C]);
        let mut state = state_of(&layout);
        state.staged = Some(c(staged));
        Locked::acquire(&layout)
            .expect("lock")
            .write(&state)
            .expect("state");
        let selected = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("select");
        assert_eq!(selected, Selected { from: Some(c(A)) });
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(B)));
        assert_eq!(state.previous, Some(c(A)));
        assert_eq!(state.pending_confirm, None);
        assert_eq!(state.staged, want, "{staged}");
        assert_eq!(link(&exec, "current"), at(B));
        assert_eq!(link(&exec, "previous"), at(A));
    }
}

/// A swap on trial ends at a select: going back to the release before the trial leaves the one
/// on trial as `previous`; naming the release on trial keeps it, and `previous` stays the one
/// before it.
///
/// Catches: a `pending_confirm` that outlives the select, so the deadline or the boot check
/// later reverts what the operator chose.
#[test]
fn select_ends_a_swap_on_trial_whichever_release_it_names() {
    for (target, want_previous) in [(A, B), (B, A)] {
        let (mut exec, layout) = host(&format!("select-trial-{target}"));
        running(&exec, &layout, A, &[B]);
        go(&mut exec, &layout, &order(B, "o1", 1), START).expect("activate");
        assert!(state_of(&layout).pending_confirm.is_some());
        let selected = select(&mut exec, &layout, &c(target), &mut Vec::new()).expect("select");
        assert_eq!(selected.from, Some(c(B)), "{target}");
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(target)), "{target}");
        assert_eq!(state.previous, Some(c(want_previous)), "{target}");
        assert_eq!(state.pending_confirm, None, "{target}");
        assert_eq!(link(&exec, "current"), at(target));
        assert_eq!(link(&exec, "previous"), at(want_previous));
    }
}

/// The release that runs, named again, keeps `previous` and makes the links agree with the
/// state; a host with no release yet takes the one named with no `previous`.
///
/// Catches: `previous` set to the release itself; a select refused on a host with no current.
#[test]
fn select_of_the_release_that_runs_or_on_an_empty_host_keeps_previous_as_it_is() {
    let (mut exec, layout) = host("select-same");
    running(&exec, &layout, A, &[B]);
    select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("to B");
    fs::remove_file(exec.at("/var/lib/kbf-installer/releases/current")).expect("unlink");
    let selected = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("again");
    assert_eq!(selected.from, Some(c(B)));
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(B)), Some(c(A))));
    assert_eq!(link(&exec, "current"), at(B));

    let (mut exec, layout) = host("select-empty");
    seed(&exec, A, 1_000);
    let selected = select(&mut exec, &layout, &c(A), &mut Vec::new()).expect("first");
    assert_eq!(selected.from, None);
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(A)), None));
}

/// A release that is not installed is refused and nothing changes: no state is written and no
/// link moves.
///
/// Catches: a select that moves `current` to a directory with no binary.
#[test]
fn select_refuses_a_release_that_is_not_installed_and_changes_nothing() {
    let (mut exec, layout) = host("select-missing");
    running(&exec, &layout, A, &[]);
    let before = state_of(&layout);
    let err = select(&mut exec, &layout, &c(D), &mut Vec::new()).expect_err("not installed");
    assert!(
        matches!(err, ReleaseError::NotInstalled(ref id) if *id == c(D)),
        "{err}"
    );
    assert_eq!(state_of(&layout), before);
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(link_steps(&exec), 0);
}
