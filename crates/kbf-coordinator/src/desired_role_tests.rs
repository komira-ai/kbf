//! The desired state's limits are keyed by role name: what stays as it was, and what the
//! name keys allow.

use std::fs;

use kbf_oidc::testkit::{STATIC_ISSUER, config_text};
use kbf_testkit::dir::TestDir;
use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};

use crate::desired::{Desired, DesiredError, DesiredStore, KBF};
use crate::service::SetError;
use crate::services::Services;
use crate::testutil::{Rig, ci_token};

const FULL: &str = r#"{"version":"0.1.9+gabcdefabcdef-2",
    "max_updating":{"frontend":1,"storage":2,"daemon":3,"mac-daemon":4},
    "min_up":{"storage":3,"frontend":2}}"#;
const SMALL: &str = r#"{"version":"1.0","min_up":{"storage":1,"frontend":1}}"#;
const PARTIAL: &str =
    r#"{"version":"1.0","max_updating":{"daemon":7},"min_up":{"storage":1,"frontend":1}}"#;

/// The state as the build before role-name keys declared it: four named limits, two named
/// minimums, every unknown key refused. A file this reads is a file that build reads.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenMaxUpdating {
    #[serde(default = "one")]
    frontend: u32,
    #[serde(default = "one")]
    storage: u32,
    #[serde(default = "two")]
    daemon: u32,
    #[serde(default = "two", rename = "mac-daemon")]
    mac_daemon: u32,
}

const fn one() -> u32 {
    1
}

const fn two() -> u32 {
    2
}

impl Default for FrozenMaxUpdating {
    fn default() -> Self {
        Self {
            frontend: 1,
            storage: 1,
            daemon: 2,
            mac_daemon: 2,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenMinUp {
    storage: u32,
    frontend: u32,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    version: Version,
    #[serde(default)]
    max_updating: FrozenMaxUpdating,
    min_up: FrozenMinUp,
}

fn frozen_text(body: &str) -> String {
    let old: Frozen = serde_json::from_str(body).expect("an old body");
    serde_json::to_string(&old).expect("json")
}

fn compact(d: &Desired) -> String {
    serde_json::to_string(d).expect("json")
}

/// Catches: a body of an earlier build read to other limits (a default of 1, 1, 2, 2 lost,
/// `mac-daemon` spelled another way, the two minimums swapped), or echoed in another key
/// order: each body reads to the numbers and the text the old structs give it.
#[test]
fn every_earlier_desired_body_reads_to_the_same_limits() {
    for body in [FULL, SMALL, PARTIAL] {
        let d = Desired::parse(body.as_bytes()).expect("parsed");
        assert_eq!(compact(&d), frozen_text(body), "{body}");
    }
    let d = Desired::parse(PARTIAL.as_bytes()).expect("parsed");
    assert_eq!(
        compact(&d),
        r#"{"version":"1.0","max_updating":{"frontend":1,"storage":1,"daemon":7,"mac-daemon":2},"min_up":{"storage":1,"frontend":1}}"#
    );
}

/// Catches: a file written for a kbf-only farm that is not byte for byte what the old
/// writer produced (a downgrade would then refuse to start), or one the old reader refuses:
/// the file equals the old structs' pretty text and the old structs read it back.
#[test]
fn a_kbf_only_write_is_byte_identical_and_read_by_the_frozen_reader() {
    for body in [FULL, SMALL, PARTIAL] {
        let dir = TestDir::under(std::env::temp_dir(), "desired-role-bytes");
        let file = dir.path().join("desired.json");
        let store = DesiredStore::open(&file).expect("opened");
        store
            .set(KBF, Desired::parse(body.as_bytes()).expect("parsed"))
            .expect("stored");
        let old: Frozen = serde_json::from_str(body).expect("an old body");
        let want = format!("{}\n", serde_json::to_string_pretty(&old).expect("json"));
        let text = fs::read_to_string(&file).expect("read");
        assert_eq!(text, want, "{body}");
        assert_eq!(
            serde_json::from_str::<Frozen>(&text).expect("read back"),
            old
        );
    }
}

/// Catches: a limits key for a role the service does not have refused (deploy-farm sends
/// the same body for every service), or kept in the stored state where an earlier build
/// would refuse the file: the key is dropped and the state is still the old format.
#[test]
fn an_unknown_role_key_from_deploy_farm_is_dropped() {
    let body = br#"{"version":"1.0","max_updating":{"builder":3,"daemon":1},
        "min_up":{"storage":1,"frontend":1,"builder":2}}"#;
    let (service, d) = Desired::parse_request(body).expect("accepted");
    assert_eq!(service, KBF);
    let text = compact(&d);
    assert!(!text.contains("builder"), "{text}");
    assert_eq!(
        text,
        r#"{"version":"1.0","max_updating":{"frontend":1,"storage":1,"daemon":1,"mac-daemon":2},"min_up":{"storage":1,"frontend":1}}"#
    );
    assert!(serde_json::from_str::<Frozen>(&text).is_ok());
}

/// Catches: kbf run without the minimums it needs (`min_up.storage = 0` by omission):
/// a body without `min_up`, with an empty one, or with either key missing is refused.
#[test]
fn kbf_refuses_a_body_without_min_up_storage_or_frontend() {
    for bad in [
        r#"{"version":"1.0"}"#,
        r#"{"version":"1.0","min_up":{}}"#,
        r#"{"version":"1.0","min_up":{"storage":1}}"#,
        r#"{"version":"1.0","min_up":{"frontend":1}}"#,
        r#"{"service":"kbf","version":"1.0","min_up":{"frontend":1}}"#,
    ] {
        assert!(
            matches!(
                Desired::parse_request(bad.as_bytes()),
                Err(DesiredError::Malformed(_))
            ),
            "{bad}"
        );
    }
}

/// Catches: a role name that is no role name accepted as a key: upper case, a space, a
/// leading hyphen and 33 bytes are refused in either limits map.
#[test]
fn a_malformed_role_name_in_a_body_is_refused() {
    let long = "a".repeat(33);
    for name in ["Cache", "a b", "-x", "", long.as_str()] {
        let in_max = format!(
            r#"{{"version":"1.0","max_updating":{{"{name}":1}},"min_up":{{"storage":1,"frontend":1}}}}"#
        );
        let in_min =
            format!(r#"{{"version":"1.0","min_up":{{"storage":1,"frontend":1,"{name}":1}}}}"#);
        for body in [in_max, in_min] {
            assert!(
                matches!(
                    Desired::parse_request(body.as_bytes()),
                    Err(DesiredError::Malformed(_))
                ),
                "{body}"
            );
        }
    }
}

const OTHER_SERVICE: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\n";

fn rig_with_other(name: &str) -> Rig {
    let mut rig = Rig::new(name);
    let table = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        config_text(STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &table)
        .with_services(Services::parse(OTHER_SERVICE).expect("services"));
    rig
}

async fn post(rig: &Rig, body: &str) -> Result<Desired, SetError> {
    rig.coordinator
        .set_desired(Some(&ci_token()), body.as_bytes())
        .await
}

/// Catches: deploy-farm's body (`min_up` for storage and frontend, and a key for a role
/// the service lacks) refused for a service that has only a daemon role: it is accepted, the
/// unknown role's key is gone, and the stored limits are the ones of the body.
#[tokio::test]
async fn deploy_farms_body_is_accepted_for_a_service_without_a_storage_role() {
    let rig = rig_with_other("desired-role-deploy-farm");
    let body = r#"{"service":"other","version":"3.0",
        "max_updating":{"daemon":1,"builder":4},"min_up":{"storage":1,"frontend":1}}"#;
    let set = post(&rig, body).await.expect("accepted");
    let text = compact(&set);
    assert!(!text.contains("builder"), "{text}");
    assert!(
        text.contains(r#""min_up":{"storage":1,"frontend":1}"#),
        "{text}"
    );
    let status = rig.coordinator.status(Some(crate::testutil::OPERATOR));
    assert_eq!(compact(&status.expect("status").services["other"]), text);
}

/// Catches: a service that requires no minimums refused for lacking `min_up`: a body without
/// it is accepted for `other` and refused for kbf.
#[tokio::test]
async fn a_service_with_no_requirements_accepts_no_min_up() {
    let rig = rig_with_other("desired-role-no-min-up");
    let set = post(&rig, r#"{"service":"other","version":"3.0"}"#).await;
    assert!(set.is_ok(), "{set:?}");
    let kbf = post(&rig, r#"{"version":"3.0"}"#).await;
    assert!(matches!(kbf, Err(SetError::Invalid(_))), "{kbf:?}");
}
