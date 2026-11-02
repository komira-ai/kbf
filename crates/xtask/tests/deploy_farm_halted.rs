//! `deploy-farm` against a coordinator that answers `halted`: the wait ends at once, as a
//! failure that names the host, the check and the version the host failed on. The fakes are
//! those of `deploy_farm.rs`.

use serde_json::{Value, json};

#[path = "support/deploy_fake.rs"]
mod deploy_fake;
use deploy_fake::{Fake, Script, farm, run, serve};

const VERSION: &str = "2.0.0+g000000000002";

/// A farm whose coordinator says `halted`, with `halt` when there is one.
fn halted(halt: Option<Value>) -> Value {
    let mut body = farm("halted", 0, 1);
    if let Some(halt) = halt {
        body["halt"] = halt;
    }
    body
}

/// The requests of a run that ended at the halted read: the baseline read, the POST of the
/// desired version, one more read, and nothing after.
fn assert_stopped_at_the_halted_read(fake: &Fake) {
    let seen = fake.seen.lock().expect("lock");
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .filter(|r| !r.path.starts_with("/token"))
        .map(|r| (r.method.as_str(), r.path.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("GET", "/v1/convergence"),
            ("POST", "/v1/desired"),
            ("GET", "/v1/convergence"),
        ]
    );
}

/// Proves: `halted` ends the wait at the read that says it, as a failure that names the host,
/// the check and the version, after the table was printed; no read follows. Catches: an
/// unknown state that would only have stalled until the stall limit.
#[test]
fn a_halted_farm_fails_at_once_naming_the_host_and_the_check() {
    let fake = serve(Script::new(vec![Ok(halted(Some(
        json!({"node_id": "s1", "check": "disk_tainted", "target": VERSION}),
    )))]));
    let (result, out) = run(&fake, &fake.plan());
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted"), "{failure}");
    assert!(failure.contains("s1"), "{failure}");
    assert!(failure.contains("disk_tainted"), "{failure}");
    assert!(failure.contains(VERSION), "{failure}");
    assert_stopped_at_the_halted_read(&fake);
    assert_eq!(fake.requests("/v1/desired").len(), 1);
    assert!(
        out.contains(&format!("farm: halted, desired {VERSION}")),
        "{out}"
    );
}

/// Proves: a coordinator that says `halted` with no `halt` object is still a failure, and the
/// message says the host, the check and the version are not known instead of naming a blank.
/// Catches: a halted answer without the halt object that is read as a missing field and stalls
/// or panics.
#[test]
fn a_halted_answer_without_the_halt_still_fails() {
    let fake = serve(Script::new(vec![Ok(halted(None))]));
    let (result, _) = run(&fake, &fake.plan());
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(
        failure.starts_with(
            "halted: unknown reported a failure no version can repair (unknown) while updating to unknown;"
        ),
        "{failure}"
    );
    assert_stopped_at_the_halted_read(&fake);
}

/// Proves: the version the failure names as the one the host failed on is the halt's own
/// `target`, even when the desired version has since become the deploy's (so the run is not
/// superseded). Catches: the deploy's version named as the one the host failed on, which sends
/// an operator looking at a release that never ran on that host.
#[test]
fn a_halt_names_the_version_the_host_failed_on_not_the_deploys() {
    const OLDER: &str = "1.0.0+g000000000001";
    let fake = serve(Script::new(vec![Ok(halted(Some(
        json!({"node_id": "s1", "check": "disk_tainted", "target": OLDER}),
    )))]));
    let (result, _) = run(&fake, &fake.plan());
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted: s1"), "{failure}");
    assert!(
        failure.contains(&format!("while updating to {OLDER}")),
        "{failure}"
    );
    assert!(
        !failure.contains(&format!("while updating to {VERSION}")),
        "{failure}"
    );
    assert_stopped_at_the_halted_read(&fake);
}
