//! `priority`: which service a host is offered first when more than one is behind. The
//! default keeps the order the coordinator always had (`kbf`, then the order the host listed
//! its services), a higher number goes first, and no priority overrides `after`.

use std::collections::BTreeMap;

use kbf_oidc::testkit::{STATIC_ISSUER, config_text};
use kbf_types::HostRole;
use kbf_updater::version::Version;
use kbf_updater::wire::{ServiceBeat, ServiceReply};

use crate::identity::Identity;
use crate::services::Services;
use crate::testutil::{Rig, beat, id};

const BODY: &str = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;

/// A service `name` running on daemons, with `extra` lines (priority, after) in its table.
fn service(name: &str, extra: &str) -> String {
    format!(
        "[[service]]\nname = \"{name}\"\n{extra}\n[service.roles.daemon]\nmax_updating = 1\nmin_up = 0\n"
    )
}

/// A coordinator over `text`, CI allowed to set `kbf` and `names`, every one of them desired
/// at 2.0, past the quiet start.
async fn rig(test: &str, text: &str, names: &[&str]) -> Rig {
    let mut rig = Rig::new(test);
    let allowed: Vec<String> = ["kbf"]
        .iter()
        .chain(names)
        .map(|n| format!("{n:?}"))
        .collect();
    let table = format!(
        "{}services = [{}]\n",
        config_text(STATIC_ISSUER),
        allowed.join(", ")
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &table)
        .with_services(Services::parse(text).expect("services"));
    rig.set(BODY).await;
    for name in names {
        rig.set(&BODY.replacen('{', &format!(r#"{{"service":"{name}","#), 1))
            .await;
    }
    rig.at(2_000);
    rig
}

/// The services a daemon is told to update, in order, when it runs 1.0 of everything and
/// lists `announced` in that order: each answer is followed by a heartbeat that has the
/// update done, until it is told nothing.
fn offered(rig: &Rig, announced: &[&str]) -> Vec<String> {
    let who = Identity {
        node_id: id("d1"),
        role: HostRole::DAEMON,
    };
    let mut running: BTreeMap<String, Version> = BTreeMap::new();
    let mut told = Vec::new();
    loop {
        let mut heartbeat = beat("1.0");
        if let Some(v) = running.get("kbf") {
            heartbeat.version = v.clone();
        }
        heartbeat.services = Some(
            announced
                .iter()
                .map(|name| {
                    let b = beat("1.0");
                    ServiceBeat {
                        name: (*name).to_owned(),
                        version: running.get(*name).cloned().unwrap_or(b.version),
                        health: b.health,
                        last_update: None,
                        updating: None,
                        step: None,
                        commit: None,
                        step_secs: Default::default(),
                        promotion_pending: false,
                        deferred: None,
                    }
                })
                .collect(),
        );
        match rig.coordinator.heartbeat(who.clone(), heartbeat).reply {
            ServiceReply::Nothing => return told,
            ServiceReply::Update { service, version } => {
                let name = service.unwrap_or_else(|| "kbf".to_owned());
                running.insert(name.clone(), version);
                told.push(name);
            }
        }
    }
}

/// With no priority set, `kbf` is offered first and the others in the order the host listed
/// them, not alphabetical and not the file's.
#[tokio::test]
async fn with_no_priority_set_kbf_is_first_then_the_announced_order() {
    let text = service("alpha", "") + &service("beta", "");
    let rig = rig("prio-default", &text, &["alpha", "beta"]).await;
    assert_eq!(offered(&rig, &["beta", "alpha"]), ["kbf", "beta", "alpha"]);
}

/// A service with a higher priority is offered before `kbf` and before one listed earlier;
/// the host that has two services behind is told the higher one's update first.
#[tokio::test]
async fn a_higher_priority_service_is_offered_first() {
    let text = service("alpha", "priority = 5") + &service("beta", "");
    let rig = rig("prio-higher", &text, &["alpha", "beta"]).await;
    assert_eq!(offered(&rig, &["beta", "alpha"]), ["alpha", "kbf", "beta"]);
}

/// A negative priority is offered after `kbf` and after services with the default.
#[tokio::test]
async fn a_negative_priority_goes_last() {
    let text = service("alpha", "priority = -1") + &service("beta", "");
    let rig = rig("prio-negative", &text, &["alpha", "beta"]).await;
    assert_eq!(offered(&rig, &["alpha", "beta"]), ["kbf", "beta", "alpha"]);
}

/// Equal priorities fall back to the tie order: `kbf` first among the services that share
/// the number, then the order the host listed them.
#[tokio::test]
async fn equal_priority_falls_back_to_the_announced_order() {
    let text = service("alpha", "priority = 3") + &service("beta", "priority = 3");
    let rig = rig("prio-equal", &text, &["alpha", "beta"]).await;
    assert_eq!(offered(&rig, &["beta", "alpha"]), ["beta", "alpha", "kbf"]);
}

/// A higher priority does not override `after`: `alpha` (priority 5) waits for `beta`, so
/// while `beta` is behind the host is offered the next service, and `alpha` follows once
/// `beta` is done.
#[tokio::test]
async fn priority_never_overrides_after() {
    let text = service("beta", "") + &service("alpha", "priority = 5\nafter = [\"beta\"]");
    let rig = rig("prio-after", &text, &["alpha", "beta"]).await;
    assert_eq!(offered(&rig, &["alpha", "beta"]), ["kbf", "beta", "alpha"]);
}

fn parse(text: &str) -> Result<Services, String> {
    Services::parse(text).map_err(|e| e.to_string())
}

/// `priority` is a whole number, negative allowed, 0 when absent.
#[test]
fn priority_is_parsed_and_defaults_to_zero() {
    let text = service("a", "priority = 7") + &service("b", "priority = -3") + &service("c", "");
    let services = parse(&text).expect("parsed");
    let priority = |name: &str| services.get(name).expect("defined").priority;
    assert_eq!((priority("a"), priority("b"), priority("c")), (7, -3, 0));
    assert_eq!(priority("kbf"), 0);
}

/// A priority that is not a whole number in range is refused, naming the field.
#[test]
fn a_priority_that_is_not_a_number_is_refused() {
    for bad in ["\"high\"", "1.5", "true", "3000000000"] {
        let err = parse(&service("a", &format!("priority = {bad}"))).expect_err(bad);
        assert!(err.contains("priority"), "{bad}: {err}");
        assert!(!err.contains("unknown field"), "{bad}: {err}");
    }
}

/// A definition field nothing implements yet is still refused, with its name.
#[test]
fn an_unknown_definition_field_is_refused() {
    for field in ["bake_beats = 2", "colour = 1", "urgent = true"] {
        let err = parse(&service("a", field)).expect_err(field);
        let name = field.split(' ').next().expect("a name");
        assert!(err.contains(name), "{field}: {err}");
    }
}

/// The shipped `kbf` definition has the default priority, so it cannot be moved by a file.
#[test]
fn kbf_has_the_default_priority() {
    assert_eq!(crate::services::ServiceDef::kbf().priority, 0);
}
