//! Refusals of the `kbf.toml` settings flags (B7): one case per rule, each naming the flag and
//! never quoting its value, with the host untouched. The helpers and fixtures are
//! `kbf_settings_tests.rs`'s.

use super::*;

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
        Mode::Storage => KbfFlags::default(),
    }
}

/// Catches an invalid flag value getting through, or a message that repeats what the person
/// typed: one refusal per rule, each naming the flag and none quoting the value, with the host
/// untouched (exit 2).
#[test]
fn every_invalid_setting_is_refused_by_flag_and_without_its_value() {
    let cases: [(Mode, &str, Mutate, &str); 18] = [
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
            "worker-tls-cert",
            |k, v| k.worker_tls_cert = some(v),
            "/etc/kbf/\"SECRET\".pem",
        ),
        (
            Mode::Frontend,
            "worker-tls-cert",
            |k, v| k.worker_tls_cert = some(v),
            "/etc/kbf/SECRET\\.pem",
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
            Mode::Storage,
            "reapi-tls-key",
            |k, v| k.reapi_tls_key = some(v),
            "/etc/kbf/SECRET.key",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://storage.example:8983/",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://storage.example/SECRET/",
        ),
    ];
    for (mode, flag, set, value) in cases {
        let mut flags = base(mode);
        set(&mut flags, value);
        let (err, exec) = refused(mode, flags);
        assert_clean(&err, &exec, flag, value);
    }
}

/// Catches a refusal that says nothing useful: PEM text given as a path is told "not its
/// contents" (not just "invalid"), and a trailing slash on a URL is named as such.
#[test]
fn refusal_messages_name_the_mistake() {
    let cases: [(&str, Mutate, &str, &str); 3] = [
        (
            "reapi-tls-key",
            |k, v| k.reapi_tls_key = some(v),
            "-----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----",
            "not its contents",
        ),
        (
            "reapi-tls-key",
            |k, v| k.reapi_tls_key = some(v),
            "-----BEGIN PRIVATE KEY-----SECRET",
            "not its contents",
        ),
        (
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://storage.example:8983/",
            "trailing slash",
        ),
    ];
    for (flag, set, value, words) in cases {
        let mut flags = base(Mode::Frontend);
        set(&mut flags, value);
        let (err, _) = refused(Mode::Frontend, flags);
        let shown = err.to_string();
        assert!(shown.contains(words), "{flag}: {shown}");
    }
}

/// Catches a flag reaching a mode that must not have it: each setting of one mode, given to
/// another, is refused with the flag, the sentence "does not take it" and the mode it is for
/// (a node's listeners and certificates come from enrollment), so a setting moved to the wrong
/// mode fails here.
#[test]
fn a_flag_of_another_mode_is_refused_naming_the_flag() {
    type Set = fn(&mut KbfFlags);
    let cases: [(Mode, &str, Set); 3] = [
        (Mode::Storage, "listen", |k| k.listen = some("0.0.0.0:8980")),
        (Mode::Storage, "worker-tls-cert", |k| {
            k.worker_tls_cert = some("/etc/kbf/worker.pem")
        }),
        (Mode::Storage, "api-listen", |k| {
            k.api_listen = some("127.0.0.1:8982")
        }),
    ];
    for (mode, flag, set) in cases {
        let mut flags = base(mode);
        set(&mut flags);
        let (err, exec) = refused(mode, flags);
        let shown = err.to_string();
        let want = format!("--{flag} is for ");
        assert!(shown.contains(&want), "{mode:?} {flag}: {shown}");
        assert!(
            shown.contains("does not take it"),
            "{mode:?} {flag}: {shown}"
        );
        assert_eq!(err.exit(), Exit::Usage);
        assert!(exec.log.is_empty(), "{mode:?} {flag}");
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
        "http://localhost",
        "http://[::1]",
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
/// is required, named with the mode.
#[test]
fn what_a_mode_cannot_start_without_is_required() {
    let mut flags = base(Mode::Frontend);
    flags.storage_url = None;
    let (err, exec) = refused(Mode::Frontend, flags);
    assert!(
        err.to_string()
            .contains("--storage-url: required for --mode=frontend"),
        "{err}"
    );
    assert_eq!(err.exit(), Exit::Usage);
    assert!(exec.log.is_empty());
    // The one it cannot start without, and no other: the listeners and certificates are
    // optional and the operator API address has its default.
    let (err, _) = refused(Mode::Frontend, KbfFlags::default());
    let SetupError::Invalid(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.len(), 1, "{found:#?}");
}

/// Catches flags that only work together being accepted alone: a certificate without its
/// key, part of the worker trio, an https storage URL without the client trio, the trio with
/// plain text; each refusal names the flags and every problem is said at once.
#[test]
fn flags_that_come_together_are_refused_alone() {
    type Clear = fn(&mut KbfFlags);
    let cases: [(Clear, &str); 5] = [
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

/// Catches the edges of a value getting through: a path with a newline and no dashes, a path
/// longer than 4096 bytes, a URL with no host, an
/// empty host with a port, a port that is not a number or does not fit, and a file the host
/// cannot look at; each is refused naming the flag, never the value.
#[test]
fn the_edges_of_a_value_are_refused() {
    let long_path = format!("/etc/kbf/{}", "a".repeat(5000));
    type Set = fn(&mut KbfFlags, &str);
    let cases: [(Mode, &str, Set, &str); 6] = [
        (
            Mode::Frontend,
            "reapi-tls-cert",
            |k, v| k.reapi_tls_cert = some(v),
            "/etc/kbf/new\nline.pem",
        ),
        (
            Mode::Frontend,
            "reapi-tls-cert",
            |k, v| k.reapi_tls_cert = some(v),
            &long_path,
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://:8983",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://host.example:99999",
        ),
        (
            Mode::Frontend,
            "storage-url",
            |k, v| k.storage_url = some(v),
            "https://host.example:port",
        ),
    ];
    for (mode, flag, set, value) in cases {
        let mut flags = base(mode);
        set(&mut flags, value);
        let (err, exec) = refused(mode, flags);
        let shown = err.to_string();
        assert!(shown.contains(&format!("--{flag}:")), "{flag}: {shown}");
        if value.len() > 1 {
            assert!(!shown.contains(value), "{flag}: {shown}");
        }
        assert_eq!(err.exit(), Exit::Usage, "{flag}");
        assert!(exec.log.is_empty(), "{flag}");
    }
    // A host that cannot say what is at a path (the fake's `/unreadable`) is a refusal too.
    let mut flags = frontend_flags();
    flags.reapi_tls_cert = some("/unreadable");
    let (err, _) = refused(Mode::Frontend, flags);
    assert!(
        err.to_string()
            .contains("--reapi-tls-cert: cannot be looked at"),
        "{err}"
    );
}

/// Catches a URL with no port being refused, or one with a port in the wrong place being
/// accepted: `https://host` is the default port and passes with the client files.
#[test]
fn an_https_storage_url_may_leave_the_port_out() {
    let raw = Raw {
        kbf: KbfFlags {
            storage_url: some("https://storage.example"),
            ..frontend_flags()
        },
        ..raw(Mode::Frontend)
    };
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    assert!(text(&exec, KBF_TOML).contains("storage = \"https://storage.example\""));
}
