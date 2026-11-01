//! Which roles are on the cold store's cluster, decided from what the stores say of themselves:
//! the deployment id `mc admin info` reports for each, or the run file's list of roles.

mod ops_support;

use serde_json::json;
use tierstore_bench::coldserver::{Basis, ColdServer, HotServer, read};
use tierstore_bench::role::Role;

use ops_support::*;

const HOT_ADDRESS: &str = "host-mark:9001";

/// A run on a machine whose stores answer `cold_id` and `hot_id` as their deployment ids
/// (`None`: an answer without one). The two stores are at two addresses.
fn decided(name: &str, cold_id: Option<&'static str>, hot_id: Option<&'static str>) -> ColdServer {
    let (dir, host, config, ops) = base(name);
    host.answer(move |cmd| {
        let hot = cmd.env.iter().any(|(_, v)| v.ends_with(HOT_ADDRESS));
        match if hot { hot_id } else { cold_id } {
            Some(id) => ok(&json!({"info": {"deploymentID": id}}).to_string()),
            None => ok(&json!({"info": {}}).to_string()),
        }
    });
    let _keep = dir;
    read(&host, &ops, &config)
}

/// Catches two addresses of one cluster being told apart: the same deployment id at the cold
/// address and the hot address is one cluster, and the hot role is among the roles counted.
#[test]
fn the_same_deployment_id_at_two_addresses_is_one_cluster() {
    let got = decided("tb-server-same", Some("dep-1"), Some("dep-1"));
    assert_eq!(got.roles, [Role::Hot, Role::Cold]);
    assert_eq!(
        (got.hot, got.decided_by),
        (HotServer::Same, Basis::DeploymentId)
    );
}

/// Catches two clusters taken for one.
#[test]
fn different_deployment_ids_are_two_clusters() {
    let got = decided("tb-server-different", Some("dep-1"), Some("dep-2"));
    assert_eq!(got.roles, [Role::Cold]);
    assert_eq!(
        (got.hot, got.decided_by),
        (HotServer::Different, Basis::DeploymentId)
    );
}

/// Catches a guess when a store does not say: an answer without an id, and a store that cannot
/// be asked, leave the hot store undecided.
#[test]
fn a_store_that_does_not_say_leaves_it_undecided() {
    let got = decided("tb-server-no-id", Some("dep-1"), None);
    assert_eq!(
        (got.hot, got.decided_by),
        (HotServer::Undecided, Basis::None)
    );
    let (dir, host, config, ops) = base("tb-server-fails");
    host.answer(|_| failed(1, "no route"));
    let got = read(&host, &ops, &config);
    assert_eq!(got.hot, HotServer::Undecided);
    let (_dir2, host, config, mut ops) = base("tb-server-no-key");
    ops.admin.remove(&Role::Hot);
    host.answer(|_| ok(&json!({"info": {"deploymentID": "dep-1"}}).to_string()));
    let got = read(&host, &ops, &config);
    assert_eq!(got.hot, HotServer::Undecided);
    assert!(host.ran().len() <= 2);
    drop(dir);
}

/// Catches the run file not overriding the ids, and a store being asked when the run file
/// already says: the list wins, and no command runs.
#[test]
fn the_run_files_list_overrides_and_asks_nothing() {
    let (_dir, host, mut config, ops) = base("tb-server-explicit");
    config.cold_cluster_roles = Some(vec![Role::Hot]);
    host.answer(|_| failed(1, "must not run"));
    let got = read(&host, &ops, &config);
    assert_eq!(got.roles, [Role::Hot, Role::Cold]);
    assert_eq!((got.hot, got.decided_by), (HotServer::Same, Basis::RunFile));
    assert!(host.ran().is_empty());
    config.cold_cluster_roles = Some(vec![]);
    let got = read(&host, &ops, &config);
    assert_eq!(
        (got.hot, got.decided_by),
        (HotServer::Different, Basis::RunFile)
    );
}

/// Catches a run without a hot store being asked about one.
#[test]
fn a_run_without_a_hot_store_asks_nothing() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-server-no-hot");
    let config = run_config(dir.path(), "http://host-mark:9000", None);
    let host = healthy_host();
    let ops = ops_from(&ops_json());
    let got = read(&host, &ops, &config);
    assert_eq!(got, ColdServer::alone());
    assert!(host.ran().is_empty());
}
