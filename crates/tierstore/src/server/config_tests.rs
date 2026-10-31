//! The configuration file: what a file makes, what it refuses, and that every flag has a key.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use kbf_testkit::dir::TestDir;

use super::{
    ConfigError, KEYS, PER_RUN, PerRun, format_duration, from_text, keys_for_flags, load,
    parse_size,
};
use crate::server::cli::{Cli, parse_duration};
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL, MINIMAL_FLAGS, golden_flags};

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `GOLDEN` with `change` made in it: the text `from` replaced by `to`, which must be in it.
fn golden_with(from: &str, to: &str) -> String {
    assert!(GOLDEN.contains(from), "{from}");
    GOLDEN.replacen(from, to, 1)
}

/// `MINIMAL` with `from` replaced by `to`.
fn minimal_with(from: &str, to: &str) -> String {
    assert!(MINIMAL.contains(from), "{from}");
    MINIMAL.replacen(from, to, 1)
}

/// Asserts that the golden storage-node file makes the settings the same values given as flags
/// make: every key of the table holds what its flag holds, so a server started from the file is
/// the server started from the flags.
#[test]
fn a_storage_node_file_makes_the_settings_its_flags_make() {
    let from_file = from_text(GOLDEN, DEFAULTS).unwrap();
    let from_flags = Cli::try_parse_from(golden_flags()).unwrap();
    assert_eq!(from_file, from_flags);
    assert_eq!(from_file.hot_bytes, 500 << 30);
    assert_eq!(from_file.hot, "s3:tierstore-hot");
    assert_eq!(from_file.skip_backend_check.len(), 1);
    assert_eq!(from_file.read_only_key_file.len(), 2);
    assert_eq!(from_file.config, None);
    assert!(!from_file.print_config);
}

/// Asserts that a file that gives the least a start needs gets, for every key it leaves out, the
/// value the flag has when it is left out, so there is one set of defaults.
#[test]
fn a_key_that_is_left_out_has_the_default_of_its_flag() {
    let from_file = from_text(MINIMAL, DEFAULTS).unwrap();
    let from_flags = Cli::try_parse_from(MINIMAL_FLAGS).unwrap();
    assert_eq!(from_file, from_flags);
    assert_eq!(DEFAULTS.drain_lb_wait, from_flags.drain_lb_wait);
    assert_eq!(DEFAULTS.drain_deadline, from_flags.drain_deadline);
    let slow = PerRun {
        drain_lb_wait: Duration::from_secs(1),
        drain_deadline: Duration::from_secs(2),
    };
    let with_run = from_text(MINIMAL, slow).unwrap();
    assert_eq!(with_run.drain_lb_wait, Duration::from_secs(1));
    assert_eq!(with_run.drain_deadline, Duration::from_secs(2));
}

/// Asserts that the tiers of the other kinds are made as `--hot dir:<path>` and `--cold memory`
/// make them, and that a size is a number or a string with a unit.
#[test]
fn a_directory_tier_and_a_size_in_bytes_are_read() {
    let text = minimal_with(
        "kind   = \"memory\"\nbudget = 1000",
        "kind = \"dir\"\npath = \"/var/hot\"\nbudget = \"2MiB\"",
    );
    let cli = from_text(&text, DEFAULTS).unwrap();
    assert_eq!(cli.hot, "dir:/var/hot");
    assert_eq!(cli.hot_bytes, 2 << 20);
    assert_eq!(cli.cold, "memory");
}

/// Asserts that a file is read from disk: a file that is not there is a refusal that names it,
/// and a file with a fault is a refusal that names the file and the fault.
#[test]
fn a_file_is_read_from_its_path_and_its_refusals_name_it() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-config-load");
    let good = dir.path().join("tierstore.toml");
    std::fs::write(&good, GOLDEN).unwrap();
    assert_eq!(
        load(&good, DEFAULTS).unwrap().into_storage(),
        Some(from_text(GOLDEN, DEFAULTS).unwrap())
    );
    let missing = load(&dir.path().join("none.toml"), DEFAULTS).unwrap_err();
    assert!(matches!(missing, ConfigError::Read { .. }), "{missing}");
    assert!(missing.to_string().contains("none.toml"), "{missing}");
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, golden_with("[cold_store]", "[cold_store]\nwhat = 1")).unwrap();
    let why = load(&bad, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("bad.toml"), "{why}");
    assert!(why.contains("what"), "{why}");
}

/// Asserts that a key or a table the format does not have is refused with its name, whichever
/// table it is in: the file is a closed set.
#[test]
fn an_unknown_key_or_table_is_refused_by_name() {
    let cases = [
        golden_with("[server]", "[server]\nbogus = 1"),
        golden_with("[hot_store]", "[hot_store]\nbogus = 1"),
        golden_with("[cold_store]", "[cold_store]\nbogus = 1"),
        golden_with("[oci]", "[oci]\nbogus = 1"),
        format!("{GOLDEN}\n[bogus]\nx = 1\n"),
        format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\nbogus = 1\n"),
        format!("bogus = 1\n{GOLDEN}"),
    ];
    for text in cases {
        let why = refusal(&text);
        assert!(why.contains("bogus"), "{why}");
    }
    let why = refusal(&golden_with(
        "listen                = \"192.0.2.10:9300\"",
        "listen = \"nowhere\"",
    ));
    assert!(why.contains("listen"), "{why}");
    let syntax = from_text("this is = not toml [", DEFAULTS);
    assert!(matches!(syntax, Err(ConfigError::Syntax { .. })));
    let lines: Vec<&str> = GOLDEN
        .lines()
        .filter(|line| !line.starts_with("access_key_file"))
        .collect();
    let why = refusal(&lines.join("\n"));
    assert!(why.contains("access_key_file"), "{why}");
}

/// Asserts that a key that holds a secret is refused with its table and name, wherever it is
/// (in a table, in an array of tables), while a key that names a file is not: a key file is a
/// path.
#[test]
fn a_secret_in_the_file_is_refused_and_a_path_to_one_is_not() {
    let cases = [
        (
            "[server]",
            "[server]\nsecret_access_key = \"x\"",
            "server.secret_access_key",
        ),
        (
            "[server]",
            "[server]\naccess_key_id = \"x\"",
            "server.access_key_id",
        ),
        (
            "[hot_store]",
            "[hot_store]\npassword = \"x\"",
            "hot_store.password",
        ),
        (
            "[cold_store]",
            "[cold_store]\nsession_token = \"x\"",
            "cold_store.session_token",
        ),
        (
            "[cold_store]",
            "[cold_store]\nsigning_key = \"x\"",
            "cold_store.signing_key",
        ),
        (
            "[cold_store]",
            "[cold_store]\naccess_key = \"x\"",
            "cold_store.access_key",
        ),
    ];
    for (from, to, key) in cases {
        let why = refusal(&golden_with(from, to));
        assert!(why.contains(key), "{why}");
        assert!(why.contains("never in the file"), "{why}");
    }
    let policy = format!("{GOLDEN}\n[[policy]]\nprefix = \"a/\"\ntoken = \"x\"\n");
    assert!(refusal(&policy).contains("policy.token"));
    assert!(from_text(GOLDEN, DEFAULTS).is_ok());
}

/// Asserts that a key whose name is a secret by another spelling is refused as one (any case, and
/// the words `key`, `credentials`, `passphrase`, `apikey`, `psk`), without its value.
#[test]
fn a_secret_by_another_name_is_refused_without_its_value() {
    let names = [
        "Secret_Key",
        "key",
        "credentials",
        "passphrase",
        "ApiKey",
        "psk",
    ];
    for name in names {
        let why = refusal(&golden_with(
            "[server]",
            &format!("[server]\n{name} = \"SENTINEL\""),
        ));
        assert!(why.contains(name), "{why}");
        assert!(why.contains("never in the file"), "{why}");
        assert!(!why.contains("SENTINEL"), "{why}");
    }
}

/// Asserts that no message about a fault in the file repeats the line or the value of the key:
/// a key that is unknown (and so a secret by a name no rule knows), a line that is not TOML, a
/// value of the wrong type, a size, a duration, a kind and a skipped check that are not valid. Each
/// still says which key (and, but for a skipped check, where).
#[test]
fn a_fault_in_the_file_is_reported_without_the_value() {
    let cases = [
        (
            golden_with("[server]", "[server]\npasskey = \"SENTINEL\""),
            "passkey",
        ),
        (
            golden_with("[server]", "[server]\nbroken = SENTINEL"),
            "line ",
        ),
        (
            golden_with(
                "max_concurrent_puts   = 8",
                "max_concurrent_puts = \"SENTINEL\"",
            ),
            "max_concurrent_puts",
        ),
        (
            golden_with("budget             = \"500GiB\"", "budget = \"SENTINEL\""),
            "budget",
        ),
        (
            golden_with(
                "presign_max_expires   = \"10m\"",
                "presign_max_expires = \"SENTINEL\"",
            ),
            "presign_max_expires",
        ),
        (
            golden_with("presign_max_expires   = \"10m\"", "presign_max_expires = 5"),
            "presign_max_expires",
        ),
        (
            golden_with("kind               = \"s3\"", "kind = \"SENTINEL\""),
            "kind",
        ),
        (
            golden_with(
                "skip_checks      = [\"multipart-conditional=store-ignores-if-none-match-on-completion\"]",
                "skip_checks = [\"SENTINEL\"]",
            ),
            "skip_checks",
        ),
    ];
    for (text, want) in cases {
        let why = refusal(&text);
        assert!(why.contains(want), "{want}: {why}");
        assert!(!why.contains("SENTINEL"), "{want}: {why}");
    }
}

/// Asserts that a fault inside a multi-line string names no "key" taken from the string's text:
/// the text before an `=` on such a line is part of a value (perhaps a pasted secret), so it is
/// neither shown as the key nor repeated.
#[test]
fn a_fault_inside_a_multi_line_string_shows_nothing_of_the_string() {
    let text = golden_with(
        "[server]",
        "[server]\nnote = \"\"\"\nabcdef=SENTINEL \\q\n\"\"\"",
    );
    let why = refusal(&text);
    assert!(why.starts_with("line "), "{why}");
    assert!(!why.contains("abcdef"), "{why}");
    assert!(!why.contains("SENTINEL"), "{why}");
}

/// Asserts that `[local_cache]` along with `[hot_store]` is refused, with the reason, and that a
/// file with neither shape's tables (`[local_cache]` alone, or a storage node's file missing
/// `[hot_store]` or `[cold_store]`) is refused for the shape.
#[test]
fn a_local_cache_with_a_hot_store_and_any_shape_but_the_two_are_refused() {
    let cache = format!("{GOLDEN}\n[local_cache]\npath = \"/var/cache\"\n");
    let why = refusal(&cache);
    assert!(
        why.contains("local_cache") && why.contains("is refused with [hot_store]"),
        "{why}"
    );
    let only_cache = minimal_with(
        "[hot_store]\nkind   = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n",
        "[local_cache]\npath = \"/c\"\n",
    );
    assert!(refusal(&only_cache).contains("[local_cache] and [cold_store]"));
    let no_cold = minimal_with("[cold_store]\nkind = \"memory\"\n", "");
    let no_hot = minimal_with("[hot_store]\nkind   = \"memory\"\nbudget = 1000\n", "");
    for text in [no_cold, no_hot] {
        let why = refusal(&text);
        assert!(why.contains("both [hot_store] and [cold_store]"), "{why}");
    }
}

/// Asserts that a key of a tier is refused when it does not fit the tier's kind (a path for a
/// memory tier, an endpoint for a directory, a path for an S3 tier), and when the kind needs a
/// key that is not there.
#[test]
fn the_keys_of_a_tier_must_fit_its_kind() {
    let hot = "kind   = \"memory\"\nbudget = 1000";
    let cases = [
        (
            "kind = \"memory\"\nendpoint = \"http://x\"\nbudget = 1",
            "hot_store.endpoint",
        ),
        (
            "kind = \"memory\"\npath = \"/x\"\nbudget = 1",
            "hot_store.path",
        ),
        (
            "kind = \"memory\"\nbucket = \"b\"\nbudget = 1",
            "hot_store.bucket",
        ),
        ("kind = \"dir\"\nbudget = 1", "hot_store.path"),
        (
            "kind = \"dir\"\npath = \"/x\"\nregion = \"r\"\nbudget = 1",
            "hot_store.region",
        ),
        (
            "kind = \"dir\"\npath = \"/x\"\nbucket = \"b\"\nbudget = 1",
            "hot_store.bucket",
        ),
        (
            "kind = \"s3\"\nendpoint = \"http://x\"\ncredentials_file = \"/k\"\nbudget = 1",
            "hot_store.bucket",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\ncredentials_file = \"/k\"\nbudget = 1",
            "hot_store.endpoint",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\nendpoint = \"http://x\"\nbudget = 1",
            "hot_store.credentials_file",
        ),
        (
            "kind = \"s3\"\nbucket = \"b\"\npath = \"/x\"\nbudget = 1",
            "hot_store.path",
        ),
        ("kind = \"memory\"", "hot_store.budget"),
        // The refused value is never shown; the key and the values it takes are.
        (
            "kind = \"tape\"\nbudget = 1",
            "kind: not a value this key takes",
        ),
    ];
    for (table, key) in cases {
        let why = refusal(&minimal_with(hot, table));
        assert!(why.contains(key), "{table}: {why}");
    }
}

/// Asserts that the keys only one tier has are refused in the other, and that a check to skip is
/// `CHECK=REASON` with a check that can be skipped.
#[test]
fn the_keys_only_one_tier_has_are_refused_in_the_other() {
    let cold = minimal_with(
        "[cold_store]\nkind = \"memory\"",
        "[cold_store]\nkind = \"memory\"\nlease_ttl = \"1s\"",
    );
    assert!(refusal(&cold).contains("cold_store.lease_ttl"));
    let every = [
        "budget = 1",
        "high_water = 1",
        "low_water = 1",
        "critical = 1",
        "copy_parallel = 1",
        "reconcile_interval = \"1s\"",
        "lock_file = \"/var/lib/tierstore/instance.lock\"",
        "usage_stale_after = \"1s\"",
        "journal_linger = \"1s\"",
        "journal_retention = \"1s\"",
    ];
    for line in every {
        let text = minimal_with(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\nkind = \"memory\"\n{line}"),
        );
        let key = line.split(' ').next().unwrap();
        assert!(
            refusal(&text).contains(&format!("cold_store.{key}")),
            "{line}"
        );
    }
    let hot = minimal_with("budget = 1000", "budget = 1000\nskip_checks = []");
    assert!(refusal(&hot).contains("hot_store.skip_checks"));
    for bad in ["nonsense", "multipart-conditional=", "create-only=because"] {
        let text = minimal_with(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\nkind = \"memory\"\nskip_checks = [\"{bad}\"]"),
        );
        let why = refusal(&text);
        assert!(why.contains("cold_store.skip_checks"), "{bad}: {why}");
    }
}

/// Asserts that the values the flags bound are bound in the file too: a water mark outside 1 to
/// 100, a budget, a count or a limit of zero, a duration that is not one, a size that is not
/// one, and a negative size.
#[test]
fn a_value_out_of_range_is_refused_with_its_key() {
    let cases = [
        ("budget = 1000", "budget = 0", "hot_store.budget"),
        ("budget = 1000", "budget = -1", "negative"),
        ("budget = 1000", "budget = true", "a number of bytes"),
        ("budget = 1000", "budget = \"lots\"", "not a size"),
        ("budget = 1000", "budget = \"99999999999TiB\"", "not a size"),
        ("budget = 1000", "budget = \"5\"", "not a size"),
        ("budget = 1000", "budget = \"5PiB\"", "not a size"),
        (
            "budget = 1000",
            "budget = 1000\nhigh_water = 0",
            "hot_store.high_water",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlow_water = 101",
            "hot_store.low_water",
        ),
        (
            "budget = 1000",
            "budget = 1000\ncritical = 0",
            "hot_store.critical",
        ),
        (
            "budget = 1000",
            "budget = 1000\ncopy_parallel = 0",
            "hot_store.copy_parallel",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlease_ttl = \"soon\"",
            "lease_ttl: not a value this key takes (it is not shown); expected a duration such as",
        ),
        // A lock file is an absolute path that names a file: two processes of one node must
        // lock one file, whatever their working directories are.
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"instance.lock\"",
            "hot_store.lock_file: \"instance.lock\" must be an absolute path",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"\"",
            "hot_store.lock_file: \"\" must be an absolute path",
        ),
        (
            "budget = 1000",
            "budget = 1000\nlock_file = \"/\"",
            "hot_store.lock_file: \"/\" must name a file",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nmax_request_bytes = 0",
            "server.max_request_bytes",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nmax_concurrent_puts = 0",
            "server.max_concurrent_puts",
        ),
        (
            "\"tier-bucket\"",
            "\"tier-bucket\"\nbody_idle_timeout = \"30\"",
            "body_idle_timeout: not a value this key takes (it is not shown); expected a duration such as",
        ),
    ];
    for (from, to, want) in cases {
        let why = refusal(&minimal_with(from, to));
        assert!(why.contains(want), "{to}: {why}");
    }
}

/// `MINIMAL` with the text of an `[oci]` table after it.
fn minimal_with_oci(oci: &str) -> String {
    format!("{MINIMAL}\n[oci]\n{oci}\n")
}

/// Asserts that an `[oci]` table makes the settings the `--oci-*` flags make: with every key
/// given (the golden file) and with `listen` alone, where the other keys stay unset so that the
/// front's own defaults apply; and that a file without the table opens no front.
#[test]
fn an_oci_table_makes_the_settings_its_flags_make() {
    let all = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(all.oci_listen, Some("192.0.2.10:5000".parse().unwrap()));
    assert_eq!(
        all.oci_tls_cert.as_deref(),
        Some(Path::new("/etc/tierstore/oci.crt"))
    );
    assert_eq!(
        all.oci_tls_key.as_deref(),
        Some(Path::new("/etc/tierstore/oci.key"))
    );
    assert_eq!(all.oci_prefix.as_deref(), Some("images/"));
    assert_eq!(all.oci_max_connections, Some(16));
    assert_eq!(all.oci_connection_lifetime, Some(Duration::from_secs(1800)));
    assert_eq!(all, Cli::try_parse_from(golden_flags()).unwrap());

    let listen_only = from_text(&minimal_with_oci("listen = \"127.0.0.1:5000\""), DEFAULTS);
    let mut flags = MINIMAL_FLAGS.to_vec();
    flags.push("--oci-listen=127.0.0.1:5000");
    assert_eq!(listen_only.unwrap(), Cli::try_parse_from(flags).unwrap());

    let none = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(none.oci_listen, None);
}

/// Asserts that the `[oci]` rules are the flags' rules, each refusal naming its key: a
/// certificate without a key and a key without a certificate, any other key without `listen`
/// (and an empty table), a connection count of zero, a duration that is not one, a table with a
/// key it does not have, and a key that holds a secret.
#[test]
fn an_oci_table_is_refused_as_the_flags_are() {
    let listen = "listen = \"127.0.0.1:5000\"\n";
    let both = "tls_cert_file = \"/c\"\ntls_key_file = \"/k\"";
    let cases = [
        (
            format!("{listen}tls_cert_file = \"/c\""),
            "oci.tls_key_file",
        ),
        (
            format!("{listen}tls_key_file = \"/k\""),
            "oci.tls_cert_file",
        ),
        (both.to_owned(), "oci.tls_cert_file: it needs oci.listen"),
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
        (format!("{listen}max_connections = -1"), "max_connections"),
        (
            format!("{listen}connection_lifetime = \"1\""),
            "connection_lifetime: not a value this key takes (it is not shown); expected a duration such as",
        ),
        (
            format!("{listen}connection_lifetime = 3600"),
            "connection_lifetime",
        ),
        (format!("{listen}bogus = 1"), "bogus"),
        (format!("{listen}tls_key = \"/k\""), "oci.tls_key: a secret"),
        ("listen = \"nowhere\"".to_owned(), "listen"),
    ];
    for (oci, want) in cases {
        let why = refusal(&minimal_with_oci(&oci));
        assert!(why.contains(want), "{oci}: {why}");
    }
}

/// Asserts that a refusal the start makes about an `--oci-*` flag names the key of the file.
#[test]
fn a_message_about_the_oci_flags_names_their_keys() {
    let message = "--oci-tls-cert and --oci-tls-key go together; --oci-listen, --oci-prefix, \
                   --oci-max-connections, --oci-connection-lifetime";
    assert_eq!(
        keys_for_flags(message),
        "oci.tls_cert_file and oci.tls_key_file go together; oci.listen, oci.prefix, \
         oci.max_connections, oci.connection_lifetime"
    );
}

/// Asserts that on a storage node a policy is read and checked and has no effect: the settings
/// with policies are the settings without; a prefix twice, a prefix inside another, a prefix that
/// does not end in `/`, one under `_journal/` or `_tiering/`, and one that asks for a cache are
/// refused.
#[test]
fn a_policy_is_checked_and_has_no_effect_on_a_storage_node() {
    let policies = "\n[[policy]]\nprefix = \"cas/b/\"\n\n[[policy]]\nprefix = \"cas/d/\"\ncache = false\n\n[[policy]]\nprefix = \"\"\n";
    let with = from_text(&format!("{GOLDEN}{policies}"), DEFAULTS).unwrap();
    assert_eq!(with, from_text(GOLDEN, DEFAULTS).unwrap());
    // An empty prefix (all the rest) with another after it, in either order, is no overlap.
    let rest_first =
        format!("{GOLDEN}\n[[policy]]\nprefix = \"\"\n\n[[policy]]\nprefix = \"a/\"\n");
    assert_eq!(
        from_text(&rest_first, DEFAULTS).unwrap(),
        from_text(GOLDEN, DEFAULTS).unwrap()
    );
    let refused = [
        (
            "prefix = \"a/\"\n\n[[policy]]\nprefix = \"a/\"",
            "also the prefix of policy[0]",
        ),
        (
            "prefix = \"a/\"\n\n[[policy]]\nprefix = \"a/b/\"",
            "overlaps",
        ),
        (
            "prefix = \"a/b/\"\n\n[[policy]]\nprefix = \"a/\"",
            "overlaps",
        ),
        (
            "prefix = \"\"\n\n[[policy]]\nprefix = \"\"",
            "also the prefix",
        ),
        ("prefix = \"cas\"", "does not end in /"),
        ("prefix = \"_journal/\"", "keeps for itself"),
        ("prefix = \"_tiering/selftest/\"", "keeps for itself"),
        ("prefix = \"a/\"\ncache = true", "policy[0].cache"),
    ];
    for (body, want) in refused {
        let why = refusal(&format!("{GOLDEN}\n[[policy]]\n{body}\n"));
        assert!(why.contains(want), "{body}: {why}");
    }
    let why = refusal(&format!(
        "{GOLDEN}\n[[policy]]\nprefix = \"a/\"\n\n[[policy]]\nprefix = \"_journal/\"\n"
    ));
    assert!(why.contains("policy[1].prefix"), "{why}");
}

/// Asserts that sizes and durations read as the flags and the docs say, and that a duration is
/// written back as a text that reads as the same duration.
#[test]
fn sizes_and_durations_are_read_and_written_back() {
    assert_eq!(parse_size("500GiB"), Ok(500 << 30));
    assert_eq!(parse_size("3B"), Ok(3));
    assert_eq!(parse_size("2KiB"), Ok(2048));
    assert_eq!(parse_size("1TiB"), Ok(1 << 40));
    assert!(parse_size("").is_err());
    let durations = [
        0, 1, 20, 999, 1000, 1500, 59_000, 60_000, 90_000, 3_600_000, 7_200_000, 86_400_000,
    ];
    for millis in durations {
        let duration = Duration::from_millis(millis);
        let text = format_duration(duration);
        assert_eq!(parse_duration(&text), Ok(duration), "{text}");
    }
    assert_eq!(format_duration(Duration::ZERO), "0s");
    assert_eq!(format_duration(Duration::from_secs(900)), "15m");
    assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
    assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
}

/// Asserts that the flags of the parser are, each one, either in the key table or a per-run flag,
/// once; that every entry of the table is a flag; and that no flag is in both. A flag added to
/// `Cli` without a key fails here.
#[test]
fn every_flag_has_one_key_or_is_a_per_run_knob() {
    let command = Cli::command();
    let flags: Vec<&str> = command
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .collect();
    let keyed: Vec<&str> = KEYS.iter().map(|(flag, _)| *flag).collect();
    let unique: BTreeSet<&str> = keyed.iter().copied().collect();
    assert_eq!(unique.len(), keyed.len(), "a flag twice in the key table");
    for flag in &flags {
        let keys = keyed.contains(flag);
        let per_run = PER_RUN.contains(flag);
        assert!(
            keys != per_run,
            "--{flag} must be in the key table or a per-run knob, once"
        );
    }
    for flag in keyed.iter().chain(PER_RUN) {
        assert!(flags.contains(flag), "--{flag} is not a flag");
    }
    for (flag, keys) in KEYS {
        assert!(!keys.is_empty(), "--{flag}");
        for key in *keys {
            let (table, name) = key.split_once('.').unwrap();
            assert!(
                ["server", "hot_store", "cold_store", "oci"].contains(&table),
                "{key}"
            );
            assert!(!name.is_empty(), "{key}");
            assert!(!name.contains('.'), "{key}");
        }
    }
}

/// Asserts that both READMEs say how to use the file and that the key table in the README of the
/// package lists every flag with every key it stands for, and the per-run flags as flags.
#[test]
fn the_readmes_document_every_flag_and_key() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let packaging =
        std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    let package = std::fs::read_to_string(root.join("README.md")).unwrap();
    for text in [&packaging, &package] {
        assert!(text.contains("--config"));
        assert!(text.contains("--print-config"));
    }
    for (flag, keys) in KEYS {
        let mut rows = packaging.lines().filter(|line| line.starts_with('|'));
        let row = rows.find(|line| line.contains(&format!("`--{flag}`")));
        assert!(row.is_some(), "no row for --{flag}");
        let row = row.unwrap();
        for key in *keys {
            assert!(
                row.contains(&format!("`{key}`")),
                "--{flag}: {key} is not in its row: {row}"
            );
        }
    }
    for flag in PER_RUN {
        assert!(packaging.contains(&format!("`--{flag}`")), "--{flag}");
    }
}

/// Asserts that a refusal the flags' checks make, in a start from a file, names the keys: the
/// longer flag name is replaced before a shorter one it begins with.
#[test]
fn a_message_about_flags_names_keys_for_a_file() {
    let flags = "--low-water must be below --high-water";
    assert_eq!(
        keys_for_flags(flags),
        "hot_store.low_water must be below hot_store.high_water"
    );
    let message = "--hot-credentials-file: --hot \"s3:b\" needs --hot-endpoint, not --hot-bytes";
    let keys = "hot_store.credentials_file: hot_store.kind \"s3:b\" needs hot_store.endpoint, not hot_store.budget";
    assert_eq!(keys_for_flags(message), keys);
    assert_eq!(
        keys_for_flags("opening the proxy: no flags here"),
        "opening the proxy: no flags here"
    );
}

/// Asserts that the example file in the README of the packaging is a file that loads: its first
/// TOML block makes settings, with the budget `1TiB` the unit's `--hot-bytes` is.
#[test]
fn the_example_in_the_readme_loads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    let (_, after) = readme.split_once("```toml\n").expect("a TOML block");
    let (example, _) = after.split_once("\n```").expect("the end of the block");
    let cli = from_text(example, DEFAULTS).unwrap();
    assert_eq!(cli.hot_bytes, 1 << 40);
    assert_eq!(cli.hot, "s3:tierstore-hot");
    assert_eq!(cli.cold, "s3:tierstore-cold");
    assert_eq!(cli.read_only_key_file.len(), 1);
}
