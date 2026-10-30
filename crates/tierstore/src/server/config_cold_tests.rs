//! The pass-through shape of the configuration file: `[server]` and `[cold_store]` only.

use std::time::Duration;

use super::config::{PerRun, from_text, settings_from_text};
use super::config_cache_tests::CACHE;
use super::config_cold::ColdSettings;
use super::config_testing::{DEFAULTS, GOLDEN};

/// The server table of the files below.
const SERVER: &str = r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "pass-1"
bucket          = "tier-bucket"
access_key_file = "key"
"#;

/// The least a pass-through file says: the server and a cold store in memory.
fn cold_text(cold_store: &str, rest: &str) -> String {
    format!("{SERVER}\n[cold_store]\nkind = \"memory\"\n{cold_store}\n{rest}")
}

/// The settings of this file text, which must be those of a pass-through front.
fn cold(text: &str, per_run: PerRun) -> ColdSettings {
    settings_from_text(text, per_run)
        .unwrap()
        .into_cold()
        .unwrap()
}

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// Asserts that a file with `[server]` and `[cold_store]` and no other tier is the settings of a
/// pass-through front and not a refusal: the cold store is the tier of the table, the front has
/// the defaults of the flags and the knobs of the run are the run's. Catches a shape that is
/// still refused, one read as a storage node or a build node, and a default that differs from
/// the flags'.
#[test]
fn a_cold_store_alone_is_a_pass_through_front() {
    let slow = PerRun {
        drain_lb_wait: Duration::from_secs(1),
        drain_deadline: Duration::from_secs(2),
    };
    let settings = cold(&cold_text("", ""), slow);
    assert_eq!(settings.cold.spec, "memory");
    assert_eq!(settings.front.proxy_id, "pass-1");
    assert_eq!(settings.front.bucket, "tier-bucket");
    assert_eq!(settings.front.drain_lb_wait, Duration::from_secs(1));
    assert_eq!(settings.front.drain_deadline, Duration::from_secs(2));
    assert_eq!(settings.backend_check_wait, Duration::from_secs(300));
    assert_eq!(settings.s3_request_timeout, Duration::from_secs(60));
}

/// Asserts that an S3 cold store with every key a pass-through file can give is read as written,
/// and that the key files of the roles reach the front (a front that serves the key roles has to
/// be given their files): the spec, the endpoint, the region, the credentials file, the two
/// waits, and the lists of key files.
#[test]
fn an_s3_cold_store_and_the_key_files_of_the_roles_are_read() {
    let text = r#"
[server]
listen              = "192.0.2.10:9300"
proxy_id            = "pass-2"
bucket              = "tierstore"
access_key_file     = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]
presign_key_files   = ["/etc/tierstore/pack-presign.key"]
gc_key_files        = ["/etc/tierstore/pack-gc.key"]
scoped_key_files    = ["/etc/tierstore/scoped.key"]
s3_request_timeout  = "2m"
backend_check_wait  = "90s"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://192.0.2.20:9000"
region           = "us-west-2"
credentials_file = "/etc/tierstore/cold.credentials"
"#;
    let settings = cold(text, DEFAULTS);
    assert_eq!(settings.cold.spec, "s3:tierstore-cold");
    assert_eq!(
        settings.cold.endpoint.as_deref(),
        Some("http://192.0.2.20:9000")
    );
    assert_eq!(settings.cold.region.as_deref(), Some("us-west-2"));
    assert!(settings.cold.credentials_file.is_some());
    assert_eq!(settings.s3_request_timeout, Duration::from_secs(120));
    assert_eq!(settings.backend_check_wait, Duration::from_secs(90));
    let front = &settings.front;
    assert_eq!(front.read_only_key_file.len(), 1);
    assert_eq!(front.presign_key_file.len(), 1);
    assert_eq!(front.gc_key_file.len(), 1);
    assert_eq!(front.scoped_key_file.len(), 1);
}

/// Asserts that no check of the start-up self-test can be skipped on a cold store that is alone
/// (create-only has to hold at the cold store, for a put and for the completion of an upload, and
/// the one check a storage node may skip is the completion's): any `skip_checks`, a valid one
/// included, is refused when the file is read, with the key named and not the reason given.
/// Catches a pass-through front that starts on a store that cannot keep create-only.
#[test]
fn a_check_cannot_be_skipped_on_a_cold_store_alone() {
    let reason = "multipart-conditional=store-ignores-if-none-match-on-completion";
    for skips in [
        format!("skip_checks = [\"{reason}\"]"),
        "skip_checks = []".to_owned(),
    ] {
        let why = refusal(&cold_text(&skips, ""));
        assert!(why.starts_with("cold_store.skip_checks: "), "{why}");
        assert!(why.contains("create-only"), "{why}");
        assert!(!why.contains("ignores-if-none-match"), "{why}");
    }
}

/// Asserts that the keys that are not for this shape are refused with their names: a list of
/// endpoints (there is one cold store), and each key that only `[hot_store]` has.
#[test]
fn a_list_of_endpoints_and_the_keys_of_a_hot_store_are_refused() {
    let s3 = |extra: &str| {
        let tier = format!(
            "bucket = \"b\"\nendpoint = \"http://192.0.2.20:9000\"\ncredentials_file = \"c\"\n{extra}"
        );
        cold_text("", "").replace("kind = \"memory\"", &format!("kind = \"s3\"\n{tier}"))
    };
    let why = refusal(&s3("endpoints = [\"http://192.0.2.21:9000\"]"));
    assert!(why.starts_with("cold_store.endpoints: "), "{why}");
    for line in [
        "budget = 1000",
        "high_water = 80",
        "lease_ttl = \"30s\"",
        "journal_retention = \"1h\"",
        "copy_parallel = 4",
    ] {
        let why = refusal(&s3(line));
        let key = line.split(' ').next().unwrap();
        assert!(why.starts_with(&format!("cold_store.{key}: ")), "{why}");
    }
}

/// Asserts that `[[policy]]` is read and checked as on a storage node: a policy that asks for a
/// cache is refused (there is none), one that does not is accepted and one that names a reserved
/// prefix is refused.
#[test]
fn a_policy_is_checked_and_a_cache_policy_is_refused() {
    let plain = "[[policy]]\nprefix = \"cas/\"\ncache = false\n";
    cold(&cold_text("", plain), DEFAULTS);
    let cached = "[[policy]]\nprefix = \"cas/\"\ncache = true\n";
    let why = refusal(&cold_text("", cached));
    assert!(why.starts_with("policy[0].cache: "), "{why}");
    let reserved = "[[policy]]\nprefix = \"_journal/\"\n";
    let why = refusal(&cold_text("", reserved));
    assert!(why.starts_with("policy[0].prefix: "), "{why}");
}

/// Asserts that the kind of the cold store is held to its keys as in the other shapes: an S3
/// cold store needs an endpoint and a credentials file, a directory needs a path.
#[test]
fn the_cold_store_is_held_to_the_keys_of_its_kind() {
    let s3 = cold_text("", "").replace("kind = \"memory\"", "kind = \"s3\"\nbucket = \"b\"");
    assert!(refusal(&s3).starts_with("cold_store.endpoint: "));
    let dir = cold_text("", "").replace("kind = \"memory\"", "kind = \"dir\"");
    assert!(refusal(&dir).starts_with("cold_store.path: "));
}

/// Asserts that the other shapes are as they were: a storage-node file is still a storage node's,
/// a hot store alone is refused with all three shapes named, and a pass-through file has no flag
/// form (`from_text` is the flags' form and says so).
#[test]
fn the_other_shapes_are_unchanged() {
    let storage = settings_from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(storage.into_cold().is_none());
    let storage = settings_from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(storage.into_storage().is_some());
    let cache = settings_from_text(CACHE, DEFAULTS).unwrap();
    assert!(cache.into_cold().is_none());
    let hot_alone = "[hot_store]\nkind = \"memory\"\nbudget = 1000\n";
    let why = refusal(&format!("{SERVER}\n{hot_alone}"));
    assert!(why.contains("both [hot_store] and [cold_store]"), "{why}");
    assert!(why.contains("[local_cache] and [cold_store]"), "{why}");
    assert!(why.contains("[cold_store] alone"), "{why}");
    let why = from_text(&cold_text("", ""), DEFAULTS)
        .unwrap_err()
        .to_string();
    assert!(why.starts_with("cold_store: "), "{why}");
    assert!(why.contains("no flag form"), "{why}");
}

/// Asserts that the README of the packaging documents the pass-through shape: its example (the
/// third TOML block, after the storage node's and the build node's) loads as a pass-through
/// front, and both READMEs have a section for it.
#[test]
fn the_readme_documents_the_cold_store_alone_file() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let packaging =
        std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    let package = std::fs::read_to_string(root.join("README.md")).unwrap();
    let blocks: Vec<&str> = packaging.split("```toml\n").skip(1).collect();
    let example = blocks[2].split_once("\n```").unwrap().0;
    let settings = cold(example, DEFAULTS);
    assert_eq!(settings.cold.spec, "s3:tierstore");
    assert_eq!(settings.front.proxy_id, "tierstore-pass-1");
    assert!(packaging.contains("### A cold store alone"));
    assert!(package.contains("## A cold store alone"));
}
