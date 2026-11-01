//! tierstore as a service of the coordinator: its shipped definition, and what a coordinator
//! that runs it does. Every time is explicit; the hosts are six of the storage role and one
//! daemon, each beating every fifteen seconds.

use std::collections::BTreeSet;

use kbf_types::HostRole;
use kbf_updater::wire::{
    Class, Heartbeat, Outcome, Phase, ServiceBeat, ServiceReply, UpdateResult,
};
use serde_json::{Value, json};

use crate::canary::Canary;
use crate::identity::Identity;
use crate::membership::{RawSelector, Selector};
use crate::ops::ServiceOps;
use crate::reconcile::{FailThreshold, Rollback};
use crate::services::{RoleDef, Services, TIERSTORE_SERVICES};
use crate::testutil::{OPERATOR, Rig, beat, id, version};

const STORAGE: [&str; 6] = ["s1", "s2", "s3", "s4", "s5", "s6"];
const ONE: &str = r#"{"service":"tierstore","version":"1.0"}"#;
const TWO: &str = r#"{"service":"tierstore","version":"2.0"}"#;

fn def() -> crate::services::ServiceDef {
    Services::parse(TIERSTORE_SERVICES)
        .expect("the shipped definition parses")
        .get("tierstore")
        .expect("it defines tierstore")
        .clone()
}

/// The shipped file is a definition of tierstore that the services file parser accepts and
/// that says each of the values the rollout rests on: the storage role, one host at a time,
/// five others up, a canary of one, a bake of three beats, automatic rollback at the default
/// threshold, and a host that never registered is not ordered. A change to the shipped data
/// shows here.
#[test]
fn the_shipped_definition_says_what_the_rollout_rests_on() {
    let def = def();
    assert_eq!(def.name, "tierstore");
    let roles: Vec<(&String, &RoleDef)> = def.roles.iter().collect();
    assert_eq!(
        roles,
        vec![(
            &"storage".to_owned(),
            &RoleDef {
                max_updating: Some(1),
                min_up: Some(5),
                never_last: false,
                after: Vec::new(),
            }
        )]
    );
    let raw: RawSelector = toml::from_str("roles = [\"storage\"]").expect("a selector");
    let selector = Selector::new(raw, &BTreeSet::from(["storage".to_owned()])).expect("valid");
    assert_eq!(def.selector, Some(selector));
    assert_eq!(def.bake_beats, 3);
    assert_eq!(def.canary, Some(Canary { count: 1 }));
    assert_eq!(def.rollback, Rollback::Automatic);
    assert_eq!(
        def.rollback_after_failed,
        FailThreshold {
            percent: 25,
            min_hosts: 2
        }
    );
    assert!(!def.install_when_absent);
    assert!(def.after.is_empty(), "tierstore waits for no service");
    assert!(def.require_min_up.is_empty(), "its body names no min_up");
}

/// The definition is a services-file entry that may follow kbf's, whether kbf is shipped
/// or replaced by the first entry of the same file, and the two together define both.
#[test]
fn the_definition_follows_kbf_in_one_services_file() {
    let shipped = Services::parse(TIERSTORE_SERVICES).expect("alone");
    assert_eq!(shipped.names().collect::<Vec<_>>(), ["kbf", "tierstore"]);
    let replaced = format!(
        "{}\n{TIERSTORE_SERVICES}",
        include_str!("kbf.services.toml")
    );
    let both = Services::parse(&replaced).expect("after kbf's own entry");
    assert_eq!(both.names().collect::<Vec<_>>(), ["kbf", "tierstore"]);
    assert_eq!(both.get("tierstore"), shipped.get("tierstore"));
}

/// The documentation shows the shipped file, whole, so an operator copies what is tested.
#[test]
fn the_deploy_document_shows_the_shipped_file() {
    let doc = include_str!("../../../docs/deploy/kbf-updater.md");
    assert!(!TIERSTORE_SERVICES.is_empty(), "there is a shipped file");
    assert!(
        doc.contains(TIERSTORE_SERVICES.trim_end()),
        "docs/deploy/kbf-updater.md does not hold the text of tierstore.services.toml"
    );
}

// ---- a coordinator that runs it ----------------------------------------------------------

/// A rig whose coordinator runs the shipped definition and whose CI token may set kbf and
/// tierstore.
fn rig(test: &str) -> Rig {
    let mut rig = Rig::new(test);
    let list = format!(
        "{}services = [\"kbf\", \"tierstore\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(TIERSTORE_SERVICES).expect("the definition"));
    rig
}

fn who(host: &str, role: HostRole) -> Identity {
    Identity {
        node_id: id(host),
        role,
    }
}

/// The heartbeat of a v2 agent whose only service is tierstore at `running`, updating to
/// `updating`, with `last` as its last update.
fn beat_of(running: &str, updating: Option<&str>, last: Option<UpdateResult>) -> Heartbeat {
    let mut b = beat("1.0");
    b.agent_features = vec!["heartbeat-v2".to_owned()];
    let mut service: ServiceBeat = serde_json::from_value(json!({
        "name": "tierstore", "version": running, "health": "healthy", "updating": updating
    }))
    .expect("a service beat");
    service.last_update = last;
    b.services = Some(vec![service]);
    b
}

fn failed(target: &str) -> UpdateResult {
    UpdateResult {
        target: version(target),
        outcome: Outcome::Failed,
        phase: Some(Phase::AfterCommit),
        class: Some(Class::Release),
        reason: None,
    }
}

/// One round at `at`: every storage host then the daemon beats, in node order. `shown` says
/// what a storage host reports as (running, updating, last); the others run `1.0`. The
/// answer is the hosts that were ordered, and what the daemon was told.
fn round(
    rig: &Rig,
    at: u64,
    shown: &[(&str, &str, Option<&str>, Option<UpdateResult>)],
) -> (Vec<String>, ServiceReply) {
    rig.at(at);
    let mut ordered = Vec::new();
    for host in STORAGE {
        let (running, updating, last) = shown
            .iter()
            .find(|(h, ..)| *h == host)
            .map_or(("1.0", None, None), |(_, r, u, l)| (*r, *u, l.clone()));
        let reply = rig
            .coordinator
            .heartbeat(
                who(host, HostRole::STORAGE),
                beat_of(running, updating, last),
            )
            .reply;
        if let ServiceReply::Update { service, version } = reply {
            assert_eq!(service.as_deref(), Some("tierstore"));
            assert_eq!(version, crate::testutil::version("2.0"));
            ordered.push(host.to_owned());
        }
    }
    let daemon = rig
        .coordinator
        .heartbeat(who("d1", HostRole::DAEMON), beat_of("1.0", None, None))
        .reply;
    (ordered, daemon)
}

fn at_two(host: &str) -> (&str, &str, Option<&str>, Option<UpdateResult>) {
    (host, "2.0", None, None)
}

fn going(host: &str) -> (&str, &str, Option<&str>, Option<UpdateResult>) {
    (host, "1.0", Some("2.0"), None)
}

/// A coordinator that runs the shipped definition orders tierstore to the storage hosts and
/// to them only; one host first (the canary), and no other until the canary has beaten
/// healthy three times at the new version; then one host at a time, the next only after
/// the last has baked. A daemon that announces tierstore is never ordered it, and the
/// convergence counts the storage role alone.
#[tokio::test]
async fn a_canary_goes_first_then_one_storage_host_at_a_time_each_baking() {
    let rig = rig("ts-roll");
    rig.set(TWO).await;
    // Every host is heard once before any can be ordered: five others must be up.
    let _ = round(&rig, 2_000, &[]);

    let (ordered, daemon) = round(&rig, 2_015, &[]);
    assert_eq!(ordered, ["s1"], "the canary alone");
    assert_eq!(daemon, ServiceReply::Nothing);

    let (ordered, _) = round(&rig, 2_030, &[going("s1")]);
    assert_eq!(ordered, Vec::<String>::new(), "the canary is updating");

    for (beats, at) in [(1, 2_045), (2, 2_060)] {
        let (ordered, _) = round(&rig, at, &[at_two("s1")]);
        assert_eq!(
            ordered,
            Vec::<String>::new(),
            "the canary has baked {beats}"
        );
    }
    let (ordered, daemon) = round(&rig, 2_075, &[at_two("s1")]);
    assert_eq!(
        ordered,
        ["s2"],
        "the canary baked three beats; one host more"
    );
    assert_eq!(daemon, ServiceReply::Nothing);

    let (ordered, _) = round(&rig, 2_090, &[at_two("s1"), going("s2")]);
    assert_eq!(ordered, Vec::<String>::new(), "one at a time");
    for at in [2_105, 2_120] {
        let (ordered, _) = round(&rig, at, &[at_two("s1"), at_two("s2")]);
        assert_eq!(ordered, Vec::<String>::new(), "s2 is baking");
    }
    let (ordered, _) = round(&rig, 2_135, &[at_two("s1"), at_two("s2")]);
    assert_eq!(ordered, ["s3"], "s2 has baked");

    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    let convergence =
        serde_json::to_value(&status.per_service["tierstore"].convergence).expect("json");
    let roles: Vec<&Value> = convergence["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .collect();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0]["role"], "storage");
}

/// Two of six storage hosts failing a release after the commit roll tierstore back by
/// itself, to the version recorded as last good; one failure alone does not. The failed
/// version is marked bad, and desired and the marks are on disk.
#[tokio::test]
async fn two_failed_hosts_of_six_roll_tierstore_back_and_one_does_not() {
    let rig = rig("ts-rollback");
    rig.set(ONE).await;
    // Converged needs every host to have baked: three healthy beats at the version.
    for at in [2_000, 2_015, 2_030, 2_045] {
        let _ = round(&rig, at, &[]);
    }
    let marks = |rig: &Rig| -> ServiceOps {
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .ops
            .services
            .get("tierstore")
            .cloned()
            .unwrap_or_default()
    };
    let wanted = |rig: &Rig| {
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["tierstore"]
            .version
            .clone()
    };
    assert_eq!(marks(&rig).last_good, Some(version("1.0")));

    rig.set(TWO).await;
    let bad = |host: &'static str| (host, "1.0", None, Some(failed("2.0")));
    let _ = round(&rig, 2_060, &[bad("s1")]);
    assert_eq!(wanted(&rig), version("2.0"), "one failed host of six");
    assert!(marks(&rig).bad.is_empty());

    let _ = round(&rig, 2_075, &[bad("s1"), bad("s2")]);
    assert_eq!(wanted(&rig), version("1.0"), "two failed hosts of six");
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
    let on_disk =
        crate::ops::OpsStore::open(&rig.dir.path().join("desired.json.ops")).expect("the ops file");
    assert_eq!(
        on_disk.marks("tierstore").bad,
        vec![version("2.0")],
        "the mark is on disk"
    );
}
