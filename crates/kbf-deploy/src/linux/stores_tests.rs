//! Rules `storage` and `state-store` read on their own: how a flag, a Postgres URI and
//! a variable are read, and what the node's environment files must be.

use std::path::Path;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

fn words(line: &str) -> Vec<String> {
    credential::words(line)
}

const KEY: &str = "/etc/kbf/s3.env";

/// The storage node's flags, as the template renders them.
const FULL: &str = "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000 \
    --meta-dir=/var/lib/kbf-server/raft --storage-listen=192.0.2.10:8983 \
    --storage-tls-cert=/etc/kbf/storage.pem --storage-tls-key=/etc/kbf/storage.key \
    --storage-client-ca=/etc/kbf/ca.pem";

fn storage_unit(flags: &str, env: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{env}ExecStart=/usr/local/bin/kbf storage {flags}\n"
    ))
}

/// Catches a flag read only in its `=` form (clap takes `--name value` too), the first
/// of two values read where kbf reads the last, and a longer flag (`--state-store-x`)
/// taken for the one asked about.
#[test]
fn a_flag_is_read_in_both_forms_and_the_last_wins() {
    let w = words("kbf storage --meta-dir /a --meta-dir=/b --meta-dir-x=/c");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    let w = words("kbf storage --meta-dir=/a \"--meta-dir\" '/b'");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    assert_eq!(flag(&words("kbf --state-store-x=/c"), "state-store"), None);
    assert_eq!(flag(&words("kbf --state-store"), "state-store"), None);
}

/// Catches each of the six flags dropped from a storage node's unit (one red per flag
/// planted), a `--cold-store` read only in its `=` form, an `s3://` cold store without
/// the key file passed, and a `file://` cold store asked for one.
#[test]
fn a_storage_unit_names_its_stores_listener_and_tls_and_its_key() {
    let env = format!("EnvironmentFile={KEY}\n");
    let key = Path::new(KEY);
    assert_eq!(
        storage(&storage_unit(FULL, &env), key),
        Vec::<String>::new()
    );
    for f in STORAGE_FLAGS {
        let dropped: Vec<&str> = FULL
            .split_whitespace()
            .filter(|w| !w.starts_with(&format!("--{f}=")))
            .collect();
        let got = storage(&storage_unit(&dropped.join(" "), &env), key);
        assert_eq!(got.len(), 1, "--{f}: {got:?}");
        assert!(got[0].starts_with(&format!("gives no --{f}:")), "{got:?}");
    }
    let empty = FULL.replace("--meta-dir=/var/lib/kbf-server/raft", "--meta-dir=");
    assert_eq!(storage(&storage_unit(&empty, &env), key).len(), 1);
    let spaced = FULL.replace("--cold-store=", "--cold-store ");
    assert_eq!(
        storage(&storage_unit(&spaced, &env), key),
        Vec::<String>::new()
    );
    assert_eq!(
        storage(&storage_unit(FULL, ""), key),
        [
            "has an s3:// cold store and no EnvironmentFile=/etc/kbf/s3.env: kbf storage starts without the bucket's key pair and refuses"
        ]
    );
    // The key given through an optional, quoted file still counts.
    let quoted = format!("EnvironmentFile=-\"{KEY}\"\n");
    assert_eq!(
        storage(&storage_unit(FULL, &quoted), key),
        Vec::<String>::new()
    );
    let file = FULL.replace(
        "s3://kbf-cas?endpoint=http://127.0.0.1:9000",
        "file:///var/lib/kbf/cold",
    );
    assert_eq!(storage(&storage_unit(&file, ""), key), Vec::<String>::new());
}

/// Catches a password missed in each spelling the Postgres client takes (before the
/// host, in a `password=` query key, after either scheme), and a URI with a user and
/// no password, or a host and port, taken for one. The last row is the documented
/// false red: a `:` before a later `@` is read as a password.
#[test]
fn a_postgres_password_is_found_in_the_userinfo_or_the_query() {
    let rows = [
        ("postgres://kbf:secret@db.example/kbf", true),
        ("postgresql://kbf:secret@db.example:5432/kbf", true),
        ("postgres://kbf:@db.example/kbf", true),
        ("postgres://db.example/kbf?user=kbf&password=secret", true),
        ("postgres://db.example/kbf?password=secret", true),
        ("postgres://kbf@db.example/kbf", false),
        ("postgres://db.example:5432/kbf", false),
        ("postgres://db.example/kbf?user=kbf&sslmode=disable", false),
        ("postgres://db.example/kbf?passwords=x", false),
        ("postgres://db.example:5432/kbf?application_name=a@b", true),
    ];
    for (url, want) in rows {
        assert_eq!(has_password(url), want, "{url}");
    }
    let found: Vec<&str> =
        postgres_uris("--state-store=postgresql://a:b@h/d,x=postgres://h/d").collect();
    assert_eq!(
        found,
        ["postgresql://a:b@h/d,x=postgres://h/d", "postgres://h/d"]
    );
    assert_eq!(postgres_uris("postgres:/h postgresx://h").count(), 0);
}

/// Catches a variable not found in either of systemd's spellings, an unclosed brace
/// or a bare `$` read as a name, and a name read past its last letter.
#[test]
fn a_state_store_value_names_its_variable() {
    let rows = [
        ("${KBF_STATE_STORE}", Some("KBF_STATE_STORE")),
        ("$KBF_STATE_STORE", Some("KBF_STATE_STORE")),
        ("x${A}y", Some("A")),
        ("$A/b", Some("A")),
        ("${A", None),
        ("$", None),
        ("${}", None),
        ("sqlite:///var/lib/kbf/state.db", None),
    ];
    for (value, want) in rows {
        assert_eq!(variable(value), want, "{value}");
    }
}

/// Catches a password missed in any directive (the command line, a pre-start command,
/// an environment variable, a quoted word, a section other than `[Service]`), a
/// password-free URI refused, and `--state-store=${VAR}` passed without a file to set
/// the variable or without the line that says the value is still in argv.
#[test]
fn the_state_store_rule_refuses_a_password_anywhere_in_the_unit() {
    let ok = unit(
        "[Service]\nExecStart=/usr/local/bin/kbf frontend --state-store=postgres://kbf@db/kbf\n",
    );
    assert_eq!(state_store(&ok), (Vec::new(), None));
    let sqlite = unit(
        "[Service]\nExecStart=/usr/local/bin/kbf all-in-one --state-store=sqlite:///var/lib/kbf/state.db\n",
    );
    assert_eq!(state_store(&sqlite), (Vec::new(), None));
    for (key, line) in [
        (
            "ExecStart",
            "ExecStart=/usr/local/bin/kbf frontend --state-store=postgres://kbf:pw@db/kbf",
        ),
        (
            "ExecStart",
            "ExecStart=/usr/local/bin/kbf frontend --state-store postgresql://db/kbf?password=pw",
        ),
        (
            "ExecStartPre",
            "ExecStartPre=/usr/bin/true \"postgres://kbf:pw@db/kbf\"",
        ),
        (
            "Environment",
            "Environment=A=1 'KBF_STATE_STORE=postgres://kbf:pw@db/kbf'",
        ),
    ] {
        let (failures, warn) = state_store(&unit(&format!("[Service]\n{line}\n")));
        assert_eq!(
            failures,
            [format!(
                "{key}= holds a postgres URI with a password: a unit file and a command line are readable by every user; put the URI in an EnvironmentFile= of mode 0600 and pass --state-store=${{VAR}}"
            )],
            "{line}"
        );
        assert_eq!(warn, None);
    }
    let elsewhere = unit("[Unit]\nDescription=postgres://kbf:pw@db/kbf\n");
    assert_eq!(state_store(&elsewhere).0.len(), 1);

    let named = "ExecStart=/usr/local/bin/kbf frontend --state-store=${KBF_STATE_STORE}\n";
    let (failures, warn) = state_store(&unit(&format!("[Service]\n{named}")));
    assert_eq!(
        failures,
        ["--state-store names ${KBF_STATE_STORE} and no EnvironmentFile= sets it"]
    );
    let warn_line = "WARN state-store: systemd expands ${KBF_STATE_STORE} into kbf's command line, which other users on the node can read in /proc unless it is mounted hidepid=2; the password is out of the unit file, not out of argv";
    assert_eq!(warn.as_deref(), Some(warn_line));
    let filed = unit(&format!(
        "[Service]\nEnvironmentFile=/etc/kbf/state-store.env\n{named}"
    ));
    assert_eq!(
        state_store(&filed),
        (Vec::new(), Some(warn_line.to_owned()))
    );
}

fn env_entry(bits: u32, text: &str) -> Entry {
    Entry::File {
        perms: Perms::new(bits),
        owner: Owner { uid: 0, gid: 0 },
        bytes: text.as_bytes().to_vec(),
    }
}

/// Catches the node's file that sets the state store's variable not read (a 0644 or
/// 0640 file passes), a file that sets other variables refused for its mode, a file that cannot
/// be read or a wildcard passed without a word, and the rule run when no variable is
/// named.
#[test]
fn on_the_node_the_file_holding_the_state_store_is_mode_0600() {
    let mut exec = FakeExec::new(0);
    let path = |p: &str| HostPath::new(p).expect("absolute");
    exec.entries.insert(
        path("/etc/kbf/state-store.env"),
        env_entry(
            0o644,
            "# the state store\nKBF_STATE_STORE=postgres://kbf:pw@db/kbf\n",
        ),
    );
    exec.entries
        .insert(path("/etc/kbf/other.env"), env_entry(0o644, "OTHER=1\n"));
    exec.entries.insert(
        path("/etc/kbf/group.env"),
        env_entry(0o640, "KBF_STATE_STORE=postgres://kbf:pw@db/kbf\n"),
    );
    exec.entries.insert(
        path("/etc/kbf/private.env"),
        env_entry(0o600, "export KBF_STATE_STORE=postgres://kbf:pw@db/kbf\n"),
    );
    let named = "ExecStart=/usr/local/bin/kbf frontend --state-store=${KBF_STATE_STORE}\n";
    let with = |files: &[&str]| {
        let lines: String = files
            .iter()
            .map(|f| format!("EnvironmentFile={f}\n"))
            .collect();
        unit(&format!("[Service]\n{lines}{named}"))
    };
    let r = on_host(
        &exec,
        &with(&["-/etc/kbf/state-store.env", "/etc/kbf/other.env"]),
    );
    assert_eq!(
        r.failures,
        [
            "EnvironmentFile=-/etc/kbf/state-store.env sets KBF_STATE_STORE, the state store's URI, and is mode 0644: other users can read its password; make it 0600"
        ]
    );
    assert_eq!(r.notes, Vec::<String>::new());
    // A group bit is enough: the group's other members read the password.
    let r = on_host(&exec, &with(&["/etc/kbf/group.env"]));
    assert_eq!(
        r.failures,
        [
            "EnvironmentFile=/etc/kbf/group.env sets KBF_STATE_STORE, the state store's URI, and is mode 0640: other users can read its password; make it 0600"
        ]
    );
    let r = on_host(
        &exec,
        &with(&["/etc/kbf/private.env", "/etc/kbf/absent.env"]),
    );
    assert_eq!(r, OnHost::default());
    let r = on_host(
        &exec,
        &with(&["/unreadable", "/etc/kbf/*.env", "relative.env"]),
    );
    assert_eq!(r.failures, Vec::<String>::new());
    assert_eq!(
        r.notes,
        [
            "SKIP state-store: cannot read EnvironmentFile=/unreadable (permission denied), so its mode was not checked",
            "SKIP state-store: EnvironmentFile=/etc/kbf/*.env is a wildcard; the modes of the files it matches were not checked",
        ]
    );
    let plain = unit(
        "[Service]\nEnvironmentFile=/etc/kbf/state-store.env\nExecStart=/usr/local/bin/kbf frontend\n",
    );
    assert_eq!(on_host(&exec, &plain), OnHost::default());
}
