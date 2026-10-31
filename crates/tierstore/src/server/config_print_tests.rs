//! `--print-config`: the file for the flags, and that it loads back to the same settings.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use clap::{CommandFactory, Parser};
use kbf_testkit::dir::TestDir;

use super::render;
use crate::server::cli::Cli;
use crate::server::config::{ConfigError, KEYS, PerRun, from_text};
use crate::server::config_testing::{MINIMAL_FLAGS, golden_flags, write_private};

/// The flags of an S3 storage node, every flag given at least once, the knobs of the run among
/// them, and `--print-config`.
fn s3_flags() -> Vec<&'static str> {
    let mut flags = golden_flags();
    flags.extend([
        "--drain-lb-wait=7s",
        "--drain-deadline=3m",
        "--print-config",
    ]);
    flags
}

/// The flags of a node whose tiers are directories.
fn dir_flags() -> Vec<&'static str> {
    vec![
        "tierstore",
        "--listen=127.0.0.1:0",
        "--proxy-id=p1",
        "--bucket=tier-bucket",
        "--access-key-file=/etc/tierstore/access.key",
        "--hot=dir:/var/lib/tierstore/hot",
        "--hot-bytes=1000",
        "--cold=dir:/var/lib/tierstore/cold",
        "--print-config",
    ]
}

/// The flags that give the least a start needs, and `--print-config`.
fn least_flags() -> Vec<&'static str> {
    let mut flags = MINIMAL_FLAGS.to_vec();
    flags.push("--print-config");
    flags
}

/// The settings the flags make, and the knobs of their run.
fn parse(flags: Vec<&str>) -> (Cli, PerRun) {
    let cli = Cli::try_parse_from(flags).unwrap();
    let per_run = PerRun {
        drain_lb_wait: cli.drain_lb_wait,
        drain_deadline: cli.drain_deadline,
    };
    (cli, per_run)
}

/// Asserts the round trip: the file printed for flags, loaded as `--config` loads it, makes
/// settings equal to the flags' (the flag `--print-config` itself aside), for flags of S3 tiers
/// with every flag given, flags of directory tiers, and flags that give the least (tiers in memory).
#[test]
fn the_printed_file_loads_to_the_settings_of_the_flags() {
    for flags in [s3_flags(), dir_flags(), least_flags()] {
        let (cli, per_run) = parse(flags);
        assert!(cli.print_config);
        let text = render(&cli).unwrap();
        assert!(
            text.starts_with("# The configuration file equivalent"),
            "{text}"
        );
        let loaded = from_text(&text, per_run).unwrap();
        let expected = Cli {
            print_config: false,
            ..cli
        };
        assert_eq!(loaded, expected);
    }
}

/// Asserts that the front's flags are written as an `[oci]` table and only then: every flag of it
/// gives its key, `--oci-listen` alone gives the table with `listen` only (a plain-HTTP front)
/// and loads back to the same settings, and flags without the front give no table.
#[test]
fn the_oci_flags_are_printed_as_the_oci_table() {
    let (cli, _) = parse(s3_flags());
    let table: toml::Table = toml::from_str(&render(&cli).unwrap()).unwrap();
    let oci = table["oci"].as_table().expect("an [oci] table");
    let keys: Vec<&str> = oci.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "connection_lifetime",
            "listen",
            "max_connections",
            "prefix",
            "tls_cert_file",
            "tls_key_file"
        ]
    );
    assert_eq!(oci["listen"].as_str(), Some("192.0.2.10:5000"));
    assert_eq!(
        oci["tls_cert_file"].as_str(),
        Some("/etc/tierstore/oci.crt")
    );
    assert_eq!(oci["connection_lifetime"].as_str(), Some("30m"));

    let mut flags = dir_flags();
    flags.push("--oci-listen=127.0.0.1:5000");
    let (cli, per_run) = parse(flags);
    let text = render(&cli).unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    let keys: Vec<&str> = table["oci"]
        .as_table()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["listen"]);
    let expected = Cli {
        print_config: false,
        ..cli
    };
    assert_eq!(from_text(&text, per_run).unwrap(), expected);

    let (cli, _) = parse(dir_flags());
    let table: toml::Table = toml::from_str(&render(&cli).unwrap()).unwrap();
    assert!(!table.contains_key("oci"));
}

/// Asserts that repeated `--listen` and `--oci-listen` are printed as lists, that the file loads
/// back to the settings of the flags, and that a flag given once is still printed as a string, so
/// that a file printed for one address is the file it always was.
#[test]
fn repeated_listen_flags_are_printed_as_lists_and_load_back() {
    let mut flags = dir_flags();
    flags.retain(|flag| !flag.starts_with("--listen="));
    flags.extend([
        "--listen=0.0.0.0:9300",
        "--listen=[::]:9300",
        "--oci-listen=0.0.0.0:5000",
        "--oci-listen=[::]:5000",
    ]);
    let (cli, per_run) = parse(flags);
    assert_eq!((cli.listen.len(), cli.oci_listen.len()), (2, 2));
    let text = render(&cli).unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    let count = |name: &str| table[name]["listen"].as_array().map(Vec::len);
    assert_eq!(count("server"), Some(2), "{text}");
    assert_eq!(count("oci"), Some(2), "{text}");
    let expected = Cli {
        print_config: false,
        ..cli
    };
    assert_eq!(from_text(&text, per_run).unwrap(), expected);

    let (cli, _) = parse(dir_flags());
    let table: toml::Table = toml::from_str(&render(&cli).unwrap()).unwrap();
    assert_eq!(table["server"]["listen"].as_str(), Some("127.0.0.1:0"));
}

/// Asserts that, across the sets of flags of these tests, every flag of the binary is given at
/// least once, so that the round trip tests all of them: a flag added without being added to a
/// set fails here.
#[test]
fn every_flag_is_given_in_one_of_the_round_trips() {
    let given: BTreeSet<String> = [s3_flags(), dir_flags(), least_flags()]
        .into_iter()
        .flatten()
        .filter_map(|word| word.strip_prefix("--"))
        .map(|word| word.split('=').next().unwrap().to_owned())
        .collect();
    let command = Cli::command();
    // `--config` is the file itself, and `--check-config` only checks one: a round trip never
    // gives either.
    for arg in command
        .get_arguments()
        .filter(|arg| !matches!(arg.get_long(), Some("config" | "check-config")))
    {
        let long = arg.get_long().unwrap();
        assert!(given.contains(long), "--{long} is in no set of flags");
    }
}

/// The `table.key` names of a printed file.
fn keys_of(text: &str) -> BTreeSet<String> {
    let table: toml::Table = toml::from_str(text).unwrap();
    let mut keys = BTreeSet::new();
    for (name, value) in &table {
        let inner = value.as_table().expect("a table");
        keys.extend(inner.keys().map(|key| format!("{name}.{key}")));
    }
    keys
}

/// Asserts that the keys the printer writes are exactly the keys of the key table: every key of
/// the table is written for some flags, and no key is written that the table does not name.
#[test]
fn the_printed_keys_are_the_keys_of_the_table() {
    let mut printed = BTreeSet::new();
    for flags in [s3_flags(), dir_flags()] {
        let (cli, _) = parse(flags);
        printed.extend(keys_of(&render(&cli).unwrap()));
    }
    let documented: BTreeSet<String> = KEYS
        .iter()
        .flat_map(|(_, keys)| keys.iter().map(|key| (*key).to_owned()))
        .collect();
    assert_eq!(printed, documented);
}

/// Asserts that the file names the key files by path and holds no key: the files are not read,
/// and what is in them is not in the output, and a path that is not there is printed as given.
#[test]
fn the_printed_file_holds_paths_and_never_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-print-config");
    let access = write_private(&dir, "access", "AKIDACCESS\nSECRETACCESS\n");
    let hot = write_private(&dir, "hot", "AKIDHOT\nSECRETHOT\n");
    let access_flag = format!("--access-key-file={}", access.display());
    let hot_flag = format!("--hot-credentials-file={}", hot.display());
    let mut flags: Vec<&str> = s3_flags();
    flags.retain(|flag| {
        !flag.starts_with("--access-key-file=") && !flag.starts_with("--hot-credentials-file=")
    });
    flags.push(&access_flag);
    flags.push(&hot_flag);
    let (cli, _) = parse(flags);
    let text = render(&cli).unwrap();
    assert!(text.contains(&access.display().to_string()), "{text}");
    assert!(text.contains(&hot.display().to_string()), "{text}");
    for secret in ["AKIDACCESS", "SECRETACCESS", "AKIDHOT", "SECRETHOT"] {
        assert!(!text.contains(secret), "{secret}");
    }
    assert!(text.contains("/etc/tierstore/cold.credentials"), "{text}");
}

/// Asserts that flags that no file can say are refused with a reason that names the flag or the
/// key: a tier that is none of the three kinds, the S3 flags of a tier that is not an S3 bucket,
/// an S3 tier with no endpoint, a path that is not text, and a number a TOML file cannot hold.
#[test]
fn flags_that_no_file_can_say_are_refused() {
    let with = |change: &dyn Fn(&mut Cli)| {
        let (mut cli, _) = parse(s3_flags());
        change(&mut cli);
        render(&cli).unwrap_err()
    };
    let why = with(&|cli| cli.hot = "tape".to_owned());
    assert!(matches!(&why, ConfigError::Invalid { .. }), "{why}");
    assert!(why.to_string().starts_with("--hot: "), "{why}");
    let why = with(&|cli| cli.cold = "/var/cold".to_owned());
    assert!(matches!(&why, ConfigError::Invalid { .. }), "{why}");
    assert!(why.to_string().starts_with("--cold: "), "{why}");
    let why = with(&|cli| cli.hot = "memory".to_owned());
    assert!(why.to_string().contains("hot_store.endpoint"), "{why}");
    let why = with(&|cli| cli.cold_endpoint = None);
    assert!(why.to_string().contains("cold_store.endpoint"), "{why}");
    let why = with(&|cli| cli.access_key_file = OsString::from_vec(vec![0xff, b'k']).into());
    assert!(matches!(why, ConfigError::Print(_)), "{why}");
    assert!(why.to_string().starts_with("--print-config: "), "{why}");
    let why = with(&|cli| cli.max_request_bytes = u64::MAX);
    assert!(
        matches!(why, ConfigError::Syntax { .. } | ConfigError::Print(_)),
        "{why}"
    );
}
