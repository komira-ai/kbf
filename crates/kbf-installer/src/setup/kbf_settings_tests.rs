//! Tests of the `kbf.toml` settings flags (B7): the file `setup` writes for each mode against
//! a golden, the rerun rules, a refusal for each invalid input, and the rule that no value of
//! a flag appears in any message. The helpers are `tests.rs`'s. Each test says the defect it
//! catches; the mutants planted to see them go red are in the pull request.

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

/// Every flag a frontend takes, the paths of a farm.
fn frontend_flags() -> KbfFlags {
    KbfFlags {
        listen: some("0.0.0.0:8980"),
        worker_listen: some("0.0.0.0:8981"),
        api_listen: some("127.0.0.1:8982"),
        admin_listen: some("127.0.0.1:8984"),
        reapi_tls_cert: some("/etc/kbf/reapi.pem"),
        reapi_tls_key: some("/etc/kbf/reapi.key"),
        worker_tls_cert: some("/etc/kbf/worker.pem"),
        worker_tls_key: some("/etc/kbf/worker.key"),
        worker_client_ca: some("/etc/kbf/ca.pem"),
        storage: some("https://storage.example:8983"),
        storage_ca: some("/etc/kbf/ca.pem"),
        storage_client_cert: some("/etc/kbf/worker.pem"),
        storage_client_key: some("/etc/kbf/worker.key"),
    }
}

/// Every flag an all-in-one host takes (no admin listener, a plain-text storage URL).
fn all_in_one_flags() -> KbfFlags {
    KbfFlags {
        admin_listen: None,
        storage: some("http://127.0.0.1:8990"),
        storage_ca: None,
        storage_client_cert: None,
        storage_client_key: None,
        ..frontend_flags()
    }
}

/// The files the flags name: keys 0600 and certificates 0644, owned as the service needs.
fn with_files(mut exec: FakeExec, service: Owner) -> FakeExec {
    for key in ["reapi.key", "worker.key"] {
        put(&mut exec, &format!("/etc/kbf/{key}"), 0o600, service, "key");
    }
    for cert in ["reapi.pem", "worker.pem", "ca.pem"] {
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

fn host_for(raw: &Raw) -> FakeExec {
    let service = if raw.mode == Mode::Frontend {
        KBF_SERVER
    } else {
        KBF
    };
    released(with_files(host(), service), Platform::Linux)
}

/// Catches a flag that is not written, written under the wrong key or table, or a mode that
/// gets another mode's file: the whole `kbf.toml` of each of the five modes, byte for byte.
/// The frontend and all-in-one files are the ones a farm wrote by hand, and the node modes'
/// are complete without listeners or certificates, which enrollment supplies.
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

/// Catches a flag that `kbf` does not have for the mode, or sits in another table than its
/// group: for every mode each key the settings can write is a key of `kbf`'s own golden for
/// that mode exactly when the mode takes the flag, and so is each key the written files hold.
/// A renamed or moved `kbf` flag fails here.
#[test]
fn the_keys_are_kbf_s_flags_in_kbf_s_tables_for_exactly_the_modes_that_take_them() {
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
        let taken = kbf_settings::taken(mode);
        for (table, key) in kbf_settings::keys() {
            let has = theirs.iter().any(|(t, k)| t == table && k == key);
            assert_eq!(has, taken.contains(&(table, key)), "{mode:?} {table} {key}");
        }
        let parsed: toml::Table = ours.parse().expect("toml");
        for (table, inner) in &parsed {
            let Some(inner) = inner.as_table() else {
                continue;
            };
            for key in inner.keys() {
                let enrollment = matches!((table.as_str(), key.as_str()), ("frontend", "url") | ("enroll", "token_file"));
                let has = theirs.iter().any(|(t, k)| t == table && k == key);
                assert!(has || enrollment, "{mode:?}: [{table}] {key} is no kbf flag");
            }
        }
    }
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
    assert!(!out.contains("9990") && !out.contains("worker.pem"), "{out}");
    let now = text(&exec, KBF_TOML);
    assert_eq!(
        now,
        before
            .replace("0.0.0.0:8980", "0.0.0.0:9990")
            .replace("storage-ca = \"/etc/kbf/ca.pem\"", "storage-ca = \"/etc/kbf/worker.pem\"")
    );
}

/// Catches setup losing a hand-written key, or a stale key outliving its mode: a flag left
/// out keeps the file's value, a key the mode takes no flag for is removed and named, and a
/// key setup does not own is untouched.
#[test]
fn a_key_left_out_stays_a_key_the_mode_does_not_take_goes_and_foreign_keys_stay() {
    let raw = Raw {
        kbf: KbfFlags {
            api_listen: some("127.0.0.1:8982"),
            ..KbfFlags::default()
        },
        ..raw(Mode::Frontend)
    };
    let mut exec = host_for(&raw);
    let theirs = "mode = \"daemon\"\n\n[frontend]\nurl = \"https://farm.example:8443\"\n\n[enroll]\ntoken_file = \"/etc/kbf/enroll.token\"\n\n[listener]\nlisten = \"0.0.0.0:7777\"\nshutdown-timeout-secs = 30\n";
    put(&mut exec, KBF_TOML, 0o640, ROOT, theirs);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw);
    let now = text(&exec, KBF_TOML);
    assert!(now.contains("listen = \"0.0.0.0:7777\""), "{now}");
    assert!(now.contains("shutdown-timeout-secs = 30"), "{now}");
    assert!(now.contains("api-listen = \"127.0.0.1:8982\""), "{now}");
    assert!(!now.contains("[frontend]") && !now.contains("[enroll]"), "{now}");
    for key in ["mode", "[frontend] url", "[enroll] token_file", "[listener] api-listen"] {
        assert!(out.contains(key), "{key}: {out}");
    }
    assert!(!out.contains("7777") && !out.contains("farm.example"), "{out}");
}

/// A host for a refusal: the flags in `bad` replace the good ones, and nothing may be written.
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
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty(), "{flag}");
}

/// Catches an invalid flag value getting through, or a message that repeats what the person
/// typed: one refusal per rule, each naming the flag and none quoting the value, with the host
/// untouched (exit 2).
#[test]
fn every_invalid_setting_is_refused_by_flag_and_without_its_value() {
    let f = frontend_flags;
    type Mutate = fn(&mut KbfFlags, &str);
    let cases: [(Mode, &str, Mutate, &str); 19] = [
        (Mode::Frontend, "listen", |k, v| k.listen = some(v), "SECRET-addr:80"),
        (Mode::Frontend, "worker-listen", |k, v| k.worker_listen = some(v), "198.51.100.7:99999"),
        (Mode::Frontend, "api-listen", |k, v| k.api_listen = some(v), "198.51.100.7"),
        (Mode::Frontend, "admin-listen", |k, v| k.admin_listen = some(v), "198.51.100.7:8984"),
        (Mode::Frontend, "reapi-tls-key", |k, v| k.reapi_tls_key = some(v), "-----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----"),
        (Mode::Frontend, "reapi-tls-cert", |k, v| k.reapi_tls_cert = some(v), "relative/SECRET.pem"),
        (Mode::Frontend, "worker-tls-key", |k, v| k.worker_tls_key = some(v), "/etc/kbf/with space SECRET.key"),
        (Mode::Frontend, "worker-client-ca", |k, v| k.worker_client_ca = some(v), "/etc/kbf/../SECRET.pem"),
        (Mode::Frontend, "storage", |k, v| k.storage = some(v), "https://user:SECRET@storage.example:8983"),
        (Mode::Frontend, "storage", |k, v| k.storage = some(v), "ftp://SECRET.example"),
        (Mode::Frontend, "storage", |k, v| k.storage = some(v), "https://storage.example:8983/SECRET"),
        (Mode::Frontend, "storage", |k, v| k.storage = some(v), "https://storage.example:0"),
        (Mode::Frontend, "storage-client-key", |k, v| k.storage_client_key = some(v), "SECRET-----BEGIN"),
        (Mode::Daemon, "listen", |k, v| k.listen = some(v), "0.0.0.0:SECRET"),
        (Mode::Storage, "reapi-tls-key", |k, v| k.reapi_tls_key = some(v), "/etc/kbf/SECRET.key"),
        (Mode::StorageAndDaemon, "storage", |k, v| k.storage = some(v), "http://SECRET.example"),
        (Mode::AllInOne, "admin-listen", |k, v| k.admin_listen = some(v), "127.0.0.1:SECRET"),
        (Mode::AllInOne, "storage-ca", |k, v| k.storage_ca = some(v), "/etc/kbf/SECRET.pem"),
        (Mode::AllInOne, "storage-client-key", |k, v| k.storage_client_key = some(v), "/etc/kbf/SECRET.key"),
    ];
    for (mode, flag, set, value) in cases {
        let mut flags = if mode == Mode::AllInOne { all_in_one_flags() } else if matches!(mode, Mode::Frontend) { f() } else { KbfFlags::default() };
        set(&mut flags, value);
        let (err, exec) = refused(mode, flags);
        assert_clean(&err, &exec, flag, value);
    }
}

/// Catches flags that only work together being accepted alone: a certificate without its
/// key, part of the worker trio, an https storage URL without the client trio, the trio
/// without a URL or with plain text; each refusal names the flags and every problem is said at
/// once.
#[test]
fn flags_that_come_together_are_refused_alone() {
    let cases: [(fn(&mut KbfFlags), &str); 6] = [
        (|k| k.reapi_tls_key = None, "--reapi-tls-cert, --reapi-tls-key: give all of them or none"),
        (|k| k.worker_client_ca = None, "--worker-tls-cert, --worker-tls-key, --worker-client-ca: give all of them or none"),
        (|k| k.storage_ca = None, "--storage with https needs --storage-ca"),
        (|k| k.storage = None, "need --storage"),
        (|k| k.storage = some("http://storage.example:8983"), "are for an https --storage"),
        (|k| { k.reapi_tls_cert = None; k.worker_tls_key = None }, "--reapi-tls-cert, --reapi-tls-key: give all of them or none"),
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
/// (the files are looked at before the first step).
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
        ("/etc/kbf/reapi.key", Some(link()), "not a regular file (a symbolic link is not followed)"),
        ("/etc/kbf/reapi.key", Some(dir()), "not a regular file"),
        ("/etc/kbf/reapi.key", Some(file(0o640, KBF_SERVER)), "mode 0600 or 0400"),
        ("/etc/kbf/reapi.key", Some(file(0o600, ROOT)), "owned by the user the service runs as"),
        ("/etc/kbf/worker.key", Some(file(0o200, KBF_SERVER)), "mode 0600 or 0400"),
        ("/etc/kbf/reapi.pem", Some(file(0o664, ROOT)), "writable by its group or by others"),
        ("/etc/kbf/ca.pem", Some(file(0o644, Owner { uid: 4242, gid: 4242 })), "owned by root or the user"),
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
}

/// Catches a key readable by its service user's group or a key owned by the wrong user on an
/// all-in-one host, which runs as `kbf`, not `kbf-server`: the owner a key needs is the
/// mode's service user.
#[test]
fn a_key_must_belong_to_the_user_the_modes_unit_runs_as() {
    let raw = all_in_one();
    let mut exec = released(with_files(host(), KBF_SERVER), Platform::Linux);
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    let shown = err.to_string();
    assert!(shown.contains("--reapi-tls-key") && shown.contains("--worker-tls-key"), "{shown}");
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
}

/// Catches a dry run that writes: without `--apply` the plan names the `kbf.toml` write, the
/// files are still checked, and nothing changes.
#[test]
fn a_dry_run_checks_the_files_and_writes_nothing() {
    let raw = frontend();
    let mut exec = host_for(&raw);
    let out = go(&FakeOs::linux(), &mut exec, &raw, Options { apply: false, no_start: false })
        .expect("dry run");
    assert!(out.contains("DRY-RUN") && out.contains(KBF_TOML), "{out}");
    assert!(exec.log.is_empty());
    exec.entries.remove(&hp("/etc/kbf/ca.pem"));
    let err = go(&FakeOs::linux(), &mut exec, &raw, Options { apply: false, no_start: false })
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
