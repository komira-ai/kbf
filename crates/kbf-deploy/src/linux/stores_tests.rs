//! Rules `storage`, `state-store` and `ports` read on their own: how a flag, a Postgres
//! URI and a variable are read, what the state store's password file must be on the
//! node, and which listeners share a port.

use std::path::Path;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::{FakeExec, reply};

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

const PG: &str = "--state-store=postgres://kbf@db/kbf";

fn frontend(service: &str, flags: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{service}ExecStart=/usr/local/bin/kbf frontend {flags}\n"
    ))
}

/// Catches a password missed in any directive (the command line, a pre-start command,
/// an environment variable, a quoted word, a section other than `[Service]`), and a
/// password-free URI, or a SQLite store, refused.
#[test]
fn the_state_store_rule_refuses_a_password_anywhere_in_the_unit() {
    let ok = frontend(
        "",
        &format!("{PG} --{PASSWORD_FILE}=/etc/kbf/state-store.password"),
    );
    assert_eq!(state_store(&ok), Vec::<String>::new());
    let sqlite = frontend("", "--state-store=sqlite:///var/lib/kbf/state.db");
    assert_eq!(state_store(&sqlite), Vec::<String>::new());
    assert_eq!(state_store(&frontend("", "")), Vec::<String>::new());
    let advice = "a unit file and a command line are readable by every user; leave the password out of the URI and give --state-store-password-file=<a file of mode 0600, the unit's User='s>";
    for (key, line) in [
        (
            "ExecStart",
            "ExecStart=/usr/local/bin/kbf frontend --state-store-password-file=/p --state-store=postgres://kbf:pw@db/kbf",
        ),
        (
            "ExecStart",
            "ExecStart=/usr/local/bin/kbf frontend --state-store-password-file=/p --state-store postgresql://db/kbf?password=pw",
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
        assert_eq!(
            state_store(&unit(&format!("[Service]\n{line}\n"))),
            [format!(
                "{key}= holds a postgres URI with a password: {advice}"
            )],
            "{line}"
        );
    }
    let elsewhere = unit("[Unit]\nDescription=postgres://kbf:pw@db/kbf\n");
    assert_eq!(state_store(&elsewhere).len(), 1);
}

/// Catches `--state-store=${VAR}` passed (with or without a file to set it: the
/// expanded value is in kbf's argv either way), a Postgres store passed without a
/// password file or with an empty one, a relative password file passed, and a `%d/`
/// credential passed with nothing loading it (another credential's name, an
/// `ImportCredential=`, or a `LoadCredential=` an empty one reset) or refused when a
/// `LoadCredential=` or `LoadCredentialEncrypted=` loads it.
#[test]
fn a_postgres_state_store_names_its_password_file_and_no_variable() {
    let filed = "EnvironmentFile=/etc/kbf/state-store.env\n";
    for (service, value) in [(filed, "${KBF_STATE_STORE}"), ("", "$KBF_STATE_STORE")] {
        assert_eq!(
            state_store(&frontend(service, &format!("--state-store={value}"))),
            [
                "--state-store names ${KBF_STATE_STORE}: systemd expands it into kbf's command line, which every user on the node can read in /proc; write the URI, with no password, and give --state-store-password-file"
            ],
            "{value}"
        );
    }
    let no_file = "--state-store is a postgres URI and the unit gives no --state-store-password-file: the password goes in a file of mode 0600, the unit's User='s";
    assert_eq!(state_store(&frontend("", PG)), [no_file]);
    assert_eq!(
        state_store(&frontend("", &format!("{PG} --{PASSWORD_FILE}="))),
        [no_file]
    );
    let pgql = "--state-store=postgresql://kbf@db/kbf";
    assert_eq!(state_store(&frontend("", pgql)), [no_file]);
    assert_eq!(
        state_store(&frontend(
            "",
            &format!("{PG} --{PASSWORD_FILE}=state.password")
        )),
        [
            "--state-store-password-file=state.password is neither an absolute path nor %d/<credential>"
        ]
    );
    let cred = format!("{PG} --{PASSWORD_FILE}=%d/state-store");
    let unloaded = [
        "--state-store-password-file=%d/state-store names the credential state-store, and no LoadCredential=state-store:<file> loads it",
    ];
    for service in [
        "",
        "LoadCredential=other:/etc/kbf/state-store.password\n",
        "ImportCredential=state-store\n",
        "LoadCredential=state-store:/etc/kbf/state-store.password\nLoadCredential=\n",
    ] {
        assert_eq!(
            state_store(&frontend(service, &cred)),
            unloaded,
            "{service}"
        );
    }
    for service in [
        "LoadCredential=state-store:/etc/kbf/state-store.password\n",
        "LoadCredentialEncrypted=state-store:/etc/credstore.encrypted/state-store\n",
        "LoadCredential=state-store\n",
    ] {
        assert_eq!(
            state_store(&frontend(service, &cred)),
            Vec::<String>::new(),
            "{service}"
        );
    }
}

fn pw_file(bits: u32, uid: u32) -> Entry {
    Entry::File {
        perms: Perms::new(bits),
        owner: Owner { uid, gid: uid },
        bytes: b"pw\n".to_vec(),
    }
}

/// A node where `getent passwd kbf-server` is uid 990 and any other user is unknown.
fn node() -> FakeExec {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd: &kbf_host::step::Cmd| {
        Ok(
            match (cmd.program.as_str(), cmd.args.last().map(String::as_str)) {
                ("/usr/bin/getent", Some("kbf-server")) => reply(
                    0,
                    "kbf-server:x:990:990::/var/lib/kbf-server:/sbin/nologin\n",
                    "",
                ),
                ("/usr/bin/getent", Some("broken")) => reply(1, "", "planted\n"),
                ("/usr/bin/id", _) => reply(0, "990\n", ""),
                _ => reply(2, "", ""),
            },
        )
    });
    exec
}

const PW: &str = "/etc/kbf/state-store.password";

fn at(p: &str) -> HostPath {
    HostPath::new(p).expect("absolute")
}

/// Catches the password file not read (a 0644 or 0640 file, one another user owns,
/// a directory passed), its owner compared with root's when the unit has a `User=`
/// (or with a user when it has none), an unknown `User=` passed, a file that is not
/// there or cannot be read, or a user that cannot be looked up, failed instead of
/// skipped with a word, and the rule run without a Postgres store.
#[test]
fn on_the_node_the_password_file_is_the_units_users_and_mode_0600() {
    let mut exec = node();
    let as_user = |user: &str, flags: &str| frontend(&format!("User={user}\n"), flags);
    let flags = format!("{PG} --{PASSWORD_FILE}={PW}");
    exec.entries.insert(at(PW), pw_file(0o600, 990));
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &flags)),
        OnHost::default()
    );
    // No User=: kbf runs as root, which must own the file.
    let r = on_host(&exec, &frontend("", &flags));
    assert_eq!(
        r.failures,
        [
            "/etc/kbf/state-store.password, the state store's password file, is uid 990's, not root's (uid 0): kbf reads it as root, and no other user may"
        ]
    );
    exec.entries.insert(at(PW), pw_file(0o600, 0));
    assert_eq!(
        on_host(&exec, &frontend("User=\n", &flags)),
        OnHost::default()
    );
    exec.entries.insert(at(PW), pw_file(0o640, 990));
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &flags)).failures,
        [
            "/etc/kbf/state-store.password, the state store's password file, is mode 0640: other users can read the password; make it 0600"
        ]
    );
    exec.entries.insert(at(PW), pw_file(0o600, 1000));
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &flags)).failures,
        [
            "/etc/kbf/state-store.password, the state store's password file, is uid 1000's, not kbf-server's (uid 990): kbf reads it as kbf-server, and no other user may"
        ]
    );
    assert_eq!(
        on_host(&exec, &as_user("nobody-here", &flags)).failures,
        ["User=nobody-here is not a user on this node"]
    );
    let r = on_host(&exec, &as_user("broken", &flags));
    assert_eq!(r.failures, Vec::<String>::new());
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(
        r.notes[0].starts_with("SKIP state-store: cannot look up User=broken ("),
        "{r:?}"
    );
    exec.entries.insert(
        at(PW),
        Entry::Dir {
            perms: Perms::new(0o700),
            owner: Owner { uid: 990, gid: 990 },
        },
    );
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &flags)).failures,
        [
            "/etc/kbf/state-store.password, the state store's password file, is not a plain file (a link or a directory)"
        ]
    );
    exec.entries.remove(&at(PW));
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &flags)).notes,
        [
            "SKIP state-store: no password file at /etc/kbf/state-store.password yet, so its owner and mode were not checked"
        ]
    );
    let unreadable = format!("{PG} --{PASSWORD_FILE}=/unreadable");
    assert_eq!(
        on_host(&exec, &as_user("kbf-server", &unreadable)).notes,
        [
            "SKIP state-store: cannot read /unreadable (permission denied), so its owner and mode were not checked"
        ]
    );
    // Not a Postgres store, no password file, or a relative one: nothing to read.
    exec.entries.insert(at(PW), pw_file(0o644, 0));
    for flags in [
        format!("--state-store=sqlite:///s.db --{PASSWORD_FILE}={PW}"),
        format!("--{PASSWORD_FILE}={PW}"),
        PG.to_owned(),
        format!("{PG} --{PASSWORD_FILE}=relative"),
    ] {
        assert_eq!(
            on_host(&exec, &frontend("", &flags)),
            OnHost::default(),
            "{flags}"
        );
    }
}

/// Catches a `%d/` credential's `LoadCredential=` source not read (a 0644 source
/// passed), its owner checked (systemd reads it as root), an encrypted or credential
/// store source failed instead of skipped with a word, and a credential nothing loads
/// read at all (rule `state-store` on the unit fails it).
#[test]
fn on_the_node_a_credentials_source_has_no_group_or_other_bit() {
    let mut exec = node();
    let flags = format!("{PG} --{PASSWORD_FILE}=%d/state-store");
    let loaded = |line: &str| frontend(&format!("User=kbf-server\n{line}\n"), &flags);
    exec.entries.insert(at(PW), pw_file(0o600, 0));
    let plain = loaded(&format!("LoadCredential=state-store:{PW}"));
    assert_eq!(on_host(&exec, &plain), OnHost::default());
    exec.entries.insert(at(PW), pw_file(0o644, 0));
    assert_eq!(
        on_host(&exec, &plain).failures,
        [
            "/etc/kbf/state-store.password, the state store's password file, is mode 0644: other users can read the password; make it 0600"
        ]
    );
    let skip = "SKIP state-store: the credential state-store is encrypted or comes from the credential store, so its source's mode was not checked";
    for line in [
        format!("LoadCredentialEncrypted=state-store:{PW}"),
        "LoadCredential=state-store".to_owned(),
        "LoadCredential=state-store:state-store.password".to_owned(),
    ] {
        let r = on_host(&exec, &loaded(&line));
        assert_eq!(
            (r.failures, r.notes),
            (Vec::new(), vec![skip.to_owned()]),
            "{line}"
        );
    }
    assert_eq!(on_host(&exec, &loaded("")), OnHost::default());
}

/// Catches two listeners on one port passed (the first and the second, the first and
/// the last, the second form `--flag value`), and listeners on their own ports, or a
/// unit with none, failed.
#[test]
fn no_two_listeners_share_a_port() {
    let ports_of = |flags: &str| ports(&frontend("", flags));
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=a:4"),
        Vec::<String>::new()
    );
    assert_eq!(ports_of(""), Vec::<String>::new());
    assert_eq!(
        ports_of("--listen=noport --api-listen=a:1"),
        Vec::<String>::new()
    );
    let tail = "give each listener its own (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)";
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen [::1]:1"),
        [format!(
            "--listen and --worker-listen are both on port 1: {tail}"
        )]
    );
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=b:1"),
        [format!(
            "--listen and --storage-listen are both on port 1: {tail}"
        )]
    );
}
