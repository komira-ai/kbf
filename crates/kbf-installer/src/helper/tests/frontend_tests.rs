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
/// Catches: `staged` not written; `current` written too; a restart owed; the order's
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
/// Catches: the frontend floor not recorded (the replay fetches again); a repeat of
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
/// Catches: the fetch error ignored; the commit comparison dropped; the platform
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
/// Catches: the busy check dropped (C replaces B); the check made after the fetch
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
/// Catches: the report error ignored (the fetch happens anyway).
#[test]
fn a_frontend_update_stops_before_the_fetch_when_status_cannot_be_written() {
    let mut rig = Rig::frontend("frontend-status-unwritable");
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
/// Catches: `previous` not moved; `current` link not moved; `staged` kept; a restart
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
/// Catches: the presence check dropped (`current` names a release with no binary).
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
/// Catches: the staged comparison dropped (C becomes current); `NotStaged` returned
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
/// Catches: the pending check dropped (C becomes current over the trial); `kind_of`
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

/// Catches a node's revert taken for the proxy's on a frontend host: with B on trial (a node
/// swap), a `revert` naming B is the swap's own and goes back to A, restarting the unit; a
/// `revert` naming another commit is the proxy's and leaves the trial alone.
///
/// Catches: `kind_of` ignoring the release on trial for a revert (B's revert drops a
/// staging that is not there and the trial stays).
#[test]
fn on_a_frontend_host_a_revert_naming_the_release_on_trial_is_the_node_s() {
    let mut rig = Rig::frontend("revert-trial");
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    rig.order(2, Verb::Revert, C);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)), "C is not what is on trial");
    assert!(rig.state().pending_confirm.is_some());
    rig.order(3, Verb::Revert, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.current(), Some(c(A)), "{}", rig.refusal());
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.restarts(), 2, "the swap and its undoing");
}

/// Catches a revert that touches `current`: a `revert` of the staged commit drops it from
/// `staged` and nothing else (state, links and unit as they were); the release stays on the
/// host. A repeat by a new order, and a revert of a commit that is not staged, change nothing.
/// After a confirm landed, a late revert of the confirmed commit leaves `current` and
/// `previous` where they are: it is not the node's revert, which would go back.
///
/// Catches: revert falls through to the node's (`current` goes back to `previous`);
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

/// A confirm collects the releases nothing needs beyond the four newest (`previous` moved on);
/// a revert, which drops a staging and confirms nothing, removes no release.
///
/// Catches: `kept` false after a flip; a keep above or below four; a collection after a drop,
/// which is no confirm.
#[test]
fn a_confirm_collects_the_releases_nothing_needs_and_a_drop_collects_none() {
    let releases = |rig: &Rig, name: &str| {
        rig.root
            .join("var/lib/kbf-installer/releases")
            .join(name)
            .exists()
    };
    // Six others: the two oldest (D, E) are beyond the four newest.
    let newer = [
        ("1".repeat(40), 3_000),
        ("2".repeat(40), 4_000),
        ("3".repeat(40), 5_000),
        ("4".repeat(40), 6_000),
    ];
    let mut rig = staged("gc-confirm", B, 1);
    old_release(&rig, D, 1_000);
    old_release(&rig, E, 2_000);
    for (commit, age) in &newer {
        old_release(&rig, commit, *age);
    }
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert!(
        !releases(&rig, D) && !releases(&rig, E),
        "the oldest two are gone"
    );
    assert!(newer.iter().all(|(commit, _)| releases(&rig, commit)));
    assert!(releases(&rig, A) & releases(&rig, B));

    let mut rig = staged("gc-revert", B, 1);
    old_release(&rig, D, 1_000);
    old_release(&rig, E, 2_000);
    for (commit, age) in &newer {
        old_release(&rig, commit, *age);
    }
    rig.order(2, Verb::Revert, B);
    rig.cycle(T0 + 3);
    // A dropped stage is no confirm: every release stays.
    assert!(releases(&rig, D) && releases(&rig, E));
    assert!(newer.iter().all(|(commit, _)| releases(&rig, commit)));
    assert!(releases(&rig, A) && releases(&rig, B));
}

/// Catches the proxy's three orders sharing the node's floor, and the floor not being theirs:
/// they ride one frontend floor, so a `confirm` of an id below the `frontend_update` before
/// it is a replay, dropped, and flips nothing; an older epoch is refused with the word
/// `floor` and a jump of more than 1000 epochs likewise; a later epoch counts its ids again.
/// `status.json` reports the id of the order acted on.
///
/// Catches: confirm and revert floored with the node pair (the low-id confirm flips);
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
/// Catches: `kind_of` ignoring the host's flag (the revert on a plain host is the
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

    // A confirm that names no swap on trial is the node's too: refused as the node's is,
    // not as a frontend's `pending` or `commit_or_ref`.
    let mut rig = Rig::new("plain-confirm");
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    rig.order(2, Verb::Confirm, C);
    rig.cycle(T0 + 3);
    assert!(
        rig.refusal().contains("the pending swap of"),
        "{}",
        rig.refusal()
    );
    assert!(rig.state().pending_confirm.is_some());

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
/// Catches: `restart_due` set whatever the host; the pending check dropped.
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
/// Catches: the error of any of the three verbs turned into a fatal one (the pass
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

/// Catches a host taking the proxy's `frontend_update` on its flag, a guess or a different
/// key: whether the order is accepted is the top-level `mode` of the host's `kbf.toml` and
/// nothing else (design section 4). `frontend` and `all-in-one` stage the release, on Linux
/// and on macOS (where the file is elsewhere); `daemon`, no file, a file that is not TOML, a
/// `mode` that is not a string, a `mode` only inside a table and no `mode` are refused as
/// `denied` with the reason in `status.json`, the pass goes on, no floor moves and nothing is
/// staged.
///
/// Catches: `frontend_host` ignoring the mode (always `Ok`) fails every refused row;
/// always `Err` fails every accepted row.
#[test]
fn a_frontend_update_is_taken_by_the_mode_in_the_hosts_kbf_toml() {
    let accepted = [
        (Platform::Linux, "mode = \"frontend\"\n"),
        (
            Platform::Linux,
            "mode = \"all-in-one\"\n[listener]\nlisten = \"x\"\n",
        ),
        (Platform::MacOs, "mode = \"frontend\"\n"),
    ];
    for (index, (platform, text)) in accepted.into_iter().enumerate() {
        let mut rig = Rig::on(&format!("mode-ok-{index}"), platform);
        write_kbf_toml(&rig.layout, text);
        rig.order(1, Verb::FrontendUpdate, B);
        rig.cycle(T0);
        assert_eq!(
            rig.state().staged,
            Some(c(B)),
            "{text:?}: {}",
            rig.refusal()
        );
    }
    let refused: [(Option<&str>, &str); 6] = [
        (Some("mode = \"daemon\"\n"), "\"daemon\""),
        (None, "is not there"),
        (Some("mode = [unclosed\n"), "is not TOML"),
        (Some("mode = 3\n"), "has no mode"),
        (Some("[x]\nmode = \"frontend\"\n"), "has no mode"),
        (Some("listen = \"x\"\n"), "has no mode"),
    ];
    for (index, (text, why)) in refused.into_iter().enumerate() {
        let mut rig = Rig::new(&format!("mode-no-{index}"));
        if let Some(text) = text {
            write_kbf_toml(&rig.layout, text);
        }
        rig.order(1, Verb::FrontendUpdate, B);
        rig.cycle(T0);
        assert_eq!(word(&rig), "denied", "{text:?}: {}", rig.refusal());
        assert!(rig.refusal().contains(why), "{text:?}: {}", rig.refusal());
        assert_eq!(rig.state().staged, None, "{text:?}");
        let memory = fs::read(rig.root.join("var/lib/kbf-installer/helper.json")).expect("memory");
        let memory: serde_json::Value = serde_json::from_slice(&memory).expect("json");
        assert_eq!(memory["frontend_order_id"], 0, "{text:?}: no floor moves");
    }
}

/// Catches a replayed `confirm` that trusts the state and leaves a lagging link: the state
/// says B is current (A previous) but `current` still points at A (a crash after the state
/// write and before the link). A new confirm order for B reports it kept, makes `current`
/// point at B, and leaves `previous` on A, in the state and on disk.
///
/// Catches: `converge` removed from the repeat-confirm path of `confirm_frontend`
/// (`current` stays on A).
#[test]
fn a_replayed_frontend_confirm_makes_a_lagging_link_match_the_state() {
    let mut rig = staged("confirm-lag", B, 1);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    assert_eq!(rig.state().previous, Some(c(A)));
    let current = rig.root.join("var/lib/kbf-installer/releases/current");
    fs::remove_file(&current).expect("unlink");
    symlink(A, &current).expect("lag");
    assert_eq!(link(&rig, "current"), PathBuf::from(A));
    let before = rig.state();
    rig.order(3, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert_eq!(link(&rig, "current"), PathBuf::from(B), "{}", rig.refusal());
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert_eq!(rig.state(), before, "the state is what it was");
}

/// Catches a `kbf.toml` the helper cannot read whole taking the pass down or passing for a
/// frontend: bytes that are not text, a file over the cap and a directory where the file
/// should be are each `denied` with their own reason, and the pass goes on.
///
/// Catches: the size check dropped (the oversized file, a comment, parses and is
/// refused for having no mode, not for its size).
#[test]
fn a_kbf_toml_that_cannot_be_read_whole_is_denied_with_its_reason() {
    let big = format!("mode = \"frontend\"\n#{}\n", "x".repeat(1 << 20));
    let rows: [(&[u8], &str); 3] = [
        (b"mode = \"frontend\"\n\xff\xfe", "is not UTF-8 text"),
        (big.as_bytes(), "is over 1048576 bytes"),
        (b"", "cannot be read"),
    ];
    for (index, (bytes, why)) in rows.into_iter().enumerate() {
        let mut rig = Rig::new(&format!("mode-unread-{index}"));
        let path = rig.layout.join(&rig.layout.kbf_toml());
        fs::create_dir_all(&path).expect("config dir");
        if !bytes.is_empty() {
            fs::remove_dir(&path).expect("a directory to replace");
            fs::write(&path, bytes).expect("kbf.toml");
        }
        rig.order(1, Verb::FrontendUpdate, B);
        rig.cycle(T0);
        assert_eq!(word(&rig), "denied", "{}", rig.refusal());
        assert!(rig.refusal().contains(why), "{}", rig.refusal());
        assert_eq!(rig.state().staged, None);
    }
}
