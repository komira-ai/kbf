//! The build-node shape of the configuration file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::config::{File, from_text, settings_from_text};
use super::config_cache::{CacheSettings, Rate};
use super::config_testing::{DEFAULTS, GOLDEN, MINIMAL};
use super::local_cache::{FILL_CHUNK, WHOLE_FILL_LIMIT};

/// A build node's file with every key given a value that is not the default.
const CACHE: &str = r#"
[server]
listen                = "192.0.2.10:9400"
proxy_id              = "tierstore-cache-1"
bucket                = "tierstore"
region                = "eu-west-1"
max_request_bytes     = 134217728
access_key_file       = "/etc/tierstore-cache/access.key"
read_only_key_files   = ["/etc/tierstore-cache/clients.key"]
presign_max_expires   = "10m"
max_concurrent_puts   = 8
put_min_bytes_per_sec = 2048
put_rate_grace        = "45s"
body_idle_timeout     = "20s"
s3_request_timeout    = "2m"

[local_cache]
path       = "/var/lib/tierstore-cache"
layout     = "digest-v1"
budget     = "40GiB"
high_water = 85
low_water  = 60
critical   = 97
free_floor = 15
max_concurrent_fills = 6
write_through = true
whole_fill_limit = "128MiB"
cache      = true
scrub_rate = "50MiB/s"

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://192.0.2.20:9300"
region           = "us-west-2"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true

[[policy]]
prefix = "cas/d/"
cache  = true

[[policy]]
prefix = "images/"
cache  = true

[[policy]]
prefix = ""
cache  = false

[oci]
listen              = "192.0.2.10:5000"
tls_cert_file       = "/etc/tierstore-cache/oci.crt"
tls_key_file        = "/etc/tierstore-cache/oci.key"
prefix              = "images/"
max_connections     = 16
connection_lifetime = "30m"
"#;

/// The least a build node's file can say.
const LEAST: &str = r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// `LEAST` with a policy that keeps the keys of the default `[oci]` prefix, `oci/`.
const KEEPS_OCI: &str = r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "oci/"
cache  = true
"#;

/// The settings of a build node's file.
fn cache_of(text: &str) -> CacheSettings {
    settings_from_text(text, DEFAULTS)
        .unwrap()
        .into_cache()
        .unwrap()
}

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `message` without the leading `line N, column M: ` of a refusal of the syntax, if it has one.
fn without_position(message: &str) -> &str {
    match message.split_once(": ") {
        Some((position, rest)) if position.starts_with("line ") => rest,
        _ => message,
    }
}

/// `CACHE` with the text `from` replaced by `to`, which must be in it.
fn cache_with(from: &str, to: &str) -> String {
    assert!(CACHE.contains(from), "{from}");
    CACHE.replacen(from, to, 1)
}

/// Asserts that a build node's file makes the settings it says, every key to its field, and that
/// a storage node's loader (`from_text`, the flags' form) says a build node has no flag form.
#[test]
fn a_build_node_file_makes_a_cache() {
    let cache = cache_of(CACHE);
    let front = &cache.front;
    assert_eq!(front.listen.to_string(), "192.0.2.10:9400");
    assert_eq!(front.proxy_id, "tierstore-cache-1");
    assert_eq!(
        (front.bucket.as_str(), front.region.as_str()),
        ("tierstore", "eu-west-1")
    );
    assert_eq!(front.max_request_bytes, 134_217_728);
    assert_eq!(
        front.access_key_file,
        Path::new("/etc/tierstore-cache/access.key")
    );
    assert_eq!(
        front.read_only_key_file,
        [PathBuf::from("/etc/tierstore-cache/clients.key")]
    );
    assert_eq!(front.presign_max_expires, Duration::from_secs(600));
    assert_eq!(front.max_concurrent_puts, 8);
    assert_eq!(front.put_min_bytes_per_sec, 2048);
    assert_eq!(front.put_rate_grace, Duration::from_secs(45));
    assert_eq!(front.body_idle_timeout, Duration::from_secs(20));
    assert_eq!(cache.s3_request_timeout, Duration::from_secs(120));
    assert_eq!(front.drain_lb_wait, DEFAULTS.drain_lb_wait);

    let config = &cache.cache;
    assert_eq!(config.path, Path::new("/var/lib/tierstore-cache"));
    assert_eq!(config.budget, 40 << 30);
    assert_eq!(
        (
            config.high_water,
            config.low_water,
            config.critical,
            config.free_floor
        ),
        (85, 60, 97, 15)
    );
    let policies = [
        ("cas/b/", true),
        ("cas/d/", true),
        ("images/", true),
        ("", false),
    ];
    let policies: Vec<(String, bool)> = policies
        .iter()
        .map(|(p, c)| ((*p).to_owned(), *c))
        .collect();
    assert_eq!(config.policies, policies);
    assert_eq!(
        (config.fill_chunk, config.whole_limit),
        (8 << 20, 128 << 20)
    );
    assert_eq!((FILL_CHUNK, WHOLE_FILL_LIMIT), (8 << 20, 64 << 20));
    assert_eq!(config.max_concurrent_fills, 6);
    assert!(config.write_through);
    let upstream = &cache.upstream;
    assert_eq!(upstream.spec, "s3:tierstore");
    assert_eq!(upstream.endpoint.as_deref(), Some("http://192.0.2.20:9300"));
    assert_eq!(upstream.region.as_deref(), Some("us-west-2"));
    assert_eq!(
        upstream.credentials_file.as_deref(),
        Some(Path::new("/etc/tierstore-cache/upstream-read.key"))
    );
    assert_eq!(cache.scrub_rate, Some(50 << 20));

    let why = from_text(CACHE, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("no flag form"), "{why}");
    let settings = settings_from_text(CACHE, DEFAULTS).unwrap();
    assert!(settings.into_storage().is_none());
    let storage = settings_from_text(GOLDEN, DEFAULTS).unwrap();
    assert!(storage.into_cache().is_none());
}

/// Asserts the defaults of a build node's file: marks at 90, 75 and 95 percent, a floor of 10
/// percent, the region of the flags, no scrub, and the limits of the flags.
#[test]
fn a_build_node_file_has_the_defaults_of_the_flags_and_of_the_cache() {
    let cache = cache_of(LEAST);
    let config = &cache.cache;
    assert_eq!(
        (
            config.high_water,
            config.low_water,
            config.critical,
            config.free_floor
        ),
        (90, 75, 95, 10)
    );
    assert_eq!(config.max_concurrent_fills, 4);
    assert!(
        !config.write_through,
        "a file that leaves it out is a read cache"
    );
    assert_eq!(config.whole_limit, WHOLE_FILL_LIMIT);
    assert_eq!(config.budget, 1000);
    assert_eq!(cache.front.region, "us-east-1");
    assert_eq!(cache.front.max_request_bytes, 256 << 20);
    assert_eq!(cache.front.max_concurrent_puts, 32);
    assert_eq!(cache.s3_request_timeout, Duration::from_secs(60));
    assert_eq!(cache.scrub_rate, None);
    assert_eq!(cache.upstream.spec, "memory");
}

/// Asserts that a single `endpoints` entry is the upstream, as `endpoint` is.
#[test]
fn a_list_of_one_endpoint_is_the_upstream() {
    let one = cache_with(
        "endpoint         = \"http://192.0.2.20:9300\"",
        "endpoints        = [\"http://192.0.2.21:9300\"]",
    );
    let cache = cache_of(&one);
    assert_eq!(
        cache.upstream.endpoint.as_deref(),
        Some("http://192.0.2.21:9300")
    );
}

/// Asserts each refusal of a build node's file, each naming the key and what is wrong, and none
/// repeating a value: the three tables together, a layout that is not the only one, a cache that
/// is turned off, marks out of order or out of range, a budget that is missing or zero, a rate
/// that is not a rate, no policy that caches, more than one upstream or two ways to give it,
/// and the keys of a storage node that do nothing here.
#[test]
fn a_build_node_file_is_refused_when_it_cannot_be_run() {
    let no_cache_policy = cache_with(
        "prefix = \"cas/b/\"\ncache  = true",
        "prefix = \"cas/b/\"\ncache  = false",
    )
    .replacen(
        "prefix = \"cas/d/\"\ncache  = true",
        "prefix = \"cas/d/\"\ncache  = false",
        1,
    )
    .replacen(
        "prefix = \"images/\"\ncache  = true",
        "prefix = \"images/\"\ncache  = false",
        1,
    );
    let cases = [
        (
            cache_with(
                "[local_cache]",
                "[hot_store]\nkind = \"memory\"\nbudget = 1\n\n[local_cache]",
            ),
            "is refused with [hot_store]",
        ),
        (
            cache_with("\"digest-v1\"", "\"digest-v2\""),
            "local_cache.layout",
        ),
        (
            cache_with("cache      = true", "cache      = false"),
            "local_cache.cache",
        ),
        (
            cache_with("budget     = \"40GiB\"\n", ""),
            "local_cache.budget",
        ),
        (cache_with("\"40GiB\"", "0"), "local_cache.budget"),
        (
            cache_with("low_water  = 60", "low_water  = 90"),
            "local_cache.low_water",
        ),
        (
            cache_with("critical   = 97", "critical   = 80"),
            "must not be above critical",
        ),
        (
            cache_with("free_floor = 15", "free_floor = 100"),
            "local_cache.free_floor",
        ),
        (
            cache_with("high_water = 85", "high_water = 101"),
            "local_cache.high_water",
        ),
        (
            cache_with("\"50MiB/s\"", "\"fast\""),
            "expected a rate such as",
        ),
        (cache_with("\"50MiB/s\"", "50"), "scrub_rate"),
        (no_cache_policy, "no policy has cache = true"),
        (
            cache_with(
                "[[policy]]\nprefix = \"cas/d/\"",
                "[[policy]]\nprefix = \"cas/b/\"",
            ),
            "also the prefix of policy[0]",
        ),
        (
            cache_with("prefix = \"cas/d/\"", "prefix = \"_journal/\""),
            "keeps for itself",
        ),
        (
            cache_with(
                "credentials_file = ",
                "skip_checks = []\ncredentials_file = ",
            ),
            "cold_store.skip_checks",
        ),
        (
            cache_with(
                "credentials_file = ",
                "copy_parallel = 4\ncredentials_file = ",
            ),
            "cold_store.copy_parallel",
        ),
        (
            cache_with(
                "s3_request_timeout    = \"2m\"",
                "backend_check_wait = \"1m\"",
            ),
            "server.backend_check_wait",
        ),
        (
            cache_with(
                "endpoint         = \"http://192.0.2.20:9300\"",
                "endpoints = [\"http://a:1\", \"http://b:2\"]",
            ),
            "give exactly one endpoint",
        ),
        (
            cache_with(
                "endpoint         = \"http://192.0.2.20:9300\"",
                "endpoints = []",
            ),
            "give exactly one endpoint",
        ),
        (
            cache_with(
                "credentials_file = ",
                "endpoints = [\"http://a:1\"]\ncredentials_file = ",
            ),
            "not both",
        ),
        (
            cache_with("endpoint         = \"http://192.0.2.20:9300\"\n", ""),
            "cold_store.endpoint",
        ),
        (
            cache_with(
                "credentials_file = ",
                "secret_access_key = \"x\"\ncredentials_file = ",
            ),
            "never in the file",
        ),
    ];
    for (text, want) in cases {
        let why = refusal(&text);
        assert!(why.contains(want), "{want}: {why}");
    }
    // A rate that does not parse is refused naming its key, and the value is not shown.
    let why = refusal(&cache_with("\"50MiB/s\"", "\"fast\""));
    assert!(why.contains("scrub_rate"), "{why}");
    assert!(!why.contains("fast"), "{why}");
}

/// Asserts that a `whole_fill_limit` that is not a size is refused naming its key and without the
/// value, as a `budget` is: the key is one a refusal may name.
#[test]
fn a_whole_fill_limit_that_is_not_a_size_is_refused_naming_the_key_and_not_the_value() {
    for bad in ["\"huge\"", "-5"] {
        let text = LEAST.replacen(
            "budget = 1000",
            &format!("budget = 1000\nwhole_fill_limit = {bad}"),
            1,
        );
        let why = refusal(&text);
        assert!(why.contains("whole_fill_limit"), "{bad}: {why}");
        assert!(!why.contains("huge"), "{bad}: {why}");
    }
}

/// Asserts the edges of the water marks: a low water mark one under the high one, a high one
/// equal to the critical one and a free floor of 99 are accepted; a low water mark equal to the
/// high one, a high one over the critical one, a high or low water mark of 0 and a floor of 100
/// are refused naming the key.
#[test]
fn the_water_marks_are_judged_at_their_edges() {
    let accepted = [
        cache_with("low_water  = 60", "low_water  = 84"),
        cache_with("high_water = 85", "high_water = 97"),
        cache_with("free_floor = 15", "free_floor = 99"),
        cache_with("free_floor = 15", "free_floor = 0"),
        cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 1"),
        cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 64"),
    ];
    for text in accepted {
        let made = settings_from_text(&text, DEFAULTS);
        assert!(made.is_ok(), "{text}");
    }
    let refused = [
        (
            cache_with("low_water  = 60", "low_water  = 85"),
            "local_cache.low_water",
        ),
        (
            cache_with("high_water = 85", "high_water = 98"),
            "local_cache.high_water",
        ),
        (
            cache_with("high_water = 85", "high_water = 0"),
            "local_cache.high_water",
        ),
        (
            cache_with("low_water  = 60", "low_water  = 0"),
            "local_cache.low_water",
        ),
        (
            cache_with("free_floor = 15", "free_floor = 100"),
            "local_cache.free_floor",
        ),
        (
            cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 0"),
            "local_cache.max_concurrent_fills",
        ),
        (
            cache_with("max_concurrent_fills = 6", "max_concurrent_fills = 65"),
            "local_cache.max_concurrent_fills",
        ),
    ];
    for (text, key) in refused {
        let why = refusal(&text);
        assert!(why.contains(key), "{key}: {why}");
    }
}

/// Asserts that a storage node refuses what only a build node has: a list of endpoints in either
/// tier, and a policy that asks for a cache, with the reason.
#[test]
fn a_storage_node_refuses_a_list_of_endpoints_and_a_cache_policy() {
    let hot = GOLDEN.replacen(
        "[hot_store]\n",
        "[hot_store]\nendpoints = [\"http://h:1\"]\n",
        1,
    );
    // Put in [cold_store] itself, not at the end of the file, which ends with another table.
    let cold = GOLDEN.replacen(
        "[cold_store]\n",
        "[cold_store]\nendpoints = [\"http://c:1\"]\n",
        1,
    );
    for (text, key) in [(hot, "hot_store.endpoints"), (cold, "cold_store.endpoints")] {
        let why = refusal(&text);
        assert!(why.contains(key), "{why}");
        assert!(why.contains("build node"), "{why}");
    }
    let policy = format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\ncache = true\n");
    let why = refusal(&policy);
    assert!(why.contains("policy[0].cache"), "{why}");
    assert!(why.contains("keeps no cache"), "{why}");
}

/// Asserts that a build node's file takes the `[oci]` table with the keys of a storage node's,
/// each to the field the `--oci-*` flag has, and that a file without the table has no front.
#[test]
fn a_build_node_file_with_an_oci_table_makes_the_front() {
    let oci = cache_of(CACHE).oci;
    assert_eq!(oci.listen, Some("192.0.2.10:5000".parse().unwrap()));
    assert_eq!(
        oci.tls_cert.as_deref(),
        Some(Path::new("/etc/tierstore-cache/oci.crt"))
    );
    assert_eq!(
        oci.tls_key.as_deref(),
        Some(Path::new("/etc/tierstore-cache/oci.key"))
    );
    assert_eq!(oci.prefix.as_deref(), Some("images/"));
    assert_eq!(oci.max_connections, Some(16));
    assert_eq!(oci.connection_lifetime, Some(Duration::from_secs(1800)));

    let plain = format!("{KEEPS_OCI}\n[oci]\nlisten = \"127.0.0.1:5000\"\n");
    let oci = cache_of(&plain).oci;
    assert_eq!(oci.listen, Some("127.0.0.1:5000".parse().unwrap()));
    assert_eq!((oci.tls_cert, oci.tls_key, oci.prefix), (None, None, None));
    assert_eq!((oci.max_connections, oci.connection_lifetime), (None, None));

    assert_eq!(cache_of(LEAST).oci.listen, None);
}

/// Asserts that a build node's `[oci]` table is refused exactly as a storage node's is: for each
/// table below the build node's message equals the storage node's and names the key, so a
/// certificate without a key, a key without a certificate, any key without `listen`, a count of
/// zero and a key the table does not have are refused by the one set of rules.
#[test]
fn a_build_node_oci_table_is_refused_as_a_storage_nodes_is() {
    let listen = "listen = \"127.0.0.1:5000\"\n";
    let cases = [
        (
            format!("{listen}tls_cert_file = \"/c\""),
            "oci.tls_key_file: it is needed with oci.tls_cert_file",
        ),
        (
            format!("{listen}tls_key_file = \"/k\""),
            "oci.tls_cert_file: it is needed with oci.tls_key_file",
        ),
        (
            "tls_cert_file = \"/c\"\ntls_key_file = \"/k\"".to_owned(),
            "oci.tls_cert_file: it needs oci.listen",
        ),
        (
            "prefix = \"oci/\"".to_owned(),
            "oci.prefix: it needs oci.listen",
        ),
        (
            "max_connections = 4".to_owned(),
            "oci.max_connections: it needs",
        ),
        (
            "connection_lifetime = \"1h\"".to_owned(),
            "oci.connection_lifetime: it needs",
        ),
        (String::new(), "oci.listen: it is needed"),
        (
            format!("{listen}max_connections = 0"),
            "oci.max_connections",
        ),
        (format!("{listen}bogus = 1"), "bogus"),
    ];
    for (oci, want) in cases {
        let build = refusal(&format!("{LEAST}\n[oci]\n{oci}\n"));
        let storage = refusal(&format!("{MINIMAL}\n[oci]\n{oci}\n"));
        assert!(build.contains(want), "{oci}: {build}");
        // A refusal of the syntax says where it is, and the two files are not laid out alike.
        assert_eq!(
            without_position(&build),
            without_position(&storage),
            "{oci}"
        );
    }
}

/// Asserts that a rate reads as a size and `/s`, is refused otherwise, and is written back as a
/// rate that reads as the same.
#[test]
fn a_rate_is_a_size_and_a_per_second() {
    let rate = |text: &str| serde_json::from_str::<Rate>(&format!("{text:?}"));
    assert_eq!(rate("50MiB/s").unwrap(), Rate(50 << 20));
    assert_eq!(rate("7B/s").unwrap(), Rate(7));
    for bad in ["50MiB", "x/s", "-1B/s", "/s", "1 B/s"] {
        let why = rate(bad).unwrap_err().to_string();
        assert!(why.contains("a rate such as"), "{bad}: {why}");
    }
    let why = serde_json::from_str::<Rate>("5").unwrap_err().to_string();
    assert!(why.contains("a rate such as"), "{why}");
    let written = serde_json::to_string(&Rate(1234)).unwrap();
    assert_eq!(serde_json::from_str::<Rate>(&written).unwrap(), Rate(1234));
}

/// Asserts that the tables written back as TOML make the same settings: every table of the
/// build-node file is written by the code that writes `--print-config`'s files.
#[test]
fn a_build_node_file_written_back_makes_the_same_settings() {
    let file: File = toml::from_str(CACHE).unwrap();
    let written = toml::to_string(&file).unwrap();
    let again = cache_of(&written);
    assert_eq!(format!("{again:?}"), format!("{:?}", cache_of(CACHE)));
}

/// Asserts that an `[oci]` front whose blobs no policy keeps is refused at load, with a message
/// that names `oci.prefix` and says which policy decides: none covers the default prefix, the
/// policy that covers it has `cache = false`, and a given prefix is checked, not the default. A
/// policy that keeps the prefix (the default, and a given one) loads.
#[test]
fn an_oci_front_over_keys_the_cache_does_not_keep_is_refused() {
    let front = "[oci]\nlisten = \"127.0.0.1:5000\"\n";
    let none = refusal(&format!("{LEAST}\n{front}"));
    assert!(none.contains("oci.prefix"), "{none}");
    assert!(none.contains("no [[policy]] covers"), "{none}");
    assert!(none.contains("\"oci/blobs/sha256/\""), "{none}");

    let off = "[[policy]]\nprefix = \"\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{off}\n{front}"));
    assert!(why.contains("oci.prefix"), "{why}");
    assert!(why.contains("cache = false"), "{why}");

    let exact = "[[policy]]\nprefix = \"oci/\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{exact}\n{front}"));
    assert!(why.contains("oci.prefix"), "{why}");
    assert!(why.contains("cache = false"), "{why}");

    // A policy prefix must end in `/` and the blobs are flat under `blobs/sha256/`, so no policy
    // can cover some of the blobs: the one that covers them all decides.
    let rest = "[[policy]]\nprefix = \"\"\ncache = true\n";
    let inside = "[[policy]]\nprefix = \"oci/blobs/sha256/0\"\ncache = false\n";
    let why = refusal(&format!("{LEAST}\n{rest}\n{inside}\n{front}"));
    assert!(why.contains("does not end in /"), "{why}");
    let outside = "[[policy]]\nprefix = \"other/\"\ncache = false\n";
    let loaded = cache_of(&format!("{LEAST}\n{rest}\n{outside}\n{front}"));
    assert!(loaded.oci.listen.is_some());

    let given = format!("{front}prefix = \"layers/\"\n");
    let why = refusal(&format!("{KEEPS_OCI}\n{given}"));
    assert!(why.contains("\"layers/blobs/sha256/\""), "{why}");
    let layers = "[[policy]]\nprefix = \"layers/\"\ncache = true\n";
    let kept = cache_of(&format!("{KEEPS_OCI}\n{layers}\n{given}"));
    assert!(kept.oci.listen.is_some());
    let default = cache_of(&format!("{KEEPS_OCI}\n{front}"));
    assert!(default.oci.listen.is_some());
    // No front, no check: a file whose policies keep nothing under `oci/` loads as before.
    assert_eq!(cache_of(LEAST).oci.listen, None);
}

/// Asserts the `whole_fill_limit` key: a size that reaches `whole_limit`, and refused at zero and
/// above the budget, naming the key.
#[test]
fn the_whole_fill_limit_is_a_size_between_one_byte_and_the_budget() {
    let with = |limit: &str| {
        let key = format!("budget = 1000\nwhole_fill_limit = {limit}");
        LEAST.replacen("budget = 1000", &key, 1)
    };
    assert_eq!(cache_of(&with("1000")).cache.whole_limit, 1000);
    assert_eq!(cache_of(&with("1")).cache.whole_limit, 1);
    for bad in ["0", "1001"] {
        let why = refusal(&with(bad));
        assert!(why.contains("local_cache.whole_fill_limit"), "{bad}: {why}");
    }
}

/// The keys of `[local_cache]`, which the README of the packaging must document each in a row.
const LOCAL_CACHE_KEYS: [&str; 12] = [
    "path",
    "layout",
    "budget",
    "high_water",
    "low_water",
    "critical",
    "free_floor",
    "max_concurrent_fills",
    "write_through",
    "whole_fill_limit",
    "cache",
    "scrub_rate",
];

/// Asserts that the README of the packaging documents the build-node shape: its example loads,
/// every key of `[local_cache]` has a row of the table, and the list above is every key the table
/// has.
#[test]
fn the_readme_documents_the_build_node_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    let blocks: Vec<&str> = readme.split("```toml\n").skip(1).collect();
    let example = blocks[1].split_once("\n```").unwrap().0;
    let cache = cache_of(example);
    assert_eq!(cache.cache.policies.len(), 3);
    for key in LOCAL_CACHE_KEYS {
        let wanted = format!("`local_cache.{key}`");
        let row = readme
            .lines()
            .find(|line| line.starts_with('|') && line.contains(&wanted));
        assert!(row.is_some(), "no row for {key}");
    }
    let file: File = toml::from_str(CACHE).unwrap();
    let table = toml::Table::try_from(file.local_cache.unwrap()).unwrap();
    let keys: BTreeSet<&str> = table.keys().map(String::as_str).collect();
    let listed: BTreeSet<&str> = LOCAL_CACHE_KEYS.iter().copied().collect();
    assert_eq!(keys, listed);
}
