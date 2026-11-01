//! A replaced kbf end to end: what convergence and the offers do with the replacement, the
//! rule for the file that carries it, and the spelling of the hold reasons in the status.

use std::os::unix::fs::PermissionsExt;

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;
use kbf_updater::wire::{Health, Heartbeat, ServiceBeat, ServiceReply};
use serde_json::json;

use crate::converge::State;
use crate::decide::Hold;
use crate::identity::Identity;
use crate::services::{Services, ServicesError};
use crate::testutil::{OPERATOR, Rig, beat, ci_token, id, version};

fn who(name: &str, role: HostRole) -> Identity {
    Identity {
        node_id: id(name),
        role,
    }
}

/// `top` that also runs `service` at `running`.
fn announcing_other(mut top: Heartbeat, service: &str, running: &str) -> Heartbeat {
    top.services = Some(vec![ServiceBeat {
        name: service.to_owned(),
        version: version(running),
        health: Health::Healthy,
        last_update: None,
        updating: None,
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    }]);
    top
}

fn refusal(text: &str) -> String {
    Services::parse(text).expect_err("refused").to_string()
}

/// A rig whose kbf is replaced by `text`.
fn replaced(name: &str, text: &str) -> Rig {
    let mut rig = Rig::new(name);
    rig.coordinator =
        Rig::start(&rig.dir, &rig.clock).with_services(Services::parse(text).expect("services"));
    rig
}

/// Catches: a replaced kbf ignored by convergence. kbf is replaced by a definition with
/// storage and daemon only; the desired state asks for a frontend to stay up (as deploy-farm's
/// body always does) and a stray frontend host, which no order can reach, runs the old
/// version. The farm of the replacement is at the desired version, so the convergence read
/// and the status both say `converged`, over the roles of the replacement and no other.
#[tokio::test]
async fn a_replaced_kbf_converges_over_its_own_roles() {
    let rig = replaced(
        "kbf-replaced-converge",
        "[[service]]\nname = \"kbf\"\n[service.roles.storage]\n[service.roles.daemon]\n",
    );
    rig.set(r#"{"version":"2.0","min_up":{"storage":1,"frontend":1}}"#)
        .await;
    rig.at(2_000);
    let c = &rig.coordinator;
    c.heartbeat(who("s1", HostRole::STORAGE), beat("2.0"));
    c.heartbeat(who("d1", HostRole::DAEMON), beat("2.0"));
    c.heartbeat(who("f1", HostRole::FRONTEND), beat("1.0"));
    let read = c
        .convergence(Some(&ci_token()), "kbf")
        .await
        .expect("convergence");
    assert_eq!(read.state, State::Converged);
    let roles: Vec<&str> = read.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(roles, ["storage", "daemon"]);
    let status = serde_json::to_value(c.status(Some(OPERATOR)).expect("status")).expect("JSON");
    let entry = &status["per_service"]["kbf"];
    assert_eq!(entry["roles"], entry["convergence"]["roles"]);
    assert_eq!(entry["convergence"]["state"], "converged");
}

/// Behaviour: the priority of a replaced kbf orders the offers. With kbf at -1 and `other` at
/// 0, a host behind on both is offered `other`, named; with kbf's priority left at 0 it is
/// offered kbf, unnamed. Catches a priority the replacement cannot set, or one that is read
/// and not used for kbf.
#[tokio::test]
async fn the_priority_of_a_replaced_kbf_orders_the_offers() {
    let other = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";
    for (priority, want_service) in [(-1, Some("other")), (0, None)] {
        let text = format!(
            "[[service]]\nname = \"kbf\"\npriority = {priority}\n[service.roles.daemon]\n{other}"
        );
        let mut rig = replaced("kbf-replaced-priority", &text);
        let list = kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
            + "services = [\"kbf\", \"other\"]\n";
        rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
            .with_services(Services::parse(&text).expect("services"));
        for body in [
            r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#,
            r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#,
        ] {
            rig.coordinator
                .set_desired(Some(&ci_token()), body.as_bytes())
                .await
                .expect("set");
        }
        rig.at(2_000);
        let reply = rig.coordinator.heartbeat(
            who("d1", HostRole::DAEMON),
            announcing_other(beat("1.0"), "other", "1.0"),
        );
        assert_eq!(
            reply,
            ServiceReply::Update {
                service: want_service.map(str::to_owned),
                version: version(if want_service.is_some() { "3.0" } else { "2.0" }),
            },
            "priority {priority}"
        );
    }
}

/// Behaviour: a replaced kbf names roles by the same rule as any service: a name that is no
/// host role is refused, and says which.
#[test]
fn a_replaced_kbf_with_an_unknown_role_is_refused() {
    assert!(
        refusal("[[service]]\nname = \"kbf\"\n[service.roles.cook]\n")
            .contains("cook is not a host role")
    );
}

/// Behaviour: the two ways to define kbf wrongly are refused each with its own text: after
/// another service (it may only be first), and a second time (the first entry already
/// replaced it). Pins the first-entry-only rule to a message.
#[test]
fn kbf_after_another_service_and_kbf_twice_are_told_apart() {
    let daemon = "[service.roles.daemon]\n";
    let after_other =
        format!("[[service]]\nname = \"a\"\n{daemon}[[service]]\nname = \"kbf\"\n{daemon}");
    assert_eq!(
        refusal(&after_other),
        "services: kbf must be the first entry of the file (it is defined after another service)"
    );
    let twice =
        format!("[[service]]\nname = \"kbf\"\n{daemon}[[service]]\nname = \"kbf\"\n{daemon}");
    assert_eq!(refusal(&twice), "services: kbf is defined twice");
}

/// Behaviour: a services file is read under the rule of the operator's token file: a regular
/// file owned by the coordinator's user, mode 0600 or 0400. A file others can write could
/// switch off every limit of kbf's rollout, so mode 0644 and 0660 are refused with the
/// mode named, and 0600 and 0400 are read.
#[test]
fn a_services_file_must_be_owner_only() {
    let dir = TestDir::under(std::env::temp_dir(), "services-file-mode");
    let path = dir.path().join("services.toml");
    std::fs::write(&path, "[[service]]\nname = \"a\"\n[service.roles.daemon]\n").expect("written");
    for (mode, ok) in [(0o644, false), (0o660, false), (0o600, true), (0o400, true)] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        let read = Services::from_file(&path);
        match (ok, read) {
            (true, Ok(services)) => assert!(services.get("a").is_some(), "{mode:o}"),
            (false, Err(ServicesError::Read(why))) => {
                assert!(why.contains("mode must be 0600 or 0400"), "{mode:o}: {why}");
            }
            (_, other) => panic!("mode {mode:o}: {other:?}"),
        }
    }
    let link = dir.path().join("link.toml");
    std::os::unix::fs::symlink(&path, &link).expect("link");
    assert!(matches!(
        Services::from_file(&link),
        Err(ServicesError::Read(_))
    ));
}

/// Behaviour: an owner-only services file that is not text, or is over a mebibyte, is refused
/// with the reason, not read as an empty file.
#[test]
fn a_services_file_must_be_text_and_small() {
    let dir = TestDir::under(std::env::temp_dir(), "services-file-shape");
    let read = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        Services::from_file(&path).expect_err("refused").to_string()
    };
    assert!(read("binary.toml", &[0xff, 0xfe]).contains("not UTF-8"));
    let big = vec![b'#'; (1 << 20) + 1];
    assert!(read("big.toml", &big).contains("over 1048576 bytes"));
}

/// Pins: the strings the status gives for each hold are public: scripts match on them.
/// The match has no wildcard, so a new variant fails to compile here until it is listed.
#[test]
fn the_hold_strings_are_the_ones_the_status_documents() {
    let spelled = |hold: Hold| -> &'static str {
        match hold {
            Hold::NoDefinition => "no_definition",
            Hold::NoDesired => "no_desired",
            Hold::NotAMember => "not_a_member",
            Hold::Quiet => "quiet",
            Hold::Updating => "updating",
            Hold::BusyElsewhere => "busy_elsewhere",
            Hold::UpToDate => "up_to_date",
            Hold::FailedHere => "failed_here",
            Hold::Deferred => "deferred",
            Hold::NoSlot => "no_slot",
            Hold::WaitingForRole => "waiting_for_role",
            Hold::WaitingForService => "waiting_for_service",
            Hold::MinUp => "min_up",
            Hold::Baking => "baking",
            Hold::CanaryWait => "canary_wait",
        }
    };
    for hold in [
        Hold::NoDefinition,
        Hold::NoDesired,
        Hold::NotAMember,
        Hold::Quiet,
        Hold::Updating,
        Hold::BusyElsewhere,
        Hold::UpToDate,
        Hold::FailedHere,
        Hold::Deferred,
        Hold::NoSlot,
        Hold::WaitingForRole,
        Hold::WaitingForService,
        Hold::MinUp,
        Hold::Baking,
        Hold::CanaryWait,
    ] {
        assert_eq!(json!(hold), json!(spelled(hold)));
    }
}
