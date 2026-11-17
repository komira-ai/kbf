//! The resume of a trial the helper has, on a node whose placement cannot be read: the
//! update's own rig ([`super::update_tests::Rig`]).

use super::local::Trial;
use super::testing::{COMMIT_B, VA, VB, version};
use super::update_tests::{NOW, Rig, succeeded};
use crate::ports::PortError;

/// An updater restarted with B on trial, on a node whose placement cannot be read, finishes the
/// trial but does not give the node back: nothing says the cordon was the update's.
#[test]
fn a_resumed_trial_on_an_unreadable_node_is_confirmed_and_the_node_is_not_given_back() {
    let rig = Rig::new();
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(NOW + 400),
        });
        world.before = Some(version(VA));
    }
    *rig.drain.drained.lock().unwrap() = true;
    *rig.drain.placed_error.lock().unwrap() = Some(PortError::other("503"));
    assert_eq!(rig.run(VB), succeeded(VB));
    assert!(rig.logged("the node could not be read, so it is not given back"));
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.events.all().contains(&format!("local:confirm {VB}")));
}
