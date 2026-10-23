//! The small classifications read off the update module: a release's name and the policy's
//! defaults.
//!
//! Each test names the defect it would catch.

use std::time::Duration;

use kbf_server::members::MembershipConfig;
use kbf_server::update::{Policy, Release};

const MIN: u64 = 60;

/// Catches: a release that does not give back the name it was made from.
#[test]
fn a_release_keeps_its_name() {
    assert_eq!(Release::new("0123abcd").as_str(), "0123abcd");
}

/// Catches: defaults that drift from the numbers an older build's rows mean when they leave a
/// field out, or from the membership config (the time to live and the dwell are
/// `MembershipConfig`'s), a Mac forced by default, and a capacity floor nobody asked for.
#[test]
fn the_default_policy_is_the_designs_and_fits_the_membership_it_reads() {
    let p = Policy::default();
    let m = MembershipConfig::default();
    assert_eq!(p.ttl, m.ttl);
    assert_eq!(p.dwell, m.dwell);
    let secs = Duration::from_secs;
    assert_eq!(p.idle_wait, secs(2 * MIN));
    assert_eq!(p.lease_limit, secs(15 * MIN));
    assert_eq!(p.mac_lease_limit, secs(60 * MIN));
    assert_eq!(p.write_limit, secs(MIN));
    assert_eq!(p.activate_limit, secs(5 * MIN));
    assert_eq!(p.health_limit, secs(2 * MIN));
    assert_eq!(p.repair_wait, secs(5 * MIN));
    assert_eq!(p.min_serving, 0);
    assert_eq!(p.max_hosts_out, 1);
    assert_eq!(p.mac_force_after, None);
    assert_eq!(p.rebuild_limit, None);
    assert!(p.auto_nodes);
    assert!(!p.auto_frontend && !p.paused);
}
