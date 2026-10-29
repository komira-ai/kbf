use std::collections::BTreeSet;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

use super::{Delivery, Nonce, PackKey, Secret, TaskArgs};

fn args<'a>(
    action: &'a Digest,
    grant: &'a Secret,
    url: &'a Secret,
    root: Option<&'a str>,
    plain: bool,
) -> TaskArgs<'a> {
    TaskArgs {
        tool: "/mnt/kbf-tools/kbf-task",
        action,
        lease: LeaseId::new(7, 3),
        grant_url: "https://10.0.0.1:9400",
        grant_token: grant,
        pack_url: url,
        deadline: Duration::from_secs(90),
        root_der: root,
        allow_plain_http_put: plain,
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

/// Catches a pack key outside the `cas/b/<build>/<nonce>-<index>` space the frontend
/// ingests from.
#[test]
fn a_pack_key_is_under_cas_b() {
    let key = PackKey {
        build: "front-1".to_owned(),
        nonce: Nonce::random(),
        index: 0,
    };
    assert_eq!(key.to_string(), format!("cas/b/front-1/{}-0", key.nonce));
}

/// Catches a secret that formats as itself: it would reach a log line or an error.
#[test]
fn a_secret_never_prints() {
    let secret = Secret::new("https://store/pack?sig=hunter2");
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(secret.expose(), "https://store/pack?sig=hunter2");
}

/// Catches an argument vector that does not match the task tool's command line: the tool
/// first, the action as `<hash>/<size>`, the interim secret flags, and the optional flags
/// only when asked for.
#[test]
fn the_argv_is_the_task_tools_command_line() {
    let (action, grant, url) = (digest(), Secret::new("grant-token"), Secret::new("http://p/key"));
    let plain = args(&action, &grant, &url, None, false).argv(Delivery::InsecureArgv);
    assert_eq!(
        plain,
        [
            "/mnt/kbf-tools/kbf-task",
            "--action",
            &format!("{}/12", "ab".repeat(32)),
            "--lease",
            &LeaseId::new(7, 3).to_string(),
            "--grant-url",
            "https://10.0.0.1:9400",
            "--deadline-ms",
            "90000",
            "--insecure-argv-secrets",
            "--pack-url",
            "http://p/key",
            "--grant-token",
            "grant-token",
        ]
    );
    let full = args(&action, &grant, &url, Some("/mnt/kbf-tools/farm-ca.der"), true)
        .argv(Delivery::InsecureArgv);
    assert_eq!(&full[plain.len()..], ["--root-der", "/mnt/kbf-tools/farm-ca.der", "--allow-plain-http-put"]);
}

/// Catches a Debug of the task arguments that shows the pack url or the read token.
#[test]
fn the_arguments_never_print_a_secret() {
    let (action, grant, url) = (digest(), Secret::new("grant-token-xyz"), Secret::new("http://p/key?sig=zz"));
    let shown = format!("{:?}", args(&action, &grant, &url, None, false));
    assert!(!shown.contains("grant-token-xyz"), "{shown}");
    assert!(!shown.contains("sig=zz"), "{shown}");
    assert!(shown.contains("<redacted>"));
}
