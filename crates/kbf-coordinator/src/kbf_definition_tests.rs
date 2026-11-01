//! kbf's definition: the shipped one is the one kbf always had, and a services file may
//! replace it as its first entry.

use crate::desired::{Desired, DesiredError, KBF};
use crate::services::{RoleDef, ServiceDef, Services};

fn refusal(text: &str) -> String {
    Services::parse(text).expect_err("refused").to_string()
}

fn role(
    max_updating: Option<u32>,
    min_up: Option<u32>,
    never_last: bool,
    after: &[&str],
) -> RoleDef {
    RoleDef {
        max_updating,
        min_up,
        never_last,
        after: after.iter().map(|r| (*r).to_owned()).collect(),
    }
}

/// Pins: the shipped kbf definition is exactly the rules kbf ran with when its definition
/// was built into the code: four roles, no limits of their own, only the frontend never the
/// last one up and waiting for storage, daemon and Mac daemon, no `after`, priority 0, and
/// a desired state that must name `min_up` for storage and frontend. The literal is a copy
/// of those rules, so a change to the shipped data shows here.
#[test]
fn the_shipped_kbf_definition_equals_the_frozen_built_in() {
    let frozen = ServiceDef {
        name: "kbf".to_owned(),
        roles: [
            (
                "frontend",
                role(None, None, true, &["storage", "daemon", "mac-daemon"]),
            ),
            ("storage", role(None, None, false, &[])),
            ("daemon", role(None, None, false, &[])),
            ("mac-daemon", role(None, None, false, &[])),
        ]
        .into_iter()
        .map(|(name, def)| (name.to_owned(), def))
        .collect(),
        after: Vec::new(),
        require_min_up: vec!["storage".to_owned(), "frontend".to_owned()],
        priority: 0,
        selector: None,
        install_when_absent: false,
        defer_backoff: crate::defer::DeferBackoff::default(),
    };
    assert_eq!(ServiceDef::kbf(), frozen);
    assert_eq!(Services::default().get(KBF), Some(&frozen));
    assert_eq!(Services::parse("").expect("empty").get(KBF), Some(&frozen));
}

/// Behaviour: a services file whose first entry is `kbf` replaces the shipped definition:
/// its roles, limits, `never_last` and priority are used, the file's other services still
/// follow, and `after = ["kbf"]` still names it. The desired-state contract of kbf stays:
/// a replaced kbf still requires `min_up` for storage and frontend. Catches a replacement
/// that is refused, ignored, or that loses kbf's required minimums.
#[test]
fn a_services_file_may_replace_kbf_as_its_first_entry() {
    let text = "[[service]]\nname = \"kbf\"\npriority = 3\n\
                [service.roles.storage]\nmax_updating = 3\nmin_up = 2\n\
                [service.roles.frontend]\nnever_last = true\nafter = [\"storage\"]\n\
                [[service]]\nname = \"agent\"\nafter = [\"kbf\"]\n[service.roles.daemon]\n";
    let services = Services::parse(text).expect("parsed");
    let kbf = services.get(KBF).expect("kbf");
    assert_eq!(kbf.priority, 3);
    assert_eq!(
        kbf.role_names(),
        ["frontend", "storage"],
        "the replacement's roles, not the shipped four"
    );
    assert_eq!(kbf.roles["storage"], role(Some(3), Some(2), false, &[]));
    assert_eq!(kbf.roles["frontend"], role(None, None, true, &["storage"]));
    assert_eq!(kbf.require_min_up, ["storage", "frontend"]);
    assert_eq!(services.names().collect::<Vec<_>>(), ["agent", "kbf"]);

    let parse = |body: &[u8]| Desired::parse_request_for(body, &services).map(|_| ());
    assert!(matches!(
        parse(br#"{"version":"2.0"}"#),
        Err(DesiredError::Malformed(_))
    ));
    assert_eq!(
        parse(br#"{"version":"2.0","min_up":{"storage":0}}"#),
        Err(DesiredError::Malformed("min_up lacks frontend".to_owned()))
    );
    assert_eq!(
        parse(br#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#),
        Ok(())
    );
}

/// Behaviour: a replaced kbf needs a role and may not wait for a service, as the shipped
/// one has none. Catches a roleless kbf (it would order nothing, silently) and one that
/// waits for itself or for a service defined later.
#[test]
fn a_replaced_kbf_needs_a_role_and_waits_for_no_service() {
    assert!(refusal("[[service]]\nname = \"kbf\"\n").contains("kbf has no roles"));
    let waits = "[[service]]\nname = \"kbf\"\nafter = [\"kbf\"]\n[service.roles.daemon]\n";
    assert!(refusal(waits).contains("kbf waits for no service"));
    let later = "[[service]]\nname = \"kbf\"\nafter = [\"a\"]\n[service.roles.daemon]\n\
                 [[service]]\nname = \"a\"\n[service.roles.daemon]\n";
    assert!(refusal(later).contains("kbf waits for no service"));
}
