use std::collections::BTreeSet;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

use super::{Delivery, Nonce, PackKey, Secret, TaskArgs, build_id};

fn args<'a>(action: &'a Digest, grant: &'a Secret, url: &'a Secret, plain: bool) -> TaskArgs<'a> {
    TaskArgs {
        action,
        lease: LeaseId::new(7, 3),
        grant_url: "https://grant.test:9400",
        grant_token: grant,
        pack_url: url,
        deadline: Duration::from_secs(90),
        work_dir: None,
        allow_plain_http_put: plain,
        allow_plain_http_grant: false,
        locate: false,
    }
}

fn digest() -> Digest {
    Digest {
        hash: "ab".repeat(32),
        size_bytes: 12,
    }
}

/// Catches a nonce drawn from anything but fresh randomness (a counter, the clock, a
/// per-process value): a zombie of an earlier run holds the url of its key, and a reused key
/// would meet it. Distinct across many draws, and written as 32 hex digits.
#[test]
fn nonces_are_random_and_never_repeat() {
    let drawn: BTreeSet<Nonce> = (0..200).map(|_| Nonce::random()).collect();
    assert_eq!(drawn.len(), 200);
    let text = Nonce::random().to_string();
    assert_eq!(text.len(), 32);
    assert!(text.bytes().all(|b| b.is_ascii_hexdigit()));
}

/// Catches a build id the neighbours refuse: the farm prints a lease as `7.3`, and the pack
/// key, the manifest and the ingest take a build of `0-9 a-z A-Z - _` only (a dot is refused,
/// and every action would be rerun and answered INTERNAL). The id is `7-3`, and passes the
/// pack crate's own check, for any lease.
#[test]
fn a_lease_is_a_build_id_the_pack_crate_accepts() {
    assert_eq!(build_id(LeaseId::new(7, 3)), "7-3");
    assert!(LeaseId::new(7, 3).to_string().contains('.'));
    for lease in [LeaseId::new(0, 0), LeaseId::new(u64::MAX, u64::MAX)] {
        assert_eq!(kbf_pack::check_build(&build_id(lease)), Ok(()), "{lease}");
    }
}

/// Catches a pack key that is not the string the mint issued: the executor passes it on to
/// the store and the ingest as it is.
#[test]
fn a_pack_key_is_the_object_key_it_was_issued_as() {
    let key = PackKey::new("cas/b/7-3/ab-0");
    assert_eq!(key.to_string(), "cas/b/7-3/ab-0");
    assert_eq!(key.as_str(), "cas/b/7-3/ab-0");
}

/// Catches a secret that formats as itself: it would reach a log line or an error.
#[test]
fn a_secret_never_prints() {
    let secret = Secret::new("https://store/pack?sig=hunter2");
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(secret.expose(), "https://store/pack?sig=hunter2");
}

/// Catches an argument vector that does not match the task tool's command line: flags only,
/// the first one `--action` (the kbf image's entrypoint is the tool, and the lease server puts
/// these after it, so a path in front would be taken for a flag), the action as `<hash>/<size>`, the interim secret flags, and the optional flags
/// only when asked for.
#[test]
fn the_argv_is_the_task_tools_command_line() {
    let (action, grant, url) = (
        digest(),
        Secret::new("grant-token"),
        Secret::new("http://p/key"),
    );
    let plain = args(&action, &grant, &url, false).argv(Delivery::InsecureArgv);
    assert_eq!(
        plain,
        [
            "--action",
            &format!("{}/12", "ab".repeat(32)),
            "--lease",
            "7-3",
            "--grant-url",
            "https://grant.test:9400",
            "--deadline-ms",
            "90000",
            "--insecure-argv-secrets",
            "--pack-url",
            "http://p/key",
            "--grant-token",
            "grant-token",
        ]
    );
    let full = args(&action, &grant, &url, true).argv(Delivery::InsecureArgv);
    assert_eq!(&full[plain.len()..], ["--allow-plain-http-put"]);
    assert!(
        !full
            .iter()
            .any(|a| a == "--root-der" || a.starts_with("/mnt/")),
        "{full:?}"
    );
    let worked = TaskArgs {
        work_dir: Some("/work"),
        ..args(&action, &grant, &url, false)
    }
    .argv(Delivery::InsecureArgv);
    assert_eq!(&worked[plain.len()..], ["--work-dir", "/work"]);
}

/// Catches the plain-http grant flag leaking into runs that did not ask for it, or being passed
/// twice or under another spelling: it is the task tool's own `--allow-plain-http-grant`, once,
/// after the other optional flags, and only when the config allows it, whether or not the put
/// is plain.
#[test]
fn the_plain_http_grant_flag_is_passed_once_and_only_when_allowed() {
    let (action, grant, url) = (
        digest(),
        Secret::new("grant-token"),
        Secret::new("https://p/key"),
    );
    let count = |argv: &[String]| {
        argv.iter()
            .filter(|a| a.contains("plain-http-grant"))
            .count()
    };
    for put in [false, true] {
        let off = args(&action, &grant, &url, put).argv(Delivery::InsecureArgv);
        assert_eq!(count(&off), 0, "{off:?}");
        let on = TaskArgs {
            allow_plain_http_grant: true,
            ..args(&action, &grant, &url, put)
        }
        .argv(Delivery::InsecureArgv);
        assert_eq!(count(&on), 1, "{on:?}");
        assert_eq!(
            on.last().map(String::as_str),
            Some("--allow-plain-http-grant")
        );
        assert_eq!(on.iter().any(|a| a == "--allow-plain-http-put"), put);
    }
}

/// Catches `--locate` leaking into runs that did not ask for it, or going out twice or under
/// another spelling: with it off the whole command line is the one a frontend without the flag
/// has always given, word for word; with it on the line is that one plus a single `--locate`,
/// after every other optional flag, whatever the others say.
#[test]
fn argv_has_locate_only_when_asked() {
    let (action, grant, url) = (
        digest(),
        Secret::new("grant-token"),
        Secret::new("https://p/key"),
    );
    let hash = "ab".repeat(32);
    let before_the_flag = [
        "--action",
        &format!("{hash}/12"),
        "--lease",
        "7-3",
        "--grant-url",
        "https://grant.test:9400",
        "--deadline-ms",
        "90000",
        "--insecure-argv-secrets",
        "--pack-url",
        "https://p/key",
        "--grant-token",
        "grant-token",
    ];
    let off = args(&action, &grant, &url, false).argv(Delivery::InsecureArgv);
    assert_eq!(off, before_the_flag, "off is today's line, byte for byte");
    let on = TaskArgs {
        locate: true,
        ..args(&action, &grant, &url, false)
    }
    .argv(Delivery::InsecureArgv);
    assert_eq!(on[..off.len()], off[..]);
    assert_eq!(&on[off.len()..], ["--locate"]);
    let everything = TaskArgs {
        locate: true,
        work_dir: Some("/work"),
        allow_plain_http_grant: true,
        ..args(&action, &grant, &url, true)
    }
    .argv(Delivery::InsecureArgv);
    let at = |flag: &str| everything.iter().position(|a| a == flag);
    assert_eq!(everything.iter().filter(|a| *a == "--locate").count(), 1);
    assert_eq!(everything.last().map(String::as_str), Some("--locate"));
    assert!(at("--allow-plain-http-grant") < at("--locate"));
    assert!(at("--work-dir") < at("--locate"));
}

/// Catches a Debug of the task arguments that shows the pack url or the read token.
#[test]
fn the_arguments_never_print_a_secret() {
    let (action, grant, url) = (
        digest(),
        Secret::new("grant-token-xyz"),
        Secret::new("http://p/key?sig=zz"),
    );
    let shown = format!("{:?}", args(&action, &grant, &url, false));
    assert!(!shown.contains("grant-token-xyz"), "{shown}");
    assert!(!shown.contains("sig=zz"), "{shown}");
    assert!(shown.contains("<redacted>"));
}
