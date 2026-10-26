//! Tests of the release keys added to `installer.toml` for hosts with no route to the internet
//! (docs/design/kbf-installer.md section 3): `release_source = "bundled"`, the frontend
//! snapshot (`frontend_url`, `frontend_ca`, `frontend_ca_sha256`), `release_ca`, and the
//! tolerant reader. The older keys' tests are `installer_toml_tests.rs`'s. Each test says the
//! defect it catches.

use std::path::PathBuf;

use super::*;
use crate::golden::{
    INSTALLER_TOML_FRONTEND, INSTALLER_TOML_URL, installer_toml_frontend, installer_toml_url,
};
use crate::sha256_hex::Sha256Hex;

/// A digest: 64 lowercase hex characters.
const SHA: &str = "d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26";
const CA: &str = "/var/lib/kbf-installer/frontend-ca.pem";
const WORKER: &str = "https://frontend.lan:8443";
const URL_HEAD: &str = "release_source = \"url\"\nrelease_url = \"https://m.example\"\n";

/// A file the proxy verb reads: the same text as the proxy verb's own tests use.
const PROXY: &str = r#"
database = "postgres:///kbf?host=/nonexistent"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
hold_max_s = 15
[[proxy.frontend]]
name = "a"
base_port = 18000
"#;

fn parsed(text: &str) -> InstallerToml {
    InstallerToml::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

fn problems(text: &str) -> String {
    InstallerToml::parse(text).expect_err("refused").to_string()
}

/// The three snapshot keys, a line each, in the order `render` writes them.
fn pull_lines(url: &str, ca: &str, sha: &str) -> [String; 3] {
    [
        format!("frontend_url = \"{url}\"\n"),
        format!("frontend_ca = \"{ca}\"\n"),
        format!("frontend_ca_sha256 = \"{sha}\"\n"),
    ]
}

fn snapshot() -> FrontendPull {
    FrontendPull {
        url: WORKER.to_owned(),
        ca: PathBuf::from(CA),
        ca_sha256: Sha256Hex::try_from(SHA.to_owned()).expect("a digest"),
    }
}

/// `bundled` is a source of its own, and a bundled host fetches nothing, so a file that also
/// names a place to fetch from (a URL, a frontend snapshot, a node identity, a CA for a file
/// server) says two things and is refused, naming the key.
///
/// Catches `bundled` missing from the set of sources, and the exclusion that covers `url`
/// not covering it: a key accepted under the wrong source.
///
/// Catches: `"bundled"` removed from the `not_frontend` test (the identity and the
/// snapshot are then accepted beside it); from the `not_url` test (`release_ca` is then
/// accepted); the `release_url` arm for `bundled` returning a source.
#[test]
fn bundled_is_a_source_that_excludes_every_key_naming_another() {
    let file = parsed("release_source = \"bundled\"\n");
    let want = InstallerToml {
        source: Source::Bundled,
        ..InstallerToml::default()
    };
    assert_eq!(file, want);
    let pull = pull_lines(WORKER, CA, SHA).concat();
    for (extra, key) in [
        ("release_url = \"https://m.example\"\n", "release_url"),
        ("node_cert = \"/c\"\nnode_key = \"/k\"\n", "node_cert"),
        ("node_key = \"/k\"\n", "node_cert"),
        (pull.as_str(), "frontend_url"),
        ("release_ca = \"/c\"\n", "release_ca"),
    ] {
        let err = problems(&format!("release_source = \"bundled\"\n{extra}"));
        assert!(err.contains(key), "{extra:?}: {err}");
    }
}

/// The frontend snapshot is the worker listener, the root-owned copy of the CA that
/// authenticates it and that copy's digest, written by `setup` together: a file with one or two
/// of them is half a snapshot and is refused, and a file with none is what every file was
/// before. The snapshot belongs to the `frontend` source, named or defaulted.
///
/// Catches a snapshot read with a key missing (a fetch with no CA pinned), and a snapshot
/// accepted under a source that never looks at it.
///
/// Catches: the `(Some, Some, Some)` arm taking any two keys; the group accepted
/// beside `release_source = "url"`.
#[test]
fn the_frontend_snapshot_is_three_keys_that_come_together() {
    let all = pull_lines(WORKER, CA, SHA);
    for head in ["", "release_source = \"frontend\"\n"] {
        let file = parsed(&format!("{head}{}", all.concat()));
        assert_eq!(file.source, Source::Frontend);
        assert_eq!(file.frontend, Some(snapshot()));
    }
    assert_eq!(parsed("release_source = \"frontend\"\n").frontend, None);
    for skip in 0..3 {
        let two: String = (0..3)
            .filter(|i| *i != skip)
            .map(|i| all[i].as_str())
            .collect();
        assert!(problems(&two).contains("come together"), "{two}");
        assert!(problems(&all[skip]).contains("come together"), "{skip}");
    }
    let err = problems(&format!("{URL_HEAD}{}", all.concat()));
    assert!(err.contains("frontend_url"), "{err}");
    assert!(err.contains("release_source = \"frontend\""), "{err}");
}

/// Each snapshot key is held to its own rule, and a bad one is one problem naming that key and
/// nothing else: the address is `https://host[:port]` and nothing after it, the CA an absolute
/// path of plain characters, the digest 64 lowercase hex. Every bad key of a file is reported
/// at once. No message repeats the text it refuses.
///
/// Catches a check dropped from one key (a path in the address, a relative CA, an upper-case or
/// short digest), and a refusal that quotes the value (a digest is public, but a URL is
/// not: it can hold user information).
///
/// Catches: the path check of the address removed; `absolute_path` skipped for the CA;
/// the digest taken as a plain string; the digest's own error text (which quotes the value)
/// passed through.
#[test]
fn each_snapshot_key_is_checked_by_its_own_rule() {
    let upper = SHA.to_uppercase();
    let long = format!("{SHA}0");
    let not_hex = format!("g{}", &SHA[1..]);
    let cases = [
        ("frontend_url:", "http://frontend.lan:8443", CA, SHA),
        ("frontend_url:", "https://frontend.lan:8443/x", CA, SHA),
        ("frontend_url:", "https://user:PW@frontend.lan", CA, SHA),
        ("frontend_url:", "https://frontend.lan?x=1", CA, SHA),
        ("frontend_url:", "https://frontend.lan:99999", CA, SHA),
        ("frontend_ca:", WORKER, "frontend-ca.pem", SHA),
        ("frontend_ca:", WORKER, "/a/../b", SHA),
        ("frontend_ca:", WORKER, "/a b", SHA),
        ("frontend_ca:", WORKER, "/", SHA),
        ("frontend_ca_sha256:", WORKER, CA, upper.as_str()),
        ("frontend_ca_sha256:", WORKER, CA, &SHA[..63]),
        ("frontend_ca_sha256:", WORKER, CA, long.as_str()),
        ("frontend_ca_sha256:", WORKER, CA, not_hex.as_str()),
    ];
    for (key, url, ca, sha) in cases {
        let err = problems(&pull_lines(url, ca, sha).concat());
        assert!(err.contains(key), "{url} {ca} {sha}: {err}");
        assert_eq!(err.lines().count(), 2, "one problem: {err}");
        assert!(!err.contains("PW"), "{err}");
    }
    let err = problems(&pull_lines("http://x", "rel", "zz").concat());
    for key in ["frontend_url:", "frontend_ca:", "frontend_ca_sha256:"] {
        assert!(err.contains(key), "{key}: {err}");
    }
}

/// `release_ca` is the roots a `url` source is checked against instead of the system's, an
/// absolute path of plain characters, and it belongs to the `url` source only: beside the
/// frontend (named or defaulted) or bundled it is refused, since those sources never read it.
///
/// Catches a key accepted under the wrong source, and a path rule loosened for it.
///
/// Catches: the exclusion for `frontend` removed (then the defaulted source takes
/// it); for `bundled` removed; `absolute` skipped for the key.
#[test]
fn release_ca_is_an_absolute_path_for_the_url_source_only() {
    let ca = "/var/lib/kbf-installer/release-ca.pem";
    let file = parsed(&format!("{URL_HEAD}release_ca = \"{ca}\"\n"));
    assert_eq!(file.release_ca, Some(PathBuf::from(ca)));
    assert_eq!(parsed(URL_HEAD).release_ca, None);
    for bad in ["release-ca.pem", "/a/../b", "/a b", "/"] {
        let err = problems(&format!("{URL_HEAD}release_ca = \"{bad}\"\n"));
        assert!(err.contains("release_ca:"), "{bad}: {err}");
    }
    for head in [
        "",
        "release_source = \"frontend\"\n",
        "release_source = \"bundled\"\n",
    ] {
        let err = problems(&format!("{head}release_ca = \"{ca}\"\n"));
        assert!(
            err.contains("release_ca is for release_source = \"url\""),
            "{head:?}: {err}"
        );
    }
}

/// A file written before these keys existed parses to what it always did, with the new fields
/// empty, and an empty file is still the defaults: the keys are optional, so an installer that
/// is upgraded reads the file its predecessor's `setup` wrote.
///
/// Catches a new key made required (or defaulted to something), which would refuse every
/// existing host's file.
///
/// Catches: the snapshot group's `(None, None, None)` arm made a problem; `release_ca`
/// defaulting to a path.
#[test]
fn a_file_written_before_the_new_keys_still_parses_to_the_same_values() {
    for (text, want) in [
        (INSTALLER_TOML_URL, installer_toml_url()),
        (INSTALLER_TOML_FRONTEND, installer_toml_frontend()),
    ] {
        let file = parsed(text);
        assert!(file.release_ca.is_none() && file.frontend.is_none());
        assert_eq!(file, want);
    }
    assert_eq!(parsed(""), InstallerToml::default());
}

/// The closed reader (`parse`) refuses a key or a value it does not know, because the installer
/// that fetches must understand the whole file; the tolerant reader (`parse_tolerant`) takes the
/// four things a reader that fetches nothing needs (the confirm window, the fetch timeout, the
/// `[proxy]` table and `frontend_config`) and ignores every other key, whatever its value, so a
/// file a later build wrote does not stop the proxy after a package downgrade. What the tolerant
/// reader does read it checks as the closed one does, and it reads the same values from the same
/// text.
///
/// Catches the closed reader made tolerant (an old installer would read `bundled` as nothing and
/// fetch from a default source), and the tolerant one left closed (the proxy refusing to start
/// over a key it never reads).
///
/// Catches: the `retain` of `parse_tolerant` removed (red: the future keys are
/// refused); `parse` filtering its table the same way (red: the closed refusals); the filter
/// keeping `release_source`.
#[test]
fn the_closed_reader_refuses_what_it_does_not_know_and_the_tolerant_one_ignores_it() {
    let future = "release_source = \"mirror-pull\"\nrelease_ca = 7\nfrontend_pin = true\n";
    let shared = format!("confirm_window_s = 300\nfetch_timeout_s = 1200\n{PROXY}");
    let text = format!("{future}{shared}");
    assert!(matches!(
        InstallerToml::parse(&text),
        Err(ConfigError::Syntax(_))
    ));
    assert!(problems("release_source = \"mirror-pull\"\n").contains("release_source"));
    let tolerant = InstallerToml::parse_tolerant(&text).expect("tolerated");
    let closed = parsed(&shared);
    assert_eq!(
        (tolerant.confirm_window_s, tolerant.fetch_timeout_s),
        (300, 1200)
    );
    assert!(tolerant.proxy.is_some());
    assert_eq!(tolerant.proxy, closed.proxy);
    assert_eq!(
        InstallerToml::parse_tolerant("").expect("empty"),
        Tolerant {
            confirm_window_s: 900,
            fetch_timeout_s: 900,
            proxy: None,
            frontend_config: None,
        }
    );
}

/// `frontend_config` names the file the proxy's children read: an absolute path of plain
/// characters, kept by `render` and read back by the closed reader and by the tolerant one (the
/// proxy's), and absent when the host has no sibling. A relative path is refused naming the key
/// and not the text.
///
/// Catches the key written and not read back (a rerun of `setup` would drop it), read by the
/// closed reader and ignored by the tolerant one (the proxy would start its children on the
/// default `kbf.toml`), and a relative path let through (the children are started from a
/// directory of the proxy's, not the operator's).
///
/// Catches: the key left out of `render`; left out of `TOLERANT_KEYS`; `absolute` not
/// asked of it in `from_raw`.
#[test]
fn frontend_config_is_an_absolute_path_that_both_readers_take() {
    let with = InstallerToml {
        frontend_config: Some("/etc/kbf/frontend.toml".into()),
        ..InstallerToml::default()
    };
    let text = with.render();
    assert!(
        text.ends_with("\nfrontend_config = \"/etc/kbf/frontend.toml\"\n"),
        "{text}"
    );
    assert_eq!(parsed(&text), with);
    let tolerant = InstallerToml::parse_tolerant(&text).expect("tolerated");
    assert_eq!(tolerant.frontend_config, with.frontend_config);
    let plain = InstallerToml::default().render();
    assert!(!plain.contains("frontend_config"), "{plain}");
    assert_eq!(parsed("").frontend_config, None);
    let err = problems("frontend_config = \"frontend.toml\"\n");
    assert!(err.contains("frontend_config"), "{err}");
    assert!(!err.contains("frontend.toml"), "{err}");
}

/// What the tolerant reader does read is refused when it is wrong, as the closed reader refuses
/// it: a wrong type, a bound, the window above its maximum, an unknown key inside `[proxy]`
/// (that table stays closed), text that is not TOML, a file over the cap. Neither a value nor a
/// key is repeated in a message.
///
/// Catches a tolerant reader that ignores everything (a `confirm_window_s` of 5 accepted), one
/// that skips the proxy table's own rules, and one that quotes the parser.
///
/// Catches: `from_raw` skipped by `parse_tolerant`; `redact` removed from its
/// deserialization error; the size check removed.
#[test]
fn the_tolerant_reader_still_checks_what_it_reads() {
    for (text, want) in [
        ("confirm_window_s = \"x\"\n", "(the text is not shown)"),
        ("fetch_timeout_s = 5\n", "fetch_timeout_s"),
        (
            "confirm_window_s = 1000\nconfirm_window_max_s = 999\n",
            "above confirm_window_max_s",
        ),
        ("[proxy]\nsurprise = 1\n", "proxy:"),
        ("confirm_window_s = = 3\n", "line 1"),
    ] {
        let err = InstallerToml::parse_tolerant(text).expect_err("refused");
        assert!(err.to_string().contains(want), "{text:?}: {err}");
    }
    let big = format!("# {}\n", "x".repeat(MAX_FILE_BYTES));
    assert!(matches!(
        InstallerToml::parse_tolerant(&big),
        Err(ConfigError::TooLarge(_))
    ));
    let planted = "confirm_window_s = \"postgres://u:PW@h/x\"\n";
    let err = InstallerToml::parse_tolerant(planted).expect_err("refused");
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("PW") && !shown.contains("postgres:"),
        "{shown}"
    );
}

/// A value typed into the wrong place of a new key never reaches a message, as for the older
/// keys: the URL is planted (a password in it) in each key, in the wrong type, and beside a
/// source that refuses the key, and neither `Display` nor `Debug` of the error may hold it.
///
/// Catches: the digest's own error (it quotes the text) passed through; `{v:?}` in a
/// message of the address, the path or the exclusion checks.
#[test]
fn a_value_typed_into_a_new_key_is_never_repeated() {
    const URL: &str = "postgres://u:PW@h/x";
    for text in [
        pull_lines(URL, URL, URL).concat(),
        format!("{URL_HEAD}release_ca = \"{URL}\"\n"),
        format!("release_source = \"bundled\"\nrelease_ca = \"{URL}\"\n"),
        format!("frontend_url = [\"{URL}\"]\n"),
        format!("release_ca = {{ k = \"{URL}\" }}\n"),
        format!("frontend_ca_sha256 = \"{URL}\"\n"),
    ] {
        let err = InstallerToml::parse(&text).expect_err("refused");
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains("PW") && !shown.contains("postgres:"),
            "{text:?}: {shown}"
        );
    }
}
