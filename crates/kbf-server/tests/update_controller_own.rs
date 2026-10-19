//! The controller held to its build (`docs/design/frontend-proxy.md` section 9, rule R4: nodes
//! are never driven to a release other than the frontend's own unless the row says rollback)
//! against a model farm. The runner's wiring, with a real farm, is in `update_quiesced.rs`;
//! the offer side of the rule is in `update_rows.rs` and `update_api.rs`.
//!
//! The case is the old child after a frontend rollback: the new child wrote its own release
//! (N+1) as the desired one, the proxy put the old child back in charge, and the old child's
//! controller reads the row. Downgrades are allowed (a downgrade is a rollback, and the row
//! says so), so the same row with the flag is driven.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{ControllerError, HostPhase, OwnCommit, Policy, Release, Rows, Tick};
use support::harness::{Backing, Rig, controller};
use support::world::{Gave, Spec};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// The build of the controller under test: its stamp, and the 40-digit commit it begins.
const OWN: &str = "0123456789ab";
const OWNED: &str = "0123456789abcdef0123456789abcdef01234567";
/// The next release: what the new child wrote.
const NEXT: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// The hosts of [`spec`].
const HOSTS: [&str; 5] = ["s1", "s2", "s3", "d1", "m1"];

fn spec() -> Spec {
    Spec {
        storage: 3,
        linux: 1,
        macs: 1,
        ..Spec::default()
    }
}

/// A rig whose controller is the one of the build `OWN`: the farm starts at the old release
/// and the rows say where it goes.
fn held_rig(backing: Backing, label: &str) -> Rig {
    let mut rig = Rig::new(backing, label, &spec(), Policy::default());
    rig.controller =
        controller(&rig.cell.store(), rig.epoch, &rig.world).held_by(OwnCommit::new(OWN));
    rig
}

/// One look, as `Rig::tick` makes it, without its assertion that the look succeeded.
fn look(rig: &mut Rig) -> Result<Tick, ControllerError> {
    rig.world.advance();
    rig.controller.tick(rig.world.time())
}

/// Everything the farm was told, and every row the controller keeps.
fn nothing_was_done(rig: &Rig) -> bool {
    let w = rig.world.lock();
    w.given.is_empty()
        && w.hosts.iter().all(|h| !h.cordoned)
        && rig.rows.hosts().expect("a read").is_empty()
        && rig.rows.slot().expect("a read").value.is_empty()
}

/// Catches an old child that drives the nodes on to a release ahead of its frontend: the row
/// names a release other than the controller's build's and is not a rollback. Over many
/// looks every one ends in the refusal naming both releases, no host is cordoned, ordered,
/// marked or given a row, and no place is claimed; the row is not touched. The control is in
/// the same test: the same row made a rollback (the flag set, the same run) is driven to the
/// end, every host activated with that release, so the refusal is not a controller that
/// drives nothing.
///
/// Mutants seen red: the check dropped from the controller (the first look cordons a host);
/// the flag ignored (the control never finishes); `&&` for `||` in the rule (the same).
#[test]
fn a_release_that_is_not_the_builds_and_not_a_rollback_is_driven_to_no_node() {
    for backing in BACKINGS {
        let mut rig = held_rig(backing, "r4-foreign");
        let run = rig.offer(NEXT);
        for _ in 0..25 {
            let refused = look(&mut rig).expect_err("the row is not this build's");
            assert!(
                matches!(
                    &refused,
                    ControllerError::Foreign { commit, own } if commit == NEXT && own == OWN
                ),
                "{backing:?}: {refused}"
            );
        }
        assert!(nothing_was_done(&rig), "{backing:?}: the controller acted");
        assert_eq!(
            rig.rows.desired().unwrap().value.unwrap().generation,
            run,
            "{backing:?}: the row moved"
        );

        // The operator states it: the same release, the same run, now a rollback.
        let rollback = rig
            .rows
            .offer_expecting(&Release::new(NEXT), rig.world.time(), None, true)
            .expect("a rollback");
        assert_eq!(rollback.generation, run, "{backing:?}");
        rig.finish(NEXT);
        let w = rig.world.lock();
        for host in HOSTS {
            let released: Vec<_> = w
                .given_to(host)
                .into_iter()
                .filter_map(|g| match g {
                    Gave::Activate(_, release) => Some(release.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(released, [NEXT], "{backing:?}: {host}");
        }
    }
}

/// Catches a controller that refuses its own release: the row names the release the build is
/// (the stamp and zeros, as the operator route writes it) and is not a rollback, and the
/// farm is driven to it.
///
/// Mutants seen red: the comparison made against the whole release instead of the stamp's
/// prefix (the build's own release is refused).
#[test]
fn the_release_of_the_build_is_driven_without_a_rollback() {
    for backing in BACKINGS {
        let mut rig = held_rig(backing, "r4-own");
        rig.offer(OWNED);
        assert!(
            !rig.rows.desired().unwrap().value.unwrap().rollback,
            "{backing:?}"
        );
        rig.finish(OWNED);
    }
}

/// Catches a refusal that is only for a run not yet begun: a host that is already in a step
/// when the row turns foreign stays where it is, with no new order and no move, until the row
/// is one the controller may drive; the roll then goes on and ends at the release. (The
/// proxy rolls the frontend back only before any node has moved, so a host in a step is not
/// the case R4 is for; the controller still does nothing rather than drive past its frontend.)
///
/// Mutants seen red: the flag ignored (the farm is never driven on to the next release); the
/// flag of a repeat of the desired release never moved (the row stays a refusal).
#[test]
fn a_host_in_a_step_is_left_where_it_is_while_the_row_is_foreign() {
    for backing in BACKINGS {
        let mut rig = held_rig(backing, "r4-midstep");
        rig.offer(OWNED);
        // Before the activate order, which a run for the next release would give again.
        rig.until("a host to be in a step", 120, |rig| {
            HOSTS.iter().any(|host| {
                let phase = rig.phase(host);
                phase == HostPhase::Selected || phase.is_early()
            })
        });
        let rows_before = rig.rows.hosts().expect("a read");
        let given_before = rig.world.lock().given.len();
        // The next release is written by a frontend this controller is not of.
        Rows::new(rig.cell.store())
            .offer(&Release::new(NEXT), rig.world.time())
            .expect("the other frontend's offer");
        for _ in 0..25 {
            assert!(
                matches!(look(&mut rig), Err(ControllerError::Foreign { .. })),
                "{backing:?}"
            );
        }
        assert_eq!(
            rig.rows.hosts().expect("a read"),
            rows_before,
            "{backing:?}"
        );
        assert_eq!(rig.world.lock().given.len(), given_before, "{backing:?}");

        // As a rollback it is driven: the farm ends at it.
        rig.rows
            .offer_expecting(&Release::new(NEXT), rig.world.time(), None, true)
            .expect("a rollback");
        rig.finish(NEXT);
    }
}
