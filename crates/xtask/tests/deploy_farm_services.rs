//! `deploy-farm --service tierstore`: the service is set by name and waited on by its own
//! convergence, and nothing that is kbf's (its `min_up` keys, its commit check) is asked of it.
//! The fakes are those of `deploy_farm.rs`.

use serde_json::{Value, json};
use xtask::deploy_farm::{Error, Plan};

#[path = "support/deploy_fake.rs"]
mod deploy_fake;
use deploy_fake::{Fake, Script, args_for, farm, run, serve, service_args};

const TIERSTORE_VERSION: &str = "0.4.2";

/// What the coordinator says of tierstore: six storage hosts and no other role.
fn tierstore(state: &str, at_desired: u64, updating: u64) -> Value {
    json!({"desired": TIERSTORE_VERSION, "quiet": false, "state": state, "roles": [
        {"role": "storage", "at_desired": at_desired, "updating": updating, "failed": 0, "down": 0},
    ]})
}

fn plan_of(fake: &Fake, service: &str, version: &str) -> Plan {
    let mut plan = fake.plan();
    plan.service = service.to_owned();
    plan.version = version.to_owned();
    plan
}

/// Proves: tierstore is posted with its name and version and no `min_up` (the minimums of
/// the storage role are the definition's), every read asks for tierstore's own convergence,
/// and the run ends when that convergence says converged. Catches: kbf's `storage` and
/// `frontend` minimums sent for a service that has no frontend.
#[test]
fn tierstore_is_set_by_name_without_kbf_minimums_and_read_by_its_own_convergence() {
    let fake = serve(Script::after(
        Ok(tierstore("converged", 6, 0)),
        vec![
            Ok(tierstore("in_progress", 5, 1)),
            Ok(tierstore("converged", 6, 0)),
        ],
    ));
    let (result, out) = run(&fake, &plan_of(&fake, "tierstore", TIERSTORE_VERSION));
    assert_eq!(result, Ok(()), "{out}");
    let posts = fake.requests("/v1/desired");
    assert_eq!(posts.len(), 1);
    let body: Value = serde_json::from_str(&posts[0].body).expect("json");
    assert_eq!(
        body,
        json!({"service": "tierstore", "version": TIERSTORE_VERSION})
    );
    let reads = fake.requests("/v1/convergence");
    assert_eq!(reads.len(), 3);
    for read in &reads {
        assert_eq!(read.path, "/v1/convergence?service=tierstore");
    }
}

/// Proves: the coordinator's commit is compared with the release's only for kbf. A tierstore
/// version has no `+g<commit>`, so with the requirement on, a coordinator that reports a
/// commit does not stop the deploy, the version is posted, and the log says the check was
/// not made. Catches: a deploy of any other service refused with "update the coordinator
/// first" because its version text is not kbf's.
#[test]
fn the_coordinator_commit_is_not_compared_with_another_services_version() {
    let mut baseline = tierstore("converged", 6, 0);
    baseline["commit"] = json!("0123456789ab");
    let fake = serve(Script::after(
        Ok(baseline),
        vec![Ok(tierstore("converged", 6, 0))],
    ));
    let mut plan = plan_of(&fake, "tierstore", TIERSTORE_VERSION);
    plan.require_coordinator_current = true;
    let (result, out) = run(&fake, &plan);
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(fake.requests("/v1/desired").len(), 1);
    assert!(
        out.contains("not checking the coordinator's commit against tierstore"),
        "{out}"
    );
}

/// Proves: for kbf the check is as it was: a coordinator on another commit than the
/// release's stops a deploy that requires it current, before any POST.
#[test]
fn kbf_is_still_refused_by_a_coordinator_on_another_commit() {
    let mut baseline = farm("converged", 0, 0);
    baseline["commit"] = json!("0123456789ab");
    let fake = serve(Script::after(
        Ok(baseline),
        vec![Ok(farm("converged", 0, 0))],
    ));
    let mut plan = fake.plan();
    plan.require_coordinator_current = true;
    let (result, _) = run(&fake, &plan);
    assert!(matches!(result, Err(Error::CoordinatorBehind { .. })));
    assert!(fake.requests("/v1/desired").is_empty());
}

/// Proves: kbf's request is the one it always was, minimums included, with no `service` key,
/// and the read is the bare path. Pins existing behaviour.
#[test]
fn kbf_still_sends_its_minimums_and_reads_the_bare_path() {
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(
        fake.requests("/v1/desired")[0].body,
        r#"{"min_up":{"frontend":1,"storage":1},"version":"2.0.0+g000000000002"}"#
    );
    for read in fake.requests("/v1/convergence") {
        assert_eq!(read.path, "/v1/convergence");
    }
}

/// Proves: a tierstore host that is live before the deploy and not at the version after it
/// fails the deploy though the coordinator says converged: the storage role alone is
/// compared with its baseline. Pins existing behaviour for a one-role service.
#[test]
fn a_storage_host_lost_during_the_deploy_fails_it() {
    let fake = serve(Script::after(
        Ok(tierstore("converged", 6, 0)),
        vec![Ok(tierstore("converged", 5, 0))],
    ));
    let (result, _) = run(&fake, &plan_of(&fake, "tierstore", TIERSTORE_VERSION));
    assert!(
        matches!(result, Err(Error::Lost { ref role, expected: 6, reached: 5, .. }) if role == "storage"),
        "{result:?}"
    );
}

/// Proves: the flags of a run name the service and the version text as given (no kbf form
/// is asked of it), and the plan keeps both.
#[test]
fn the_flags_carry_tierstore_and_its_version_into_the_plan() {
    let fake = serve(Script::new(vec![Ok(tierstore("converged", 6, 0))]));
    let args = service_args(&fake, "tierstore", TIERSTORE_VERSION);
    assert_eq!(args.service, "tierstore");
    let plan = Plan::of(&args);
    assert_eq!(plan.service, "tierstore");
    assert_eq!(plan.version, TIERSTORE_VERSION);
    assert_eq!(args_for(&fake, &fake.ca_pem).service, "kbf");
}
