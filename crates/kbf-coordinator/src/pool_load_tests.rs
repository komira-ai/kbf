//! Loading named slot pools: which names a services file may give, how each refusal reads,
//! the note for a pool that shares with nothing, and the files of the release before pools.

use crate::services::{Services, ServicesError, TIERSTORE_SERVICES};

/// A file of one service `a` whose daemon role has the TOML value `pool` as its pool.
fn file(pool: &str) -> String {
    format!("[[service]]\nname = \"a\"\n[service.roles.daemon]\npool = {pool}\n")
}

fn pool_of(services: &Services, service: &str, role: &str) -> Option<String> {
    services.get(service).expect("defined").roles[role]
        .pool
        .clone()
}

/// Catches: an off-by-one at the length bound. A name of 64 bytes is kept, one of 65 is
/// refused as invalid, naming the service and the bound.
#[test]
fn a_pool_name_may_be_64_bytes_and_no_more() {
    let longest = "p".repeat(64);
    let services = Services::parse(&file(&format!("{longest:?}"))).expect("64 bytes");
    assert_eq!(pool_of(&services, "a", "daemon"), Some(longest));
    let over = "p".repeat(65);
    match Services::parse(&file(&format!("{over:?}"))) {
        Err(ServicesError::Invalid(why)) => {
            assert!(why.starts_with("a: pool "), "{why}");
            assert!(why.contains("1 to 64 bytes"), "{why}");
        }
        other => panic!("65 bytes: {other:?}"),
    }
}

/// Catches: a name test that counts characters, or accepts letters beyond ASCII. Lowercase
/// letters outside ASCII, a dot and an underscore are refused as invalid, naming the service.
#[test]
fn a_pool_name_beyond_ascii_letters_digits_and_dash_is_refused() {
    for bad in ["stóra", "pool-é", "a.b", "a_b"] {
        match Services::parse(&file(&format!("{bad:?}"))) {
            Err(ServicesError::Invalid(why)) => {
                assert!(why.starts_with("a: pool "), "{bad}: {why}")
            }
            other => panic!("{bad}: {other:?}"),
        }
    }
}

/// Catches: a pool that is not a string reaching the decision. An integer, a boolean, an
/// array or a table is refused by the parse (not as an invalid name), and the reason says
/// a string was expected.
#[test]
fn a_pool_that_is_not_a_string_is_refused_by_the_parse() {
    for value in ["3", "true", "[\"p\"]", "{ name = \"p\" }"] {
        match Services::parse(&file(value)) {
            Err(ServicesError::Parse(why)) => assert!(why.contains("string"), "{value}: {why}"),
            other => panic!("{value}: {other:?}"),
        }
    }
}

/// Catches: a misspelt pool going unnoticed. Pools are named, not declared, so the load keeps
/// a pool only one service names (it decides as no pool) and notes it, naming the service,
/// the pool and the role. A pool that the shipped kbf's storage role also names, or that two
/// entries name on one role, is not noted; one named on different roles is, once per role.
#[test]
fn a_pool_no_other_service_names_is_kept_and_noted() {
    let entry = |name: &str, role: &str, pool: &str| {
        format!("[[service]]\nname = \"{name}\"\n[service.roles.{role}]\npool = \"{pool}\"\n")
    };
    let typo = Services::parse(&entry("t", "storage", "storag")).expect("kept");
    assert_eq!(pool_of(&typo, "t", "storage").as_deref(), Some("storag"));
    assert_eq!(
        typo.notes(),
        [
            "t: pool \"storag\" on storage is named by no other service's storage role, so it shares no slots"
        ]
    );

    let with_kbf = Services::parse(&entry("t", "storage", "storage")).expect("parsed");
    assert!(with_kbf.notes().is_empty(), "{:?}", with_kbf.notes());

    let two = format!("{}{}", entry("a", "daemon", "p"), entry("b", "daemon", "p"));
    assert!(Services::parse(&two).expect("parsed").notes().is_empty());

    let across = format!(
        "{}{}",
        entry("a", "daemon", "p"),
        entry("b", "storage", "p")
    );
    let notes = Services::parse(&across).expect("parsed").notes().to_vec();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes[0].starts_with("a: pool \"p\" on daemon"), "{notes:?}");
    assert!(
        notes[1].starts_with("b: pool \"p\" on storage"),
        "{notes:?}"
    );

    assert!(
        Services::default().notes().is_empty(),
        "the shipped kbf alone"
    );
}

/// tierstore's entry as the release before pools shipped it, word for word.
const TIERSTORE_BEFORE_POOLS: &str = r#"[[service]]
name = "tierstore"
bake_beats = 3
canary = { count = 1 }
rollback = "automatic"
rollback_after_failed = { percent = 25, min_hosts = 2 }
install_when_absent = false

[service.selector]
roles = ["storage"]

[service.roles.storage]
max_updating = 1
min_up = 4
"#;

/// Catches: this release refusing a file written for the one before. That release's
/// tierstore entry, after a replaced kbf entry with no pool, loads with no pool on any role,
/// no note, and the same tierstore definition as the shipped entry of today.
#[test]
fn a_file_of_the_release_before_pools_loads_with_no_pool() {
    let old = format!(
        "[[service]]\nname = \"kbf\"\n[service.roles.storage]\n[service.roles.daemon]\n\n{TIERSTORE_BEFORE_POOLS}"
    );
    let services = Services::parse(&old).expect("loads");
    for def in services.defs() {
        assert!(
            def.roles.values().all(|r| r.pool.is_none()),
            "{}: {:?}",
            def.name,
            def.roles
        );
    }
    assert!(services.notes().is_empty());
    let shipped = Services::parse(TIERSTORE_SERVICES).expect("shipped");
    assert_eq!(services.get("tierstore"), shipped.get("tierstore"));
}

/// The services-file reader of the release before pools (v0.0.57), frozen: the same tables,
/// each refusing a field it does not know, as that release's `RawFile`, `RawService` and
/// `RawRole` do. The values of the tables pools did not touch are kept as TOML values.
mod before_pools {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "read only to be refused or accepted, as that release did"
    )]
    pub struct File {
        #[serde(default)]
        service: Vec<Service>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "read only to be refused or accepted, as that release did"
    )]
    struct Service {
        name: String,
        after: Option<Vec<String>>,
        roles: Option<BTreeMap<String, Role>>,
        priority: Option<i32>,
        selector: Option<toml::Value>,
        install_when_absent: Option<bool>,
        bake_beats: Option<u32>,
        canary: Option<toml::Value>,
        rollback: Option<String>,
        rollback_after_failed: Option<toml::Value>,
        min_version: Option<String>,
        defer_backoff: Option<toml::Value>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "read only to be refused or accepted, as that release did"
    )]
    struct Role {
        max_updating: Option<u32>,
        min_up: Option<u32>,
        never_last: Option<bool>,
        after: Option<Vec<String>>,
    }

    /// Whether that release starts on the file text `text`, or why it refuses it.
    pub fn read(text: &str) -> Result<(), String> {
        toml::from_str::<File>(text)
            .map(drop)
            .map_err(|e| e.to_string())
    }
}

/// Catches: the shipped tierstore entry gaining a field the release before pools refuses,
/// which would stop that coordinator's start on a file holding the entry (one rolled back to,
/// or not yet updated). That release's reader takes the shipped entry, alone and after a kbf
/// entry of its own, and takes this release's own copy of that release's file.
#[test]
fn the_release_before_pools_reads_the_shipped_tierstore_entry() {
    before_pools::read(TIERSTORE_SERVICES).expect("the shipped entry");
    let kbf_before = "[[service]]\nname = \"kbf\"\n[service.roles.storage]\n\n";
    before_pools::read(&format!("{kbf_before}{TIERSTORE_SERVICES}")).expect("after kbf");
    before_pools::read(TIERSTORE_BEFORE_POOLS).expect("its own entry");
}

/// Catches: the second step of the deploy document's two (a pool in the file) being safe
/// before the first has reached every coordinator. A file that names a pool, the shipped
/// tierstore entry with the documented line added, is refused by the release before pools
/// for the unknown field `pool`, so the pool waits until that release is no longer run or
/// rolled back to; this release, the one the second step waits for, reads it with the pool.
#[test]
fn the_release_before_pools_refuses_a_file_that_names_a_pool() {
    let pooled = TIERSTORE_SERVICES.replace("min_up = 4\n", "min_up = 4\npool = \"storage\"\n");
    assert_ne!(pooled, TIERSTORE_SERVICES);
    let why = before_pools::read(&pooled).expect_err("refused");
    assert!(why.contains("unknown field `pool`"), "{why}");
    let services = Services::parse(&pooled).expect("read here");
    assert_eq!(
        pool_of(&services, "tierstore", "storage").as_deref(),
        Some("storage")
    );
}
