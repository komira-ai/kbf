//! The root half of the frontend proxy's orders (`frontend_update`, `confirm`, `revert`) on a
//! frontend host, through the real cycle over a scratch host. See `cycle/frontend.rs`.

use super::*;

/// The `refusal` word of `status.json`: what precedes the first colon.
fn word(rig: &Rig) -> String {
    rig.refusal().split(':').next().unwrap_or("").to_owned()
}

/// A frontend host with `commit` staged by an order of id `id`.
fn staged(name: &str, commit: &str, id: u64) -> Rig {
    let mut rig = Rig::frontend(name);
    rig.order(id, Verb::FrontendUpdate, commit);
    rig.cycle(T0);
    assert_eq!(rig.state().staged, Some(c(commit)), "{}", rig.refusal());
    rig
}

/// An installed release older than the others, for `gc` to find.
fn old_release(rig: &Rig, commit: &str, age: u64) {
    rig.install(commit);
    fs::File::open(rig.root.join("var/lib/kbf-installer/releases").join(commit))
        .expect("open")
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(age))
        .expect("mtime");
}

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// Catches a frontend update that swaps (a link moves, a unit restarts, a trial starts) or
/// that stages nothing: a `frontend_update` is a locator, so the fetcher is asked for the
/// commit with no components; the release becomes the state's `staged` one and its facts
/// (with the profile) appear under `staged{}` for the proxy; `current` and both links stay
/// on the running release, no trial is armed, no unit is restarted, and the order is
/// acknowledged and removed.
///
/// Mutants planted: `staged` not written; `current` written too; a restart owed; the order's
/// `window_s` turned into a trial; the profile left out of `staged{}`.
#[test]
fn a_frontend_update_fetches_by_commit_and_stages_without_flipping() {
    let rig = staged("stage", B, 9);
    let calls = rig.fetcher.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!((&calls[0].commit, calls[0].components.len()), (&c(B), 0));
    let state = rig.state();
    assert_eq!(state.current, Some(c(A)));
    assert_eq!(state.previous, None);
    assert_eq!(state.pending_confirm, None, "window_s arms no trial");
    assert_eq!(link(&rig, "current"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 0);
    let status = rig.status();
    assert_eq!(status["state"], "staged");
    assert_eq!(status["staged_commit"], B);
    assert_eq!(status["installed_commit"], A);
    assert_eq!(status["acked_order_id"], 9);
    assert_eq!(status["staged"]["protocol"], 7);
    assert_eq!(status["staged"]["min_protocol"], 6);
    assert_eq!(status["staged"]["state_schema"], 3);
    assert_eq!(status["staged"]["profile"], "release");
    assert_eq!(rig.left(), 0);
}

/// Catches an order acted on twice, and a repeated stage that fetches or errs: the same record
/// put back is dropped (no second fetch), and a fresh order for the staged commit stages it
/// again (the fetcher re-verifies) and keeps it staged.
///
/// Mutants planted: the frontend floor not recorded (the replay fetches again); a repeat of
/// the staged commit refused as busy.
#[test]
fn a_replayed_frontend_update_is_a_no_op_and_a_new_one_for_the_same_commit_stages_again() {
    let mut rig = staged("replay", B, 9);
    rig.order(9, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.fetcher.count(), 1, "a replay fetches nothing");
    assert_eq!(rig.left(), 0, "removed unread");
    rig.order(10, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.fetcher.count(), 2, "verified again");
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.status()["state"], "staged");
}

/// Catches an unverifiable release being staged: the fetcher says it could not reach the
/// source, rejects the bytes, installs another commit or a release for another platform, or
/// reports a release it did not put on the host; each is refused with its word, nothing is
/// staged, `current` stays, and the floor still moved (the proxy sees its order answered).
///
/// Mutants planted: the fetch error ignored; the commit comparison dropped; the platform
/// comparison dropped; the presence of the release not checked under the lock.
#[test]
fn a_frontend_update_for_a_release_that_does_not_verify_stages_nothing() {
    /// What a case does to the rig before the order.
    type Plant = fn(&mut Rig);
    let cases: [(&str, Plant); 5] = [
        ("fetch", |rig| {
            *rig.fetcher.fail.lock().expect("lock") =
                Some(FetchError::Unavailable("source unreachable".to_owned()));
        }),
        ("stage", |rig| {
            *rig.fetcher.fail.lock().expect("lock") =
                Some(FetchError::Rejected("not attested".to_owned()));
        }),
        ("facts", |rig| {
            *rig.fetcher.tamper.lock().expect("lock") = Some(|r| r.commit = c(C));
        }),
        ("platform", |rig| {
            *rig.fetcher.tamper.lock().expect("lock") =
                Some(|r| r.platform = "darwin-arm64".to_owned());
        }),
        // A report of a good release with nothing on the host.
        ("stage", |rig| rig.helper.set_fetcher(Box::new(Hollow))),
    ];
    for (index, (want, plant)) in cases.iter().enumerate() {
        let mut rig = Rig::frontend(&format!("unverifiable-{index}"));
        plant(&mut rig);
        rig.order(4, Verb::FrontendUpdate, B);
        rig.cycle(T0);
        assert_eq!(rig.status()["state"], "refused", "case {index}");
        assert_eq!(word(&rig), *want, "case {index}: {}", rig.refusal());
        assert_eq!(rig.state().staged, None, "case {index}");
        assert_eq!(rig.current(), Some(c(A)), "case {index}");
        assert_eq!(rig.status()["acked_order_id"], 4, "case {index}");
        assert!(rig.status()["staged"].is_null(), "case {index}");
    }
}

/// Catches two updates in flight and an update to what already runs: with B staged, an order
/// for C is refused `frontend_busy` before anything is fetched and B stays staged; an order
/// for the installed release is refused `commit_or_ref`, also before a fetch.
///
/// Mutants planted: the busy check dropped (C replaces B); the check made after the fetch
/// (the fetcher is called); the installed-release check dropped (A is staged).
#[test]
fn a_second_frontend_update_and_one_for_the_installed_release_are_refused_before_a_fetch() {
    let mut rig = staged("busy", B, 1);
    rig.order(2, Verb::FrontendUpdate, C);
    rig.cycle(T0 + 3);
    assert_eq!(word(&rig), "frontend_busy", "{}", rig.refusal());
    assert_eq!(rig.state().staged, Some(c(B)));
    rig.order(3, Verb::FrontendUpdate, A);
    rig.cycle(T0 + 6);
    assert_eq!(word(&rig), "commit_or_ref", "{}", rig.refusal());
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.fetcher.count(), 1, "neither refusal fetched");
}

/// Catches a fetch started when the node cannot be told one is under way, as an activation
/// does: status.json cannot be written, so the order is refused before the fetch and nothing
/// is staged; the pass reports the failure to write.
///
/// Mutant planted: the report error ignored (the fetch happens anyway).
#[test]
fn a_frontend_update_stops_before_the_fetch_when_status_cannot_be_written() {
    let mut rig = Rig::frontend("status-unwritable");
    fs::remove_file(rig.root.join(UPDATE).join("status.json")).expect("status.json");
    fs::create_dir(rig.root.join(UPDATE).join("status.json")).expect("a directory in its place");
    rig.order(1, Verb::FrontendUpdate, B);
    assert!(rig.helper.cycle(T0).is_err());
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.state().staged, None);
}

/// Catches the flip being wrong: a `confirm` of the staged commit makes `previous` the release
/// that ran and `current` the staged one, in state and in both links, clears `staged`, restarts
/// no unit, and `status.json` reports the new `installed_commit`. The same commit confirmed
/// again, by a new order, changes nothing: `previous` stays the first release (it is not
/// overwritten by the new current).
///
/// Mutants planted: `previous` not moved; `current` link not moved; `staged` kept; a restart
/// owed; the repeat flipping again (`previous` becomes B).
#[test]
fn a_confirm_of_the_staged_commit_flips_previous_then_current_and_is_idempotent() {
    let mut rig = staged("confirm", B, 1);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    let state = rig.state();
    assert_eq!(state.current, Some(c(B)));
    assert_eq!(state.previous, Some(c(A)));
    assert_eq!(state.staged, None);
    assert_eq!(link(&rig, "current"), PathBuf::from(B));
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 0);
    let status = rig.status();
    assert_eq!(status["installed_commit"], B);
    assert_eq!(status["previous_commit"], A);
    assert_eq!(status["state"], "confirmed");
    assert_eq!(status["staged"]["profile"], "release", "facts of current");
    assert_eq!(status["acked_order_id"], 2);

    rig.order(3, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.state(), state, "a repeat changes nothing");
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert_eq!(rig.status()["state"], "confirmed");
}

/// Catches a flip onto a release that is no longer on the host: the staged directory was
/// removed after staging, so the confirm is refused (`stage`) and `current`, `previous`, the
/// links and `staged` stay as they were.
///
/// Mutant planted: the presence check dropped (`current` names a release with no binary).
#[test]
fn a_confirm_of_a_staged_release_whose_directory_is_gone_flips_nothing() {
    let mut rig = staged("confirm-gone", B, 1);
    fs::remove_dir_all(rig.root.join("var/lib/kbf-installer/releases").join(B)).expect("remove");
    let before = rig.state();
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(word(&rig), "stage", "{}", rig.refusal());
    assert_eq!(rig.state(), before);
    assert_eq!(link(&rig, "current"), PathBuf::from(A));
}

/// Catches a confirm that flips what is not staged: with B staged, a confirm of C (never
/// staged) is refused `commit_or_ref`; with nothing staged, a confirm of an unknown commit is
/// refused too; in both `current`, `previous` and `staged` are as they were.
///
/// Mutants planted: the staged comparison dropped (C becomes current); `NotStaged` returned
/// only when nothing is staged.
#[test]
fn a_confirm_of_a_commit_that_is_not_staged_flips_nothing() {
    let mut rig = staged("confirm-other", B, 1);
    let before = rig.state();
    rig.order(2, Verb::Confirm, C);
    rig.cycle(T0 + 3);
    assert_eq!(word(&rig), "commit_or_ref", "{}", rig.refusal());
    assert_eq!(rig.state(), before);
    assert_eq!(link(&rig, "current"), PathBuf::from(A));

    let mut rig = Rig::frontend("confirm-none");
    let before = rig.state();
    rig.order(2, Verb::Confirm, C);
    rig.cycle(T0);
    assert_eq!(word(&rig), "commit_or_ref", "{}", rig.refusal());
    assert_eq!(rig.state(), before);
}

/// Catches a confirm that flips the staged release while a node swap is on trial (two timers,
/// and `previous` taking an unconfirmed release), and one that mistakes a node's confirm for
/// the proxy's: with B on trial and C staged, a confirm of C is refused `pending` and nothing
/// moves; a confirm of B, the release on trial, is the node's, and keeps the swap.
///
/// Mutants planted: the pending check dropped (C becomes current over the trial); `kind_of`
/// ignoring the release on trial (the confirm of B is the proxy's and is refused as not
/// staged, the trial stays).
#[test]
fn a_confirm_is_refused_while_a_node_swap_is_on_trial_and_the_swap_s_own_confirm_still_works() {
    let mut rig = Rig::frontend("confirm-pending");
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    rig.order(2, Verb::FrontendUpdate, C);
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().staged, Some(c(C)));
    rig.order(3, Verb::Confirm, C);
    rig.cycle(T0 + 6);
    assert_eq!(word(&rig), "pending", "{}", rig.refusal());
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.state().staged, Some(c(C)));
    assert!(rig.state().pending_confirm.is_some());
    rig.first_start(B, T0 + 7);
    rig.order(4, Verb::Confirm, B);
    rig.cycle(T0 + 9);
    assert_eq!(rig.state().pending_confirm, None, "{}", rig.refusal());
    assert_eq!(rig.status()["state"], "staged");
    assert_eq!(
        rig.state().staged,
        Some(c(C)),
        "the frontend's staging stays"
    );
}

/// Catches a revert that touches `current`: a `revert` of the staged commit drops it from
/// `staged` and nothing else (state, links and unit as they were); the release stays on the
/// host. A repeat by a new order, and a revert of a commit that is not staged, change nothing.
/// After a confirm landed, a late revert of the confirmed commit leaves `current` and
/// `previous` where they are: it is not the node's revert, which would go back.
///
/// Mutants planted: revert falls through to the node's (`current` goes back to `previous`);
/// `staged` not cleared; the commit comparison dropped (C drops B's staging).
#[test]
fn a_revert_drops_only_the_staged_release_and_never_touches_current() {
    let mut rig = staged("revert", B, 1);
    rig.order(2, Verb::Revert, C);
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().staged, Some(c(B)), "C is not what is staged");
    let before = rig.state();
    rig.order(3, Verb::Revert, B);
    rig.cycle(T0 + 6);
    assert_eq!(
        rig.state(),
        State {
            staged: None,
            ..before.clone()
        }
    );
    assert_eq!(link(&rig, "current"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.status()["state"], "reverted");
    assert!(
        rig.refusal().contains("dropped from staged"),
        "{}",
        rig.refusal()
    );
    assert!(
        rig.root
            .join("var/lib/kbf-installer/releases")
            .join(B)
            .exists()
    );
    let after = rig.state();
    rig.order(4, Verb::Revert, B);
    rig.cycle(T0 + 9);
    assert_eq!(rig.state(), after, "a repeat is a no-op");
    assert_eq!(rig.refusal(), "", "and says nothing was dropped");

    let mut rig = staged("revert-after-confirm", B, 1);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    let landed = rig.state();
    rig.order(3, Verb::Revert, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.state(), landed);
    assert_eq!(link(&rig, "current"), PathBuf::from(B));
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 0);
}

/// Catches the release a confirm or revert leaves behind never being collected: both owe a
/// `gc` (the confirm because `previous` moved on, the revert because a staging was dropped),
/// which removes the releases nothing needs beyond the newest one.
///
/// Mutants planted: `kept` false after a flip; `kept` false after a drop.
#[test]
fn a_confirm_and_a_drop_each_collect_the_releases_nothing_needs() {
    let releases = |rig: &Rig, name: &str| {
        rig.root
            .join("var/lib/kbf-installer/releases")
            .join(name)
            .exists()
    };
    let mut rig = staged("gc-confirm", B, 1);
    old_release(&rig, D, 1_000);
    old_release(&rig, E, 2_000);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert!(!releases(&rig, D), "the oldest unneeded release is gone");
    assert!(releases(&rig, E));
    assert!(releases(&rig, A) & releases(&rig, B));

    let mut rig = staged("gc-revert", B, 1);
    old_release(&rig, D, 1_000);
    old_release(&rig, E, 2_000);
    rig.order(2, Verb::Revert, B);
    rig.cycle(T0 + 3);
    assert!(!releases(&rig, D));
    assert!(releases(&rig, A));
}

/// Catches the proxy's three orders sharing the node's floor, and the floor not being theirs:
/// they ride one frontend floor, so a `confirm` of an id below the `frontend_update` before
/// it is a replay, dropped, and flips nothing; an older epoch is refused with the word
/// `floor` and a jump of more than 1000 epochs likewise; a later epoch counts its ids again.
/// `status.json` reports the id of the order acted on.
///
/// Mutants planted: confirm and revert floored with the node pair (the low-id confirm flips);
/// the older-epoch arm or the jump arm dropped for the frontend kind; the word missing.
#[test]
fn the_proxy_s_three_orders_share_the_frontend_floor_and_stale_ones_are_refused() {
    let mut rig = staged("floor", B, 5);
    rig.order(4, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(A)), "id 4 is below 5: a replay");
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.status()["acked_order_id"], 5);

    rig.put(&rig.rec(6, 3, 1, Verb::Revert, B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().staged, None, "epoch 3 is a later epoch");
    rig.put(&rig.rec(9, 2, 1, Verb::FrontendUpdate, B));
    rig.cycle(T0 + 9);
    assert!(rig.refusal().starts_with("floor: "), "{}", rig.refusal());
    assert!(rig.refusal().contains("older"), "{}", rig.refusal());
    assert_eq!(rig.state().staged, None);
    rig.put(&rig.rec(1, 2000, 1, Verb::FrontendUpdate, B));
    rig.cycle(T0 + 12);
    assert!(rig.refusal().starts_with("floor: "), "{}", rig.refusal());
    assert!(rig.refusal().contains("more than"), "{}", rig.refusal());
    assert_eq!(rig.state().staged, None);
    assert_eq!(rig.fetcher.count(), 1);
}

/// Catches an order of the proxy changing the node's side, and the reverse: on a host that
/// runs no frontend a `confirm` and a `revert` are the node's (a revert with nothing on
/// trial goes back to `previous`), and on a frontend host the node floor is not moved by the
/// proxy's orders.
///
/// Mutant planted: `kind_of` ignoring the host's flag (the revert on a plain host is the
/// proxy's and drops nothing).
#[test]
fn on_a_host_without_a_frontend_confirm_and_revert_stay_the_node_s() {
    let mut rig = Rig::new("plain-revert");
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    rig.order(3, Verb::Revert, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.current(), Some(c(A)), "the node's revert went back");
    assert_eq!(rig.restarts(), 2);

    let mut rig = staged("floors-apart", B, 8);
    let memory: serde_json::Value = serde_json::from_slice(
        &fs::read(rig.root.join("var/lib/kbf-installer/helper.json")).expect("memory"),
    )
    .expect("json");
    assert_eq!(memory["acked_order_id"], 0);
    assert_eq!(memory["frontend_order_id"], 8);
    assert_eq!(memory["last_acted_id"], 8);
    rig.order(9, Verb::Revert, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["acked_order_id"], 9);
}

/// Catches the start-up repair owing a restart on a frontend host: the links disagree with the
/// state (a flip died between the state and the links), and `recover` makes them agree. On a
/// frontend host that owes no restart (the proxy owns the children and reads
/// `installed_commit`); on any other host it does; and on a frontend host with a node swap on
/// trial it does too.
///
/// Mutants planted: `restart_due` set whatever the host; the pending check dropped.
#[test]
fn recover_owes_a_restart_after_a_repair_unless_a_frontend_host_has_no_swap_on_trial() {
    let lagging = |rig: &mut Rig, pending: bool| {
        let layout = rig.layout.clone();
        rig.install(B);
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(B)),
                previous: Some(c(A)),
                pending_confirm: pending.then(|| crate::state::PendingConfirm {
                    activation: "1-1".to_owned(),
                    commit: c(B),
                    previous: Some(c(A)),
                    deadline_wall: None,
                    deadline_mono_s: None,
                    starts_left: Some(3),
                }),
                ..State::default()
            })
            .expect("state");
        rig.helper.recover().expect("recover");
        rig.cycle(T0);
        assert_eq!(link(rig, "current"), PathBuf::from(B), "repaired");
    };
    let mut rig = Rig::frontend("recover-frontend");
    lagging(&mut rig, false);
    assert_eq!(rig.restarts(), 0);
    let mut rig = Rig::new("recover-plain");
    lagging(&mut rig, false);
    assert_eq!(rig.restarts(), 1);
    let mut rig = Rig::frontend("recover-trial");
    lagging(&mut rig, true);
    assert_eq!(rig.restarts(), 1);
}

/// Catches a failure of the state lock taking the pass down: with the lock file unopenable
/// (a directory stands in its place) each of the three orders is refused with the word
/// `stage`, the pass goes on, and nothing is staged, flipped or dropped.
///
/// Mutant planted: the error of any of the three verbs turned into a fatal one (the pass
/// ends in an error).
#[test]
fn a_state_lock_that_cannot_be_opened_is_a_refusal_of_each_of_the_three_orders() {
    for (verb, commit) in [
        (Verb::FrontendUpdate, B),
        (Verb::Confirm, B),
        (Verb::Revert, B),
    ] {
        let mut rig = if verb == Verb::FrontendUpdate {
            Rig::frontend("locked-update")
        } else {
            staged("locked", B, 1)
        };
        let before = rig.state();
        let lock = rig.layout.join(&rig.layout.lock());
        fs::remove_file(&lock).expect("lock file");
        fs::create_dir(&lock).expect("a directory in its place");
        rig.order(2, verb, commit);
        let result = rig.helper.cycle(T0 + 3);
        assert!(result.is_ok(), "{verb:?}: {result:?}");
        assert_eq!(word(&rig), "stage", "{verb:?}: {}", rig.refusal());
        assert_eq!(rig.state(), before, "{verb:?}");
    }
}
