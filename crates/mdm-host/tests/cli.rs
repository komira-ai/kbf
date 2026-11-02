//! The command line: parsing, dispatch and exit codes, on the fake host.

mod common;

use common::{FakeNet, Kit, example, kit, ready_host, with_env};
use host_step::fake::FakeHost;
use mdm_host::{run, run_with};

fn args(words: &[&str]) -> Vec<String> {
    std::iter::once("mdm-host")
        .chain(words.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// Runs the command line `words` on `host`; the exit code, standard output, standard error.
fn cli(host: &FakeHost, net: &FakeNet, kit: &Kit, words: &[&str]) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = with_env(host, net, |env| {
        run_with(&args(words), env, &kit.pins, &mut out, &mut err)
    });
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Catches a command line that accepts what the verbs cannot act on: a missing or repeated
/// flag, an unknown tool, a missing subcommand, and `install` with neither or both of
/// `--bin-dir` and `--uninstall`. Each exits 2 with the reason on stderr; help exits 0.
#[test]
fn bad_command_lines_exit_2_and_help_exits_0() {
    let kit = kit();
    let (host, net) = (FakeHost::new(1000), FakeNet::new(Default::default()));
    for words in [
        &[][..],
        &["frobnicate"],
        &["fetch"],
        &["fetch", "--dir", "/d", "--only", "nonesuch"],
        &["install", "--config", "/c"],
        &[
            "install",
            "--config",
            "/c",
            "--bin-dir",
            "/b",
            "--uninstall",
        ],
        &["tls"],
        &["verify", "--config"],
    ] {
        let (code, out, err) = cli(&host, &net, &kit, words);
        assert_eq!((code, out.as_str()), (2, ""), "{words:?}: {err}");
        assert!(!err.is_empty(), "{words:?}");
    }
    let (code, out, err) = cli(&host, &net, &kit, &["--help"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("Usage: mdm-host"), "{out}");
    for verb in ["fetch", "install", "tls", "verify"] {
        assert!(out.contains(verb));
    }
    // The built-in pins are the default.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = with_env(&host, &net, |env| {
        run(&args(&["--help"]), env, &mut out, &mut err)
    });
    assert_eq!(code, 0);
}

/// Catches a fetch verb not wired to its module: a dry run prints the plan (exit 0), `--apply`
/// writes the archives, `--only` is passed on, root is refused with REFUSED on stderr (exit 2)
/// and a failed download with FAILED (exit 1).
#[test]
fn fetch_runs_through_the_command_line() {
    let kit = kit();
    let (host, net) = (FakeHost::new(1000), FakeNet::new(kit.urls.clone()));
    let (code, out, _) = cli(
        &host,
        &net,
        &kit,
        &["fetch", "--dir", "/srv/op/a", "--only", "caddy"],
    );
    assert_eq!(code, 0);
    assert_eq!(out.matches("DRY-RUN: download").count(), 1);
    assert!(host.paths().is_empty());
    let (code, _, _) = cli(
        &host,
        &net,
        &kit,
        &["fetch", "--dir", "/srv/op/a", "--apply"],
    );
    assert_eq!(code, 0);
    assert!(host.node("/srv/op/a/caddy-v1.0.0.tar.gz").is_some());

    let (code, out, err) = cli(
        &FakeHost::new(0),
        &net,
        &kit,
        &["fetch", "--dir", "/d", "--apply"],
    );
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert_eq!(
        err,
        "REFUSED: run fetch as yourself, not root: downloads never run as root\n"
    );

    let down = FakeNet::new(Default::default());
    let (code, _, err) = cli(
        &FakeHost::new(1000),
        &down,
        &kit,
        &["fetch", "--dir", "/d", "--apply"],
    );
    assert_eq!(code, 1);
    assert!(
        err.starts_with("FAILED: download of https://downloads.example.org/"),
        "{err}"
    );
}

/// Catches install, uninstall, tls and verify not wired to their modules, and a settings file
/// that is missing or wrong passing as a plan: the file is read through the host, a missing
/// one or one with an unknown key is REFUSED (exit 2), and verify exits 1 when a check fails.
#[test]
fn install_tls_and_verify_run_through_the_command_line() {
    let kit = kit();
    let host = ready_host(&kit);
    host.put_file("/cfg", 0o644, (0, 0), example().as_bytes());
    let net = FakeNet::new(Default::default());

    let (code, out, _) = cli(
        &host,
        &net,
        &kit,
        &["install", "--config", "/cfg", "--bin-dir", "/srv/fetch"],
    );
    assert_eq!(code, 0);
    assert!(
        out.contains("DRY-RUN: mkdir /var/lib/js-mdm mode 0700"),
        "{out}"
    );
    assert!(host.node("/etc/js-mdm").is_none());
    let (code, out, _) = cli(
        &host,
        &net,
        &kit,
        &["install", "--config", "/cfg", "--uninstall"],
    );
    assert_eq!(code, 0);
    assert!(out.contains("no unit of the kit is installed"));
    let (code, _, _) = cli(
        &host,
        &net,
        &kit,
        &[
            "install",
            "--config",
            "/cfg",
            "--bin-dir",
            "/srv/fetch",
            "--apply",
        ],
    );
    assert_eq!(code, 0);
    assert!(host.node("/etc/js-mdm/Caddyfile").is_some());

    let (code, _, err) = cli(&host, &net, &kit, &["tls", "--config", "/cfg"]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("REFUSED: /etc/js-mdm/dns.env has no CLOUDFLARE_DNS_API_TOKEN="),
        "{err}"
    );

    let (code, out, _) = cli(
        &host,
        &net,
        &kit,
        &[
            "verify",
            "--config",
            "/cfg",
            "--ignore",
            "zzz-absent",
            "--ignore",
            "qqq-absent",
        ],
    );
    assert_eq!(code, 1);
    assert!(
        out.contains("same       /etc/systemd/system/js-mdm-scep.service"),
        "{out}"
    );
    assert!(out.contains("verify: "));

    let (code, out, err) = cli(&host, &net, &kit, &["verify", "--config", "/nope"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert_eq!(err, "REFUSED: /nope is not there\n");

    host.put_file(
        "/bad",
        0o644,
        (0, 0),
        format!("{}BOGUS_KEY=1\n", example()).as_bytes(),
    );
    let (code, _, err) = cli(
        &host,
        &net,
        &kit,
        &["install", "--config", "/bad", "--uninstall"],
    );
    assert_eq!(code, 2);
    assert!(
        err.starts_with("REFUSED: ") && err.contains("BOGUS_KEY"),
        "{err}"
    );

    // A settings file without the ACME keys still verifies (the certificate job is skipped).
    let plain: String = example()
        .lines()
        .filter(|l| !l.starts_with("MDM_ACME_"))
        .map(|l| format!("{l}\n"))
        .collect();
    host.put_file("/plain", 0o644, (0, 0), plain.as_bytes());
    let (code, out, _) = cli(&host, &net, &kit, &["verify", "--config", "/plain"]);
    assert_eq!(code, 1);
    assert!(
        out.contains("skipped  the certificate job's units"),
        "{out}"
    );
}
