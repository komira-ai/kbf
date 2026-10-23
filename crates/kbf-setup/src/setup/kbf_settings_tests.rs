//! Tests of the `kbf.toml` settings flags (B7): the file `setup` writes for each mode against
//! a golden, that every key is a real `kbf` flag in its real table, the rerun rules (the Keep
//! and Refuse contract covers the new keys), a refusal for each invalid input, and the rule
//! that no value of a flag appears in any message. The helpers are `tests.rs`'s. Each test says
//! the defect it catches.
//!
//! An all-in-one host has two such files: the sibling's `kbf.toml` (`AIO_SIBLING_GOLDEN`, a
//! `storage-and-daemon` node) and the frontend children's `frontend.toml`
//! (`AIO_FRONTEND_GOLDEN`); its public listeners and certificates are the proxy's.

use super::*;
use crate::setup::kbf_settings::{self, KbfFlags};
use kbf_host::testkit::ScratchHost;

const FRONTEND_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/frontend.toml");
const AIO_SIBLING_GOLDEN: &str =
    include_str!("../../tests/golden/kbf_toml/all-in-one-sibling.toml");
const AIO_FRONTEND_GOLDEN: &str =
    include_str!("../../tests/golden/kbf_toml/all-in-one-frontend.toml");
const DAEMON_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/daemon.toml");
const STORAGE_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/storage.toml");
const STORAGE_AND_DAEMON_GOLDEN: &str =
    include_str!("../../tests/golden/kbf_toml/storage-and-daemon.toml");

/// The goldens of `kbf`'s own flags, one per mode: the file the settings must be valid in.
const KBF_FRONTEND: &str = include_str!("../../../kbf-cli/tests/golden/kbf-frontend.toml");
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

/// Every flag an all-in-one host takes: the proxy's certificate pair and client CA (the host is
/// always behind the proxy, which owns the listeners), no storage client (the sibling is its
/// store, and the address is derived), and the cold store and the daemon half.
fn all_in_one_flags() -> KbfFlags {
    KbfFlags {
        listen: None,
        worker_listen: None,
        api_listen: None,
        worker_tls_cert: None,
        worker_tls_key: None,
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

/// The files the flags name: keys 0600, certificates 0644. A frontend's keys and the proxy's are
/// `kbf-server`'s; the daemon half's key, the sibling's, is `kbf`'s.
fn with_files(mut exec: FakeExec) -> FakeExec {
    for (key, owner) in [
        ("reapi.key", KBF_SERVER),
        ("worker.key", KBF_SERVER),
        ("node.key", KBF),
    ] {
        put(&mut exec, &format!("/etc/kbf/{key}"), 0o600, owner, "key");
    }
    for cert in ["reapi.pem", "worker.pem", "ca.pem", "node.pem"] {
        put(&mut exec, &format!("/etc/kbf/{cert}"), 0o644, ROOT, "cert");
    }
    exec
}

pub(super) fn frontend() -> Raw {
    Raw {
        kbf: frontend_flags(),
        ..raw(Mode::Frontend)
    }
}

pub(super) fn all_in_one() -> Raw {
    Raw {
        kbf: all_in_one_flags(),
        ..raw(Mode::AllInOne)
    }
}

/// A host with a release and the files the flags of every mode name; each key belongs to the
/// user that reads it, whatever the mode.
pub(super) fn host_for(_: &Raw) -> FakeExec {
    released(with_files(host()), Platform::Linux)
}

/// Catches a flag that is not written, written under the wrong key or table, or a mode that
/// gets another mode's file: the whole `kbf.toml` of each of the five modes, byte for byte.
/// The frontend file is complete (it starts with these keys and the mode's own defaults), the
/// node modes' are complete without listeners or certificates, which enrollment supplies, and
/// an all-in-one host's `kbf.toml` is the sibling's: a `storage-and-daemon` node on loopback.
/// Each is root's, 0640, with the group of the user that reads it.
#[test]
fn each_mode_gets_its_complete_kbf_toml() {
    let cases = [
        (frontend(), FRONTEND_GOLDEN, KBF_SERVER.gid),
        (all_in_one(), AIO_SIBLING_GOLDEN, KBF.gid),
        (raw(Mode::Daemon), DAEMON_GOLDEN, KBF.gid),
        (raw(Mode::Storage), STORAGE_GOLDEN, KBF_SERVER.gid),
        (
            raw(Mode::StorageAndDaemon),
            STORAGE_AND_DAEMON_GOLDEN,
            KBF.gid,
        ),
    ];
    for (raw, golden, group) in cases {
        let mut exec = host_for(&raw);
        go_ok(&FakeOs::linux(), &mut exec, &raw);
        assert_eq!(text(&exec, KBF_TOML), golden, "{:?}", raw.mode);
        let (mode, owner) = file_meta(&exec, KBF_TOML);
        assert_eq!(mode, 0o640, "{:?}", raw.mode);
        assert_eq!((owner.uid, owner.gid), (0, group), "{:?}", raw.mode);
    }
}

/// Catches the operator API address left out of a frontend: with no `--api-listen` it still gets
/// `kbf`'s documented default, since the token file `setup` writes requires a listener. An
/// all-in-one host has none in either of its files: the proxy gives its children the address on
/// their command line, and the sibling has no operator API.
#[test]
fn the_api_listener_has_its_documented_default() {
    let mut raw = frontend();
    raw.kbf.api_listen = None;
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let file = text(&exec, KBF_TOML);
    assert!(file.contains("api-listen = \"127.0.0.1:8982\""), "{file}");

    let raw = all_in_one();
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    for file in [KBF_TOML, AIO_FRONTEND_TOML] {
        assert!(!text(&exec, file).contains("api-listen"), "{file}");
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

    // An all-in-one host has two files: the sibling's is a `storage-and-daemon` node's and the
    // children's a frontend's. The `[tls]` rows it takes are the proxy's, and `setup` moves them
    // into `installer.toml`; every other row is the sibling's.
    let sibling = keys_of(KBF_STORAGE_AND_DAEMON);
    let rows = kbf_settings::written(Mode::AllInOne);
    assert_eq!(rows.len(), 11, "{rows:?}");
    for (table, key) in rows.into_iter().filter(|row| row.0 != "tls") {
        assert!(
            sibling.contains(&(table.to_owned(), key.to_owned())),
            "the sibling's kbf has no [{table}] {key}"
        );
    }
    let all_keys = [
        (AIO_SIBLING_GOLDEN, KBF_STORAGE_AND_DAEMON),
        (AIO_FRONTEND_GOLDEN, KBF_FRONTEND),
    ];
    for (ours, kbf) in all_keys {
        assert_eq!(strangers(ours, kbf), Vec::<String>::new());
    }
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
    // A whole table kbf has no group for.
    let wrong = format!("{FRONTEND_GOLDEN}\n[nonesuch]\nx = \"y\"\n");
    assert_eq!(strangers(&wrong, KBF_FRONTEND), ["[nonesuch] x"]);
}

/// Catches a rerun that is not a no-op, or a different run that is: the same flags again plan
/// nothing and write no byte, and a changed flag rewrites the file in one write, says the file
/// changed (never a value), and keeps every other key.
#[test]
fn the_same_flags_again_change_nothing_and_a_changed_flag_rewrites_once() {
    let raw = frontend();
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let before = text(&exec, KBF_TOML);
    exec.log.clear();
    let os = settled(&["kbf-frontend.service"]);
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
        out.contains("kbf.toml changed: a running kbf keeps the old one"),
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
        (all_in_one(), AIO_SIBLING_GOLDEN),
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
            "storage-listen = \"127.0.0.1:8990\"",
            "storage-listen = \"127.0.0.1:9\"",
            "[listener] storage-listen",
        ),
    ];
    for (mode, from, to, key) in differing {
        let (raw, golden) = if mode == Mode::Frontend {
            (frontend(), FRONTEND_GOLDEN)
        } else {
            (all_in_one(), AIO_SIBLING_GOLDEN)
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
        assert_eq!(
            shown.contains("--api-listen"),
            key == "[listener] api-listen",
            "{key}: the way forward is named: {shown}"
        );
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
    other.database = Some("postgres:///other?host=/run/postgresql&user=kbf-server".to_owned());
    let err = go(&FakeOs::linux(), &mut exec, &other, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("[scheduler] state-store"), "{err}");
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_GOLDEN);
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
    let cases: [(&str, Option<Entry>, &str); 11] = [
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
            "/etc/kbf/reapi.pem",
            Some(file(0o646, ROOT)),
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
    // A certificate the service user owns passes as root's does.
    let raw = frontend();
    let mut exec = host_for(&raw);
    put(&mut exec, "/etc/kbf/ca.pem", 0o644, KBF_SERVER, "cert");
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

/// Catches a key owned by the wrong user on an all-in-one host, which has two accounts: the
/// proxy's key is read by `kbf-server` and the daemon half's by the sibling, which runs as `kbf`.
/// With the two owners swapped both keys are refused, naming their flags; with the right ones the
/// host is set up.
///
/// Catches: the proxy's files checked against the sibling's user (the service); the
/// daemon half's against the proxy's.
#[test]
fn a_key_must_belong_to_the_user_that_reads_it() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    put(&mut exec, "/etc/kbf/reapi.key", 0o600, KBF, "key");
    put(&mut exec, "/etc/kbf/node.key", 0o600, KBF_SERVER, "key");
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    let shown = err.to_string();
    for flag in ["--reapi-tls-key", "--daemon-key"] {
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

#[path = "kbf_settings_refuse_tests.rs"]
mod refuse;
