//! Which roles' requests are counted by the cold store's own request counter.
//!
//! The cold store guard subtracts the harness's own requests from the cold MinIO's S3 request
//! counter, and that counter counts every server of the cluster, whoever sent the request. The
//! hot store of a farm is very likely the same MinIO (the same service on a NodePort of every
//! node), reached at an address that differs from the cold store's: `127.0.0.1:30900` on one
//! node, another node's address elsewhere. So a hot request is in the counter, and must be in
//! the own count, when the two are one cluster, and must not when they are two.
//!
//! The decision is by the cluster's identity, never by comparing addresses:
//! 1. `cold_cluster_roles` in the run file, when given, names the roles that share the cold
//!    cluster. It overrides everything.
//! 2. Otherwise the deployment id of each of the hot and cold stores, which `mc admin info`
//!    reports (needs the admin key of each in the ops file): equal ids are one cluster,
//!    different ids are two.
//! 3. Otherwise nothing decides, and a pass to the hot store is not judged: its cluster could not
//!    be told apart from the cold store's.
//!
//! The instance's own requests to MinIO are its copies and are never counted as the harness's:
//! the instance role is not in the list, whatever the run file says.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::RunConfig;
use crate::preflight::host::Host;
use crate::preflight::mc::Mc;
use crate::preflight::ops::OpsFile;
use crate::role::Role;

/// How the hot store stands to the cold store's cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotServer {
    /// The same cluster: hot requests are in the cold counter and in the own count.
    Same,
    /// Another cluster: hot requests are in neither.
    Different,
    /// Could not be decided: a pass to the hot store is not judged.
    Undecided,
    /// The run has no hot store, or no cold store.
    Absent,
}

/// What decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Basis {
    /// `cold_cluster_roles` in the run file.
    RunFile,
    /// The deployment ids of the two stores.
    DeploymentId,
    /// Nothing: there was nothing to decide, or nothing decided it.
    None,
}

/// The roles whose requests the cold counter counts, and how that was decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColdServer {
    /// The roles on the cold store's cluster, the cold role among them, in the order of
    /// [`Role`]; never the instance.
    pub roles: Vec<Role>,
    /// The hot store's relation to it.
    pub hot: HotServer,
    /// How `hot` was decided.
    pub decided_by: Basis,
}

impl ColdServer {
    /// A run with only the cold store on the cluster, and nothing decided.
    #[must_use]
    pub fn alone() -> Self {
        Self {
            roles: vec![Role::Cold],
            hot: HotServer::Absent,
            decided_by: Basis::None,
        }
    }
}

/// Decides from what is known: whether the run has both a hot and a cold store, the run file's
/// list of roles sharing the cold cluster, and the deployment ids of the cold and the hot store
/// (`None` where not read).
#[must_use]
pub fn decide(
    _has_hot: bool,
    _explicit: Option<&[Role]>,
    _ids: Option<(&str, &str)>,
) -> ColdServer {
    ColdServer::alone()
}

/// The deployment id in an `mc admin info` answer.
#[must_use]
pub fn deployment_id(info: &Value) -> Option<String> {
    info.pointer("/info/deploymentID")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Decides for the run `config`: reads the deployment id of the hot and the cold store through
/// `host` unless the run file names the roles. A store that cannot be asked leaves it undecided.
#[must_use]
pub fn read(host: &dyn Host, ops: &OpsFile, config: &RunConfig) -> ColdServer {
    let (Some(cold), Some(hot)) = (
        config.stores.get(&Role::Cold),
        config.stores.get(&Role::Hot),
    ) else {
        return decide(false, config.cold_cluster_roles.as_deref(), None);
    };
    let explicit = config.cold_cluster_roles.as_deref();
    if explicit.is_some() {
        return decide(true, explicit, None);
    }
    let id = |role: Role, spec| {
        let mc = Mc::for_role(host, ops, role, spec, &config.result_dir).ok()?;
        deployment_id(&mc.info().ok()?)
    };
    let ids = id(Role::Cold, cold).zip(id(Role::Hot, hot));
    decide(
        true,
        None,
        ids.as_ref().map(|(c, h)| (c.as_str(), h.as_str())),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CH: [Role; 2] = [Role::Hot, Role::Cold];

    /// Catches a deployment id read from the wrong place, or an answer without one read as an id.
    #[test]
    fn the_deployment_id_is_the_one_of_the_admin_info() {
        let info = json!({"info": {"deploymentID": "dep-1", "servers": []}});
        assert_eq!(deployment_id(&info).as_deref(), Some("dep-1"));
        assert_eq!(deployment_id(&json!({"info": {}})), None);
        assert_eq!(deployment_id(&json!({"info": {"deploymentID": 3}})), None);
    }

    /// Catches two addresses of one cluster being told apart: equal deployment ids are the same
    /// server whatever the addresses (the ids are all that is passed), and the hot role is then
    /// among the roles the counter counts.
    #[test]
    fn equal_deployment_ids_are_one_cluster() {
        let got = decide(true, None, Some(("dep-1", "dep-1")));
        assert_eq!(got.roles, CH);
        assert_eq!(
            (got.hot, got.decided_by),
            (HotServer::Same, Basis::DeploymentId)
        );
    }

    /// Catches two clusters taken for one: different ids leave the hot role out of the count.
    #[test]
    fn different_deployment_ids_are_two_clusters() {
        let got = decide(true, None, Some(("dep-1", "dep-2")));
        assert_eq!(got.roles, [Role::Cold]);
        assert_eq!(
            (got.hot, got.decided_by),
            (HotServer::Different, Basis::DeploymentId)
        );
    }

    /// Catches the run file not overriding the ids: it wins in both directions, and the instance
    /// is never counted whatever it lists.
    #[test]
    fn the_run_files_list_overrides_the_ids_and_never_counts_the_instance() {
        let got = decide(true, Some(&[Role::Hot, Role::Instance]), Some(("a", "b")));
        assert_eq!(got.roles, CH);
        assert_eq!((got.hot, got.decided_by), (HotServer::Same, Basis::RunFile));
        let got = decide(true, Some(&[]), Some(("a", "a")));
        assert_eq!(got.roles, [Role::Cold]);
        assert_eq!(
            (got.hot, got.decided_by),
            (HotServer::Different, Basis::RunFile)
        );
        let extra = decide(true, Some(&[Role::TestSilo, Role::Cold]), None);
        assert_eq!(extra.roles, [Role::Cold, Role::TestSilo]);
        assert_eq!(extra.hot, HotServer::Different);
    }

    /// Catches a guess: with neither a list nor both ids, nothing is decided.
    #[test]
    fn with_nothing_to_decide_by_the_hot_store_is_undecided() {
        let got = decide(true, None, None);
        assert_eq!(got.roles, [Role::Cold]);
        assert_eq!(
            (got.hot, got.decided_by),
            (HotServer::Undecided, Basis::None)
        );
    }

    /// Catches a run with no hot store being called undecided.
    #[test]
    fn a_run_without_a_hot_store_has_nothing_to_decide() {
        let got = decide(false, None, Some(("a", "a")));
        assert_eq!(got, ColdServer::alone());
    }

    /// Catches the roles of a result being written in a way the readers do not expect.
    #[test]
    fn a_cold_server_round_trips_as_kebab_names() {
        let got = decide(true, None, Some(("a", "a")));
        let text = serde_json::to_string(&got).unwrap();
        assert_eq!(
            text,
            r#"{"roles":["hot","cold"],"hot":"same","decided_by":"deployment-id"}"#
        );
        assert_eq!(serde_json::from_str::<ColdServer>(&text).unwrap(), got);
    }
}
