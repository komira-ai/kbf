//! `hook.timeout_secs.prepare = 0`: a `prepare` with no bound (a drain waits as long as the work
//! it waits for runs), and no other step.

mod support;

use support::{refusal, sample, with};
use update_hook::Step;
use update_rights::{Declaration, properties, widening};

/// Catches a drain cut by a bound nobody asked for, or no bound granted where it is not meant:
/// `prepare = 0` is a declaration whose `prepare` unit runs with `RuntimeMaxSec=infinity` (never
/// systemd's 0); a bounded `prepare` keeps its number; any other step at 0 is refused.
#[test]
fn prepare_alone_may_have_no_bound_and_its_unit_then_has_none() {
    let unbounded = Declaration::parse(&with("prepare = 600", "prepare = 0")).unwrap();
    assert!(properties(&unbounded, Step::Prepare).contains(&"RuntimeMaxSec=infinity".to_owned()));
    assert!(properties(&sample(), Step::Prepare).contains(&"RuntimeMaxSec=600".to_owned()));
    for step in [
        "cutover = 300",
        "verify = 1000",
        "rollback = 600",
        "cleanup = 1300",
    ] {
        let (name, _) = step.split_once(" = ").unwrap();
        let refused = refusal(&with(step, &format!("{name} = 0"))).to_string();
        assert!(
            refused.contains(&format!("hook.timeout_secs.{name}")),
            "{refused}"
        );
    }
}

/// Catches no bound slipping in as a narrowing: going from a bounded `prepare` to none asks for
/// more (it is the highest bound), and from none to a bound asks for less.
#[test]
fn no_bound_is_the_highest_bound() {
    let bounded = sample();
    let unbounded = Declaration::parse(&with("prepare = 600", "prepare = 0")).unwrap();
    assert_eq!(
        widening(&bounded, &unbounded),
        ["hook.timeout_secs.prepare"]
    );
    assert!(widening(&unbounded, &bounded).is_empty());
}
