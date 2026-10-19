//! Tests of the `kbf.toml` settings flags (B7): the file `setup` writes for each mode against
//! a golden, that every key is a real `kbf` flag in its real table, the rerun rules (the Keep
//! and Refuse contract covers the new keys), a refusal for each invalid input, and the rule
//! that no value of a flag appears in any message. The helpers are `tests.rs`'s. Each test says
//! the defect it catches; the mutants planted to see them go red are in the pull request.

use super::*;
use crate::setup::kbf_settings::{self, KbfFlags};
use kbf_host::testkit::ScratchHost;

const FRONTEND_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/frontend.toml");
const ALL_IN_ONE_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one.toml");
const DAEMON_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/daemon.toml");
const STORAGE_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/storage.toml");
const STORAGE_AND_DAEMON_GOLDEN: &str =
    include_str!("../../tests/golden/kbf_toml/storage-and-daemon.toml");

/// The goldens of `kbf`'s own flags, one per mode: the file the settings must be valid in.
const KBF_FRONTEND: &str = include_str!("../../../kbf-cli/tests/golden/kbf-frontend.toml");
const KBF_ALL_IN_ONE: &str = include_str!("../../../kbf-cli/tests/golden/kbf-all-in-one.toml");
const KBF_STORAGE: &str = include_str!("../../../kbf-cli/tests/golden/kbf-storage.toml");
const KBF_DAEMON: &str = include_str!("../../../kbf-cli/tests/golden/kbf-daemon.toml");
const KBF_STORAGE_AND_DAEMON: &str =
    include_str!("../../../kbf-cli/tests/golden/kbf-storage-and-daemon.toml");

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// Every flag a frontend takes, with an https storage node.
fn frontend_flags() -> KbfFlags {
    KbfFlags {
        listen: some("0.0.0.0:8980"),
        worker_listen: some("0.0.0.0:8981"),
        api_listen: some("127.0.0.1:8982"),
        reapi_tls_cert: some("/etc/kbf/reapi.pem"),
        reapi_tls_key: some("/etc/kbf/reapi.key"),
        worker_tls_cert: some("/etc/kbf/worker.pem"),
        worker_tls_key: some("/etc/kbf/worker.key"),
        worker_client_ca: some("/etc/kbf/ca.pem"),
        storage_url: some("https://storage.example:8983"),
        storage_ca: some("/etc/kbf/ca.pem"),
        storage_client_cert: some("/etc/kbf/worker.pem"),
        storage_client_key: some("/etc/kbf/worker.key"),
        ..KbfFlags::default()
    }
}

/// Every flag an all-in-one host takes: the listeners and certificates of a frontend, no
/// storage client (its store is its own), and the cold store and the daemon half.
fn all_in_one_flags() -> KbfFlags {
    KbfFlags {
        storage_url: None,
        storage_ca: None,
        storage_client_cert: None,
        storage_client_key: None,
        cold_store: some("file:///var/lib/kbf/cold"),
        meta_dir: some("/var/lib/kbf-meta"),
        daemon_server: some("https://farm.example:8981"),
        daemon_ca_cert: some("/etc/kbf/ca.pem"),
        daemon_cert: some("/etc/kbf/node.pem"),
        daemon_key: some("/etc/kbf/node.key"),
        node_id: some("farm-1"),
        ..frontend_flags()
    }
}

/// The files the flags name, for the user the mode's unit runs as: keys 0600, certificates
/// 0644 (the daemon's pair is `tests.rs`'s `host()`).
fn with_files(mut exec: FakeExec, service: Owner) -> FakeExec {
    for key in ["reapi.key", "worker.key", "node.key"] {
        put(&mut exec, &format!("/etc/kbf/{key}"), 0o600, service, "key");
    }
    for cert in ["reapi.pem", "worker.pem", "ca.pem", "node.pem"] {
        put(&mut exec, &format!("/etc/kbf/{cert}"), 0o644, ROOT, "cert");
    }
    exec
}

fn frontend() -> Raw {
    Raw {
        kbf: frontend_flags(),
        ..raw(Mode::Frontend)
    }
}

fn all_in_one() -> Raw {
    Raw {
        kbf: all_in_one_flags(),
        ..raw(Mode::AllInOne)
    }
}

fn service_of(mode: Mode) -> Owner {
    if mode == Mode::Frontend {
        KBF_SERVER
    } else {
        KBF
    }
}

fn host_for(raw: &Raw) -> FakeExec {
    released(with_files(host(), service_of(raw.mode)), Platform::Linux)
}

/// Catches a flag that is not written, written under the wrong key or table, or a mode that
/// gets another mode's file: the whole `kbf.toml` of each of the five modes, byte for byte.
/// The frontend and all-in-one files are complete (they start with these keys and the mode's
/// own defaults), and the node modes' are complete without listeners or certificates, which
/// enrollment supplies.
#[test]
fn each_mode_gets_its_complete_kbf_toml() {
    let cases = [
        (frontend(), FRONTEND_GOLDEN),
        (all_in_one(), ALL_IN_ONE_GOLDEN),
        (raw(Mode::Daemon), DAEMON_GOLDEN),
        (raw(Mode::Storage), STORAGE_GOLDEN),
        (raw(Mode::StorageAndDaemon), STORAGE_AND_DAEMON_GOLDEN),
    ];
    for (raw, golden) in cases {
        let mut exec = host_for(&raw);
        go_ok(&FakeOs::linux(), &mut exec, &raw);
        assert_eq!(text(&exec, KBF_TOML), golden, "{:?}", raw.mode);
        let (mode, owner) = file_meta(&exec, KBF_TOML);
        assert_eq!(mode, 0o640, "{:?}", raw.mode);
        assert_eq!(owner.uid, 0);
    }
}

/// Catches the operator API address left out: with no `--api-listen` a serving host still
/// gets `kbf`'s documented default, since the token file `setup` writes requires a listener.
#[test]
fn the_api_listener_has_its_documented_default() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let mut raw = if mode == Mode::Frontend {
            frontend()
        } else {
            all_in_one()
        };
        raw.kbf.api_listen = None;
        let mut exec = host_for(&raw);
        go_ok(&FakeOs::linux(), &mut exec, &raw);
        assert!(
            text(&exec, KBF_TOML).contains("api-listen = \"127.0.0.1:8982\""),
            "{mode:?}"
        );
    }
}

/// The keys of a `kbf` golden as `(table, key)`: a key is a line `key = ...` or its commented
/// default `# key = ...` under a `[table]`.
fn keys_of(golden: &str) -> Vec<(String, String)> {
    let mut table = String::new();
    let mut out = Vec::new();
    for line in golden.lines() {
        if let Some(t) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = t.to_owned();
            continue;
        }
        let body = line.strip_prefix("# ").unwrap_or(line);
        if let Some((key, _)) = body.split_once(" = ")
            && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        {
            out.push((table.clone(), key.to_owned()));
        }
    }
    out
}

/// The `[table] key` of `ours` (a file `setup` wrote) that `kbf` (its golden for the mode)
/// does not have, leaving out the enrollment's own keys.
fn strangers(ours: &str, kbf: &str) -> Vec<String> {
    let theirs = keys_of(kbf);
    let parsed: toml::Table = ours.parse().expect("toml");
    let mut out = Vec::new();
    for (table, inner) in &parsed {
        let Some(inner) = inner.as_table() else {
            continue;
        };
        for key in inner.keys() {
            let enrollment = matches!(
                (table.as_str(), key.as_str()),
                ("frontend", "url") | ("enroll", "token_file")
            );
            if !enrollment && !theirs.iter().any(|(t, k)| t == table && k == key) {
                out.push(format!("[{table}] {key}"));
            }
        }
    }
    out
}

/// Catches a flag that `kbf` does not have for the mode, or sits in another table than its
/// group: for every mode each key the settings write is a key of `kbf`'s own golden for that
/// mode, and so is each key the written file holds. A renamed or moved `kbf` flag fails here.
#[test]
fn the_keys_are_kbf_s_flags_in_kbf_s_tables_for_the_modes_that_write_them() {
    let modes = [
        (Mode::Frontend, KBF_FRONTEND, FRONTEND_GOLDEN),
        (Mode::AllInOne, KBF_ALL_IN_ONE, ALL_IN_ONE_GOLDEN),
        (Mode::Storage, KBF_STORAGE, STORAGE_GOLDEN),
        (Mode::Daemon, KBF_DAEMON, DAEMON_GOLDEN),
        (
            Mode::StorageAndDaemon,
            KBF_STORAGE_AND_DAEMON,
            STORAGE_AND_DAEMON_GOLDEN,
        ),
    ];
    for (mode, kbf, ours) in modes {
        let theirs = keys_of(kbf);
        for (table, key) in kbf_settings::written(mode) {
            assert!(
                theirs.iter().any(|(t, k)| t == table && k == key),
                "{mode:?}: kbf has no [{table}] {key}"
            );
        }
        assert_eq!(strangers(ours, kbf), Vec::<String>::new(), "{mode:?}");
    }
    assert_eq!(
        kbf_settings::written(Mode::Daemon),
        Vec::<(&str, &str)>::new(),
        "a node's listeners and certificates come from enrollment"
    );
}

/// Catches the guard above passing for everything: a file with a renamed key, a key moved to
/// another table and a key of another mode's golden each fail it, naming the key.
#[test]
fn a_planted_key_that_kbf_does_not_have_fails_the_guard() {
    let renamed = FRONTEND_GOLDEN.replace("api-listen", "api-listn");
    assert_eq!(strangers(&renamed, KBF_FRONTEND), ["[listener] api-listn"]);
    let moved = FRONTEND_GOLDEN
        .replace("storage-ca = ", "listen-ca = ")
        .replace("[storage-client]", "[storage-client]\nstorage-ca = \"/x\"");
    assert_eq!(
        strangers(&moved, KBF_FRONTEND),
        ["[storage-client] listen-ca"]
    );
    // A cold store is the all-in-one's: kbf's frontend golden has no such key in `[stores]`
    // for setup to write, and the check says so for a file that claims it.
    let wrong = format!("{FRONTEND_GOLDEN}\n[nonesuch]\nx = \"y\"\n");
    assert_eq!(strangers(&wrong, KBF_FRONTEND), ["[nonesuch] x"]);
}

/// Catches a rerun that is not a no-op, or a different run that is: the same flags again plan
/// nothing and write no byte, and a changed flag rewrites the file in one atomic write, names
/// the key (never the value), and keeps every other key.
#[test]
fn the_same_flags_again_change_nothing_and_a_changed_flag_names_its_key() {
    let raw = frontend();
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let before = text(&exec, KBF_TOML);
    exec.log.clear();
    let os = settled(&["kbf-frontend@a.service"]);
    let out = go_ok(&os, &mut exec, &raw);
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert!(out.contains(kbf_host::plan::NOTHING_TO_DO), "{out}");
    assert_eq!(text(&exec, KBF_TOML), before);

    // A file of setup's keys alone is setup's: a different flag rewrites it.
    let mut moved = raw.clone();
    moved.kbf.listen = some("0.0.0.0:9990");
    moved.kbf.storage_ca = some("/etc/kbf/worker.pem");
    let out = go_ok(&os, &mut exec, &moved);
    let writes = exec
        .log
        .iter()
        .filter(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == KBF_TOML))
        .count();
    assert_eq!(writes, 1, "{:?}", exec.log);
    assert!(
        out.contains("changed in these keys: [listener] listen, [storage-client] storage-ca;"),
        "{out}"
    );
    assert!(
        !out.contains("9990") && !out.contains("worker.pem"),
        "{out}"
    );
    assert_eq!(
        text(&exec, KBF_TOML),
        before.replace("0.0.0.0:8980", "0.0.0.0:9990").replace(
            "storage-ca = \"/etc/kbf/ca.pem\"",
            "storage-ca = \"/etc/kbf/worker.pem\""
        )
    );
}

/// `golden` with a key of the operator's own in `[listener]`.
fn with_operator_key(golden: &str) -> String {
    golden.replace("[listener]\n", "[listener]\nshutdown-timeout-secs = 30\n")
}

/// Catches the Keep and Refuse contract not covering the new keys: a file that holds the
/// operator's own settings is left byte for byte when every key the flags give matches
/// (exit 0, said in the result), and refused (exit 5) naming the key, never its value, when one
/// differs, for each kind of new key: an address, a certificate path, the storage URL, a
/// daemon-half identity key, the cold store.
#[test]
fn a_file_with_the_operators_settings_is_kept_when_the_new_keys_match_and_refused_when_not() {
    let flags = [
        (frontend(), FRONTEND_GOLDEN),
        (all_in_one(), ALL_IN_ONE_GOLDEN),
    ];
    for (raw, golden) in flags {
        let theirs = with_operator_key(golden);
        let mut exec = host_for(&raw);
        put(&mut exec, KBF_TOML, 0o640, ROOT, &theirs);
        let out = go_ok(&FakeOs::linux(), &mut exec, &raw);
        assert_eq!(text(&exec, KBF_TOML), theirs, "{:?}", raw.mode);
        assert!(out.contains("other settings"), "{out}");
    }

    let differing: [(Mode, &str, &str, &str); 7] = [
        (
            Mode::Frontend,
            "0.0.0.0:8980",
            "0.0.0.0:7777",
            "[listener] listen",
        ),
        (
            Mode::Frontend,
            "/etc/kbf/reapi.pem",
            "/etc/kbf/other.pem",
            "[tls] reapi-tls-cert",
        ),
        (
            Mode::Frontend,
            "https://storage.example:8983",
            "https://other.example:8983",
            "[storage-client] storage",
        ),
        (
            Mode::AllInOne,
            "file:///var/lib/kbf/cold",
            "file:///srv/cold",
            "[stores] cold-store",
        ),
        (Mode::AllInOne, "farm-1", "farm-2", "[identity] node-id"),
        (
            Mode::AllInOne,
            "/etc/kbf/node.pem",
            "/etc/kbf/other.pem",
            "[identity] cert",
        ),
        (
            Mode::AllInOne,
            "api-listen = \"127.0.0.1:8982\"",
            "api-listen = \"127.0.0.1:9\"",
            "[listener] api-listen",
        ),
    ];
    for (mode, from, to, key) in differing {
        let (raw, golden) = if mode == Mode::Frontend {
            (frontend(), FRONTEND_GOLDEN)
        } else {
            (all_in_one(), ALL_IN_ONE_GOLDEN)
        };
        let theirs = with_operator_key(golden).replace(from, to);
        assert_ne!(
            theirs,
            with_operator_key(golden),
            "{key}: the fixture has the text"
        );
        let mut exec = host_for(&raw);
        put(&mut exec, KBF_TOML, 0o640, ROOT, &theirs);
        let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
        assert_eq!(err.exit(), Exit::NeedsOperator, "{key}");
        let shown = err.to_string();
        assert!(shown.contains(key), "{key}: {shown}");
        assert!(
            !shown.contains(to) || key == "[identity] node-id",
            "{key}: {shown}"
        );
        assert_eq!(
            text(&exec, KBF_TOML),
            theirs,
            "{key}: the file is untouched"
        );
        assert!(write_index(&exec, KBF_TOML).is_none(), "{key}");
    }
}

/// Catches `setup` rewriting a file with a listener it has no flag for in this run: a file of
/// setup's keys that also holds `listen` (from an earlier run with `--listen`) is the
/// operator's when this run gives no `--listen`, so it is kept as it is, `listen` included, and
/// a differing key still refuses instead of dropping the listener.
#[test]
fn a_key_this_run_has_no_flag_for_is_not_dropped_by_a_rewrite() {
    let mut raw = frontend();
    raw.kbf.listen = None;
    let mut exec = host_for(&raw);
    put(&mut exec, KBF_TOML, 0o640, ROOT, FRONTEND_GOLDEN);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_GOLDEN);

    let mut other = raw.clone();
    other.database = Some("postgres:///other?host=/run/postgresql".to_owned());
    let err = go(&FakeOs::linux(), &mut exec, &other, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("[scheduler] state-store"), "{err}");
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_GOLDEN);
}

/// A host for a refusal: `flags` replace the good ones, and nothing may be written.
fn refused(mode: Mode, flags: KbfFlags) -> (SetupError, FakeExec) {
    let raw = Raw {
        kbf: flags,
        ..raw(mode)
    };
    let mut exec = host_for(&raw);
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    (err, exec)
}

fn assert_clean(err: &SetupError, exec: &FakeExec, flag: &str, value: &str) {
    let shown = format!("{err} {err:?}");
    assert!(shown.contains(&format!("--{flag}")), "{flag}: {shown}");
    for secret in [value, "SECRET", "198.51.100.7"] {
        assert!(!shown.contains(secret), "{flag}: {shown}");
    }
    assert_eq!(err.exit(), Exit::Usage, "{flag}");
    assert!(
        exec.log.is_empty() && exec.queries.borrow().is_empty(),
        "{flag}"
    );
}

type Mutate = fn(&mut KbfFlags, &str);

fn base(mode: Mode) -> KbfFlags {
    match mode {
        Mode::Frontend => frontend_flags(),
        Mode::AllInOne => all_in_one_flags(),
        _ => KbfFlags::default(),
    }
}

/// Catches an invalid flag value getting through, or a message that repeats what the person
/// typed: one refusal per rule, each naming the flag and none quoting the value, with the host
/// untouched (exit 2).
#[test]
fn every_invalid_setting_is_refused_by_flag_and_without_its_value() {
    let cases: [(Mode, &str, Mutate, &str); 31] = [
        (
            Mode::Frontend,
            "listen",
            |k, v| k.listen = some(v),
            "SECRET-addr:80",
        ),
        (
            Mode::Frontend,
            "listen",
            |k, v| k.listen = some(v),
            "0.0.0.0:0",
        ),
        (
            Mode::Frontend,
            "worker-listen",
            |k, v| k.worker_listen = some(v),
            "198.51.100.7:99999",
        ),
        (
            Mode::Frontend,
            "api-listen",
            |k, v| k.api_listen = some(v),
            "198.51.100.7",
        ),
        (
            Mode::Frontend,
            "reapi-tls-key",
            |k, v| k.reapi_tls_key = some(v),
            "-----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----",
        ),
        (
            Mode::Frontend,
            "reapi-tls-cert",
            |k, v| k.reapi_tls_cert = some(v),
            "relative/SECRET.pem",
        ),
        (
            Mode::Frontend,
            "worker-tls-key",
            |k, v| k.worker_tls_key = some(v),
            "/etc/kbf/with space SECRET.key",
        ),
        (
            Mode::Frontend,
            "worker-client-ca",
            |k, v| k.worker_client_ca = some(v),
            "/etc/kbf/../SECRET.pem",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://user:SECRET@storage.example:8983",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "ftp://SECRET.example",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://storage.example:8983/SECRET",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://storage.example:0",
        ),
        (
            Mode::Frontend,
            "storage-client-key",
            |k, v| k.storage_client_key = some(v),
            "SECRET-----BEGIN",
        ),
        (
            Mode::Daemon,
            "listen",
            |k, v| k.listen = some(v),
            "0.0.0.0:SECRET",
        ),
        (
            Mode::Storage,
            "reapi-tls-key",
            |k, v| k.reapi_tls_key = some(v),
            "/etc/kbf/SECRET.key",
        ),
        (
            Mode::StorageAndDaemon,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "http://127.0.0.1:1/SECRET",
        ),
        (
            Mode::AllInOne,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "http://127.0.0.1:8990",
        ),
        (
            Mode::AllInOne,
            "storage-client-key",
            |k, v| k.storage_client_key = some(v),
            "/etc/kbf/SECRET.key",
        ),
        (
            Mode::Frontend,
            "cold-store",
            |k, v| k.cold_store = some(v),
            "file:///var/lib/SECRET",
        ),
        (
            Mode::Frontend,
            "node-id",
            |k, v| k.node_id = some(v),
            "SECRET",
        ),
        (
            Mode::AllInOne,
            "cold-store",
            |k, v| k.cold_store = some(v),
            "s3://SECRET-bucket/prefix",
        ),
        (
            Mode::AllInOne,
            "cold-store",
            |k, v| k.cold_store = some(v),
            "file://SECRET/relative",
        ),
        (
            Mode::AllInOne,
            "cold-store",
            |k, v| k.cold_store = some(v),
            "/var/lib/SECRET",
        ),
        (
            Mode::AllInOne,
            "meta-dir",
            |k, v| k.meta_dir = some(v),
            "SECRET/relative",
        ),
        (
            Mode::AllInOne,
            "daemon-server",
            |k, v| k.daemon_server = some(v),
            "http://farm.example:8981/SECRET",
        ),
        (
            Mode::AllInOne,
            "daemon-server",
            |k, v| k.daemon_server = some(v),
            "https://u:SECRET@farm.example:8981",
        ),
        (
            Mode::AllInOne,
            "daemon-key",
            |k, v| k.daemon_key = some(v),
            "-----BEGIN-----SECRET",
        ),
        (
            Mode::AllInOne,
            "daemon-cert",
            |k, v| k.daemon_cert = some(v),
            "SECRET.pem",
        ),
        (
            Mode::AllInOne,
            "node-id",
            |k, v| k.node_id = some(v),
            "SECRET_Id",
        ),
        (
            Mode::AllInOne,
            "node-id",
            |k, v| k.node_id = some(v),
            "-secret",
        ),
        (
            Mode::AllInOne,
            "daemon-ca-cert",
            |k, v| k.daemon_ca_cert = some(v),
            "/etc/kbf/\"SECRET\".pem",
        ),
    ];
    for (mode, flag, set, value) in cases {
        let mut flags = base(mode);
        set(&mut flags, value);
        let (err, exec) = refused(mode, flags);
        assert_clean(&err, &exec, flag, value);
    }
}

/// Catches plain text to a remote storage node being accepted at setup when `kbf` refuses it
/// at start: `http://` is for a loopback host (an IP of 127/8, `::1`, or `localhost`), `https://`
/// is for any host.
#[test]
fn plain_text_storage_is_for_loopback_only() {
    for url in [
        "http://127.0.0.1:8990",
        "http://localhost:8990",
        "http://[::1]:8990",
    ] {
        let raw = Raw {
            kbf: KbfFlags {
                storage_url: some(url),
                ..KbfFlags::default()
            },
            ..raw(Mode::Frontend)
        };
        let mut exec = host_for(&raw);
        go_ok(&FakeOs::linux(), &mut exec, &raw);
    }
    for url in ["http://storage.example:8990", "http://192.0.2.7:8990"] {
        let (err, _) = refused(
            Mode::Frontend,
            KbfFlags {
                storage_url: some(url),
                ..KbfFlags::default()
            },
        );
        assert!(err.to_string().contains("loopback host only"), "{err}");
        assert!(!err.to_string().contains("192.0.2.7"), "{err}");
    }
}

/// Catches a host that cannot start being set up: each setting the mode cannot start without
/// is required, named with the mode, and every missing one is said at once.
#[test]
fn what_a_mode_cannot_start_without_is_required() {
    type Clear = fn(&mut KbfFlags);
    let frontend: [(&str, Clear); 1] = [("storage-url", |k| k.storage_url = None)];
    let all: [(&str, Clear); 6] = [
        ("cold-store", |k| k.cold_store = None),
        ("meta-dir", |k| k.meta_dir = None),
        ("daemon-server", |k| k.daemon_server = None),
        ("daemon-ca-cert", |k| k.daemon_ca_cert = None),
        ("daemon-cert", |k| k.daemon_cert = None),
        ("daemon-key", |k| k.daemon_key = None),
    ];
    let mut cases: Vec<(Mode, &str, Clear)> = frontend
        .iter()
        .map(|(f, c)| (Mode::Frontend, *f, *c))
        .collect();
    cases.extend(all.iter().map(|(f, c)| (Mode::AllInOne, *f, *c)));
    cases.push((Mode::AllInOne, "node-id", |k| k.node_id = None));
    for (mode, flag, clear) in cases {
        let mut flags = base(mode);
        clear(&mut flags);
        let (err, exec) = refused(mode, flags);
        let want = format!("--{flag}: required for --mode=");
        assert!(err.to_string().contains(&want), "{flag}: {err}");
        assert_eq!(err.exit(), Exit::Usage);
        assert!(exec.log.is_empty(), "{flag}");
    }
    let (err, _) = refused(Mode::AllInOne, KbfFlags::default());
    let SetupError::Invalid(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.len(), 7, "{found:#?}");
}

/// Catches flags that only work together being accepted alone: a certificate without its
/// key, part of the worker trio, an https storage URL without the client trio, the trio with
/// plain text; each refusal names the flags and every problem is said at once.
#[test]
fn flags_that_come_together_are_refused_alone() {
    let cases: [(fn(&mut KbfFlags), &str); 5] = [
        (
            |k| k.reapi_tls_key = None,
            "--reapi-tls-cert, --reapi-tls-key: give all of them or none",
        ),
        (
            |k| k.worker_client_ca = None,
            "--worker-tls-cert, --worker-tls-key, --worker-client-ca: give all of them or none",
        ),
        (
            |k| k.storage_ca = None,
            "--storage-url with https needs --storage-ca",
        ),
        (
            |k| k.storage_url = some("http://127.0.0.1:8983"),
            "are for an https --storage-url",
        ),
        (
            |k| {
                k.reapi_tls_cert = None;
                k.worker_tls_key = None
            },
            "--reapi-tls-cert, --reapi-tls-key: give all of them or none",
        ),
    ];
    for (mutate, expect) in cases {
        let mut flags = frontend_flags();
        mutate(&mut flags);
        let (err, exec) = refused(Mode::Frontend, flags);
        assert!(err.to_string().contains(expect), "{expect}: {err}");
        assert_eq!(err.exit(), Exit::Usage);
        assert!(exec.log.is_empty());
    }
    let mut flags = frontend_flags();
    flags.reapi_tls_key = None;
    flags.worker_tls_key = None;
    let (err, _) = refused(Mode::Frontend, flags);
    let SetupError::Invalid(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.len(), 2, "{found:#?}");
}

/// Catches a file flag accepted without looking at the file: each file that is missing, a
/// link, a directory, a key with the wrong mode or owner, or a certificate others can write
/// is refused with the flag and the rule, the path never in the message, and nothing written
/// (the files are looked at before the first step). A key with an owner-execute bit is
/// refused as `kbf` refuses it (any group or other bit, or no owner read).
#[test]
fn a_file_that_is_not_what_its_kind_needs_is_refused_before_anything_is_written() {
    let link = || Entry::Link {
        target: "/etc/kbf/real.key".into(),
    };
    let file = |perms: u32, owner: Owner| Entry::File {
        perms: Perms::new(perms),
        owner,
        bytes: b"x".to_vec(),
    };
    let dir = || Entry::Dir {
        perms: Perms::new(0o755),
        owner: ROOT,
    };
    let cases: [(&str, Option<Entry>, &str); 10] = [
        ("/etc/kbf/reapi.key", None, "no such file"),
        (
            "/etc/kbf/reapi.key",
            Some(link()),
            "not a regular file (a symbolic link is not followed)",
        ),
        ("/etc/kbf/reapi.key", Some(dir()), "not a regular file"),
        (
            "/etc/kbf/reapi.key",
            Some(file(0o640, KBF_SERVER)),
            "mode 0600 or 0400",
        ),
        (
            "/etc/kbf/reapi.key",
            Some(file(0o600, ROOT)),
            "owned by the user the service runs as",
        ),
        (
            "/etc/kbf/worker.key",
            Some(file(0o200, KBF_SERVER)),
            "mode 0600 or 0400",
        ),
        (
            "/etc/kbf/reapi.pem",
            Some(file(0o664, ROOT)),
            "writable by its group or by others",
        ),
        (
            "/etc/kbf/ca.pem",
            Some(file(
                0o644,
                Owner {
                    uid: 4242,
                    gid: 4242,
                },
            )),
            "owned by root or the user",
        ),
        ("/etc/kbf/ca.pem", Some(link()), "not a regular file"),
        ("/etc/kbf/worker.pem", None, "no such file"),
    ];
    for (path, entry, rule) in cases {
        let raw = frontend();
        let mut exec = host_for(&raw);
        match entry {
            Some(e) => drop(exec.entries.insert(hp(path), e)),
            None => drop(exec.entries.remove(&hp(path))),
        }
        let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
        let shown = err.to_string();
        assert!(shown.contains(rule), "{path}: {shown}");
        assert!(!shown.contains("/etc/kbf"), "{path}: {shown}");
        assert_eq!(err.exit(), Exit::Usage);
        assert!(exec.log.is_empty(), "{path}: {:?}", exec.log);
    }
    // 0700 has an owner-execute bit and no group or other bit: `kbf` accepts it, so setup does.
    let raw = frontend();
    let mut exec = host_for(&raw);
    put(&mut exec, "/etc/kbf/reapi.key", 0o700, KBF_SERVER, "key");
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    // The all-in-one daemon half's files get the same look.
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    put(&mut exec, "/etc/kbf/node.key", 0o644, KBF, "key");
    exec.entries.remove(&hp("/etc/kbf/node.pem"));
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    let shown = err.to_string();
    assert!(
        shown.contains("--daemon-key") && shown.contains("--daemon-cert"),
        "{shown}"
    );
}

/// Catches a key readable by its service user's group or a key owned by the wrong user on an
/// all-in-one host, which runs as `kbf`, not `kbf-server`: the owner a key needs is the
/// mode's service user.
#[test]
fn a_key_must_belong_to_the_user_the_modes_unit_runs_as() {
    let raw = all_in_one();
    let mut exec = released(with_files(host(), KBF_SERVER), Platform::Linux);
    put(&mut exec, "/etc/kbf/node.key", 0o600, KBF_SERVER, "key");
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    let shown = err.to_string();
    for flag in ["--reapi-tls-key", "--worker-tls-key", "--daemon-key"] {
        assert!(shown.contains(flag), "{flag}: {shown}");
    }
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
}

/// Catches a dry run that writes: without `--apply` the plan names the `kbf.toml` write, the
/// files are still checked, and nothing changes.
#[test]
fn a_dry_run_checks_the_files_and_writes_nothing() {
    let raw = frontend();
    let mut exec = host_for(&raw);
    let out = go(
        &FakeOs::linux(),
        &mut exec,
        &raw,
        Options {
            apply: false,
            no_start: false,
        },
    )
    .expect("dry run");
    assert!(out.contains("DRY-RUN") && out.contains(KBF_TOML), "{out}");
    assert!(exec.log.is_empty());
    exec.entries.remove(&hp("/etc/kbf/ca.pem"));
    let err = go(
        &FakeOs::linux(),
        &mut exec,
        &raw,
        Options {
            apply: false,
            no_start: false,
        },
    )
    .expect_err("refused");
    assert!(err.to_string().contains("--worker-client-ca"), "{err}");
}

/// Catches the file check following a link on the real file system: a key that is a symbolic
/// link to a good key is refused, a key of mode 0640 is refused, and a regular 0600 key owned
/// by the service user passes. (The fake host cannot say whether `stat` follows links.)
#[test]
fn on_a_real_file_system_a_link_is_not_followed_and_a_loose_key_is_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let host = ScratchHost::new("settings-links");
    std::fs::create_dir_all(host.at("/etc/kbf")).expect("dir");
    let real = host.at("/etc/kbf/real.key");
    std::fs::write(&real, "k").expect("key");
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("mode");
    symlink(&real, host.at("/etc/kbf/link.key")).expect("link");
    let loose = host.at("/etc/kbf/loose.key");
    std::fs::write(&loose, "k").expect("key");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o640)).expect("mode");
    let me = rustix::process::geteuid().as_raw();
    let setting = |path: &str| {
        [kbf_settings::Setting {
            table: "tls",
            key: "reapi-tls-key",
            value: path.to_owned(),
        }]
    };
    assert!(kbf_settings::check_files(&host, &setting("/etc/kbf/real.key"), Some(me)).is_ok());
    for path in ["/etc/kbf/link.key", "/etc/kbf/loose.key"] {
        let err = kbf_settings::check_files(&host, &setting(path), Some(me)).expect_err("refused");
        assert!(!err.to_string().contains("kbf/"), "{err}");
    }
}
